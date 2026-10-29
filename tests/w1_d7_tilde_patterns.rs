//! DRIFT W1 D7 (upstream `6a51a39d4`, fixes #10765): `~` expands inside the
//! resource setting filter patterns (`!pattern`, `+path`, `-path`), so
//! `!~/skills/pdf` excludes the user's home skills instead of matching a
//! literal `~`. The compile step mirrors upstream `normalizePath` inside
//! `matchesAnyPattern`/`normalizeExactPattern`, and the absolute posix path
//! of each file participates in matching (upstream `filePathPosix`).
//!
//! Also locks the `applyPatterns` rule an excludes-only pattern list needs:
//! without a plain include, the starting set is every path.
//!
//! The second half covers the D7 follow-up: the **top-level** settings arrays
//! (`skills` / `extensions` / `prompts` / `themes` entries) as a filter layer
//! over discovered resources — upstream `resolveLocalEntries` (plain entries
//! are sources, `!`/`+`/`-` entries filter the files they yield) and
//! `isEnabledByOverrides` for the auto-discovered scope directories
//! (package-manager.ts:2396-2479, 724-791).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use pir::coding_agent::package_manager::{
    resolve_package_resources_with_home, TopLevelResourceFilter,
};
use pir::coding_agent::resources::{DiscoveredResources, ResourceDiscoveryOptions};
use pir::config::settings::{expand_tilde_in_pattern, SettingsManager};

/// Serializes the tests that mutate other process-global state (the current
/// directory). The `~` expansion itself is hermetic: every test passes the
/// home directory explicitly instead of mutating `HOME`.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn unique_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-w1-d7-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// One installed package under `$HOME/pkgs/one` with a skills directory.
fn write_package(home: &Path) -> PathBuf {
    let package_root = home.join("pkgs").join("one");
    for name in ["keep", "glob-excluded"] {
        let dir = package_root.join("skills").join(name);
        fs::create_dir_all(&dir).expect("create skill dir");
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name}\n---\n"),
        )
        .expect("write skill");
    }
    package_root
}

/// Write one `settings.json` with a single package entry carrying `skills`
/// filters, and resolve the package resources against an explicit home
/// directory (no ambient `$HOME` read).
fn resolve_with_filters(
    root: &Path,
    home: &Path,
    source: &str,
    skills_filters: serde_json::Value,
) -> Vec<String> {
    let agent_dir = root.join("agent");
    let cwd = root.join("project");
    let project_dir = cwd.join(".pi");
    fs::create_dir_all(&agent_dir).expect("agent dir");
    fs::create_dir_all(&project_dir).expect("project dir");
    let settings_json = serde_json::json!({
        "packages": [{ "source": source, "skills": skills_filters }],
    });
    fs::write(
        project_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings_json).expect("settings json"),
    )
    .expect("write settings");
    let settings = SettingsManager::load(&agent_dir, &project_dir, &cwd, true);
    resolve_package_resources_with_home(&settings, &agent_dir, &cwd, true, Some(home))
        .skills
        .iter()
        .map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect()
}

/// #10765: `!~/…` and `-~/…` patterns are expanded against `$HOME` and match
/// the file's absolute path, so the excluded skill is dropped while the
/// others are kept (upstream test: `keep` enabled, `glob-excluded` and
/// `exact-excluded` disabled).
#[test]
fn tilde_expands_in_exclude_and_force_exclude_patterns() {
    let root = unique_dir("tilde");
    let home = root.join("home");
    fs::create_dir_all(&home).expect("home dir");

    let package_root = write_package(&home);
    // `!~/…` addresses the package through `~`; the excluded skill is dropped
    // and the other one stays (upstream: `keep` enabled, `glob-excluded`
    // disabled).
    let kept = resolve_with_filters(
        &root,
        &home,
        &package_root.to_string_lossy(),
        serde_json::json!(["!~/pkgs/one/skills/glob-*"]),
    );
    // `-~/…` force-excludes through the same expansion (and wins over an
    // include when the two collide).
    let force_excluded = resolve_with_filters(
        &root,
        &home,
        &package_root.to_string_lossy(),
        serde_json::json!(["!~/pkgs/one/skills/glob-*", "-~/pkgs/one/skills/keep"]),
    );

    // The tilde helper itself: the rest of the pattern stays verbatim so the
    // glob survives (`None` = no home: the pattern stays literal).
    let expanded = expand_tilde_in_pattern("~/pkgs/one/skills/glob-*", Some(&home));
    assert!(
        expanded.ends_with("pkgs/one/skills/glob-*")
            || expanded.ends_with("pkgs\\one\\skills\\glob-*"),
        "the glob body survives the expansion: {expanded}"
    );
    assert_ne!(expanded, "~/pkgs/one/skills/glob-*", "the tilde expanded");
    assert_eq!(
        expand_tilde_in_pattern("plain/dir", Some(&home)),
        "plain/dir"
    );
    assert_eq!(expand_tilde_in_pattern("~/x", None), "~/x");

    assert_eq!(
        kept,
        vec!["keep".to_string()],
        "`!~/…` dropped the globbed skill"
    );
    assert!(
        force_excluded.is_empty(),
        "`-~/…` force-excluded the remaining skill: {force_excluded:?}"
    );
}

