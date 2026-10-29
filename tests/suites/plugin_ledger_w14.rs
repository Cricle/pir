//! W14-B: the plugin registration-failure LEDGER across registry rebuilds.
//!
//! The contract under test (the invariant, in one line): **a registration
//! failure must never disappear without the failed plugin also disappearing
//! from the configuration.**
//!
//! Why a rebuild is the interesting event: [`PluginHost::build`] isolates a
//! plugin whose `register()` fails by dropping it from the DISPATCH list, so
//! every later pass (a tool-registry refresh, an MCP discovery rebuild,
//! `/reload`) runs over a list that no longer contains it. Nothing in that
//! pass can re-observe the failure. A rebuild that simply REPLACED the ledger
//! with its own (empty) result therefore laundered a real failure into a clean
//! ledger — the same class of defect as the earlier `std::mem::take` one, and
//! the exact regression `sdk_defaults_w13::registration_failure_survives_a_registry_rebuild`
//! caught.
//!
//! The semantics pinned here:
//!
//! - an entry STANDS while its plugin is still configured (the host's plugin
//!   list is immutable after build, so "still configured" is exactly "this
//!   host");
//! - only a pass that actually RE-RAN a path may change that path's entry —
//!   a transient failure that heals is dropped, a new error is recorded, and
//!   neither is duplicated;
//! - a plugin removed from the configuration is observed as a new
//!   [`PluginHost::build`], whose ledger starts clean;
//! - `PluginHost::diagnostics()` stays the SAME set as the ledger: one error
//!   diagnostic per failed plugin, before and after a rebuild (never zero,
//!   never two).
//!
//! Everything here is hermetic: inline plugins only, no JS runtime, no
//! filesystem, no environment.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use pir::plugins::{AgentPlugin, PluginHost, PluginInfo, RegistrationContext};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// An inline plugin whose `register()` result is a shared, flippable cell, so
/// one host can be observed across several rebuilds in both directions.
struct LedgerPlugin {
    info: PluginInfo,
    healthy: Arc<AtomicBool>,
    /// How many times `register()` ran — proves whether a rebuild actually
    /// re-ran the path (and therefore owns its ledger entry).
    register_calls: Arc<AtomicUsize>,
}

impl LedgerPlugin {
    fn new(name: &str, healthy: bool) -> (Arc<Self>, Arc<AtomicBool>, Arc<AtomicUsize>) {
        let health = Arc::new(AtomicBool::new(healthy));
        let calls = Arc::new(AtomicUsize::new(0));
        let plugin = Arc::new(Self {
            info: PluginInfo::inline(name),
            healthy: Arc::clone(&health),
            register_calls: Arc::clone(&calls),
        });
        (plugin, health, calls)
    }

    /// The same plugin with no health handle (a permanently-failing or
    /// permanently-healthy participant).
    fn fixed(name: &str, healthy: bool) -> Arc<Self> {
        Self::new(name, healthy).0
    }
}

impl AgentPlugin for LedgerPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, _ctx: &mut RegistrationContext<'_>) -> Result<(), anyhow::Error> {
        self.register_calls.fetch_add(1, Ordering::SeqCst);
        if !self.healthy.load(Ordering::SeqCst) {
            return Err(anyhow::anyhow!("intentional register failure"));
        }
        Ok(())
    }
}

/// The ledger as `(path, message)` pairs — the shape the assertions read.
fn ledger(host: &PluginHost) -> Vec<(String, String)> {
    host.registration_failures()
        .into_iter()
        .map(|failure| (failure.path, failure.message))
        .collect()
}

/// The dispatch list's paths.
fn dispatch_paths(host: &PluginHost) -> Vec<String> {
    host.plugins()
        .iter()
        .map(|plugin| plugin.info().path.clone())
        .collect()
}

/// The `plugin failed to load:` error diagnostics the registry carries — the
/// half that must agree with the ledger one-for-one.
fn failure_diagnostics(host: &PluginHost) -> Vec<String> {
    host.diagnostics()
        .into_iter()
        .filter(|diagnostic| diagnostic.message.starts_with("plugin failed to load: "))
        .map(|diagnostic| diagnostic.path)
        .collect()
}