/// Without the tilde fix the pattern only ever matched a literal `~`; a
/// plain absolute pattern (no `~`) still exercises the same matching rule.
#[test]
fn absolute_exclude_pattern_matches_without_a_plain_include() {
    let root = unique_dir("absolute");
    let home = root.join("home");
    fs::create_dir_all(&home).expect("home dir");
    let package_root = write_package(&home);
    let kept = resolve_with_filters(
        &root,
        &home,
        &package_root.to_string_lossy(),
        serde_json::json!([format!(
            "!{}",
            package_root
                .join("skills")
                .join("glob-*")
                .to_string_lossy()
                .replace('\\', "/")
        )]),
    );
    assert_eq!(
        kept,
        vec!["keep".to_string()],
        "an excludes-only list keeps everything it does not exclude"
    );
}

// ---------------------------------------------------------------------------
// Top-level settings arrays: sources + `!`/`+`/`-` filter layer
// (upstream `resolveLocalEntries` / `isEnabledByOverrides`)
// ---------------------------------------------------------------------------

/// A discovery layout: a working directory with its `.pi` config dir and the
/// user-global agent dir.
struct Layout {
    #[allow(dead_code)]
    root: PathBuf,
    cwd: PathBuf,
    agent_dir: PathBuf,
    project_dir: PathBuf,
}

fn layout(tag: &str) -> Layout {
    let root = unique_dir(tag);
    let cwd = root.join("project");
    let agent_dir = root.join("agent");
    let project_dir = cwd.join(".pi");
    // A git marker at the fixture root: the ancestor `.agents/skills` walk
    // stops there, so the temp cwd (which lives under the real user profile)
    // can never pick up the machine's own `~/.agents/skills`.
    fs::create_dir_all(root.join(".git")).expect("git marker");
    fs::write(
        root.join(".git").join("HEAD"),
        "ref: refs/heads/main
",
    )
    .expect("git head");
    fs::create_dir_all(&agent_dir).expect("agent dir");
    fs::create_dir_all(&project_dir).expect("project dir");
    Layout {
        root,
        cwd,
        agent_dir,
        project_dir,
    }
}

/// One skill directory with a `SKILL.md` (the file upstream's pattern
/// matching resolves through its parent directory).
fn write_skill(parent: &Path, name: &str) {
    let dir = parent.join(name);
    fs::create_dir_all(&dir).expect("skill dir");
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name}\n---\nBody\n"),
    )
    .expect("write skill");
}

fn write_settings(path: &Path, value: serde_json::Value) {
    fs::write(
        path,
        serde_json::to_string_pretty(&value).expect("settings json"),
    )
    .expect("write settings");
}

/// Discover with an explicit home directory (`None` = no home available):
/// the `~` expansion never reads the ambient `$HOME`, so the caller decides
/// what it resolves against.
fn discover(layout: &Layout, trusted: bool, home: Option<&Path>) -> DiscoveredResources {
    DiscoveredResources::discover_with_home(
        &ResourceDiscoveryOptions {
            cwd: &layout.cwd,
            agent_dir: &layout.agent_dir,
            project_trusted: trusted,
            cli_skill_paths: &[],
            cli_prompt_paths: &[],
            no_skills: false,
            no_prompt_templates: false,
        },
        home,
    )
}

fn skill_names(resources: &DiscoveredResources) -> Vec<String> {
    let mut names: Vec<String> = resources
        .skills
        .iter()
        .map(|skill| skill.name.clone())
        .collect();
    names.sort();
    names
}

fn template_names(resources: &DiscoveredResources) -> Vec<String> {
    let mut names: Vec<String> = resources
        .prompt_templates
        .iter()
        .map(|template| template.name.clone())
        .collect();
    names.sort();
    names
}

/// The user scope: an excludes-only array keeps the auto-discovered skills it
/// does not exclude, and a `-path` force-excludes a skill a plain entry
/// listed. `-`/`!` address the skill directory (upstream matches a `SKILL.md`
/// through its parent directory, so no `…/SKILL.md` suffix is needed).
#[test]
fn top_level_skills_filter_listed_and_auto_discovered_entries() {
    let layout = layout("top-skills");
    // Auto-discovered user skills (`<agent_dir>/skills`).
    write_skill(&layout.agent_dir.join("skills"), "auto-keep");
    write_skill(&layout.agent_dir.join("skills"), "auto-drop");
    // Skills listed by a plain top-level entry.
    let listed = layout.root.join("extra");
    write_skill(&listed, "listed-a");
    write_skill(&listed, "listed-b");
    write_settings(
        &layout.agent_dir.join("settings.json"),
        serde_json::json!({
            "skills": [
                listed.to_string_lossy(),
                "!auto-drop",
                format!("-{}/listed-b", listed.to_string_lossy().replace('\\', "/")),
            ],
        }),
    );

    // Hermetic home: an empty fixture home so the REAL `~/.agents/skills`
    // (the user's installed skills) cannot join the expected list.
    let names = skill_names(&discover(&layout, true, Some(&layout.root.join("no-home"))));
    assert_eq!(
        names,
        vec!["auto-keep".to_string(), "listed-a".to_string()],
        "`!auto-drop` drops the auto skill, `-<path>` drops the listed one, \
         and the plain entry is the starting set"
    );
}

/// The project scope: a relative plain entry resolves against `<cwd>/.pi`
/// (upstream `resolvePathFromBase`) and the project array's `!` glob filters
/// the files it yields, matched against the `<cwd>/.pi`-relative path.
#[test]
fn top_level_skills_relative_entry_and_project_scope_filter() {
    let layout = layout("top-project");
    write_skill(&layout.project_dir.join("my-skills"), "keep");
    write_skill(&layout.project_dir.join("my-skills"), "skip-me");
    write_settings(
        &layout.project_dir.join("settings.json"),
        serde_json::json!({ "skills": ["my-skills", "!my-skills/skip-me"] }),
    );

    // Hermetic home: an empty fixture home so the REAL `~/.agents/skills`
    // (the user's installed skills) cannot join the expected list.
    let names = skill_names(&discover(&layout, true, Some(&layout.root.join("no-home"))));
    assert_eq!(
        names,
        vec!["keep".to_string()],
        "the project `!` glob excludes the skill directory it names"
    );

    // Untrusted project scope: the array (and its filter) never applies.
    let names = skill_names(&discover(&layout, false, None));
    assert!(
        names.is_empty(),
        "an untrusted project contributes nothing: {names:?}"
    );
}

/// Prompt templates: the plain entry expands `~`, and the `!` pattern
/// addresses the file through its expanded absolute path (`!~/…`).
#[test]
fn top_level_prompts_tilde_entry_and_exclude_pattern() {
    let layout = layout("top-prompts");
    let home = layout.root.join("home");
    fs::create_dir_all(&home).expect("home dir");

    let prompts = home.join("my-prompts");
    fs::create_dir_all(&prompts).expect("prompts dir");
    fs::write(prompts.join("keep.md"), "keep body\n").expect("template");
    fs::write(prompts.join("skip.md"), "skip body\n").expect("template");
    fs::write(prompts.join("other.md"), "other body\n").expect("template");
    write_settings(
        &layout.agent_dir.join("settings.json"),
        serde_json::json!({ "prompts": ["~/my-prompts", "!~/my-prompts/skip.md"] }),
    );

    let names = template_names(&discover(&layout, true, Some(&home)));
    assert_eq!(
        names,
        vec!["keep".to_string(), "other".to_string()],
        "`~/my-prompts` is the source, `!~/my-prompts/skip.md` filters it"
    );
}