// ---------------------------------------------------------------------------
// (a) The ledger is populated by `build` itself
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn a_build_time_failure_is_in_the_ledger_and_off_the_dispatch_list() {
    let (failing, _health, calls) = LedgerPlugin::new("w14-failing", false);
    let host = PluginHost::build(None, vec![failing]);

    assert_eq!(
        ledger(&host),
        vec![(
            "<inline:w14-failing>".to_owned(),
            "intentional register failure".to_owned()
        )],
        "the plugin's own error is what the ledger carries"
    );
    assert!(!host.all_plugins_registered());
    assert_eq!(
        dispatch_paths(&host),
        Vec::<String>::new(),
        "an isolated failure never reaches the dispatch list"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "build runs one pass");
    assert_eq!(
        failure_diagnostics(&host),
        vec!["<inline:w14-failing>".to_owned()],
        "the registry carries exactly one diagnostic for the failure"
    );
}

// ---------------------------------------------------------------------------
// (b) The entry SURVIVES a rebuild while the plugin is still configured
// ---------------------------------------------------------------------------

/// The regression this wave fixes. The failed plugin is not on the dispatch
/// list, so the rebuild cannot re-observe it: its entry must be carried, not
/// recomputed away.
#[tokio::test(flavor = "current_thread")]
async fn a_standing_failure_survives_rebuilds_without_being_duplicated() {
    let host = PluginHost::build(None, vec![LedgerPlugin::fixed("w14-standing", false)]);
    assert_eq!(host.registration_failures().len(), 1);

    for rebuild in 1..=3 {
        host.rebuild_registry().await;
        assert_eq!(
            ledger(&host),
            vec![(
                "<inline:w14-standing>".to_owned(),
                "intentional register failure".to_owned()
            )],
            "rebuild #{rebuild} must not invent a clean ledger, and must not \
             duplicate the standing entry"
        );
        assert!(!host.all_plugins_registered());
        assert_eq!(
            failure_diagnostics(&host),
            vec!["<inline:w14-standing>".to_owned()],
            "rebuild #{rebuild} keeps ledger and diagnostics the same set \
             (one failure, reported once)"
        );
    }
}

// ---------------------------------------------------------------------------
// (c) Removal from the configuration is what clears the entry
// ---------------------------------------------------------------------------

/// A host's plugin list is immutable after build, so a plugin leaving the
/// configuration is observed as a NEW build (the `/reload`, `-e` list change
/// and settings-toggle paths all rebuild the host). That new host's ledger
/// starts clean — the entry follows the configuration, not a hidden cache.
#[tokio::test(flavor = "current_thread")]
async fn the_entry_clears_once_the_plugin_leaves_the_configuration() {
    let host = PluginHost::build(None, vec![LedgerPlugin::fixed("w14-dropped", false)]);
    assert_eq!(host.registration_failures().len(), 1);
    host.rebuild_registry().await;
    assert_eq!(host.registration_failures().len(), 1);

    // Same session, plugin no longer configured: a fresh build, no carry-over.
    let host = PluginHost::build(None, vec![LedgerPlugin::fixed("w14-survivor", true)]);
    assert_eq!(
        ledger(&host),
        Vec::<(String, String)>::new(),
        "a failure must not outlive its plugin's configuration"
    );
    assert!(host.all_plugins_registered());
    assert_eq!(
        failure_diagnostics(&host),
        Vec::<String>::new(),
        "no diagnostic survives the plugin either"
    );
    host.rebuild_registry().await;
    assert!(host.registration_failures().is_empty());
}

// ---------------------------------------------------------------------------
// (d) Two failures produce two entries, one each
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn two_failed_plugins_produce_two_entries_through_rebuilds() {
    let host = PluginHost::build(
        None,
        vec![
            LedgerPlugin::fixed("w14-alpha", false),
            LedgerPlugin::fixed("w14-beta", false),
        ],
    );
    assert_eq!(
        ledger(&host)
            .into_iter()
            .map(|(path, _)| path)
            .collect::<Vec<_>>(),
        vec![
            "<inline:w14-alpha>".to_owned(),
            "<inline:w14-beta>".to_owned()
        ],
        "first-failure order, one entry per plugin"
    );

    host.rebuild_registry().await;
    assert_eq!(
        ledger(&host)
            .into_iter()
            .map(|(path, _)| path)
            .collect::<Vec<_>>(),
        vec![
            "<inline:w14-alpha>".to_owned(),
            "<inline:w14-beta>".to_owned()
        ],
        "a rebuild neither drops nor duplicates a per-plugin entry"
    );
    assert_eq!(
        failure_diagnostics(&host),
        vec![
            "<inline:w14-alpha>".to_owned(),
            "<inline:w14-beta>".to_owned()
        ],
        "diagnostics stay in step with the ledger"
    );
}