/// Force patterns: `-path` excludes and `+path` re-includes a path the `!`
/// glob excluded, for every resource kind the top-level arrays carry
/// (`extensions` / `themes` have no discovery hook in pir yet, so they are
/// exercised through the filter itself — it is kind-agnostic).
#[test]
fn top_level_force_patterns_win_over_excludes() {
    let root = unique_dir("top-force");
    let home = root.join("home");
    fs::create_dir_all(&home).expect("home dir");

    let extensions = home.join("my-extensions");
    let themes = home.join("my-themes");
    let ext_keep = extensions.join("index.js");
    let ext_forced = extensions.join("forced.ts");
    let theme_forced = themes.join("brand.json");
    let theme_dropped = themes.join("old.json");
    for path in [&ext_keep, &ext_forced, &theme_forced, &theme_dropped] {
        fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        fs::write(path, "// fixture\n").expect("fixture file");
    }

    // Extensions: the `!` glob drops every entry script, `+` brings one back.
    let filter = TopLevelResourceFilter::from_entries_with_home(
        &[
            "!~/my-extensions/*".to_string(),
            format!("+{}", ext_forced.to_string_lossy().replace('\\', "/")),
        ],
        Some(&home),
    );
    assert!(!filter.keeps(&ext_keep, &home), "`!~/…/*` drops index.js");
    assert!(
        filter.keeps(&ext_forced, &home),
        "`+<path>` re-includes the forced script"
    );

    // Themes: `-` force-excludes even a path the plain source listed.
    let filter = TopLevelResourceFilter::from_entries_with_home(
        &[
            format!("-{}", theme_dropped.to_string_lossy().replace('\\', "/")),
            format!("+{}", theme_forced.to_string_lossy().replace('\\', "/")),
        ],
        Some(&home),
    );
    assert!(
        filter.keeps(&theme_forced, &home),
        "`+<path>` keeps the listed theme"
    );
    assert!(
        !filter.keeps(&theme_dropped, &home),
        "`-<path>` force-excludes the listed theme"
    );

    // Plain entries are sources, never filters: an array without overrides
    // keeps everything, and a plain path does not narrow the candidate set.
    let plain_only = TopLevelResourceFilter::from_entries_with_home(
        &["~/my-extensions".to_string(), "index.js".into()],
        Some(&home),
    );
    assert!(plain_only.is_empty());
    assert!(plain_only.keeps(&ext_keep, &home));
    assert!(plain_only.keeps(&theme_dropped, &home));
}

// ---------------------------------------------------------------------------
// The `themes` top-level array consumption point
// (`load_custom_themes` -> per-scope source + filter roots)
// ---------------------------------------------------------------------------

/// The posix spelling the compiled filter patterns match against.
fn posix(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// A valid theme document named `name` (the built-in dark palette's
/// vars/colors satisfy the required-token validation).
fn theme_json(name: &str) -> String {
    let vars: serde_json::Map<String, serde_json::Value> = pir::tui::theme::DARK_PALETTE
        .vars
        .iter()
        .map(|(key, value)| ((*key).to_string(), serde_json::json!(value)))
        .collect();
    let colors: serde_json::Map<String, serde_json::Value> = pir::tui::theme::DARK_PALETTE
        .colors
        .iter()
        .map(|(key, value)| ((*key).to_string(), serde_json::json!(value)))
        .collect();
    serde_json::json!({ "name": name, "vars": vars, "colors": colors }).to_string()
}

/// Restores the process cwd when the project-scope test drops it (also on
/// panic), so the other tests of this binary keep their cwd.
struct CwdGuard(PathBuf);

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

/// The user scope: the settings `themes` array is a source + filter layer
/// (plain entries register their files, `!` excludes), and the auto
/// `<agent_dir>/themes` dir still loads through the same entry point.
#[test]
fn themes_settings_entries_are_consumed_and_filtered_in_the_user_scope() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let root = unique_dir("themes-user");
    let agent_dir = root.join("agent");
    let themes_dir = agent_dir.join("themes");
    fs::create_dir_all(&themes_dir).expect("themes dir");

    // The auto-dir theme (the pre-settings discovery path).
    fs::write(themes_dir.join("auto.json"), theme_json("d7-auto-theme")).expect("auto theme");
    // Plain settings entries: one kept, one excluded by the `!` pattern.
    let listed = root.join("extra").join("listed.json");
    let dropped = root.join("extra").join("dropped.json");
    fs::create_dir_all(listed.parent().expect("parent")).expect("extra dir");
    fs::write(&listed, theme_json("d7-listed-theme")).expect("listed theme");
    fs::write(&dropped, theme_json("d7-dropped-theme")).expect("dropped theme");
    write_settings(
        &agent_dir.join("settings.json"),
        serde_json::json!({
            "themes": [
                posix(&listed),
                posix(&dropped),
                format!("!{}", posix(&dropped)),
            ],
        }),
    );

    let loaded = pir::tui::theme::load_custom_themes(&themes_dir).expect("load themes");
    assert!(
        loaded.names.contains(&"d7-auto-theme".to_string()),
        "the auto dir still loads: names {:?}, errors {:?}",
        loaded.names,
        loaded.errors
    );
    assert!(
        loaded.names.contains(&"d7-listed-theme".to_string()),
        "the plain entry registers: names {:?}, errors {:?}",
        loaded.names,
        loaded.errors
    );
    assert!(
        !loaded.names.contains(&"d7-dropped-theme".to_string()),
        "the `!` pattern dropped the excluded entry: {:?}",
        loaded.names
    );
    assert!(
        pir::tui::theme::custom_theme_names().contains(&"d7-listed-theme".to_string()),
        "the settings theme is registered globally"
    );
}