// ---------------------------------------------------------------------------
// (e) Successful plugins are unaffected; a re-run path owns its own entry
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn a_successful_plugin_never_appears_in_the_ledger() {
    let calls = Arc::new(AtomicUsize::new(0));
    // `healthy` is the flippable cell the helper `LedgerPlugin` shares; this
    // test never flips it.
    let healthy = Arc::new(AtomicBool::new(true));
    let healthy_plugin = Arc::new(LedgerPlugin {
        info: PluginInfo::inline("w14-healthy"),
        healthy: Arc::clone(&healthy),
        register_calls: Arc::clone(&calls),
    });
    let host = PluginHost::build(
        None,
        vec![
            LedgerPlugin::fixed("w14-broken", false),
            healthy_plugin.clone(),
        ],
    );

    assert_eq!(
        dispatch_paths(&host),
        vec!["<inline:w14-healthy>".to_owned()],
        "only the surviving plugin dispatches"
    );
    assert_eq!(host.registration_failures().len(), 1);
    assert_eq!(host.registration_failures()[0].path, "<inline:w14-broken>");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // A rebuild re-runs the dispatch list, so the healthy plugin registers
    // again; it still never enters the ledger, and the sibling's standing
    // entry is neither cleared nor duplicated by that pass.
    host.rebuild_registry().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "a rebuild re-runs the dispatch list"
    );
    assert!(healthy.load(Ordering::SeqCst));
    assert_eq!(
        ledger(&host)
            .into_iter()
            .map(|(path, _)| path)
            .collect::<Vec<_>>(),
        vec!["<inline:w14-broken>".to_owned()],
        "a successful pass neither adds an entry nor clears an unrelated one"
    );
    assert_eq!(
        failure_diagnostics(&host),
        vec!["<inline:w14-broken>".to_owned()]
    );
}

/// A plugin that DID register at build and fails on a later pass is recorded by
/// that pass (it is the authoritative observer for its own path), and every
/// later pass records it again — the ledger must still hold exactly ONE entry,
/// not one per rebuild. This is the "a re-run path owns its entry" half of the
/// contract; the seed must not double-count it.
#[tokio::test(flavor = "current_thread")]
async fn a_dispatch_plugin_that_fails_later_is_recorded_once_per_path() {
    let (flaky, health, calls) = LedgerPlugin::new("w14-flaky", true);
    let host = PluginHost::build(None, vec![flaky]);
    assert!(host.registration_failures().is_empty());
    assert!(failure_diagnostics(&host).is_empty());
    assert_eq!(dispatch_paths(&host), vec!["<inline:w14-flaky>".to_owned()]);

    // The plugin degrades; the next pass observes it and records the failure.
    health.store(false, Ordering::SeqCst);
    host.rebuild_registry().await;
    assert_eq!(
        ledger(&host),
        vec![(
            "<inline:w14-flaky>".to_owned(),
            "intentional register failure".to_owned()
        )],
        "the pass that re-ran the path records its failure"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        failure_diagnostics(&host),
        vec!["<inline:w14-flaky>".to_owned()]
    );

    // The rebuild only refreshes registrations (the dispatch list is fixed at
    // build), so the path is re-run and re-recorded — with the SAME single
    // entry. Neither a duplicate nor a clean ledger.
    host.rebuild_registry().await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        ledger(&host),
        vec![(
            "<inline:w14-flaky>".to_owned(),
            "intentional register failure".to_owned()
        )],
        "a re-run path is recorded once, however many passes observe it"
    );
    assert_eq!(
        failure_diagnostics(&host),
        vec!["<inline:w14-flaky>".to_owned()]
    );

    // The path is still on the dispatch list (that list is fixed at build), so
    // a pass DOES re-run it: healing it is observable there, and only there.
    health.store(true, Ordering::SeqCst);
    host.rebuild_registry().await;
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert!(
        host.registration_failures().is_empty(),
        "a pass that re-ran the path and succeeded clears its entry: {:?}",
        ledger(&host)
    );
    assert!(failure_diagnostics(&host).is_empty());
    assert!(host.all_plugins_registered());

    // Recovery of a plugin that failed at BUILD is not observable this way: it
    // is off the dispatch list, so no pass re-runs it and its entry stands. A
    // fresh build of a healed plugin starts clean.
    let recovered = PluginHost::build(None, vec![LedgerPlugin::fixed("w14-flaky", true)]);
    assert!(recovered.registration_failures().is_empty());
    assert!(recovered.all_plugins_registered());
}