/// A settings-listed DIRECTORY expands to its `.json` files, the scope's
/// `!` glob filters them, `+path` restores one, and a missing settings path
/// is reported (upstream "theme path does not exist").
#[test]
fn themes_settings_directory_entry_force_patterns_and_missing_path() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let root = unique_dir("themes-force");
    let agent_dir = root.join("agent");
    let themes_dir = agent_dir.join("themes");
    fs::create_dir_all(&themes_dir).expect("themes dir");
    let dir = root.join("theme-dir");
    let keep = dir.join("keep.json");
    let other = dir.join("other.json");
    fs::create_dir_all(&dir).expect("theme dir");
    fs::write(&keep, theme_json("d7-force-keep")).expect("keep theme");
    fs::write(&other, theme_json("d7-force-other")).expect("other theme");
    let missing = root.join("missing.json");
    write_settings(
        &agent_dir.join("settings.json"),
        serde_json::json!({
            "themes": [
                posix(&dir),
                format!("!{}/*.json", posix(&dir)),
                format!("+{}", posix(&keep)),
                posix(&missing),
            ],
        }),
    );

    let loaded = pir::tui::theme::load_custom_themes(&themes_dir).expect("load themes");
    assert!(
        loaded.names.contains(&"d7-force-keep".to_string()),
        "`+path` restored the forced file: names {:?}, errors {:?}",
        loaded.names,
        loaded.errors
    );
    assert!(
        !loaded.names.contains(&"d7-force-other".to_string()),
        "`!<dir>/*.json` dropped the globbed file: {:?}",
        loaded.names
    );
    assert!(
        loaded
            .errors
            .iter()
            .any(|(file, error)| file.contains("missing.json") && error.contains("does not exist")),
        "the missing settings path is reported: {:?}",
        loaded.errors
    );
}

/// The project scope: a relative plain entry resolves against `<cwd>/.pi`
/// (upstream `resolvePathFromBase`), the project `!` pattern filters it, and
/// an untrusted project contributes nothing (`defaultProjectTrust`).
#[test]
fn themes_project_scope_is_trust_gated_and_resolves_relative_entries() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let root = unique_dir("themes-project");
    let agent_dir = root.join("agent");
    let cwd = root.join("project");
    let project_base = cwd.join(".pi");
    let themes_dir = agent_dir.join("themes");
    fs::create_dir_all(&themes_dir).expect("themes dir");
    fs::create_dir_all(&project_base).expect("project dir");
    let listed = project_base.join("extra").join("proj-listed.json");
    let dropped = project_base.join("extra").join("proj-dropped.json");
    fs::create_dir_all(listed.parent().expect("parent")).expect("extra dir");
    fs::write(&listed, theme_json("d7-proj-listed")).expect("listed theme");
    fs::write(&dropped, theme_json("d7-proj-dropped")).expect("dropped theme");
    write_settings(
        &project_base.join("settings.json"),
        serde_json::json!({
            "themes": [
                "extra/proj-listed.json",
                "extra/proj-dropped.json",
                "!extra/proj-dropped.json",
            ],
        }),
    );
    // `.pi` holds trust-requiring resources, so `defaultProjectTrust` (read
    // from the agent settings) decides the trust state.
    write_settings(
        &agent_dir.join("settings.json"),
        serde_json::json!({ "defaultProjectTrust": "never" }),
    );

    let previous = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&cwd).expect("enter project");
    let guard = CwdGuard(previous);

    let loaded = pir::tui::theme::load_custom_themes(&themes_dir).expect("untrusted load");
    assert!(
        loaded
            .names
            .iter()
            .all(|name| !name.starts_with("d7-proj-")),
        "an untrusted project contributes nothing: {:?}",
        loaded.names
    );

    write_settings(
        &agent_dir.join("settings.json"),
        serde_json::json!({ "defaultProjectTrust": "always" }),
    );
    let loaded = pir::tui::theme::load_custom_themes(&themes_dir).expect("trusted load");
    assert!(
        loaded.names.contains(&"d7-proj-listed".to_string()),
        "the relative project entry registered: names {:?}, errors {:?}",
        loaded.names,
        loaded.errors
    );
    assert!(
        !loaded.names.contains(&"d7-proj-dropped".to_string()),
        "`!extra/…` filtered the project entry: {:?}",
        loaded.names
    );

    drop(guard);
}
