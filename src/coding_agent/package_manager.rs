//! Package-management subcommands (upstream `src/package-manager-cli.ts`
//! subset): `pir install/remove/uninstall/update/list/config`.
//!
//! pir mirrors upstream `core/package-manager.ts` (`DefaultPackageManager`)
//! for the managed-source surface:
//!
//! - a *source* is a git URL (bare protocol URLs `https://…`, `ssh://…`,
//!   `git://…`, or the `git:`-prefixed shorthand `git:host/path` /
//!   `git:git@host:path`, with an optional `#ref`/`@ref` suffix), an
//!   `npm:<name>[@version]` registry package, or a local path;
//! - `install` clones git sources into `<agent-dir>/git/<host>/<path>`
//!   (project scope: `<cwd>/.pi/git/...`), installs npm sources with the
//!   configured `npmCommand` (default `npm`) into `<agent-dir>/npm`
//!   (project scope: `<cwd>/.pi/npm`; upstream `getNpmInstallRoot`,
//!   package-manager.ts:2085-2094), and records the source in the settings
//!   `packages` array (the upstream settings key, with an identity-matching
//!   entry replaced in place when it stores a different source form);
//! - `remove` drops the settings entry and the installed copy (git clone or
//!   `npm uninstall` in the scope root);
//! - `update` re-clones/pulls git packages at every scope they are configured
//!   at (user + project, upstream `update` matching per-scope identities) and
//!   refreshes non-pinned npm packages whose latest registry version is
//!   newer; `update --extensions`/`--extension <source>`/`--all` refresh the
//!   selected packages, `update --models` force-refreshes the remote model
//!   catalogs (upstream `refreshModelCatalogs`), and a target that includes
//!   `self`/`pi` (bare `update`, `--self`, `--all`, with or without
//!   `--force`) fails with the honest "cannot self-update" error instead of
//!   reporting success;
//! - cloned git packages with a `package.json` get their production
//!   dependencies installed with the configured npm command (upstream
//!   `installGit` → `getGitDependencyInstallArgs`); a missing npm binary
//!   degrades to a warning so the package still installs;
//! - package commands resolve project trust like upstream
//!   `createCommandSettingsManager`: an untrusted project's `.pi` settings
//!   are neither read nor written, `install/remove --local` and
//!   `config --local` require `--approve`, and `update` uses the saved trust
//!   decision only;
//! - `list` prints the installed-package listing; `config` prints/edits the
//!   package-RESOURCE surface: a line-oriented selector on a TTY (upstream
//!   `selectConfig` → `ConfigSelectorComponent`, reduced to the resource
//!   toggle), the read-only listing off a TTY, and the scriptable
//!   `--json`/`--enable/--disable <kind:pattern>` surface. A toggle writes the
//!   same `+pattern`/`-pattern` filter entry upstream's selector writes.
//!
//! Exit-code/text parity with upstream `handlePackageCommand`: missing
//! sources, unmatched removes, untrusted local writes, and command failures
//! all surface as errors (the CLI host prints `Error: …` and exits 1).
//!
//! Divergences from upstream: the full-screen `ConfigSelectorComponent`
//! (grouped tree, project inherit/load/unload cycling, live reload) is
//! replaced by the line-oriented toggle + scriptable flags, because the
//! subcommand runs before any interactive session exists; package extension
//! entry points (`pi` manifest / index files; upstream
//! `collectPackageResources`) are consumed for `config`'s resource rows but
//! `list` still reports the installed package directory only; npm/pnpm
//! self-update is not ported, and every npm/git child (including version
//! lookups) runs under a bounded deadline (`PIR_PACKAGE_COMMAND_TIMEOUT_MS`,
//! default 5 minutes) with the child killed on expiry.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use semver::{Version, VersionReq};
use serde_json::{json, Value};

use crate::config::settings::{SettingsManager, SettingsScope};

// ---------------------------------------------------------------------------
// Command surface
// ---------------------------------------------------------------------------

/// One parsed package command (upstream `PackageCommandOptions` subset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageCommand {
    /// `pir install <source> [-l|--local]`
    Install { source: String, local: bool },
    /// `pir remove|uninstall <source> [-l|--local]`
    Remove { source: String, local: bool },
    /// `pir update [source|self|pi] [--models|--extensions|--all]`
    Update {
        target: UpdateTarget,
        /// Upstream `--force`: re-install the pi package even when it is up
        /// to date. pir cannot self-update, so a self-including target still
        /// fails honestly.
        force: bool,
        /// Bare `pir update` prints the skipped-extensions note
        /// (upstream `showExtensionsSkippedNote`).
        show_extensions_skipped_note: bool,
    },
    /// `pir list`
    List,
    /// `pir config [-l|--local] [--json] [--enable <kind:pattern>]
    /// [--disable <kind:pattern>] [--source <package>]`
    ///
    /// Without flags the command is interactive on a TTY (select → write
    /// settings → confirmation) and the read-only listing otherwise. `--json`
    /// prints the machine-readable resource state; `--enable`/`--disable`
    /// write the same `+pattern`/`-pattern` package filters the interactive
    /// selection writes, so the whole surface is scriptable. The flags are
    /// parsed by the CLI (`Command::Config`) and forwarded here.
    Config {
        /// Show the project-scope configuration (<cwd>/.pi/settings.json)
        local: bool,
        /// Print the resource configuration as JSON (non-interactive)
        json: bool,
        /// Enable one package resource; selector `<kind>:<pattern>`
        enable: Option<String>,
        /// Disable one package resource (same selector shape as `enable`)
        disable: Option<String>,
        /// Restrict an `enable`/`disable` write to one package source
        source: Option<String>,
    },
}

/// One `pir update` target (upstream `UpdateTarget`,
/// package-manager-cli.ts:526-584). Built by
/// `crate::coding_agent::cli::Cli::update_request`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateTarget {
    /// `pir update` / `pir update self|pi`: upstream updates the pi package
    /// itself; pir reports that the Rust binary cannot self-update.
    SelfUpdate,
    /// `pir update <source>`: refresh the identity-matched configured
    /// packages at every scope they are installed at (upstream
    /// `updateTarget { type: "extensions", source }`).
    Package(String),
    /// `pir update --extensions`: refresh every configured package in both
    /// scopes (upstream `updateTarget { type: "extensions" }`).
    Extensions,
    /// `pir update --models`: force-refresh the remote model catalogs
    /// (upstream `updateTarget { type: "models" }`).
    Models,
    /// `pir update --all` (and `pir update self --extensions`): refresh all
    /// configured packages, then the pi package itself.
    All,
}

impl From<crate::coding_agent::cli::UpdateRequest> for UpdateTarget {
    fn from(request: crate::coding_agent::cli::UpdateRequest) -> Self {
        use crate::coding_agent::cli::UpdateRequest;
        match request {
            UpdateRequest::SelfUpdate => Self::SelfUpdate,
            UpdateRequest::Package(source) | UpdateRequest::Extension(source) => {
                Self::Package(source)
            }
            UpdateRequest::Extensions => Self::Extensions,
            UpdateRequest::Models => Self::Models,
            UpdateRequest::All => Self::All,
        }
    }
}

/// The project-trust decision for one package command (upstream
/// `createCommandSettingsManager`, package-manager-cli.ts:773-812):
///
/// - `update` uses the SAVED trust decision only (`useSavedProjectTrustOnly`),
///   so `--approve` cannot make update read an untrusted project's settings;
/// - every other command resolves the usual way (explicit `--approve` /
///   `--no-approve` override, trust-requiring resources, save selection,
///   `defaultProjectTrust`); headless resolution never prompts.
fn package_project_trusted(
    command: &PackageCommand,
    agent_dir: &Path,
    cwd: &Path,
    trust_override: Option<bool>,
) -> bool {
    let store = crate::config::trust::ProjectTrustStore::new(agent_dir);
    if matches!(command, PackageCommand::Update { .. }) {
        return store.get(cwd).is_some_and(|entry| entry.decision);
    }
    let default_project_trust =
        SettingsManager::load(agent_dir, &cwd.join(".pi"), cwd, false).default_project_trust();
    matches!(
        crate::config::trust::resolve_project_trusted(
            cwd,
            &store,
            trust_override,
            default_project_trust,
            false,
        ),
        crate::config::trust::ProjectTrustResolution::Resolved(true)
    )
}

/// Run one package command; returns `Err` for every upstream exit-1 path
/// (missing source, unmatched remove, clone/npm failure, unmatched update,
/// untrusted project config write).
pub fn run_package_command(command: &PackageCommand) -> crate::Result<()> {
    run_package_command_with_trust(command, None)
}

/// [`run_package_command`] with the CLI's `--approve`/`--no-approve` override
/// (upstream `projectTrustOverride`).
pub fn run_package_command_with_trust(
    command: &PackageCommand,
    trust_override: Option<bool>,
) -> crate::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let agent_dir = crate::coding_agent::context::agent_dir();
    let project_trusted = package_project_trusted(command, &agent_dir, &cwd, trust_override);
    // Untrusted projects' `.pi` settings are neither read nor written
    // (upstream `SettingsManager` guard): the manager owns both the load and
    // the write refusal.
    let mut settings = SettingsManager::load(&agent_dir, &cwd.join(".pi"), &cwd, project_trusted);

    match command {
        PackageCommand::Install { source, local } => {
            if *local && !project_trusted {
                return Err(anyhow::anyhow!(
                    "Project is not trusted. Use --approve to modify local package config."
                ));
            }
            install(&mut settings, source, *local, &agent_dir, &cwd)
        }
        PackageCommand::Remove { source, local } => {
            if *local && !project_trusted {
                return Err(anyhow::anyhow!(
                    "Project is not trusted. Use --approve to modify local package config."
                ));
            }
            remove(&mut settings, source, *local, &agent_dir, &cwd)
        }
        PackageCommand::Update {
            target,
            force,
            show_extensions_skipped_note,
        } => update(
            &settings,
            target,
            *force,
            *show_extensions_skipped_note,
            &agent_dir,
            &cwd,
        ),
        PackageCommand::List => list(&settings, &agent_dir, &cwd),
        PackageCommand::Config {
            local,
            json,
            enable,
            disable,
            source,
        } => {
            if *local && !project_trusted {
                return Err(anyhow::anyhow!(
                    "Project is not trusted. Use --approve to modify local resource config."
                ));
            }
            if let Some(selector) = enable {
                let message = set_config_resource_enabled(
                    &mut settings,
                    &agent_dir,
                    &cwd,
                    project_trusted,
                    *local,
                    selector,
                    source.as_deref(),
                    true,
                )?;
                // `--json` owns stdout: the confirmation is a diagnostic, so
                // a scripted consumer receives one parseable document.
                if *json {
                    eprintln!("{message}");
                } else {
                    println!("{message}");
                }
                if *json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&config_json(
                            &settings,
                            &agent_dir,
                            &cwd,
                            project_trusted,
                            *local
                        ))?
                    );
                }
                return Ok(());
            }
            if let Some(selector) = disable {
                let message = set_config_resource_enabled(
                    &mut settings,
                    &agent_dir,
                    &cwd,
                    project_trusted,
                    *local,
                    selector,
                    source.as_deref(),
                    false,
                )?;
                // See the `--enable` arm: no plain-text prefix in JSON mode.
                if *json {
                    eprintln!("{message}");
                } else {
                    println!("{message}");
                }
                if *json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&config_json(
                            &settings,
                            &agent_dir,
                            &cwd,
                            project_trusted,
                            *local
                        ))?
                    );
                }
                return Ok(());
            }
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&config_json(
                        &settings,
                        &agent_dir,
                        &cwd,
                        project_trusted,
                        *local
                    ))?
                );
                return Ok(());
            }
            // Upstream `selectConfig` opens the full-screen selector on a TTY.
            // pir's subcommand path runs before any TUI session exists, so the
            // equivalent line-oriented selection runs here; a non-TTY keeps
            // the read-only listing (CI/scripts have `--json`/`--enable`).
            if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                let stdin = std::io::stdin();
                let stdout = std::io::stdout();
                let mut reader = stdin.lock();
                let mut writer = stdout.lock();
                return run_config_interactive_with(
                    &mut settings,
                    &agent_dir,
                    &cwd,
                    project_trusted,
                    *local,
                    &mut reader,
                    &mut writer,
                );
            }
            print_config(&settings, *local, &agent_dir, &cwd)
        }
    }
}

// ---------------------------------------------------------------------------
// Source parsing (upstream `parseSource` + `parseGitUrl`; utils/git.ts)
// ---------------------------------------------------------------------------

/// A parsed package source (upstream `ParsedSource`). Exposed for the
/// source-parsing parity tests; production callers reach it through
/// [`run_package_command`].
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedSource {
    /// `npm:<spec>` — a registry package installed into the scope npm root.
    Npm(NpmSource),
    /// A git URL with a normalized `host/path` identity, the clone URL
    /// (without any ref suffix), and the pinned ref when one was given.
    Git {
        host: String,
        path: String,
        url: String,
        git_ref: Option<String>,
    },
    /// A filesystem path (relative paths resolve against the scope base dir).
    Local { path: String },
}

/// One `npm:` source (upstream `NpmSource`, package-manager.ts:142-149).
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpmSource {
    /// The trimmed spec behind `npm:` (verbatim input, version included).
    spec: String,
    /// Package name without the version (upstream `parseNpmSpec`).
    name: String,
    /// The raw version/range part, when the spec carries one.
    version: Option<String>,
    /// Exact-version pin (`isExactNpmVersion`): pinned sources are fixed and
    /// `update` never touches them (package-manager.ts:1132-1137).
    pinned: bool,
}

/// Parse a package source (upstream `parseSource`, package-manager.ts:1493):
/// `npm:` first, then a git URL, else a local path (the original string is
/// kept verbatim). Exposed for the parity tests.
#[doc(hidden)]
pub fn parse_source(source: &str) -> ParsedSource {
    if let Some(spec) = source.strip_prefix("npm:") {
        let spec = spec.trim().to_owned();
        let (name, version) = parse_npm_spec(&spec);
        return ParsedSource::Npm(NpmSource {
            pinned: is_exact_npm_version(version.as_deref()),
            spec,
            name,
            version,
        });
    }
    if let Some((host, path, url, git_ref)) = parse_git_source(source) {
        return ParsedSource::Git {
            host,
            path,
            url,
            git_ref,
        };
    }
    ParsedSource::Local {
        path: source.to_owned(),
    }
}

/// Parse the git-source forms of upstream `parseGitUrl` (utils/git.ts:172):
/// bare protocol URLs (`https://`, `http://`, `ssh://`, `git://`,
/// case-insensitive), and the `git:`-prefixed shorthand forms
/// (`git:host/path`, `git:git@host:path`, `git:<protocol-url>`). A `#ref`
/// fragment or `@ref` path suffix pins the checkout. Returns
/// `(host, path, clone_url, ref)`; `None` means "not a git source", so the
/// caller treats the input as a local path (upstream `parseSource`). Bare
/// `git@host:path` and bare `host/path` are local paths, matching upstream's
/// protocol gate (its `parseGitUrl` tests pin the rejection).
///
/// Divergences from upstream's quirks, on purpose: the `git:` strip there
/// also swallows the `git://` scheme (the bare form then parses into a
/// mangled `https:////…` clone URL for hosted providers), and a `#ref`
/// stays embedded in the clone URL for unrecognized hosts; pir treats
/// `git://` as a first-class protocol and strips `#ref`/`@ref` from the
/// clone URL for every form.
fn parse_git_source(source: &str) -> Option<(String, String, String, Option<String>)> {
    let trimmed = source.trim();
    let (repo, had_prefix) = match trimmed.strip_prefix("git:") {
        Some(rest) if !trimmed.starts_with("git://") => (rest.trim(), true),
        _ => (trimmed, false),
    };
    // Without the `git:` prefix only explicit protocol URLs qualify
    // (upstream `^(https?|ssh|git)://`, case-insensitive).
    let scheme_len = repo.find("://").filter(|scheme_end| {
        matches!(
            repo[..*scheme_end].to_ascii_lowercase().as_str(),
            "http" | "https" | "ssh" | "git"
        )
    });
    let scp_style = had_prefix && repo.starts_with("git@") && !repo.contains("://");
    let (host, path_with_ref, authority) = if scp_style {
        // `git:git@host:path` — scp-style (requires the `git:` prefix).
        let after = repo.strip_prefix("git@")?;
        let (host, path) = after.split_once(':')?;
        (host.to_owned(), path.to_owned(), String::new())
    } else if let Some(scheme_end) = scheme_len {
        let after = &repo[scheme_end + 3..];
        let (authority, path) = after.split_once('/')?;
        // Userinfo (`git@`) and a port are dropped from the host identity
        // but kept in the URL (upstream WHATWG `URL.hostname` semantics).
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let host = authority.split(':').next().unwrap_or(authority);
        (
            host.to_owned(),
            path.trim_start_matches('/').to_owned(),
            authority.to_owned(),
        )
    } else if had_prefix {
        // `git:host/path` shorthand; the host must look like a host (a dot or
        // `localhost`, upstream `parseGenericGitUrl`).
        let (host, path) = repo.split_once('/')?;
        if !host.contains('.') && host != "localhost" {
            return None;
        }
        (host.to_owned(), path.to_owned(), String::new())
    } else {
        return None;
    };
    // `#ref` fragment first, then an `@ref` path suffix (upstream
    // hosted-git-info committish + `splitRef`); both leave the path and the
    // clone URL.
    let (path, hash_ref) = match path_with_ref.split_once('#') {
        Some((path, reference)) if !reference.is_empty() => (path, Some(reference.to_owned())),
        _ => (path_with_ref.as_str(), None),
    };
    let (path, at_ref) = match path.split_once('@') {
        Some((path, reference)) if !path.is_empty() && !reference.is_empty() => {
            (path, Some(reference.to_owned()))
        }
        _ => (path, None),
    };
    let path = {
        let path = path.trim_end_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        path.trim_end_matches('/')
    };
    // Upstream `buildGitSource` (utils/git.ts:98): reject an absolute path,
    // an empty host/path, a one-segment path, and unsafe parts (NUL, `\`, a
    // leading `/`, a `..` segment, a malformed percent escape).
    if path.starts_with('/') || host.is_empty() || path.is_empty() {
        return None;
    }
    if path.split('/').count() < 2 {
        return None;
    }
    if has_unsafe_git_install_part(host.as_str(), false) || has_unsafe_git_install_part(path, true)
    {
        return None;
    }
    let url = if let Some(scheme_end) = scheme_len {
        format!("{}://{authority}/{path}", &repo[..scheme_end])
    } else if scp_style {
        format!("git@{host}:{path}")
    } else {
        format!("https://{host}/{path}")
    };
    Some((host, path.to_owned(), url, hash_ref.or(at_ref)))
}

/// Percent-decode one component for the safety gate (upstream
/// `decodeForValidation` via `decodeURIComponent`): `None` on a malformed
/// escape or an invalid UTF-8 byte sequence.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = (*bytes.get(index + 1)? as char).to_digit(16)?;
            let low = (*bytes.get(index + 2)? as char).to_digit(16)?;
            decoded.push((high * 16 + low) as u8);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Upstream `hasUnsafeGitInstallPart` (utils/git.ts:87): reject NUL, `\`, a
/// leading `/`, a `/` inside a host part, and any `..` segment — in the raw
/// value and in its percent-decoded form (a malformed escape is itself
/// unsafe, like upstream's `null` decode result).
fn has_unsafe_git_install_part(value: &str, allow_slash: bool) -> bool {
    let Some(decoded) = percent_decode(value) else {
        return true;
    };
    for candidate in [value, decoded.as_str()] {
        if candidate.contains('\0') || candidate.contains('\\') || candidate.starts_with('/') {
            return true;
        }
        if !allow_slash && candidate.contains('/') {
            return true;
        }
        if candidate.split('/').any(|segment| segment == "..") {
            return true;
        }
    }
    false
}

/// Split an npm spec into its name and optional version (upstream
/// `parseNpmSpec` regex `^(@?[^@]+(?:\/[^@]+)?)(?:@(.+))?$`,
/// package-manager.ts:1765-1773): the first `@` after a leading scope `@`
/// separates the two; a spec without a non-empty version part stays a name.
fn parse_npm_spec(spec: &str) -> (String, Option<String>) {
    let start = usize::from(spec.starts_with('@'));
    let Some(offset) = spec[start..].find('@') else {
        return (spec.to_owned(), None);
    };
    let index = start + offset;
    let version = &spec[index + 1..];
    // The name group needs one non-`@` character beyond a leading `@`, and
    // the version group needs at least one character.
    if version.is_empty() || index == start {
        return (spec.to_owned(), None);
    }
    (spec[..index].to_owned(), Some(version.to_owned()))
}

/// Whether a version string is an exact semver version (upstream
/// `isExactNpmVersion` via `semver.valid`, package-manager.ts:60-62). A
/// single leading `v`/`=` and surrounding whitespace are accepted, mirroring
/// the npm `semver` package.
fn is_exact_npm_version(version: Option<&str>) -> bool {
    let Some(version) = version else {
        return false;
    };
    let trimmed = version.trim();
    let trimmed = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('='))
        .unwrap_or(trimmed);
    parse_npm_version(trimmed).is_some()
}

/// Parse a strict `<major>.<minor>.<patch>[-prerelease][+build]` version into
/// its precedence-relevant parts; build metadata is validated but dropped
/// (semver precedence ignores it). The npm `semver` package accepts a wider
/// grammar — this covers the plain versions `npm view` returns.
fn parse_npm_version(version: &str) -> Option<(u64, u64, u64, Vec<String>)> {
    let (core_and_pre, build) = match version.split_once('+') {
        Some((left, build)) => (left, Some(build)),
        None => (version, None),
    };
    if let Some(build) = build {
        if !valid_semver_identifiers(build, false) {
            return None;
        }
    }
    let (core, prerelease) = match core_and_pre.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (core_and_pre, None),
    };
    let mut parts = core.split('.');
    let major = parse_version_number(parts.next()?)?;
    let minor = parse_version_number(parts.next()?)?;
    let patch = parse_version_number(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    let mut identifiers: Vec<String> = Vec::new();
    if let Some(prerelease) = prerelease {
        if !valid_semver_identifiers(prerelease, true) {
            return None;
        }
        identifiers = prerelease.split('.').map(str::to_owned).collect();
    }
    Some((major, minor, patch, identifiers))
}

/// Validate a dot-separated semver identifier list; numeric identifiers must
/// not carry leading zeros (prerelease only — build metadata is exempt).
fn valid_semver_identifiers(text: &str, numeric_no_leading_zero: bool) -> bool {
    text.split('.').all(|identifier| {
        if identifier.is_empty()
            || !identifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return false;
        }
        if numeric_no_leading_zero
            && identifier.len() > 1
            && identifier.starts_with('0')
            && identifier.chars().all(|c| c.is_ascii_digit())
        {
            return false;
        }
        true
    })
}

/// A numeric version component (ASCII digits, no leading zero unless `0`).
fn parse_version_number(text: &str) -> Option<u64> {
    if text.is_empty()
        || !text.chars().all(|c| c.is_ascii_digit())
        || (text.len() > 1 && text.starts_with('0'))
    {
        return None;
    }
    text.parse().ok()
}

/// Semver precedence comparison (upstream `semver.gt` in
/// `shouldUpdateNpmSource`, package-manager.ts:1190); `None` when either side
/// does not parse as a plain version.
fn compare_npm_versions(left: &str, right: &str) -> Option<Ordering> {
    let (left_major, left_minor, left_patch, left_pre) = parse_npm_version(left.trim())?;
    let (right_major, right_minor, right_patch, right_pre) = parse_npm_version(right.trim())?;
    let core = (left_major, left_minor, left_patch).cmp(&(right_major, right_minor, right_patch));
    if core != Ordering::Equal {
        return Some(core);
    }
    Some(compare_prerelease(&left_pre, &right_pre))
}

/// Prerelease precedence: a release outranks any prerelease; numeric
/// identifiers compare numerically and rank below alphanumeric ones.
fn compare_prerelease(left: &[String], right: &[String]) -> Ordering {
    match (left.is_empty(), right.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            for (left_identifier, right_identifier) in left.iter().zip(right) {
                let left_numeric = left_identifier.chars().all(|c| c.is_ascii_digit());
                let right_numeric = right_identifier.chars().all(|c| c.is_ascii_digit());
                let ordering = match (left_numeric, right_numeric) {
                    (true, true) => match (
                        left_identifier.parse::<u64>(),
                        right_identifier.parse::<u64>(),
                    ) {
                        (Ok(left_number), Ok(right_number)) => left_number.cmp(&right_number),
                        _ => left_identifier.cmp(right_identifier),
                    },
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    (false, false) => left_identifier.cmp(right_identifier),
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            left.len().cmp(&right.len())
        }
    }
}

/// One npm range component set after the partial-version grammar has been
/// applied (`1` / `1.2` / `1.2.x` / `1.2.3[-pre][+build]`).
struct NpmPartialVersion {
    major: u64,
    minor: Option<u64>,
    patch: Option<u64>,
    /// The `-prerelease` / `+build` suffix carried through verbatim.
    suffix: String,
}

impl NpmPartialVersion {
    /// The concrete lower bound npm substitutes for a partial version
    /// (`1.2` → `1.2.0`, `1` → `1.0.0`).
    fn floor(&self) -> String {
        format!(
            "{}.{}.{}{}",
            self.major,
            self.minor.unwrap_or(0),
            self.patch.unwrap_or(0),
            self.suffix
        )
    }

    /// The exclusive upper bound npm uses for a partial version: bump the
    /// last specified component (`1` → `<2.0.0`, `1.2` → `<1.3.0`).
    fn exclusive_upper(&self) -> String {
        match (self.minor, self.patch) {
            (Some(minor), None) => format!("{}.{}.0", self.major, minor + 1),
            (None, _) => format!("{}.0.0", self.major + 1),
            // Fully specified versions never take this path.
            (Some(minor), Some(patch)) => format!("{}.{}.{}", self.major, minor, patch + 1),
        }
    }

    /// The caret upper bound with npm's 0.x rules (`^1.2` → `<2.0.0`,
    /// `^0.2` → `<0.3.0`, `^0.0` → `<0.1.0`, `^0` → `<1.0.0`).
    fn caret_upper(&self) -> String {
        match (self.major, self.minor) {
            (0, None) => "<1.0.0".to_owned(),
            (0, Some(minor)) if minor > 0 => format!("<0.{}.0", minor + 1),
            (0, Some(_)) => "<0.1.0".to_owned(),
            (major, _) => format!("<{}.0.0", major + 1),
        }
    }
}

fn is_wildcard_npm_component(component: &str) -> bool {
    matches!(component, "*" | "x" | "X")
}

/// Parse one npm range component set (`^`-body), applying the partial-version
/// grammar (`1` / `1.2` / `1.2.x`). `None` when the text is not a valid range
/// component.
fn parse_npm_partial_version(body: &str) -> Option<NpmPartialVersion> {
    let body = body.trim();
    let body = body.strip_prefix('v').unwrap_or(body);
    let (core, suffix) = match body.find(['-', '+']) {
        Some(index) => (&body[..index], &body[index..]),
        None => (body, ""),
    };
    if core.is_empty() {
        return None;
    }
    let components: Vec<&str> = core.split('.').collect();
    if components.is_empty() || components.len() > 3 {
        return None;
    }
    let mut numbers: Vec<u64> = Vec::new();
    let mut wildcard = false;
    for (index, component) in components.iter().enumerate() {
        if is_wildcard_npm_component(component) {
            // Once a component is a wildcard every remaining one must be too
            // (`1.x.2` is not a valid npm range).
            if components[index..]
                .iter()
                .any(|component| !is_wildcard_npm_component(component))
            {
                return None;
            }
            wildcard = true;
            break;
        }
        if wildcard || component.is_empty() {
            return None;
        }
        numbers.push(parse_version_number(component)?);
    }
    if numbers.is_empty() {
        return None;
    }
    // A prerelease/build suffix needs the fully specified version npm's
    // grammar expects (`^1.2-beta` is not a valid range).
    if !suffix.is_empty() && (numbers.len() < 3 || wildcard) {
        return None;
    }
    Some(NpmPartialVersion {
        major: numbers[0],
        minor: numbers.get(1).copied(),
        patch: numbers.get(2).copied(),
        suffix: suffix.to_owned(),
    })
}

/// Compile one npm comparator (`^1.2.3`, `~1.2`, `>=1.0.0`, `1.2.x`, …).
/// `Ok(None)` is a comparator that matches every version (`*`); `Err(())`
/// marks the whole configured version as not-a-range.
fn normalize_npm_comparator(token: &str) -> Result<Option<String>, ()> {
    let (operator, body) = ["~>", ">=", "<=", ">", "<", "=", "^", "~"]
        .iter()
        .find_map(|operator| token.strip_prefix(operator).map(|rest| (*operator, rest)))
        .unwrap_or(("", token));
    let body = body.trim();
    if body.is_empty() || body == "*" || is_wildcard_npm_component(body) {
        return match operator {
            "" | "=" | "^" | "~" | "~>" | ">=" => Ok(None),
            _ => Err(()),
        };
    }
    if body.split('.').all(is_wildcard_npm_component) {
        // `x.x` / `*.*` are wildcard ranges too.
        return match operator {
            "" | "=" | "^" | "~" | "~>" | ">=" => Ok(None),
            _ => Err(()),
        };
    }
    let partial = parse_npm_partial_version(body).ok_or(())?;
    let fully_specified = partial.minor.is_some() && partial.patch.is_some();
    if fully_specified {
        let version = partial.floor();
        let operator = match operator {
            "" | "=" => "=",
            "~>" => "~",
            other => other,
        };
        return Ok(Some(format!("{operator}{version}")));
    }
    // `^1.2` keeps the caret grammar (the 0.x bumps depend on the major).
    if operator == "^" {
        return Ok(Some(format!(
            ">={}, <{}",
            partial.floor(),
            partial.caret_upper()
        )));
    }
    let floor = partial.floor();
    let comparator = match operator {
        // Bare `1` / `1.2` and `=` are wildcard ranges: `>=1.2.0 <1.3.0`.
        "" | "=" => format!(">={floor}, <{}", partial.exclusive_upper()),
        "~" | "~>" => format!(">={floor}, <{}", partial.exclusive_upper()),
        ">=" => format!(">={floor}"),
        "<" => format!("<{floor}"),
        // `>1.2` is `>=1.3.0`; `<=1.2` is `<1.3.0` (npm's partial semantics).
        ">" => format!(">={}", partial.exclusive_upper()),
        "<=" => format!("<{}", partial.exclusive_upper()),
        _ => return Err(()),
    };
    Ok(Some(comparator))
}

/// Normalize one `||` alternative into the `semver` crate's comparator
/// grammar (comma-separated). npm's hyphen ranges (`1.2.3 - 2.3.4`) and
/// whitespace-separated conjunctions (`>=1.0.0 <2.0.0`) are translated;
/// anything the npm range grammar rejects yields `None`.
fn normalize_npm_range_alternative(alternative: &str) -> Option<String> {
    let alternative = alternative.trim();
    if alternative.is_empty() {
        return Some("*".to_owned());
    }
    let tokens: Vec<&str> = alternative.split_whitespace().collect();
    if let Some(index) = tokens.iter().position(|token| *token == "-") {
        if index != 1 || tokens.len() != 3 {
            return None;
        }
        let lower = parse_npm_partial_version(tokens[0])?;
        let upper = parse_npm_partial_version(tokens[2])?;
        let upper = match (upper.minor, upper.patch) {
            (Some(_), Some(_)) => format!("<={}", upper.floor()),
            (Some(_), None) => format!("<{}", upper.exclusive_upper()),
            (None, _) => format!("<{}", upper.exclusive_upper()),
        };
        return Some(format!(">={}, {upper}", lower.floor()));
    }
    let mut comparators: Vec<String> = Vec::new();
    for token in alternative.split(|character: char| character.is_whitespace() || character == ',')
    {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if let Some(comparator) = normalize_npm_comparator(token).ok()? {
            comparators.push(comparator);
        }
    }
    if comparators.is_empty() {
        return Some("*".to_owned());
    }
    Some(comparators.join(", "))
}

/// The npm version ranges a configured `@version` denotes (upstream
/// `getNpmVersionRange`: `semver.validRange(version)`). `None` means "not a
/// range" — a dist-tag such as `latest`, or a form outside the supported
/// grammar — and any installed version counts as matching, exactly like
/// upstream's `source.range ? satisfies(...) : true`.
fn npm_version_ranges(version: &str) -> Option<Vec<VersionReq>> {
    let version = version.trim();
    if version.is_empty() {
        return None;
    }
    let mut ranges = Vec::new();
    for alternative in version.split("||") {
        let normalized = normalize_npm_range_alternative(alternative)?;
        let range = VersionReq::parse(&normalized).ok()?;
        ranges.push(range);
    }
    (!ranges.is_empty()).then_some(ranges)
}

/// Whether the installed copy satisfies the configured version
/// (upstream `installedNpmMatchesConfiguredVersion`,
/// package-manager.ts:1533-1539): no version part, a dist-tag, or a range the
/// translator does not understand always matches; otherwise the installed
/// `package.json` version must satisfy the range.
fn installed_npm_matches_configured_version(npm: &NpmSource, install_path: &Path) -> bool {
    let Some(version) = npm.version.as_deref() else {
        return true;
    };
    let Some(ranges) = npm_version_ranges(version) else {
        return true;
    };
    let Some(installed) = installed_npm_version(install_path) else {
        return false;
    };
    let Ok(installed) = Version::parse(installed.trim().trim_start_matches('v')) else {
        return false;
    };
    ranges.iter().any(|range| range.matches(&installed))
}

/// The upstream match key for a SETTINGS entry: npm by name, git by
/// `host/path`, local by the path resolved against the entry's scope base
/// (upstream `getSourceMatchKey*`). A stored local source is relative to the
/// base dir (see [`normalized_source_string`]), so it must resolve there —
/// not against the process cwd.
fn source_identity(source: &str, base_dir: &Path) -> String {
    source_identity_with(source, |path| local_path_from_base(path, base_dir))
}

/// The match key for a user-supplied CLI source: a relative local input
/// resolves against the invocation cwd (upstream `resolvePath`), unlike a
/// settings entry.
fn input_source_identity(source: &str, cwd: &Path) -> String {
    source_identity_with(source, |path| resolve_cli_local_path(path, cwd))
}

fn source_identity_with(source: &str, local: impl FnOnce(&str) -> PathBuf) -> String {
    match parse_source(source) {
        ParsedSource::Npm(npm) => format!("npm:{}", npm.name),
        ParsedSource::Git { host, path, .. } => format!("git:{host}/{path}"),
        ParsedSource::Local { path } => format!("local:{}", local(&path).display()),
    }
}

/// Whether a stored settings entry names the same source as a user-supplied
/// CLI argument. Local entries are stored relative to their scope base, while
/// a CLI argument resolves against the cwd; accept either resolution for the
/// entry so `remove`/`update` match both the stored form and the historical
/// pir form that stored cwd-relative paths.
fn sources_match(entry_source: &str, input: &str, base_dir: &Path, cwd: &Path) -> bool {
    let input_id = input_source_identity(input, cwd);
    source_identity(entry_source, base_dir) == input_id
        || input_source_identity(entry_source, cwd) == input_id
}

/// The string recorded in settings for a source (upstream
/// `normalizePackageSourceForSettings`: git/npm stay verbatim, local paths
/// are stored relative to the scope base dir). The input is a CLI argument, so
/// a relative path resolves against the invocation cwd before being made
/// relative to the base.
fn normalized_source_string(source: &str, base_dir: &Path, cwd: &Path) -> String {
    match parse_source(source) {
        ParsedSource::Local { path } => {
            let resolved = resolve_cli_local_path(&path, cwd);
            match resolved.strip_prefix(base_dir) {
                Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
                Ok(_) => ".".to_owned(),
                Err(_) => resolved.to_string_lossy().into_owned(),
            }
        }
        _ => source.to_owned(),
    }
}

/// Expand a leading `~` in a local package source.
fn expand_local_source_tilde(path: &str) -> PathBuf {
    if path == "~" {
        crate::session_manager::home_dir().unwrap_or_else(|| PathBuf::from(path))
    } else if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        match crate::session_manager::home_dir() {
            Some(home) => home.join(rest),
            None => PathBuf::from(path),
        }
    } else {
        PathBuf::from(path)
    }
}

/// Resolve a local SETTINGS entry against its scope base dir (upstream
/// `resolvePathFromBase`): `~` expansion, absolute paths unchanged, and a
/// relative path joined to the scope base.
fn local_path_from_base(path: &str, base_dir: &Path) -> PathBuf {
    let expanded = expand_local_source_tilde(path);
    if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    }
}

/// Resolve a local CLI argument against the invocation cwd (upstream
/// `resolvePath` for install sources).
fn resolve_cli_local_path(path: &str, cwd: &Path) -> PathBuf {
    let expanded = expand_local_source_tilde(path);
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

// ---------------------------------------------------------------------------
// Scope plumbing
// ---------------------------------------------------------------------------

fn scope_base_dir(local: bool, agent_dir: &Path, cwd: &Path) -> PathBuf {
    if local {
        cwd.join(".pi")
    } else {
        agent_dir.to_path_buf()
    }
}

fn git_install_root(local: bool, agent_dir: &Path, cwd: &Path) -> PathBuf {
    scope_base_dir(local, agent_dir, cwd).join("git")
}

/// Refuse install targets that escape the scope's install root (upstream
/// `resolveManagedPath`, package-manager.ts:2186-2193). Both parts are split
/// on `/` and `\` (a Windows `..\` must not slip through a `/`-only split)
/// and must be plain relative names: `..` segments, leading separators, and
/// drive-letter prefixes (`C:`) are rejected. The parser already filters
/// these shapes; this re-check covers hand-edited settings entries. Exposed
/// for the parity tests.
#[doc(hidden)]
pub fn scoped_git_target(root: &Path, host: &str, path: &str) -> crate::Result<PathBuf> {
    let target = root.join(host).join(path);
    let is_unsafe_part = |part: &str| {
        let bytes = part.as_bytes();
        let drive_prefixed =
            bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1) == Some(&b':');
        drive_prefixed
            || part.starts_with(['/', '\\'])
            || part.split(['/', '\\']).any(|segment| segment == "..")
    };
    if is_unsafe_part(host) || is_unsafe_part(path) {
        return Err(anyhow::anyhow!(
            "Refusing to use path outside package install root: {}",
            target.display()
        ));
    }
    Ok(target)
}

/// The `packages` array of one scope as raw JSON (`[]` when absent).
fn scope_packages(settings: &SettingsManager, scope: SettingsScope) -> Vec<Value> {
    let raw = match scope {
        SettingsScope::Global => &settings.global_raw,
        SettingsScope::Project => &settings.project_raw,
    };
    raw.get("packages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// The source string of one raw settings entry (string or `{source}` object).
fn package_entry_source(entry: &Value) -> String {
    match entry {
        Value::String(text) => text.clone(),
        Value::Object(map) => map
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    }
}

/// Whether a settings entry matches the input source (upstream
/// `packageSourcesMatch`: identity comparison against the entry's own scope).
fn entry_matches(
    entry: &Value,
    input: &str,
    scope: SettingsScope,
    agent_dir: &Path,
    cwd: &Path,
) -> bool {
    let entry_source = package_entry_source(entry);
    let base = match scope {
        SettingsScope::Project => cwd.join(".pi"),
        SettingsScope::Global => agent_dir.to_path_buf(),
    };
    sources_match(&entry_source, input, &base, cwd)
}

fn scope_base_dir_for(scope: SettingsScope, agent_dir: &Path, cwd: &Path) -> PathBuf {
    match scope {
        SettingsScope::Project => cwd.join(".pi"),
        SettingsScope::Global => agent_dir.to_path_buf(),
    }
}

/// Persist the packages array of one scope; surfaced load/write errors become
/// command errors (upstream exit 1).
fn persist_packages(
    settings: &mut SettingsManager,
    scope: SettingsScope,
    packages: Vec<Value>,
) -> crate::Result<()> {
    settings.persist_field(scope, "packages", Value::Array(packages));
    let errors = settings.drain_errors();
    if let Some(error) = errors.into_iter().next() {
        return Err(anyhow::anyhow!(
            "settings {}: {}",
            error.scope_name(),
            error.message
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// npm sources (upstream `NpmSource` handling)
// ---------------------------------------------------------------------------

/// The managed npm install root for one scope (upstream `getNpmInstallRoot`,
/// `package-manager.ts:2085-2094`): `<agent-dir>/npm` or `<cwd>/.pi/npm`.
fn npm_install_root(local: bool, agent_dir: &Path, cwd: &Path) -> PathBuf {
    scope_base_dir(local, agent_dir, cwd).join("npm")
}

/// Whether an npm package name can be joined under
/// `<root>/node_modules` without escaping it. Scoped names (`@scope/name`)
/// are allowed; empty names, absolute paths, backslashes (a Windows
/// separator), drive prefixes, and `.`/`..` segments are not. `npm:` is a
/// user/settings-supplied string and must not be trusted as a filesystem
/// component.
fn npm_install_name_is_safe(name: &str) -> bool {
    if name.is_empty()
        || name.contains('\0')
        || name.contains('\\')
        || name.starts_with(['/', '\\'])
    {
        return false;
    }
    let mut segments = name.split('/');
    let Some(first) = segments.next() else {
        return false;
    };
    if first.is_empty() || first == "." || first == ".." || first.as_bytes().get(1) == Some(&b':') {
        return false;
    }
    match segments.next() {
        None => true,
        Some(second) => {
            !second.is_empty()
                && second != "."
                && second != ".."
                // At most `@scope/name`: a third segment is not an npm name.
                && segments.next().is_none()
        }
    }
}

/// Reject an npm name that cannot be used as a managed filesystem component.
fn ensure_safe_npm_name(name: &str) -> crate::Result<()> {
    if npm_install_name_is_safe(name) {
        Ok(())
    } else {
        Err(anyhow::anyhow!("Invalid npm package name \"{name}\""))
    }
}

/// The managed install path of one npm package (upstream
/// `getManagedNpmInstallPath`, `package-manager.ts:2126-2135`). `None` when
/// the configured name would escape the managed root.
fn managed_npm_install_path(
    name: &str,
    local: bool,
    agent_dir: &Path,
    cwd: &Path,
) -> Option<PathBuf> {
    if !npm_install_name_is_safe(name) {
        return None;
    }
    Some(
        npm_install_root(local, agent_dir, cwd)
            .join("node_modules")
            .join(name),
    )
}

/// Prepare the npm install root like upstream `ensureNpmProject`
/// (`package-manager.ts:2062-2073`): create the directory, its
/// `*\n!.gitignore\n` ignore file, and a private `pi-extensions`
/// package.json stub. Upstream additionally stamps cloud-sync ignore xattrs
/// (`markPathIgnoredByCloudSync`, utils/paths.ts:125-140) — not ported.
fn ensure_npm_project(install_root: &Path) -> crate::Result<()> {
    std::fs::create_dir_all(install_root)?;
    let ignore_path = install_root.join(".gitignore");
    if !ignore_path.exists() {
        std::fs::write(&ignore_path, "*\n!.gitignore\n")?;
    }
    let package_json_path = install_root.join("package.json");
    if !package_json_path.exists() {
        std::fs::write(
            &package_json_path,
            "{\n  \"name\": \"pi-extensions\",\n  \"private\": true\n}",
        )?;
    }
    Ok(())
}

/// Prepare a git install root like upstream `ensureGitIgnore`
/// (package-manager.ts:2101-2109): create it and hide the managed clones
/// behind a `*\n!.gitignore\n` ignore file (project scope:
/// `<cwd>/.pi/git`).
fn ensure_git_ignore(install_root: &Path) -> crate::Result<()> {
    std::fs::create_dir_all(install_root)?;
    let ignore_path = install_root.join(".gitignore");
    if !ignore_path.exists() {
        std::fs::write(&ignore_path, "*\n!.gitignore\n")?;
    }
    Ok(())
}

/// The npm command from settings (upstream `getNpmCommand`,
/// `package-manager.ts:1781-1791`): the `npmCommand` argv list, default
/// `npm`; an empty first entry is the upstream config error.
fn npm_command_parts(settings: &SettingsManager) -> crate::Result<(String, Vec<String>)> {
    let configured = settings.settings.npm_command.as_deref().unwrap_or(&[]);
    let Some((command, args)) = configured.split_first() else {
        return Ok(("npm".to_owned(), Vec::new()));
    };
    if command.is_empty() {
        return Err(anyhow::anyhow!(
            "Invalid npmCommand: first array entry must be a non-empty command"
        ));
    }
    Ok((command.clone(), args.to_vec()))
}

/// Detect the package manager behind the configured npm command (upstream
/// `getPackageManagerName`, `package-manager.ts:1793-1814`): the command
/// basename with `.cmd`/`.exe` stripped, else the command wrapped after a
/// `--` argument, else a supported manager named among the args.
fn package_manager_name(command: &str, args: &[String]) -> crate::Result<String> {
    let normalize = |entry: &str| -> String {
        let base = Path::new(entry)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.to_owned());
        let lower = base.to_ascii_lowercase();
        if lower.ends_with(".cmd") || lower.ends_with(".exe") {
            base[..base.len() - 4].to_owned()
        } else {
            base
        }
    };
    const SUPPORTED: [&str; 3] = ["npm", "pnpm", "bun"];
    let direct = normalize(command);
    if let Some(index) = args.iter().rposition(|arg| arg == "--") {
        return Ok(args
            .get(index + 1)
            .map_or(direct, |wrapped| normalize(wrapped)));
    }
    if SUPPORTED.contains(&direct.as_str()) {
        return Ok(direct);
    }
    let mut wrapped: Vec<String> = Vec::new();
    for arg in args {
        let name = normalize(arg);
        if SUPPORTED.contains(&name.as_str()) && !wrapped.contains(&name) {
            wrapped.push(name);
        }
    }
    if wrapped.len() > 1 {
        return Err(anyhow::anyhow!(
            "Ambiguous npmCommand package managers: {}",
            wrapped.join(", ")
        ));
    }
    Ok(wrapped.into_iter().next().unwrap_or(direct))
}

/// The child argv for one npm invocation: the configured `npmCommand` prefix
/// (`argv[1..]`, e.g. a wrapper such as `["cmd", "/c", script]`) followed by
/// the npm-family subcommand args — upstream's
/// `runCommand(npmCommand[0], [...npmCommand.slice(1), ...args])`
/// (`package-manager.ts:1781-1791`). Every npm spawn (install, uninstall,
/// `view`, legacy global lookups, git dependency installs) goes through this
/// one assembly so the configured prefix is never dropped.
fn npm_argv(prefix: Vec<String>, args: Vec<String>) -> Vec<String> {
    let mut argv = prefix;
    argv.extend(args);
    argv
}

/// The npm-family install argv for a managed install (upstream
/// `getNpmInstallArgs`, `package-manager.ts:1845-1866`): peer dependency
/// resolution is disabled so the package manager never installs or solves
/// host-provided pi peers.
fn npm_install_args(package_manager: &str, specs: &[String], install_root: &Path) -> Vec<String> {
    let root = install_root.display().to_string();
    let mut args = vec!["install".to_owned()];
    args.extend(specs.iter().cloned());
    match package_manager {
        "bun" => args.extend(["--cwd".to_owned(), root, "--omit=peer".to_owned()]),
        "pnpm" => args.extend([
            "--prefix".to_owned(),
            root,
            "--config.auto-install-peers=false".to_owned(),
            "--config.strict-peer-dependencies=false".to_owned(),
            "--config.strict-dep-builds=false".to_owned(),
        ]),
        _ => args.extend(["--prefix".to_owned(), root, "--legacy-peer-deps".to_owned()]),
    }
    args
}

/// Default deadline for one package-manager / git child process. A hung
/// registry lookup or remote clone must not block startup (or an interactive
/// `pir install`) forever on a low-resource device; override with
/// `PIR_PACKAGE_COMMAND_TIMEOUT_MS` when a slow network legitimately needs
/// longer.
const DEFAULT_PACKAGE_COMMAND_TIMEOUT_MS: u64 = 300_000;

fn package_command_timeout() -> Duration {
    std::env::var("PIR_PACKAGE_COMMAND_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(DEFAULT_PACKAGE_COMMAND_TIMEOUT_MS))
}

/// Run one child process with a hard deadline, killing it when the deadline
/// expires. Output is optionally captured with reader threads so a chatty
/// child can never fill a pipe buffer and deadlock the wait loop.
fn run_command_bounded(
    command: &str,
    args: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
    capture: bool,
    inherit_stdin: bool,
) -> std::io::Result<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;

    fn spawn_reader(mut pipe: impl Read + Send + 'static) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut data = Vec::new();
            let _ = pipe.read_to_end(&mut data);
            let _ = sender.send(data);
        });
        receiver
    }

    fn collect(receiver: Option<std::sync::mpsc::Receiver<Vec<u8>>>) -> Vec<u8> {
        // The process already exited, so EOF normally arrives immediately; the
        // timeout only protects against a stray grandchild holding the pipe.
        receiver
            .and_then(|receiver| receiver.recv_timeout(Duration::from_secs(5)).ok())
            .unwrap_or_default()
    }

    let mut process = StdCommand::new(command);
    process.args(args);
    if let Some(cwd) = cwd {
        process.current_dir(cwd);
    }
    if !inherit_stdin {
        process.stdin(Stdio::null());
    }
    if capture {
        process.stdout(Stdio::piped());
        process.stderr(Stdio::piped());
    }
    let mut child = process.spawn()?;
    let stdout = if capture {
        child.stdout.take().map(spawn_reader)
    } else {
        None
    };
    let stderr = if capture {
        child.stderr.take().map(spawn_reader)
    } else {
        None
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
        if Instant::now() >= deadline {
            // Kill AND reap: a zombie child on Unix would otherwise keep the
            // parent's process slot until the host exits.
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "timed out after {}ms; the child process was killed",
                    timeout.as_millis()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    Ok(std::process::Output {
        status,
        stdout: collect(stdout),
        stderr: collect(stderr),
    })
}

/// Map a bounded-run failure to the user-facing error: timeouts name the
/// command and the killed child, launch failures keep the old wording.
fn command_run_error(command: &str, error: std::io::Error) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::TimedOut {
        anyhow::anyhow!("{command}: {error}")
    } else {
        anyhow::anyhow!("cannot run {command}: {error}")
    }
}

/// Run a command with inherited stdio (upstream `runCommand`,
/// `package-manager.ts:2732-2744`): npm output streams to the terminal.
///
/// stdin stays inherited on purpose: upstream's `isStdoutTakenOver()` branch
/// is the only one that rewires the stdio slots, and in pir this path is
/// reached solely from CLI subcommands, where stdin is the user's terminal
/// and npm/pnpm interactive prompts must be able to read it. The child is
/// bounded by [`package_command_timeout`] so a stuck install cannot hang the
/// command forever.
fn run_command(command: &str, args: &[String], cwd: Option<&Path>) -> crate::Result<()> {
    let output = run_command_bounded(command, args, cwd, package_command_timeout(), false, true)
        .map_err(|error| command_run_error(command, error))?;
    if output.status.success() {
        return Ok(());
    }
    let code = output
        .status
        .code()
        .map_or_else(|| "unknown".to_owned(), |code| code.to_string());
    Err(anyhow::anyhow!(
        "{command} {} failed with code {code}",
        args.join(" ")
    ))
}

/// [`run_command`] that reports a missing executable as `Ok(false)` instead of
/// an error, for optional dependency installation (a git package without npm
/// in the environment still installs, with a warning).
fn run_command_allow_missing(
    command: &str,
    args: &[String],
    cwd: Option<&Path>,
) -> crate::Result<bool> {
    let output =
        match run_command_bounded(command, args, cwd, package_command_timeout(), false, true) {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(command_run_error(command, error)),
        };
    if output.status.success() {
        return Ok(true);
    }
    let code = output
        .status
        .code()
        .map_or_else(|| "unknown".to_owned(), |code| code.to_string());
    Err(anyhow::anyhow!(
        "{command} {} failed with code {code}",
        args.join(" ")
    ))
}

/// [`run_command_capture`] that reports a missing executable as `Ok(None)`
/// (the startup install path; a git package without npm still installs with
/// a warning, matching [`run_command_allow_missing`]). Capturing keeps npm's
/// output off the host's stdout, which RPC/JSON sessions own.
fn run_command_capture_allow_missing(
    command: &str,
    args: &[String],
    cwd: Option<&Path>,
) -> crate::Result<Option<String>> {
    let output =
        match run_command_bounded(command, args, cwd, package_command_timeout(), true, false) {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(command_run_error(command, error)),
        };
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() {
        return Ok(Some(stdout.trim().to_owned()));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_status = output.status.code().map_or_else(
        || "signal unknown".to_owned(),
        |code| format!("code {code}"),
    );
    let detail = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    Err(anyhow::anyhow!(
        "{command} {} failed with {exit_status}: {detail}",
        args.join(" ")
    ))
}

/// Run a command capturing its output (upstream `runCommandCapture`,
/// `package-manager.ts:2646-2706`). Bounded by [`package_command_timeout`]
/// (the upstream `NETWORK_TIMEOUT_MS` kill timer): a hung registry lookup is
/// killed and reported instead of blocking the caller forever.
fn run_command_capture(
    command: &str,
    args: &[String],
    cwd: Option<&Path>,
) -> crate::Result<String> {
    let output = run_command_bounded(command, args, cwd, package_command_timeout(), true, false)
        .map_err(|error| command_run_error(command, error))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() {
        return Ok(stdout.trim().to_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_status = output.status.code().map_or_else(
        || "signal unknown".to_owned(),
        |code| format!("code {code}"),
    );
    let detail = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    Err(anyhow::anyhow!(
        "{command} {} failed with {exit_status}: {detail}",
        args.join(" ")
    ))
}

/// Install one npm source (upstream `installNpm`,
/// `package-manager.ts:1868-1872`).
fn install_npm(
    npm: &NpmSource,
    local: bool,
    settings: &SettingsManager,
    agent_dir: &Path,
    cwd: &Path,
) -> crate::Result<()> {
    ensure_safe_npm_name(&npm.name)?;
    let install_root = npm_install_root(local, agent_dir, cwd);
    ensure_npm_project(&install_root)?;
    let (command, args) = npm_command_parts(settings)?;
    let package_manager = package_manager_name(&command, &args)?;
    let install_args = npm_install_args(
        &package_manager,
        std::slice::from_ref(&npm.spec),
        &install_root,
    );
    run_command(&command, &npm_argv(args, install_args), None)
}

/// [`install_npm`] for the startup resolver: the child's stdout/stderr are
/// captured (and folded into the error) instead of inherited, so an RPC/JSON
/// host's stdout stays parseable while a missing package installs.
fn install_npm_captured(
    npm: &NpmSource,
    local: bool,
    settings: &SettingsManager,
    agent_dir: &Path,
    cwd: &Path,
) -> crate::Result<()> {
    ensure_safe_npm_name(&npm.name)?;
    let install_root = npm_install_root(local, agent_dir, cwd);
    ensure_npm_project(&install_root)?;
    let (command, args) = npm_command_parts(settings)?;
    let package_manager = package_manager_name(&command, &args)?;
    let install_args = npm_install_args(
        &package_manager,
        std::slice::from_ref(&npm.spec),
        &install_root,
    );
    run_command_capture(&command, &npm_argv(args, install_args), None).map(|_| ())
}

/// Uninstall one npm source from the scope root (upstream `uninstallNpm`,
/// `package-manager.ts:1874-1889`); a missing install root is a no-op.
fn uninstall_npm(
    npm: &NpmSource,
    local: bool,
    settings: &SettingsManager,
    agent_dir: &Path,
    cwd: &Path,
) -> crate::Result<()> {
    ensure_safe_npm_name(&npm.name)?;
    let install_root = npm_install_root(local, agent_dir, cwd);
    if !install_root.exists() {
        return Ok(());
    }
    let (command, args) = npm_command_parts(settings)?;
    let package_manager = package_manager_name(&command, &args)?;
    let root = install_root.display().to_string();
    let mut uninstall_args = vec!["uninstall".to_owned(), npm.name.clone()];
    if package_manager == "bun" {
        uninstall_args.extend(["--cwd".to_owned(), root]);
    } else {
        uninstall_args.extend(["--prefix".to_owned(), root]);
        if package_manager != "pnpm" {
            uninstall_args.push("--legacy-peer-deps".to_owned());
        }
    }
    run_command(&command, &npm_argv(args, uninstall_args), None)
}

/// The version field of an installed package.json (upstream
/// `getInstalledNpmVersion`, `package-manager.ts:1533-1543`).
fn installed_npm_version(install_path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(install_path.join("package.json")).ok()?;
    let value: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    value
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The latest version of a package spec (upstream `getLatestNpmVersion`,
/// `package-manager.ts:1545-1564`): `npm view <spec> version --json`. An
/// array response (range specs) yields the highest version; npm has already
/// applied the range, so picking the maximum mirrors `maxSatisfying`.
fn get_latest_npm_version(
    settings: &SettingsManager,
    package_spec: &str,
    cwd: &Path,
) -> crate::Result<String> {
    let (command, args) = npm_command_parts(settings)?;
    let view_args = vec![
        "view".to_owned(),
        package_spec.to_owned(),
        "version".to_owned(),
        "--json".to_owned(),
    ];
    let raw = run_command_capture(&command, &npm_argv(args, view_args), Some(cwd))?;
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(anyhow::anyhow!("Empty response from npm view"));
    }
    let parsed: Value = serde_json::from_str(raw)
        .map_err(|_| anyhow::anyhow!("Unexpected response from npm view"))?;
    if let Some(version) = parsed.as_str() {
        return Ok(version.to_owned());
    }
    if let Some(items) = parsed.as_array() {
        let mut best: Option<String> = None;
        for candidate in items
            .iter()
            .filter_map(Value::as_str)
            .filter(|version| !version.is_empty())
        {
            let replace = match &best {
                None => true,
                Some(current) => {
                    compare_npm_versions(candidate, current) == Some(Ordering::Greater)
                }
            };
            if replace {
                best = Some(candidate.to_owned());
            }
        }
        if let Some(best) = best {
            return Ok(best);
        }
    }
    Err(anyhow::anyhow!("Unexpected response from npm view"))
}

/// Whether an npm source has a newer version to install (upstream
/// `shouldUpdateNpmSource`, package-manager.ts:1181-1195): a missing install
/// always updates; lookup failures preserve upstream's update-anyway behavior.
fn should_update_npm(
    npm: &NpmSource,
    local: bool,
    settings: &SettingsManager,
    agent_dir: &Path,
    cwd: &Path,
) -> bool {
    let Some(install_path) = managed_npm_install_path(&npm.name, local, agent_dir, cwd) else {
        // An unsafe name cannot be inspected or installed; let the caller's
        // install path report it.
        return true;
    };
    let Some(installed) = installed_npm_version(&install_path) else {
        return true;
    };
    let package_spec = if npm.version.is_some() {
        &npm.spec
    } else {
        &npm.name
    };
    match get_latest_npm_version(settings, package_spec, cwd) {
        Ok(latest) => compare_npm_versions(&latest, &installed) == Some(Ordering::Greater),
        Err(_) => true,
    }
}

/// The legacy global npm install path used before the managed `<dir>/npm`
/// root existed (upstream `getNpmInstallPath` fallback +
/// `getGlobalNpmRoot`/`getPnpmGlobalPackagePath`, package-manager.ts:2096-2152).
/// Any lookup failure yields `None` (upstream wraps this in try/catch).
fn legacy_global_npm_install_path(settings: &SettingsManager, name: &str) -> Option<PathBuf> {
    let (command, args) = npm_command_parts(settings).ok()?;
    let package_manager = package_manager_name(&command, &args).ok()?;
    if package_manager == "pnpm" {
        let list_args = vec![
            "list".to_owned(),
            "-g".to_owned(),
            "--depth".to_owned(),
            "0".to_owned(),
            "--json".to_owned(),
        ];
        if let Ok(raw) = run_command_capture(&command, &npm_argv(args.clone(), list_args), None) {
            if let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(&raw) {
                for entry in entries {
                    let path = entry
                        .get("dependencies")
                        .and_then(|dependencies| dependencies.get(name))
                        .and_then(|dependency| dependency.get("path"))
                        .and_then(Value::as_str);
                    if let Some(path) = path {
                        return Some(PathBuf::from(path));
                    }
                }
            }
        }
    }
    let root = if package_manager == "bun" {
        let bin_args = vec!["pm".to_owned(), "bin".to_owned(), "-g".to_owned()];
        let bin = run_command_capture(&command, &npm_argv(args, bin_args), None).ok()?;
        Path::new(bin.trim())
            .parent()?
            .join("install")
            .join("global")
            .join("node_modules")
    } else {
        let root_args = vec!["root".to_owned(), "-g".to_owned()];
        PathBuf::from(
            run_command_capture(&command, &npm_argv(args, root_args), None)
                .ok()?
                .trim(),
        )
    };
    Some(root.join(name))
}

/// The upstream update no-match error (`buildNoMatchingPackageMessage`,
/// package-manager.ts:1430-1461) with its shorthand suggestion: a CLI
/// argument that matches a configured npm name/spec (or git shorthand) is
/// suggested back with the `npm:`/`git:` prefix.
fn no_matching_package_message(source: &str, entries: &[Value]) -> String {
    let trimmed = source.trim();
    let mut suggestion: Option<String> = None;
    for entry in entries {
        let entry_source = package_entry_source(entry);
        if entry_source.is_empty() {
            continue;
        }
        match parse_source(&entry_source) {
            ParsedSource::Npm(npm) => {
                if trimmed == npm.name || trimmed == npm.spec {
                    suggestion = Some(entry_source);
                    break;
                }
            }
            ParsedSource::Git {
                host,
                path,
                git_ref,
                ..
            } => {
                let shorthand = format!("{host}/{path}");
                let shorthand_with_ref = git_ref
                    .as_deref()
                    .map(|reference| format!("{shorthand}@{reference}"));
                if trimmed == shorthand || shorthand_with_ref.is_some_and(|value| trimmed == value)
                {
                    suggestion = Some(entry_source);
                    break;
                }
            }
            ParsedSource::Local { .. } => {}
        }
    }
    match suggestion {
        Some(suggestion) => {
            format!(
                "No matching package found for {}. Did you mean {}?",
                redact_url_credentials(source),
                redact_url_credentials(&suggestion)
            )
        }
        None => format!(
            "No matching package found for {}",
            redact_url_credentials(source)
        ),
    }
}

// ---------------------------------------------------------------------------
// Install / remove / update
// ---------------------------------------------------------------------------

fn install(
    settings: &mut SettingsManager,
    source: &str,
    local: bool,
    agent_dir: &Path,
    cwd: &Path,
) -> crate::Result<()> {
    if source.trim().is_empty() {
        return Err(anyhow::anyhow!("Missing install source."));
    }
    let scope = if local {
        SettingsScope::Project
    } else {
        SettingsScope::Global
    };
    let base = scope_base_dir(local, agent_dir, cwd);
    // Serialize with other pir processes installing/removing in this scope.
    // Explicit commands wait up to the full command deadline (the user asked
    // for a synchronous install); a session-level cache is not enough
    // because the competing writer may be another process.
    let _install_lock = acquire_package_install_lock(&base, package_command_timeout())?;
    match parse_source(source) {
        ParsedSource::Npm(npm) => install_npm(&npm, local, settings, agent_dir, cwd)?,
        ParsedSource::Git {
            host,
            path,
            url,
            git_ref,
        } => {
            let install_root = git_install_root(local, agent_dir, cwd);
            let target = scoped_git_target(&install_root, &host, &path)?;
            // Upstream `installGit` prepares the git root (and its
            // `.gitignore`) before cloning; project scope installs under
            // `<cwd>/.pi/git`.
            ensure_git_ignore(&install_root)?;
            git_checkout(&url, &target, git_ref.as_deref(), settings, false)?;
        }
        ParsedSource::Local { path } => {
            let resolved = resolve_cli_local_path(&path, cwd);
            if !resolved.exists() {
                return Err(anyhow::anyhow!(
                    "Path does not exist: {}",
                    resolved.display()
                ));
            }
        }
    }
    // Record the source (upstream `installAndPersist` → `addSourceToSettings`,
    // package-manager.ts:832-861): an identity-matching entry whose stored
    // source differs is replaced in place (`npm:foo@1` → `npm:foo@2`), else
    // the normalized source is appended.
    let mut packages = scope_packages(settings, scope);
    let normalized = normalized_source_string(source, &base, cwd);
    let match_index = packages.iter().position(|entry| {
        let entry_source = package_entry_source(entry);
        !entry_source.is_empty() && sources_match(&entry_source, source, &base, cwd)
    });
    match match_index {
        Some(index) => {
            if package_entry_source(&packages[index]) != normalized {
                packages[index] = match packages[index].clone() {
                    Value::Object(mut map) => {
                        map.insert("source".to_owned(), json!(normalized));
                        Value::Object(map)
                    }
                    _ => json!(normalized),
                };
            }
        }
        None => packages.push(json!(normalized)),
    }
    persist_packages(settings, scope, packages)?;
    println!("Installed {}", redact_url_credentials(source));
    Ok(())
}

fn remove(
    settings: &mut SettingsManager,
    source: &str,
    local: bool,
    agent_dir: &Path,
    cwd: &Path,
) -> crate::Result<()> {
    if source.trim().is_empty() {
        return Err(anyhow::anyhow!("Missing remove source."));
    }
    let scope = if local {
        SettingsScope::Project
    } else {
        SettingsScope::Global
    };
    // Serialize with installs/removals in this scope (npm uninstall and the
    // clone deletion must not race another process's install of the same
    // package).
    let base = scope_base_dir(local, agent_dir, cwd);
    let _install_lock = acquire_package_install_lock(&base, package_command_timeout())?;
    // Validate the settings entry BEFORE touching the installed copy: a
    // source that matches nothing in this scope must fail without
    // uninstalling a package that may belong to the other scope (or to
    // another project's shared global root).
    let packages = scope_packages(settings, scope);
    let kept: Vec<Value> = packages
        .iter()
        .filter(|entry| !entry_matches(entry, source, scope, agent_dir, cwd))
        .cloned()
        .collect();
    if kept.len() == packages.len() {
        // Upstream `removeAndPersist` → "No matching package found for …".
        return Err(anyhow::anyhow!("No matching package found for {source}"));
    }
    // Upstream `removeAndPersist` (package-manager.ts:1085-1088) removes the
    // installed copy before the settings entry; the npm arm uninstalls from
    // the scope root (a missing root is a no-op).
    if let ParsedSource::Npm(npm) = parse_source(source) {
        uninstall_npm(&npm, local, settings, agent_dir, cwd)?;
    }
    // Delete the cloned files for git sources (upstream `removeGit`).
    if let ParsedSource::Git { host, path, .. } = parse_source(source) {
        if let Ok(target) =
            scoped_git_target(&git_install_root(local, agent_dir, cwd), &host, &path)
        {
            if target.exists() {
                let _ = std::fs::remove_dir_all(&target);
            }
        }
    }
    persist_packages(settings, scope, kept)?;
    println!("Removed {}", redact_url_credentials(source));
    Ok(())
}

fn update(
    settings: &SettingsManager,
    target: &UpdateTarget,
    force: bool,
    show_extensions_skipped_note: bool,
    agent_dir: &Path,
    cwd: &Path,
) -> crate::Result<()> {
    match target {
        UpdateTarget::Models => update_model_catalogs(agent_dir),
        UpdateTarget::SelfUpdate => {
            // Upstream bare `update` / `update self` reports the skipped
            // extension refresh (showExtensionsSkippedNote) before the
            // self-update attempt.
            if show_extensions_skipped_note {
                println!(
                    "Extensions are skipped. Run pir update --extensions to update extensions."
                );
            }
            self_update_unavailable(force)
        }
        UpdateTarget::Extensions => {
            update_all_sources(settings, agent_dir, cwd)?;
            println!("Updated packages");
            Ok(())
        }
        UpdateTarget::All => {
            update_all_sources(settings, agent_dir, cwd)?;
            println!("Updated packages");
            // Upstream updates the pi package itself after the extensions;
            // the Rust binary has no self-update path, so this remains a real
            // failure instead of a silent success.
            self_update_unavailable(force)
        }
        UpdateTarget::Package(source) => {
            let matched = matched_configured_sources(settings, source, agent_dir, cwd);
            if matched.is_empty() {
                // Upstream `update(source)` throws when no configured package
                // matches (`buildNoMatchingPackageMessage`).
                let mut entries = scope_packages(settings, SettingsScope::Global);
                entries.extend(scope_packages(settings, SettingsScope::Project));
                return Err(anyhow::anyhow!(
                    "{}",
                    no_matching_package_message(source, &entries)
                ));
            }
            for (configured, local) in matched {
                update_configured_source(settings, &configured, local, agent_dir, cwd)?;
            }
            println!("Updated {}", redact_url_credentials(source));
            Ok(())
        }
    }
}

/// The honest self-update failure (upstream `printSelfUpdateUnavailable`,
/// package-manager-cli.ts:634-648): a Rust binary installed by cargo, a
/// distro package, or a source checkout has no self-update path. `--force`
/// does not change that, so both paths fail non-zero with the instruction
/// instead of the old silent success.
fn self_update_unavailable(_force: bool) -> crate::Result<()> {
    let executable = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_owned());
    Err(anyhow::anyhow!(
        "pir cannot self-update this installation.\n\
         Update pir using the package manager, wrapper, or source checkout that provides this installation.\n\n\
         Location of pir executable: {executable}"
    ))
}

/// Refresh every configured package in both scopes (upstream
/// `update(undefined)` matching all entries, package-manager.ts:1090-1115).
fn update_all_sources(
    settings: &SettingsManager,
    agent_dir: &Path,
    cwd: &Path,
) -> crate::Result<()> {
    for (local, scope) in [
        (false, SettingsScope::Global),
        (true, SettingsScope::Project),
    ] {
        for entry in scope_packages(settings, scope) {
            let entry_source = package_entry_source(&entry);
            if entry_source.is_empty() {
                continue;
            }
            update_configured_source(settings, &entry_source, local, agent_dir, cwd)?;
        }
    }
    Ok(())
}

/// The configured entries whose source identity matches `source`, in both
/// scopes (upstream `update(source)`'s identity matching,
/// package-manager.ts:1090-1110).
fn matched_configured_sources(
    settings: &SettingsManager,
    source: &str,
    agent_dir: &Path,
    cwd: &Path,
) -> Vec<(String, bool)> {
    let mut matched: Vec<(String, bool)> = Vec::new();
    for (local, scope) in [
        (false, SettingsScope::Global),
        (true, SettingsScope::Project),
    ] {
        let base = scope_base_dir_for(scope, agent_dir, cwd);
        for entry in scope_packages(settings, scope) {
            let entry_source = package_entry_source(&entry);
            if !entry_source.is_empty() && sources_match(&entry_source, source, &base, cwd) {
                matched.push((entry_source, local));
            }
        }
    }
    matched
}

/// Refresh one configured package at its scope (upstream
/// `updateConfiguredSources`, package-manager.ts:1119-1204): unpinned npm
/// packages reinstall when a newer registry version exists, git checkouts
/// re-pull (pinned refs reconcile an existing clone), local paths have
/// nothing to refresh. Offline mode skips the network work (upstream
/// `isOfflineModeEnabled`).
fn update_configured_source(
    settings: &SettingsManager,
    entry_source: &str,
    local: bool,
    agent_dir: &Path,
    cwd: &Path,
) -> crate::Result<()> {
    if crate::config::env::is_offline() {
        return Ok(());
    }
    // One package at a time across processes; `update_all_sources` loops over
    // several entries and each acquires the scope lock for its own refresh.
    let base_dir = scope_base_dir(local, agent_dir, cwd);
    let _install_lock = acquire_package_install_lock(&base_dir, package_command_timeout())?;
    match parse_source(entry_source) {
        ParsedSource::Npm(npm) => {
            ensure_safe_npm_name(&npm.name)?;
            // Pinned npm versions are fixed (upstream package-manager.ts:1132-1137).
            if npm.pinned {
                return Ok(());
            }
            if !should_update_npm(&npm, local, settings, agent_dir, cwd) {
                return Ok(());
            }
            // `updateNpmBatch`: `<name>@latest` for unpinned unversioned
            // sources, the configured spec otherwise.
            let spec = if npm.version.is_some() {
                npm.spec.clone()
            } else {
                format!("{}@latest", npm.name)
            };
            let install_root = npm_install_root(local, agent_dir, cwd);
            ensure_npm_project(&install_root)?;
            let (command, args) = npm_command_parts(settings)?;
            let package_manager = package_manager_name(&command, &args)?;
            let install_args =
                npm_install_args(&package_manager, std::slice::from_ref(&spec), &install_root);
            run_command(&command, &npm_argv(args, install_args), None)
        }
        ParsedSource::Git {
            host,
            path,
            url,
            git_ref,
        } => {
            // Re-pull the checkout at its own scope (upstream `updateGit`
            // per entry scope); `git_checkout` re-syncs an existing checkout
            // and clones when it is missing.
            let install_root = git_install_root(local, agent_dir, cwd);
            let checkout = scoped_git_target(&install_root, &host, &path)?;
            ensure_git_ignore(&install_root)?;
            git_checkout(&url, &checkout, git_ref.as_deref(), settings, false)
        }
        // Local-path sources are not update candidates (upstream
        // `updateConfiguredSources` collects npm and git entries only).
        ParsedSource::Local { .. } => Ok(()),
    }
}

/// Overall deadline for the `update --models` catalog refresh (upstream
/// `refreshModelCatalogs`, package-manager-cli.ts:605-631).
const MODEL_CATALOG_REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The bare upstream-style message of a strict-refresh error: the typed
/// [`AiError`] variant prefixes are stripped so the aggregate reads
/// `provider: Model catalog request failed for …` exactly like upstream
/// `error.message` (package-manager-cli.ts:625-627).
fn strict_refresh_error_text(error: &crate::ai::error::AiError) -> String {
    match error {
        crate::ai::error::AiError::NonRetryable(message)
        | crate::ai::error::AiError::Http(message) => message.clone(),
        _ => error.to_string(),
    }
}

/// Whether one provider has a resolvable credential: a stored `auth.json`
/// entry (upstream `readCredential`) or a set env API key (upstream
/// `envApiKeyAuth.resolve`). Upstream gates the network refresh of every
/// provider on `resolveRefreshCredential` returning a credential and skips
/// the provider silently otherwise (packages/ai/src/models.ts:581-583).
fn provider_credential_configured(agent_dir: &Path, provider_id: &str) -> bool {
    let storage = crate::ai::auth::FileAuthStorage::new(agent_dir.join("auth.json"));
    if storage
        .read_stored(provider_id)
        .map(|credential| credential.is_some())
        .unwrap_or(false)
    {
        return true;
    }
    crate::ai::auth::env_api_keys::all()
        .iter()
        .any(|(id, env_vars)| {
            *id == provider_id
                && env_vars.iter().any(|env_var| {
                    std::env::var(env_var)
                        .ok()
                        .is_some_and(|value| !value.trim().is_empty())
                })
        })
}

/// `pir update --models` (upstream `refreshModelCatalogs`,
/// package-manager-cli.ts:605-631): force-refresh every built-in catalog
/// provider's remote overlay (the same per-provider request the installed
/// [`crate::ai::providers::RemoteCatalogOverlay`]s run on their 4h cadence)
/// under a 15s overall deadline, using the strict refresh mode
/// ([`crate::ai::providers::RemoteCatalogOverlay::refresh_once_strict`]):
/// non-OK statuses (except 404/501, which skip) and transport errors
/// aggregate per provider into the upstream error text
/// `Could not refresh model catalogs: {provider}: {error}; …`.
///
/// Providers without a resolvable credential are skipped silently (upstream
/// `resolveRefreshCredential` gate, models.ts:581-583).
///
/// Each overlay is built with the provider's built-in personalizer
/// ([`crate::ai::providers::RemoteCatalogOverlay::new_with_provider_personalizer`],
/// upstream `withPersonalizedCatalog`), so the strict refresh applies the
/// same OpenRouter key-based personalization as the session-runtime install
/// path.
///
/// pir difference: the catalog base URL is overridable with
/// `PI_CATALOG_BASE_URL` (a pir feature; upstream hardcodes `https://pi.dev`
/// here as everywhere else). Public for tests/.
#[doc(hidden)]
pub fn update_model_catalogs(agent_dir: &Path) -> crate::Result<()> {
    use crate::ai::provider::ProviderRegistry;
    use crate::ai::providers::{RemoteCatalogOverlay, RemoteCatalogOverlayConfig};
    use std::sync::Arc;

    // Offline mode wins over an explicit refresh: the flag is documented to
    // disable network operations process-wide.
    if crate::config::env::is_offline() {
        println!("Offline mode: skipped model catalog refresh");
        return Ok(());
    }
    let base = std::env::var("PI_CATALOG_BASE_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| crate::ai::providers::REMOTE_CATALOG_DEFAULT_BASE_URL.to_string());
    let agent_dir = agent_dir.to_path_buf();
    let outcome = block_on_model_refresh(async move {
        // Upstream refreshes every provider concurrently (`Promise.all`,
        // packages/ai/src/models.ts:552-585); the overall 15s deadline then
        // bounds the slowest provider rather than the sum.
        let refresh = async {
            let registry = Arc::new(ProviderRegistry::default());
            let mut handles = Vec::new();
            for (provider_id, _) in crate::ai::providers::catalog::CATALOG_FILES {
                // Upstream credential gate (models.ts:581-583): a provider
                // whose credential cannot be resolved is skipped silently.
                if !provider_credential_configured(&agent_dir, provider_id) {
                    continue;
                }
                let config = RemoteCatalogOverlayConfig {
                    provider_id: (*provider_id).to_string(),
                    catalog_url: format!(
                        "{base}/api/models/providers/{provider_id}?types=chat,image,classifier"
                    ),
                    agent_dir: agent_dir.clone(),
                    interval: None,
                };
                let Ok(overlay) = RemoteCatalogOverlay::new_with_provider_personalizer(
                    config,
                    Arc::clone(&registry),
                ) else {
                    continue;
                };
                let provider_id = (*provider_id).to_string();
                handles.push(tokio::spawn(async move {
                    let result = Arc::new(overlay).refresh_once_strict().await;
                    (provider_id, result)
                }));
            }
            let mut errors: Vec<(String, String)> = Vec::new();
            for handle in handles {
                match handle.await {
                    Ok((_provider_id, Ok(_updated))) => {}
                    Ok((provider_id, Err(error))) => {
                        errors.push((provider_id, strict_refresh_error_text(&error)))
                    }
                    Err(error) => errors.push(("refresh".to_string(), error.to_string())),
                }
            }
            errors
        };
        match tokio::time::timeout(MODEL_CATALOG_REFRESH_TIMEOUT, refresh).await {
            Ok(errors) => Err(errors),
            Err(_elapsed) => Ok(()),
        }
    })?;
    match outcome {
        // The overall deadline elapsed (upstream "Model catalog refresh
        // timed out.").
        Ok(()) => Err(anyhow::anyhow!("Model catalog refresh timed out.")),
        Err(errors) if !errors.is_empty() => {
            let details = errors
                .iter()
                .map(|(provider, error)| format!("{provider}: {error}"))
                .collect::<Vec<_>>()
                .join("; ");
            Err(anyhow::anyhow!(
                "Could not refresh model catalogs: {details}"
            ))
        }
        Err(_) => {
            println!("Model catalogs refreshed");
            Ok(())
        }
    }
}

/// Drive the model-catalog refresh future to completion from a sync command
/// path that may already be inside the process runtime.
///
/// `pir update --models` is dispatched from the async `run()` body, so a
/// plain nested `Runtime::block_on` would panic ("Cannot start a runtime from
/// within a runtime"). When a runtime is current, the future runs on a
/// dedicated thread with its own runtime (never blocking the host executor's
/// scheduler); outside any runtime a small runtime is built in place.
fn block_on_model_refresh<F>(future: F) -> crate::Result<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map(|runtime| runtime.block_on(future))
        })
        .join()
        .map_err(|_| anyhow::anyhow!("model catalog refresh worker panicked"))?
        .map_err(|error| anyhow::anyhow!("Model catalog refresh could not start: {error}"));
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| anyhow::anyhow!("Model catalog refresh could not start: {error}"))?;
    Ok(runtime.block_on(future))
}

/// Clone (or re-sync) a git source into `target`. A pinned `reference` is
/// checked out after the clone, then the package's npm dependencies are
/// installed when it has a `package.json` (upstream `installGit`,
/// package-manager.ts:1900-1944); an existing checkout is re-synced and
/// missing dependencies are repaired. A failed fresh install removes the
/// partial clone (upstream's catch block).
fn git_checkout(
    url: &str,
    target: &Path,
    reference: Option<&str>,
    settings: &SettingsManager,
    quiet: bool,
) -> crate::Result<()> {
    if target.exists() {
        if target.join(".git").exists() {
            sync_git_checkout(target, reference)?;
            return repair_missing_git_dependencies(target, settings, quiet);
        }
        // Never delete a pre-existing directory that is not a checkout: it may
        // be a user tree or a clone another process is still producing.
        return Err(anyhow::anyhow!(
            "Cannot install {}: {} already exists and is not a git checkout",
            redact_url_credentials(url),
            target.display()
        ));
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Clone into a private staging directory and move it into place only after
    // the checkout and its dependency install succeeded. A crash or failure
    // therefore never leaves a half-populated managed checkout, and a
    // concurrent process that installs the same source first keeps its tree
    // instead of having it deleted by our cleanup.
    let Some(parent) = target.parent() else {
        return Err(anyhow::anyhow!(
            "Cannot install {}: {} has no parent directory",
            redact_url_credentials(url),
            target.display()
        ));
    };
    let target_name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "checkout".to_owned());
    static STAGE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let staging = parent.join(format!(
        ".{target_name}.pir-clone-{}-{}",
        std::process::id(),
        STAGE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let staged = (|| -> crate::Result<()> {
        run_git(&["clone", url, &staging.display().to_string()])?;
        if let Some(reference) = reference {
            run_git(&["-C", &staging.display().to_string(), "checkout", reference])?;
        }
        install_git_dependencies_impl(&staging, settings, quiet)
    })();
    if let Err(error) = staged {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    match std::fs::rename(&staging, target) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            // Another process may have completed the same checkout while we
            // were staging ours (its directory appeared first): keep it.
            if target.join(".git").exists() {
                return Ok(());
            }
            Err(anyhow::anyhow!(
                "Cannot install {}: cannot move it into {}: {error}",
                redact_url_credentials(url),
                target.display()
            ))
        }
    }
}

/// Bring an existing checkout to its configured state (upstream
/// `ensureGitRef`): a pinned ref is fetched and hard-reset to its commit; an
/// unpinned checkout fast-forwards its current branch.
fn sync_git_checkout(target: &Path, reference: Option<&str>) -> crate::Result<()> {
    let dir = target.display().to_string();
    match reference {
        Some(reference) => {
            run_git(&["-C", &dir, "fetch", "origin", reference])?;
            run_git(&[
                "-C",
                &dir,
                "reset",
                "--hard",
                &format!("{reference}^{{commit}}"),
            ])
        }
        None => run_git(&["-C", &dir, "pull", "--ff-only"]),
    }
}

/// The dependency-install argv for a cloned git package (upstream
/// `getGitDependencyInstallArgs`, package-manager.ts:1846-1864): production
/// dependencies only, without installing or solving host-provided pi peers.
/// Exposed for the parity tests.
#[doc(hidden)]
pub fn git_dependency_install_args(package_manager: &str) -> Vec<String> {
    let args: &[&str] = match package_manager {
        "bun" => &["install", "--omit=dev", "--omit=peer"],
        "pnpm" => &[
            "install",
            "--prod",
            "--config.auto-install-peers=false",
            "--config.strict-peer-dependencies=false",
            "--config.strict-dep-builds=false",
        ],
        "npm" => &["install", "--omit=dev", "--legacy-peer-deps"],
        _ => &["install"],
    };
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// Install a cloned git package's dependencies with the configured npm
/// command (upstream `installGit`'s `package.json` arm,
/// package-manager.ts:1942-1944). A missing npm binary degrades to a warning:
/// the extension is still installed, matching the task's "no npm in the
/// environment" fallback. Exposed for the parity tests.
#[doc(hidden)]
pub fn install_git_dependencies(target: &Path, settings: &SettingsManager) -> crate::Result<()> {
    install_git_dependencies_impl(target, settings, false)
}

/// [`install_git_dependencies`] with the startup resolver's output mode:
/// `quiet` captures the child instead of inheriting its stdout, so an
/// RPC/JSON host's protocol stream stays clean.
fn install_git_dependencies_impl(
    target: &Path,
    settings: &SettingsManager,
    quiet: bool,
) -> crate::Result<()> {
    if !target.join("package.json").is_file() {
        return Ok(());
    }
    let (command, args) = npm_command_parts(settings)?;
    let package_manager = package_manager_name(&command, &args)?;
    let argv = npm_argv(args, git_dependency_install_args(&package_manager));
    let installed = if quiet {
        run_command_capture_allow_missing(&command, &argv, Some(target))?.is_some()
    } else {
        run_command_allow_missing(&command, &argv, Some(target))?
    };
    if !installed {
        eprintln!(
            "Warning: cannot install dependencies for {}: `{command}` was not found on PATH. The package may fail to load.",
            target.display()
        );
    }
    Ok(())
}

/// Whether a cloned git package declares dependencies that are not present
/// under `node_modules` (upstream `hasMissingGitDependencies`,
/// package-manager.ts:1984-2012). Exposed for the parity tests.
#[doc(hidden)]
pub fn git_package_has_missing_dependencies(target: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(target.join("package.json")) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) else {
        return false;
    };
    let Some(dependencies) = manifest.get("dependencies").and_then(Value::as_object) else {
        return false;
    };
    let node_modules = target.join("node_modules");
    dependencies.keys().any(|name| {
        // Upstream's `resolve(...).startsWith(nodeModules + sep)` guard: a
        // dependency name may not escape the package directory.
        if name.is_empty()
            || name.starts_with(['/', '\\'])
            || name.split(['/', '\\']).any(|segment| segment == "..")
        {
            return false;
        }
        !node_modules.join(name).exists()
    })
}

/// Reinstall dependencies a synced checkout is missing (upstream
/// `repairMissingGitDependencies`, package-manager.ts:1993-1995). Called after
/// an update re-sync, so a dependency added by a new revision is installed.
fn repair_missing_git_dependencies(
    target: &Path,
    settings: &SettingsManager,
    quiet: bool,
) -> crate::Result<()> {
    if git_package_has_missing_dependencies(target) {
        install_git_dependencies_impl(target, settings, quiet)?;
    }
    Ok(())
}

/// Redact URL userinfo (`scheme://user:secret@host`) from child-tool output.
/// Git echoes the clone URL in some failures (`repository 'https://user:token@host/x' not found`),
/// and a git source may embed a credential; the message is also cached and
/// replayed as a startup warning, so it must not carry the secret.
/// `#[doc(hidden)]` so the parity tests can pin the rule.
#[doc(hidden)]
pub fn redact_url_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find("://") {
        let (prefix, after_scheme) = rest.split_at(index + 3);
        out.push_str(prefix);
        let authority_end = after_scheme
            .find(|ch: char| ch.is_whitespace() || matches!(ch, '/' | '\'' | '"' | ')' | '>' | '<'))
            .unwrap_or(after_scheme.len());
        let authority = &after_scheme[..authority_end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***");
                out.push_str(&authority[at..]);
            }
            None => out.push_str(authority),
        }
        rest = &after_scheme[authority_end..];
    }
    out.push_str(rest);
    out
}

fn run_git(args: &[&str]) -> crate::Result<()> {
    let owned: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let output = run_command_bounded("git", &owned, None, package_command_timeout(), true, false)
        .map_err(|error| {
        // `git clone/fetch/pull` are network operations: the same bound the
        // npm paths use keeps a dead remote from wedging startup.
        if error.kind() == std::io::ErrorKind::TimedOut {
            anyhow::anyhow!("git {} {error}", args.first().unwrap_or(&""))
        } else {
            anyhow::anyhow!("cannot run git: {error}")
        }
    })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(anyhow::anyhow!(
        "git {} failed: {}",
        args.first().unwrap_or(&""),
        redact_url_credentials(stderr.trim())
    ))
}

// ---------------------------------------------------------------------------
// list / config
// ---------------------------------------------------------------------------

/// One configured package row (upstream `ConfiguredPackage`).
struct ConfiguredPackage {
    source: String,
    scope: &'static str,
    filtered: bool,
    installed_path: Option<PathBuf>,
}

fn configured_packages(
    settings: &SettingsManager,
    agent_dir: &Path,
    cwd: &Path,
) -> Vec<ConfiguredPackage> {
    let mut rows: Vec<ConfiguredPackage> = Vec::new();
    for (scope_name, scope, local) in [
        ("user", SettingsScope::Global, false),
        ("project", SettingsScope::Project, true),
    ] {
        for entry in scope_packages(settings, scope) {
            let source = package_entry_source(&entry);
            if source.is_empty() {
                continue;
            }
            let base = scope_base_dir_for(scope, agent_dir, cwd);
            // Read-only listing: never ask npm/pnpm/bun for the legacy global
            // root (that is a package-manager invocation, and `pir list` must
            // stay usable with no npm on PATH).
            let installed_path = installed_path_of(&source, &base, local, cwd, settings, false);
            rows.push(ConfiguredPackage {
                source,
                scope: scope_name,
                filtered: entry.is_object(),
                installed_path,
            });
        }
    }
    rows
}

/// The on-disk install path of a configured source, when present (upstream
/// `getInstalledPath`: npm under the scope npm root — with the legacy global
/// npm root as the user-scope fallback — git under the scope git root, local
/// resolved paths).
fn installed_path_of(
    source: &str,
    base_dir: &Path,
    local: bool,
    cwd: &Path,
    settings: &SettingsManager,
    probe_legacy_npm: bool,
) -> Option<PathBuf> {
    match parse_source(source) {
        ParsedSource::Npm(npm) => {
            if !npm_install_name_is_safe(&npm.name) {
                return None;
            }
            let managed = base_dir.join("npm").join("node_modules").join(&npm.name);
            if managed.exists() || local {
                return managed.exists().then_some(managed);
            }
            if !probe_legacy_npm {
                return None;
            }
            // Upstream `getNpmInstallPath` (package-manager.ts:2145-2152)
            // falls back to the legacy global npm root for user scope.
            let legacy = legacy_global_npm_install_path(settings, &npm.name)?;
            legacy.exists().then_some(legacy)
        }
        ParsedSource::Git { host, path, .. } => {
            let target = base_dir.join("git").join(host).join(path);
            // A configured git source is a checkout: a directory without a
            // `.git` entry is a partial/crashed install (or a foreign tree)
            // and must not be read as the package.
            target.join(".git").exists().then_some(target)
        }
        ParsedSource::Local { path } => {
            // Settings entries resolve against their scope base. Fall back to
            // the cwd form for entries written by older pir versions, which
            // stored local sources relative to the invocation cwd.
            let from_base = local_path_from_base(&path, base_dir);
            if from_base.exists() {
                return Some(from_base);
            }
            let from_cwd = resolve_cli_local_path(&path, cwd);
            from_cwd.exists().then_some(from_cwd)
        }
    }
}

/// One configured package the startup resolver could not install (upstream
/// `PackageInstallFailure`, package-manager.ts:93-98). The package's
/// resources are skipped and startup continues; [`Self::warning`] is the
/// user-visible diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInstallFailure {
    /// The configured source string (the settings entry spelling).
    pub source: String,
    /// `user` or `project` — the scope that names the package.
    pub scope: &'static str,
    /// The install error's message.
    pub message: String,
}

impl PackageInstallFailure {
    /// The one-line startup warning (upstream `formatPackageInstallWarning`,
    /// resource-loader.ts:104-116): says how to drop the broken entry from
    /// settings.
    pub fn warning(&self) -> String {
        // The configured source may embed `https://user:token@host`; the
        // warning is cached and replayed, so redact both the source spelling
        // and any URL the install error echoed back.
        let source = redact_url_credentials(&self.source);
        let message = redact_url_credentials(&self.message);
        let hint = format!(
            "Run \"pir remove {}{}\" to remove it from settings.json.",
            source,
            if self.scope == "project" {
                " --local"
            } else {
                ""
            }
        );
        format!(
            "Extension package \"{}\": failed to install ({}). {hint}",
            source, message
        )
    }
}

// ---------------------------------------------------------------------------
// Startup install-attempt cache
// ---------------------------------------------------------------------------

/// One configured source's startup install attempt, keyed for the
/// process-wide cache below.
///
/// `resolve_package_resources*` runs several times per launch (the untrusted
/// project plan, the strict extension gate, resource discovery), so without
/// this cache a broken npm/git package would be installed — and time out on
/// the network — once per resolve. The first attempt's result is replayed for
/// later resolves: a failure never retries, a success keeps reporting the
/// path it produced.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SourceInstallKey {
    /// The configured source spelling (the settings entry text).
    source: String,
    /// Project scope (`true`) vs user scope (`false`).
    local: bool,
    /// The scope's install base (`<cwd>/.pi` or the agent dir). The same
    /// source string under a different scope/tree must attempt its own
    /// install.
    base_dir: PathBuf,
}

/// The remembered result of one [`SourceInstallKey`] attempt.
#[derive(Debug, Clone)]
enum SourceInstallOutcome {
    /// The install succeeded; this is the path re-read afterwards (`None`
    /// when the manager reported success without producing the package).
    Installed(Option<PathBuf>),
    /// The install failed with this message (the user-facing diagnostic is
    /// rebuilt from it on every resolve).
    Failed(String),
}

/// Single-flight state for one source's install attempt: one caller runs the
/// attempt, concurrent callers wait for its outcome instead of spawning a
/// second `npm install`.
#[derive(Debug, Default)]
struct SourceInstallAttempt {
    state: Mutex<SourceInstallAttemptState>,
    ready: Condvar,
}

#[derive(Debug, Default)]
struct SourceInstallAttemptState {
    /// A caller is running the attempt right now.
    running: bool,
    /// The attempt's result, set once it finished.
    outcome: Option<SourceInstallOutcome>,
}

fn source_install_attempts() -> &'static Mutex<HashMap<SourceInstallKey, Arc<SourceInstallAttempt>>>
{
    static ATTEMPTS: OnceLock<Mutex<HashMap<SourceInstallKey, Arc<SourceInstallAttempt>>>> =
        OnceLock::new();
    ATTEMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lock a cache mutex, recovering the guard when a previous holder panicked
/// (a poisoned install cache must not turn into a startup failure).
fn lock_ignoring_poison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How long a startup resolve waits for another process that is already
/// installing packages in the same scope before giving up and skipping the
/// package for this launch. Explicit `pir install` waits up to the command
/// timeout instead (the user asked for the install synchronously).
const PACKAGE_INSTALL_LOCK_STARTUP_WAIT: Duration = Duration::from_secs(10);

/// One cross-process package-install lock (`<scope>/.package-install.lock`).
/// The token is checked on release so a previous owner whose stale lock was
/// taken over cannot delete the new owner's file.
struct PackageInstallLock {
    path: PathBuf,
    token: String,
}

impl Drop for PackageInstallLock {
    fn drop(&mut self) {
        if std::fs::read_to_string(&self.path)
            .ok()
            .is_some_and(|contents| contents == self.token)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Serialize npm/git installs in one scope across processes. Without this,
/// two pir processes starting against the same project can run concurrent
/// `npm install`/`git clone` operations into the same root, corrupting
/// `node_modules` or racing the settings write. A lock left by a crashed
/// owner is taken over once it is older than two command deadlines.
fn acquire_package_install_lock(
    base_dir: &Path,
    wait: Duration,
) -> crate::Result<PackageInstallLock> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static LOCK_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    std::fs::create_dir_all(base_dir)
        .map_err(|error| anyhow::anyhow!("cannot create {}: {error}", base_dir.display()))?;
    let path = base_dir.join(".package-install.lock");
    let token = format!(
        "{}-{}",
        std::process::id(),
        LOCK_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let stale_after = package_command_timeout().saturating_mul(2);
    let deadline = Instant::now() + wait;
    loop {
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(error) = file.write_all(token.as_bytes()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(anyhow::anyhow!(
                        "cannot write install lock {}: {error}",
                        path.display()
                    ));
                }
                return Ok(PackageInstallLock { path, token });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .map(|modified| modified.elapsed().unwrap_or_default() > stale_after)
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                if Instant::now() >= deadline {
                    return Err(anyhow::anyhow!(
                        "another pir process is installing packages in {}; try again after it finishes",
                        base_dir.display()
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "cannot create install lock {}: {error}",
                    path.display()
                ))
            }
        }
    }
}

/// Run `f` while holding the scope's cross-process install lock.
fn with_package_install_lock<T>(
    base_dir: &Path,
    wait: Duration,
    f: impl FnOnce() -> T,
) -> crate::Result<T> {
    let _lock = acquire_package_install_lock(base_dir, wait)?;
    Ok(f())
}

/// Run `attempt` at most once per process for `key`, replaying the first
/// result on every later resolve of the same source/scope.
fn source_install_once(
    key: SourceInstallKey,
    attempt: impl FnOnce() -> SourceInstallOutcome,
) -> SourceInstallOutcome {
    let slot = {
        let mut attempts = lock_ignoring_poison(source_install_attempts());
        Arc::clone(
            attempts
                .entry(key)
                .or_insert_with(|| Arc::new(SourceInstallAttempt::default())),
        )
    };
    {
        let mut state = lock_ignoring_poison(&slot.state);
        loop {
            if let Some(outcome) = &state.outcome {
                // A previous resolve already attempted this source: replay
                // its result (failed installs must not hit the network again).
                return outcome.clone();
            }
            if !state.running {
                break;
            }
            state = slot
                .ready
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.running = true;
    }
    // If the attempt unwinds, record a failure and wake the waiters, so a
    // panicking leader cannot hang later resolves.
    let mut interrupted = InterruptedAttemptGuard {
        slot: &slot,
        armed: true,
    };
    let outcome = attempt();
    interrupted.armed = false;
    {
        let mut state = lock_ignoring_poison(&slot.state);
        state.outcome = Some(outcome.clone());
        state.running = false;
    }
    slot.ready.notify_all();
    outcome
}

/// [`source_install_once`]'s panic path: publishes a failure outcome so
/// waiters observing the poisoned/aborted attempt do not block forever.
struct InterruptedAttemptGuard<'a> {
    slot: &'a SourceInstallAttempt,
    armed: bool,
}

impl Drop for InterruptedAttemptGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        {
            let mut state = lock_ignoring_poison(&self.slot.state);
            state.outcome = Some(SourceInstallOutcome::Failed(
                "package install aborted".to_owned(),
            ));
            state.running = false;
        }
        self.slot.ready.notify_all();
    }
}

/// Ensure one configured package is installed at its scope before its
/// resources are read (upstream `resolvePackageSources`'s `installMissing`,
/// package-manager.ts:1318-1366).
///
/// - npm: a missing install OR an installed version that does not satisfy the
///   configured version/range is reinstalled (`installedNpmMatchesConfiguredVersion`);
/// - git: only a missing checkout is cloned (upstream does not update existing
///   checkouts during startup);
/// - local paths never install; a missing path is skipped;
/// - offline mode (upstream `isOfflineModeEnabled`) never installs and skips
///   silently;
/// - an install error is recorded in `failures` and the package is skipped —
///   it must never abort startup;
/// - install attempts are memoized per process ([`source_install_once`]): the
///   same source is installed at most once per launch, and a failed attempt is
///   replayed (still reported as a diagnostic) on every later resolve instead
///   of hitting npm/git again.
/// - the attempt runs under the scope's cross-process install lock
///   ([`acquire_package_install_lock`]), so two pir processes starting
///   together cannot run concurrent `npm install`/`git clone` into one root.
///   A short startup wait bounds the block; if another process still holds
///   the lock, this launch skips the package with a diagnostic.
///
/// Returns the package root to read, or `None` when the package is skipped.
fn ensure_source_installed(
    source: &str,
    local: bool,
    settings: &SettingsManager,
    agent_dir: &Path,
    cwd: &Path,
    offline: bool,
    failures: &mut Vec<PackageInstallFailure>,
) -> Option<PathBuf> {
    let scope = if local {
        SettingsScope::Project
    } else {
        SettingsScope::Global
    };
    let base_dir = scope_base_dir_for(scope, agent_dir, cwd);
    let scope_name = if local { "project" } else { "user" };
    let installed = installed_path_of(source, &base_dir, local, cwd, settings, true);
    match parse_source(source) {
        ParsedSource::Local { .. } => installed,
        ParsedSource::Npm(npm) => {
            let needs_install = match &installed {
                None => true,
                Some(path) => !installed_npm_matches_configured_version(&npm, path),
            };
            if !needs_install {
                return installed;
            }
            if offline {
                return None;
            }
            let key = SourceInstallKey {
                source: source.to_owned(),
                local,
                base_dir: base_dir.clone(),
            };
            let outcome = match with_package_install_lock(
                &base_dir,
                PACKAGE_INSTALL_LOCK_STARTUP_WAIT,
                || {
                    source_install_once(key, || {
                        match install_npm_captured(&npm, local, settings, agent_dir, cwd) {
                            // Upstream re-reads `getNpmInstallPath` after a
                            // successful install; a manager that reports success
                            // without producing the package still contributes
                            // nothing.
                            Ok(()) => SourceInstallOutcome::Installed(installed_path_of(
                                source, &base_dir, local, cwd, settings, true,
                            )),
                            Err(error) => SourceInstallOutcome::Failed(error.to_string()),
                        }
                    })
                },
            ) {
                Ok(outcome) => outcome,
                Err(error) => {
                    // A competing process may have completed the install while
                    // we waited for the lock. Only trust a COMPLETE checkout:
                    // a partial `node_modules`/clone is worse than skipping the
                    // package for this launch.
                    if let Some(path) =
                        installed_path_of(source, &base_dir, local, cwd, settings, true)
                    {
                        if installed_npm_version(&path).is_some()
                            && installed_npm_matches_configured_version(&npm, &path)
                        {
                            return Some(path);
                        }
                    }
                    failures.push(PackageInstallFailure {
                        source: source.to_owned(),
                        scope: scope_name,
                        message: error.to_string(),
                    });
                    return None;
                }
            };
            match outcome {
                SourceInstallOutcome::Installed(path) => path,
                SourceInstallOutcome::Failed(message) => {
                    failures.push(PackageInstallFailure {
                        source: source.to_owned(),
                        scope: scope_name,
                        message,
                    });
                    None
                }
            }
        }
        ParsedSource::Git {
            host,
            path,
            url,
            git_ref,
        } => {
            if installed.is_some() {
                return installed;
            }
            if offline {
                return None;
            }
            let install_root = git_install_root(local, agent_dir, cwd);
            let key = SourceInstallKey {
                source: source.to_owned(),
                local,
                base_dir: base_dir.clone(),
            };
            let outcome = match with_package_install_lock(
                &base_dir,
                PACKAGE_INSTALL_LOCK_STARTUP_WAIT,
                || {
                    source_install_once(key, || {
                        match scoped_git_target(&install_root, &host, &path) {
                            Ok(target) => {
                                // Upstream `installGit` ensures the git root's
                                // `.gitignore` before the clone; the project scope
                                // therefore writes `<cwd>/.pi/git/.gitignore` and
                                // keeps its clones out of git.
                                let result = ensure_git_ignore(&install_root).and_then(|()| {
                                    git_checkout(&url, &target, git_ref.as_deref(), settings, true)
                                });
                                match result {
                                    Ok(()) => SourceInstallOutcome::Installed(installed_path_of(
                                        source, &base_dir, local, cwd, settings, true,
                                    )),
                                    Err(error) => SourceInstallOutcome::Failed(error.to_string()),
                                }
                            }
                            Err(error) => SourceInstallOutcome::Failed(error.to_string()),
                        }
                    })
                },
            ) {
                Ok(outcome) => outcome,
                Err(error) => {
                    if let Some(path) =
                        installed_path_of(source, &base_dir, local, cwd, settings, true)
                    {
                        // Only a checkout with its `.git` marker is complete;
                        // a staging copy or an interrupted clone is not.
                        if path.join(".git").exists() {
                            return Some(path);
                        }
                    }
                    failures.push(PackageInstallFailure {
                        source: source.to_owned(),
                        scope: scope_name,
                        message: error.to_string(),
                    });
                    return None;
                }
            };
            match outcome {
                SourceInstallOutcome::Installed(path) => path,
                SourceInstallOutcome::Failed(message) => {
                    failures.push(PackageInstallFailure {
                        source: source.to_owned(),
                        scope: scope_name,
                        message,
                    });
                    None
                }
            }
        }
    }
}

fn list(settings: &SettingsManager, agent_dir: &Path, cwd: &Path) -> crate::Result<()> {
    let packages = configured_packages(settings, agent_dir, cwd);
    if packages.is_empty() {
        println!("No packages installed.");
        return Ok(());
    }
    let mut first_scope = true;
    for scope_name in ["user", "project"] {
        let rows: Vec<&ConfiguredPackage> = packages
            .iter()
            .filter(|package| package.scope == scope_name)
            .collect();
        if rows.is_empty() {
            continue;
        }
        if !first_scope {
            println!();
        }
        first_scope = false;
        println!(
            "{}",
            if scope_name == "user" {
                "User packages:"
            } else {
                "Project packages:"
            }
        );
        for package in rows {
            let display_source = redact_url_credentials(&package.source);
            let display = if package.filtered {
                format!("{display_source} (filtered)")
            } else {
                display_source
            };
            println!("  {display}");
            if let Some(path) = &package.installed_path {
                println!("    {}", path.display());
            }
        }
    }
    Ok(())
}

/// The interactive-equivalent resource listing (upstream `handleConfigCommand`
/// opens the config TUI; the headless pir surface prints the same
/// package/resource state read-only). `-l` scopes to the project settings.
fn print_config(
    settings: &SettingsManager,
    local: bool,
    agent_dir: &Path,
    cwd: &Path,
) -> crate::Result<()> {
    let packages = configured_packages(settings, agent_dir, cwd);
    println!(
        "Resource configuration ({} settings):",
        if local { "project" } else { "global" }
    );
    println!("  cwd: {}", cwd.display());
    if packages.is_empty() {
        println!("  No packages installed.");
    } else {
        println!("  Packages:");
        for package in &packages {
            println!(
                "    [{}] {}{}",
                package.scope,
                redact_url_credentials(&package.source),
                if package.filtered { " (filtered)" } else { "" }
            );
            match &package.installed_path {
                Some(path) => println!("      installed: {}", path.display()),
                None => println!("      installed: no"),
            }
        }
        let resources = config_resource_rows(settings, agent_dir, cwd, true);
        if !resources.is_empty() {
            println!("  Package resources:");
            for resource in &resources {
                println!(
                    "    [{}] {} ({}: {}, {})",
                    if resource.enabled { "x" } else { " " },
                    resource.pattern,
                    resource.kind,
                    redact_url_credentials(&resource.source),
                    resource.scope
                );
            }
        }
    }
    let raw = if local {
        &settings.project_raw
    } else {
        &settings.global_raw
    };
    for resource in ["extensions", "skills", "prompts", "themes"] {
        let entries = raw
            .get(resource)
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(redact_url_credentials)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let state = if entries.is_empty() {
            "(none)"
        } else {
            entries.as_str()
        };
        println!("  {resource}: {state}");
    }
    // Keep the former debug-dump's redaction guarantee (the production-safety
    // gate): the server auth token is only ever NAMED, never printed.
    if std::env::var_os("PIR_SERVER_AUTH_TOKEN").is_some_and(|value| !value.is_empty()) {
        println!("  server_auth_token: [redacted]");
    } else {
        println!("  server_auth_token: (not set)");
    }
    println!(
        "Interactive enable/disable: run `pir config` in a TTY; scripted: \
         --enable/--disable <kind:pattern> or --json."
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Interactive / scriptable resource configuration (upstream
// `modes/interactive/components/config-selector.ts` + `cli/config-selector.ts`)
// ---------------------------------------------------------------------------

/// The resource kinds a package can contribute, in the config selector's
/// order (`RESOURCE_TYPES`, config-selector.ts:33).
pub const CONFIG_RESOURCE_KINDS: [&str; 4] = ["extensions", "skills", "prompts", "themes"];

/// One package-contributed resource as the `config` surfaces see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigResource {
    /// `user` or `project` (the settings scope that names the package).
    pub scope: &'static str,
    /// The configured package source (the settings entry's source string).
    pub source: String,
    /// `extensions` / `skills` / `prompts` / `themes`.
    pub kind: String,
    /// Package-relative posix path — the filter pattern written by a toggle
    /// (upstream `getPackageResourcePattern`).
    pub pattern: String,
    /// The resource file or directory on disk.
    pub path: PathBuf,
    /// Whether the package entry's filter currently keeps this resource.
    pub enabled: bool,
}

impl ConfigResource {
    /// The non-interactive selector `--enable`/`--disable` accept.
    pub fn selector(&self) -> String {
        format!("{}:{}", self.kind, self.pattern)
    }

    fn to_json(&self) -> Value {
        json!({
            "scope": self.scope,
            "source": redact_url_credentials(&self.source),
            "kind": self.kind,
            "pattern": self.pattern,
            "path": self.path.to_string_lossy(),
            "enabled": self.enabled,
            "selector": self.selector(),
        })
    }
}

/// One configured package as the `config` output lists it.
fn config_packages(settings: &SettingsManager, agent_dir: &Path, cwd: &Path) -> Vec<Value> {
    configured_packages(settings, agent_dir, cwd)
        .into_iter()
        .map(|package| {
            json!({
                "source": redact_url_credentials(&package.source),
                "scope": package.scope,
                "filtered": package.filtered,
                "installed": package.installed_path.is_some(),
                "path": package.installed_path.map(|path| path.to_string_lossy().into_owned()),
            })
        })
        .collect()
}

/// Enumerate every installed package's resources with their current
/// enable/disable state. One row per package (project scope first when
/// trusted, deduped by source identity — the same precedence
/// [`resolve_package_resources`] applies).
pub fn config_resource_rows(
    settings: &SettingsManager,
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
) -> Vec<ConfigResource> {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut rows: Vec<ConfigResource> = Vec::new();
    for (scope, local, enabled) in [
        (SettingsScope::Project, true, project_trusted),
        (SettingsScope::Global, false, true),
    ] {
        if !enabled {
            continue;
        }
        let scope_name = if scope == SettingsScope::Project {
            "project"
        } else {
            "user"
        };
        let base_dir = scope_base_dir_for(scope, agent_dir, cwd);
        for entry in scope_packages(settings, scope) {
            let source = package_entry_source(&entry);
            let identity = source_identity(&source, &base_dir);
            if source.is_empty() || !seen.insert(identity) {
                continue;
            }
            let Some(package_root) =
                installed_path_of(&source, &base_dir, local, cwd, settings, false)
            else {
                continue;
            };
            let collected = collect_package_resources(&package_root);
            for (kind, paths) in [
                ("extensions", collected.extensions),
                ("skills", collected.skills),
                ("prompts", collected.prompts),
                ("themes", collected.themes),
            ] {
                let patterns: Vec<String> = entry
                    .get(kind)
                    .and_then(Value::as_array)
                    .map(|entries| {
                        entries
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let filter = TopLevelResourceFilter::from_entries(&patterns);
                for path in paths {
                    let pattern = path
                        .strip_prefix(&package_root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    if pattern.is_empty() {
                        continue;
                    }
                    rows.push(ConfigResource {
                        scope: scope_name,
                        source: source.clone(),
                        kind: kind.to_string(),
                        pattern,
                        enabled: filter.keeps(&path, &package_root),
                        path,
                    });
                }
            }
        }
    }
    rows
}

/// The machine-readable configuration (`--json`): scope, packages, and every
/// package resource with its effective state and its `--enable`/`--disable`
/// selector.
pub fn config_json(
    settings: &SettingsManager,
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    local: bool,
) -> Value {
    json!({
        "cwd": cwd.to_string_lossy(),
        "writeScope": if local { "project" } else { "global" },
        "projectTrusted": project_trusted,
        "packages": config_packages(settings, agent_dir, cwd),
        "resources": config_resource_rows(settings, agent_dir, cwd, project_trusted)
            .iter()
            .map(ConfigResource::to_json)
            .collect::<Vec<_>>(),
    })
}

/// Split a `--enable`/`--disable` selector `<kind>:<pattern>`; the pattern is
/// the remainder (posix path, may itself contain `:`).
fn parse_config_selector(selector: &str) -> crate::Result<(&'static str, String)> {
    let Some((kind, pattern)) = selector.split_once(':') else {
        return Err(anyhow::anyhow!(
            "Invalid resource selector \"{selector}\": expected <kind>:<pattern>"
        ));
    };
    let kind = CONFIG_RESOURCE_KINDS
        .iter()
        .copied()
        .find(|candidate| *candidate == kind)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Invalid resource kind \"{kind}\": expected one of {}",
                CONFIG_RESOURCE_KINDS.join(", ")
            )
        })?;
    let pattern = pattern.replace('\\', "/");
    if pattern.is_empty() {
        return Err(anyhow::anyhow!(
            "Invalid resource selector \"{selector}\": the pattern is empty"
        ));
    }
    Ok((kind, pattern))
}

/// Write one package resource's `+pattern`/`-pattern` filter entry
/// (upstream `togglePackageResource`, config-selector.ts:586-643): the entry
/// for that pattern is replaced, the other filters and unknown object fields
/// survive, and an object left without filters collapses back to the string
/// form.
fn set_package_resource_filter(
    settings: &mut SettingsManager,
    scope: SettingsScope,
    source: &str,
    kind: &str,
    pattern: &str,
    enabled: bool,
) -> crate::Result<()> {
    let mut packages = scope_packages(settings, scope);
    let index = packages
        .iter()
        .position(|entry| package_entry_source(entry) == source)
        .ok_or_else(|| anyhow::anyhow!("Package \"{source}\" is not configured in this scope"))?;
    let mut object = match &packages[index] {
        Value::Object(map) => map.clone(),
        Value::String(source) => {
            let mut map = serde_json::Map::new();
            map.insert("source".to_string(), Value::String(source.clone()));
            map
        }
        _ => {
            return Err(anyhow::anyhow!(
                "Package \"{source}\" has an unsupported settings entry"
            ))
        }
    };
    let mut entries: Vec<String> = object
        .get(kind)
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    // Replace the entry for this resource: a later `+`/`-` wins over every
    // earlier pattern for the same path (upstream filters the target string
    // after stripping any `!`/`+`/`-` prefix).
    entries.retain(|entry| {
        entry
            .strip_prefix(['!', '+', '-'])
            .unwrap_or(entry)
            .replace('\\', "/")
            != pattern
    });
    entries.push(format!("{}{}", if enabled { '+' } else { '-' }, pattern));
    object.insert(
        kind.to_string(),
        Value::Array(entries.into_iter().map(Value::String).collect()),
    );
    let has_filters = CONFIG_RESOURCE_KINDS
        .iter()
        .any(|known| object.get(*known).is_some());
    packages[index] = if has_filters {
        Value::Object(object)
    } else {
        Value::String(source.to_string())
    };
    persist_packages(settings, scope, packages)?;
    // `persist_field` only writes the file; refresh the in-memory snapshot so
    // the interactive loop (which recomputes the resource rows every
    // iteration) and the JSON output see the toggle immediately.
    settings.reload();
    Ok(())
}

/// Enabled/disable one package resource by selector (the non-interactive
/// writable surface). Returns the confirmation line the command prints.
// The arguments mirror the CLI surface (scope/selector/source/enabled); the
// function is a single-pass resolver, so a request struct would be a refactor.
#[allow(clippy::too_many_arguments)]
pub fn set_config_resource_enabled(
    settings: &mut SettingsManager,
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    local: bool,
    selector: &str,
    source: Option<&str>,
    enabled: bool,
) -> crate::Result<String> {
    if local && !project_trusted {
        return Err(anyhow::anyhow!(
            "Project is not trusted. Use --approve to modify local resource config."
        ));
    }
    let (kind, pattern) = parse_config_selector(selector)?;
    let scope = if local {
        SettingsScope::Project
    } else {
        SettingsScope::Global
    };
    let scope_name = if local { "project" } else { "user" };
    let rows = config_resource_rows(settings, agent_dir, cwd, project_trusted);
    let candidates: Vec<&ConfigResource> = rows
        .iter()
        .filter(|row| {
            row.scope == scope_name
                && row.kind == kind
                && row.pattern == pattern
                && source.is_none_or(|wanted| row.source == wanted)
        })
        .collect();
    let target = match candidates.as_slice() {
        [target] => *target,
        [] => {
            return Err(anyhow::anyhow!(
                "No {kind} resource \"{pattern}\" is contributed by an installed {} package",
                if local { "project" } else { "user" }
            ))
        }
        _ => {
            let sources: Vec<String> = candidates
                .iter()
                .map(|candidate| redact_url_credentials(&candidate.source))
                .collect();
            return Err(anyhow::anyhow!(
                "Resource \"{kind}:{pattern}\" is ambiguous (provided by {}); pass --source <package>",
                sources.join(", ")
            ));
        }
    };
    let source = target.source.clone();
    let row_enabled = target.enabled;
    set_package_resource_filter(settings, scope, &source, kind, &pattern, enabled)?;
    // A no-op toggle (the requested state already holds) is still a valid
    // scripted call; the message states the resulting state either way.
    let _ = row_enabled;
    Ok(format!(
        "{} {kind} resource \"{pattern}\" for {} ({} settings)",
        if enabled { "Enabled" } else { "Disabled" },
        redact_url_credentials(&source),
        if local { "project" } else { "global" }
    ))
}

/// The line-oriented `pir config` selection (upstream `selectConfig` +
/// `ConfigSelectorComponent` reduced to the resource toggle the headless
/// command can drive): list the package resources, read one number per
/// iteration, write the same `+`/`-` filter the non-interactive surface
/// writes, and confirm each change. Empty input/`q` finishes.
pub fn run_config_interactive_with<R: BufRead, W: Write>(
    settings: &mut SettingsManager,
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    local: bool,
    reader: &mut R,
    writer: &mut W,
) -> crate::Result<()> {
    let write_scope = if local { "project" } else { "user" };
    writeln!(
        writer,
        "Resource configuration ({} settings):",
        if local { "project" } else { "global" }
    )?;
    writeln!(writer, "  cwd: {}", cwd.display())?;
    loop {
        let rows = config_resource_rows(settings, agent_dir, cwd, project_trusted);
        if rows.is_empty() {
            writeln!(writer, "  No package resources to configure.")?;
            return Ok(());
        }
        writeln!(writer, "  Package resources:")?;
        for (index, row) in rows.iter().enumerate() {
            let editable = row.scope == write_scope;
            writeln!(
                writer,
                "    {:>3}. [{}] {} ({}, {}){}",
                index + 1,
                if row.enabled { "x" } else { " " },
                row.pattern,
                redact_url_credentials(&row.source),
                row.scope,
                if editable {
                    ""
                } else {
                    " — other scope, not writable here"
                }
            )?;
        }
        write!(writer, "Toggle resource number (Enter to finish): ")?;
        writer.flush()?;
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("q") {
            return Ok(());
        }
        let Ok(picked) = trimmed.parse::<usize>() else {
            writeln!(writer, "Invalid selection \"{trimmed}\".")?;
            continue;
        };
        let Some(target) = rows.get(picked.wrapping_sub(1)) else {
            writeln!(writer, "Invalid selection \"{trimmed}\".")?;
            continue;
        };
        if target.scope != write_scope {
            writeln!(
                writer,
                "This resource belongs to the {} scope; {} does not write it.",
                target.scope,
                if local { "--local" } else { "the global scope" }
            )?;
            continue;
        }
        let message = set_config_resource_enabled(
            settings,
            agent_dir,
            cwd,
            project_trusted,
            local,
            &target.selector(),
            Some(&target.source),
            !target.enabled,
        )?;
        writeln!(writer, "  {message}")?;
    }
}

// ---------------------------------------------------------------------------
// Package consumption (upstream `DefaultPackageManager.resolve()` +
// `collectPackageResources`, package-manager.ts:920-989 / 2214-2263): the
// read side of `pir install` — settings.packages entries resolve to installed
// package directories and their extensions/skills/prompts/themes paths.
// ---------------------------------------------------------------------------

/// The resource paths contributed by installed packages.
#[derive(Debug, Default, Clone)]
pub struct PackageResources {
    pub extensions: Vec<PathBuf>,
    pub skills: Vec<PathBuf>,
    pub prompts: Vec<PathBuf>,
    pub themes: Vec<PathBuf>,
}

/// One resource kind's collection rules (upstream `collectResourceFiles`).
fn collect_convention_resources(package_root: &Path, kind: &str, out: &mut Vec<PathBuf>) {
    let dir = package_root.join(kind);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            !name.starts_with('.')
        })
        .collect();
    found.sort();
    for path in found {
        match kind {
            // Extensions: a directory loads via its own discovery (index
            // file / manifest), a file loads directly (upstream
            // `collectAutoExtensionEntries`).
            "extensions" => out.push(path),
            // Skills: the loader accepts directories (it resolves each
            // entry's SKILL.md).
            "skills" => out.push(path),
            // Prompts: recursive `*.md` (upstream `collectPromptFiles`).
            "prompts" => collect_recursive_suffix(&path, "md", out),
            // Themes: recursive `*.json` (upstream `collectThemeFiles`).
            "themes" => collect_recursive_suffix(&path, "json", out),
            _ => {}
        }
    }
}

/// Whether a path's extension matches `suffix` (case-insensitive, like the
/// manifest filter's `*.md` / `*.json` rules).
fn file_has_suffix(path: &Path, suffix: &str) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(suffix))
}

/// Collect `*.{suffix}` files under `path` recursively (upstream
/// `collectPromptFiles` / `collectThemeFiles`). `path` may itself be a
/// matching FILE: `collect_convention_resources` passes each entry of the
/// convention directory, so a prompt or theme stored directly in
/// `<package>/prompts/` / `<package>/themes/` must be collected too (a
/// `read_dir` on a file silently dropped it before).
fn collect_recursive_suffix(path: &Path, suffix: &str, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        if file_has_suffix(path, suffix) {
            out.push(path.to_path_buf());
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    let mut found: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    found.sort();
    for path in found {
        if path.is_dir() {
            collect_recursive_suffix(&path, suffix, out);
        } else if file_has_suffix(&path, suffix) {
            out.push(path);
        }
    }
}

/// A minimal dependency-free glob matcher for manifest/filter patterns
/// (upstream uses the minimatch grammar subset, package-manager.ts:661-687):
/// patterns are `/`-separated; a whole-segment `**` matches ZERO OR MORE
/// segments (so `**/*.md` matches `readme.md` as well as `docs/a/readme.md`),
/// `*` matches any run of non-separator characters WITHIN one segment, and
/// every other segment matches literally. Matching is segment-based — a `**`
/// tail is never compared as a literal suffix and segment boundaries are
/// respected (`*` never crosses `/`).
fn glob_like_match(pattern: &str, text: &str) -> bool {
    let pattern_segments: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let text_segments: Vec<&str> = text.split('/').filter(|s| !s.is_empty()).collect();
    segment_glob_match(&pattern_segments, &text_segments)
}

/// Recursive segment matcher: `**` as one whole segment consumes zero or
/// more text segments; every other pattern segment consumes exactly one.
fn segment_glob_match(pattern: &[&str], text: &[&str]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((&"**", rest)) => (0..=text.len()).any(|skip| segment_glob_match(rest, &text[skip..])),
        Some((&first, rest)) => match text.split_first() {
            Some((&head, tail_rest)) => {
                segment_match(first, head) && segment_glob_match(rest, tail_rest)
            }
            None => false,
        },
    }
}

/// Wildcard match within ONE segment: `*` matches any run of characters
/// (no `/` can appear — the caller split on it), multiple stars included.
fn segment_match(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((head, rest)) => {
            let Some(after_head) = text.strip_prefix(head) else {
                return false;
            };
            (0..=after_head.len())
                .filter(|&skip| after_head.is_char_boundary(skip))
                .any(|skip| segment_match(rest, &after_head[skip..]))
        }
    }
}

/// One normalized per-resource filter pattern (upstream `applyPatterns`,
/// package-manager.ts:745-791).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterPatternKind {
    /// A plain glob: a match selects the path.
    Include,
    /// `!glob`: a match deselects the path (excludes beat earlier includes).
    Exclude,
    /// `+glob`: a match force-selects the path, overriding any non-force
    /// decision.
    ForceInclude,
    /// `-glob`: a match force-deselects the path, overriding everything.
    ForceExclude,
}

/// The process home directory (`$HOME`, then `$USERPROFILE` on Windows) — the
/// one ambient resolution behind the filter boundaries whose callers hold no
/// home ([`TopLevelResourceFilter::from_entries`],
/// [`resolve_package_resources`]). Everything deeper takes the resolved home
/// as an explicit argument, so a compiled pattern never captures an ambient
/// read.
fn process_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Normalize one filter pattern: a leading `~` expanded against `home` (the
/// caller's already-resolved home directory; `None` = no home available —
/// upstream `normalizePath` inside `matchesAnyPattern` / `normalizeExactPattern`),
/// separators to `/`, a leading `./` stripped, and its force/exclude kind
/// decoded.
fn compile_filter_pattern(
    pattern: &str,
    home: Option<&Path>,
) -> Option<(FilterPatternKind, String)> {
    let (kind, body) = match pattern.strip_prefix('!') {
        Some(body) => (FilterPatternKind::Exclude, body),
        None => match pattern.strip_prefix('+') {
            Some(body) => (FilterPatternKind::ForceInclude, body),
            None => match pattern.strip_prefix('-') {
                Some(body) => (FilterPatternKind::ForceExclude, body),
                None => (FilterPatternKind::Include, pattern),
            },
        },
    };
    let body = crate::config::settings::expand_tilde_in_pattern(body, home);
    let body = body.replace('\\', "/");
    let body = body.strip_prefix("./").unwrap_or(&body).to_owned();
    if body.is_empty() {
        return None;
    }
    Some((kind, body))
}

/// The selection decision for one path under [`compile_filter_pattern`] list:
/// plain includes/excludes apply in order with the LAST match winning
/// (so a later `!exclude` beats an earlier include), and any `+`/`-` force
/// match overrides the non-force decision (the last force match wins).
/// A path matches when the pattern matches its package-relative posix path,
/// its basename, or its absolute posix path — the absolute form is what
/// makes a `~`-expanded pattern match (upstream package-manager.ts:661-687
/// matches `rel`, `name` and `filePathPosix`).
fn filter_selects(
    patterns: &[(FilterPatternKind, String)],
    relative: &str,
    basename: &str,
    absolute: &str,
) -> bool {
    selects_any(patterns, &[relative, basename, absolute])
}

/// [`filter_selects`]'s decision loop over an arbitrary candidate set: a
/// pattern matches when it matches ANY candidate (upstream `matchesAnyPattern`
/// tries the relative path, the basename and the absolute posix path, plus the
/// parent skill directory for `SKILL.md`).
fn selects_any(patterns: &[(FilterPatternKind, String)], candidates: &[&str]) -> bool {
    let matches = |pattern: &str| {
        candidates
            .iter()
            .any(|candidate| glob_like_match(pattern, candidate))
    };
    // Without a plain include the starting set is every path (upstream
    // `applyPatterns`: `includes.length > 0` decides whether the include
    // filter narrows the set at all), so an excludes-only list keeps
    // everything it does not exclude.
    let mut selected = !patterns
        .iter()
        .any(|(kind, _)| matches!(kind, FilterPatternKind::Include));
    let mut forced: Option<bool> = None;
    for (kind, pattern) in patterns {
        if !matches(pattern) {
            continue;
        }
        match kind {
            FilterPatternKind::Include => selected = true,
            FilterPatternKind::Exclude => selected = false,
            FilterPatternKind::ForceInclude => forced = Some(true),
            FilterPatternKind::ForceExclude => forced = Some(false),
        }
    }
    forced.unwrap_or(selected)
}

/// The top-level settings filter layer (upstream `resolveLocalEntries` /
/// `isEnabledByOverrides` / `addAutoDiscoveredResources`,
/// package-manager.ts:724-791, 2396-2479): the `!`/`+`/`-` entries of a
/// top-level `skills` / `extensions` / `prompts` / `themes` array decide which
/// of that scope's discovered resource files survive.
///
/// Plain (non-prefixed) entries are sources, never filters — upstream
/// `splitPatterns` + `getOverridePatterns` — so a plain entry does not narrow
/// anything here; it only contributes files that this filter then judges.
/// Compilation is shared with the package filters ([`compile_filter_pattern`]),
/// including the `~` expansion and posix normalization the W1-D7 fix added.
#[derive(Debug, Clone, Default)]
pub struct TopLevelResourceFilter {
    compiled: Vec<(FilterPatternKind, String)>,
}

impl TopLevelResourceFilter {
    /// Compile one settings array's override patterns (`!`/`+`/`-` entries);
    /// entries without a prefix are ignored (they are sources).
    ///
    /// Boundary constructor: resolves the ambient home directory exactly once
    /// here, for callers that hold no home (the CLI/config load and the theme
    /// registry), so a `~/…` pattern expands against the process
    /// `$HOME`/`$USERPROFILE`. Callers with a home in hand use
    /// [`Self::from_entries_with_home`] instead (hermetic: no ambient read).
    pub fn from_entries(entries: &[String]) -> Self {
        Self::from_entries_with_home(entries, process_home_dir().as_deref())
    }

    /// [`Self::from_entries`] with the home directory the `~/…` patterns
    /// expand against passed in explicitly (`None` = no home: `~` patterns
    /// stay literal).
    pub fn from_entries_with_home(entries: &[String], home: Option<&Path>) -> Self {
        Self {
            compiled: entries
                .iter()
                .filter(|entry| {
                    entry.starts_with('!') || entry.starts_with('+') || entry.starts_with('-')
                })
                .filter_map(|entry| compile_filter_pattern(entry, home))
                .collect(),
        }
    }

    /// `true` when no override pattern is in play (the caller can skip the
    /// per-file check entirely).
    pub fn is_empty(&self) -> bool {
        self.compiled.is_empty()
    }

    /// Whether a resource file discovered under `base_dir` is enabled:
    /// `!pattern` excludes a glob match, `+path` force-includes, `-path`
    /// force-excludes, and everything else stays enabled (upstream
    /// `isEnabledByOverrides` defaults to enabled).
    pub fn keeps(&self, path: &Path, base_dir: &Path) -> bool {
        if self.compiled.is_empty() {
            return true;
        }
        let candidates = resource_match_candidates(path, base_dir);
        let borrowed: Vec<&str> = candidates.iter().map(String::as_str).collect();
        selects_any(&self.compiled, &borrowed)
    }
}

/// The strings one resource path is matched against (upstream
/// `matchesAnyPattern` / `matchesAnyExactPattern`): the `base_dir`-relative
/// posix path, the file name, the absolute posix path, and — for a skill's
/// `SKILL.md` — its parent skill directory in all three forms, so `!my-skill`
/// disables the whole skill the way upstream does.
fn resource_match_candidates(path: &Path, base_dir: &Path) -> Vec<String> {
    let to_posix = |path: &Path| path.to_string_lossy().replace('\\', "/");
    let relative = path.strip_prefix(base_dir).unwrap_or(path);
    let mut candidates = vec![to_posix(relative), to_posix(path)];
    if let Some(name) = path.file_name() {
        candidates.push(name.to_string_lossy().into_owned());
    }
    let is_skill_file = path
        .file_name()
        .is_some_and(|name| name.to_string_lossy() == "SKILL.md");
    if is_skill_file {
        if let Some(parent) = path.parent() {
            candidates.push(to_posix(parent.strip_prefix(base_dir).unwrap_or(parent)));
            candidates.push(to_posix(parent));
            if let Some(name) = parent.file_name() {
                candidates.push(name.to_string_lossy().into_owned());
            }
        }
    }
    candidates
}

/// Apply one settings entry's per-resource filter (the object form's
/// `extensions`/`skills`/`prompts`/`themes` glob arrays; upstream
/// `applyPackageFilter`, package-manager.ts:2287-2311). The string form
/// keeps everything collected. Patterns match the package-relative posix
/// path or the file's basename (upstream package-manager.ts:661-687), and
/// apply as includes-then-excludes-then-force (upstream `applyPatterns`):
/// a later `!exclude` beats an earlier include, `+`/`-` force patterns
/// override the non-force decision. `home` is the caller's already-resolved
/// home directory the `~`-prefixed patterns expand against (`None` = no home
/// available).
///
/// A filter array is read item by item: a non-string item is recorded in
/// `diagnostics` (P2-5 — the whole array used to become unreadable, which
/// failed open and kept every resource) while the string items still filter.
/// Only an absent kind key or an explicitly empty array means "no filter"; a
/// non-empty array without a single usable pattern selects nothing.
fn apply_entry_filter(
    entry: &Value,
    source_label: &str,
    package_root: &Path,
    collected: PackageResources,
    home: Option<&Path>,
    diagnostics: &mut Vec<String>,
) -> PackageResources {
    let Some(map) = entry.as_object() else {
        return collected;
    };
    let mut kept = PackageResources::default();
    let mut keep = |kind: &'static str,
                    patterns: Option<&Vec<Value>>,
                    source_paths: Vec<PathBuf>,
                    out: &mut Vec<PathBuf>| {
        let Some(patterns) = patterns else {
            // The kind key is absent: no filter (upstream's default).
            out.extend(source_paths);
            return;
        };
        if patterns.is_empty() {
            // An explicitly empty array: no filter. This and the absent key
            // are the only "no filter" shapes — a populated array that
            // yields no usable pattern below filters everything out instead
            // of failing open.
            out.extend(source_paths);
            return;
        }
        let mut strings: Vec<String> = Vec::with_capacity(patterns.len());
        for (index, item) in patterns.iter().enumerate() {
            match item.as_str() {
                Some(pattern) => strings.push(pattern.to_owned()),
                None => diagnostics.push(format!(
                    "Extension package \"{source_label}\": ignoring non-string \"{kind}\" \
                     filter entry #{} ({item})",
                    index + 1
                )),
            }
        }
        let compiled: Vec<(FilterPatternKind, String)> = strings
            .iter()
            .filter_map(|pattern| compile_filter_pattern(pattern, home))
            .collect();
        if compiled.is_empty() {
            // The array had entries but no usable pattern (all non-strings,
            // or strings that normalize to nothing): filter everything out
            // rather than fail open.
            return;
        }
        for path in source_paths {
            let relative = path
                .strip_prefix(package_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let basename = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            // `~`-expanded filter patterns are absolute; compare against the
            // file's absolute posix path too (upstream `filePathPosix`).
            let absolute = path.to_string_lossy().replace('\\', "/");
            if filter_selects(&compiled, &relative, &basename, &absolute) {
                out.push(path);
            }
        }
    };
    keep(
        "extensions",
        map.get("extensions").and_then(Value::as_array),
        collected.extensions,
        &mut kept.extensions,
    );
    keep(
        "skills",
        map.get("skills").and_then(Value::as_array),
        collected.skills,
        &mut kept.skills,
    );
    keep(
        "prompts",
        map.get("prompts").and_then(Value::as_array),
        collected.prompts,
        &mut kept.prompts,
    );
    keep(
        "themes",
        map.get("themes").and_then(Value::as_array),
        collected.themes,
        &mut kept.themes,
    );
    kept
}

/// Everything one startup package resolution produced.
///
/// [`resolve_package_resources_with_home`] renders the warnings on stderr and
/// returns only [`PackageResources`]; hosts/tests that need the diagnostics
/// programmatically call
/// [`resolve_package_resources_with_home_and_diagnostics`].
#[doc(hidden)]
#[derive(Debug, Default, Clone)]
pub struct PackageResourceResolution {
    /// The resource paths the installed packages contribute.
    pub resources: PackageResources,
    /// The configured packages skipped because their install failed.
    pub failures: Vec<PackageInstallFailure>,
    /// Malformed filter-array diagnostics: a non-string item inside a
    /// package entry's `extensions`/`skills`/`prompts`/`themes` array. The
    /// string items of that same array still filter (P2-5).
    pub diagnostics: Vec<String>,
}

/// Resolve `settings.packages` into the resource paths its installed
/// packages contribute (upstream `resolve()` + `collectPackageResources`).
///
/// Project scope wins over global per source identity (upstream
/// `dedupePackages`). Like upstream's `resolve()`, a missing npm/git install
/// is installed before its resources are read, and an npm install whose
/// `package.json` version no longer satisfies the configured version/range is
/// reinstalled (`installedNpmMatchesConfiguredVersion`); offline mode and
/// install failures skip that package and startup continues. Failed installs
/// and malformed filter arrays (a non-string item inside a package entry's
/// `skills`/`prompts`/… array) are reported as `Warning: …` startup
/// diagnostics on stderr (upstream `formatPackageInstallWarning`); callers
/// needing them programmatically use
/// [`resolve_package_resources_with_home_and_diagnostics`]. A package
/// contributes resources through its `package.json` `"pi"` manifest when
/// present, else through the convention directories.
pub fn resolve_package_resources(
    settings: &SettingsManager,
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
) -> PackageResources {
    // Boundary: resolve the ambient home directory exactly once here, for the
    // package filter patterns (`~` in a `skills`/`prompts`/… array) compiled
    // below. Callers with a home in hand use
    // [`resolve_package_resources_with_home`] instead (hermetic: no ambient
    // read).
    resolve_package_resources_with_home(
        settings,
        agent_dir,
        cwd,
        project_trusted,
        process_home_dir().as_deref(),
    )
}

/// [`resolve_package_resources`] with the home directory the package filter
/// patterns expand against passed in explicitly (`None` = no home). Offline
/// mode (`PI_OFFLINE` / `--offline`) is read from the process environment at
/// this boundary; [`resolve_package_resources_with_home_and_diagnostics`]
/// takes it explicitly.
pub fn resolve_package_resources_with_home(
    settings: &SettingsManager,
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    home: Option<&Path>,
) -> PackageResources {
    let resolution = resolve_package_resources_with_home_and_diagnostics(
        settings,
        agent_dir,
        cwd,
        project_trusted,
        home,
        crate::config::env::is_offline(),
    );
    // Startup diagnostics (upstream routes the same text through the extension
    // loader warnings): never abort startup for an uninstallable package or a
    // malformed filter entry.
    for failure in &resolution.failures {
        eprintln!("Warning: {}", failure.warning());
    }
    for diagnostic in &resolution.diagnostics {
        eprintln!("Warning: {diagnostic}");
    }
    resolution.resources
}

/// [`resolve_package_resources_with_home`] with the offline flag, the install
/// failures and the malformed-filter diagnostics returned explicitly
/// (hermetic: no ambient `PI_OFFLINE` read). The failures are the packages
/// that were skipped because their install failed; offline skips are not
/// failures (upstream's `installMissing` returns false without recording one).
#[doc(hidden)]
pub fn resolve_package_resources_with_home_and_diagnostics(
    settings: &SettingsManager,
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    home: Option<&Path>,
    offline: bool,
) -> PackageResourceResolution {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut collected = PackageResources::default();
    let mut failures: Vec<PackageInstallFailure> = Vec::new();
    let mut diagnostics: Vec<String> = Vec::new();
    // Project scope first (highest precedence), then user/global; entries
    // whose SOURCE IDENTITY (npm by name without version, git by host/path,
    // local by resolved path — upstream `dedupePackages`) was already seen
    // are skipped, so a differently-spelled duplicate entry (e.g.
    // `npm:foo` vs `npm:foo@1`) cannot install the same package twice and
    // the project-scope entry wins over the user-scope one.
    let scopes: [(SettingsScope, bool, bool); 2] = [
        (SettingsScope::Project, true, project_trusted),
        (SettingsScope::Global, false, true),
    ];
    for (scope, local, enabled) in scopes {
        if !enabled {
            continue;
        }
        let base_dir = scope_base_dir_for(scope, agent_dir, cwd);
        for entry in scope_packages(settings, scope) {
            let source = package_entry_source(&entry);
            let identity = source_identity(&source, &base_dir);
            if source.is_empty() || !seen.insert(identity) {
                continue;
            }
            let Some(package_root) = ensure_source_installed(
                &source,
                local,
                settings,
                agent_dir,
                cwd,
                offline,
                &mut failures,
            ) else {
                tracing::debug!(
                    target: "pir::packages",
                    source = %redact_url_credentials(&source),
                    offline,
                    "packages: entry not installed; skipping"
                );
                continue;
            };
            let contributed = apply_entry_filter(
                &entry,
                &source,
                &package_root,
                collect_package_resources_with_home(&package_root, home),
                home,
                &mut diagnostics,
            );
            collected.extensions.extend(contributed.extensions);
            collected.skills.extend(contributed.skills);
            collected.prompts.extend(contributed.prompts);
            collected.themes.extend(contributed.themes);
        }
    }
    PackageResourceResolution {
        resources: collected,
        failures,
        diagnostics,
    }
}

/// Collect one installed package's resources: the `package.json` `"pi"`
/// manifest when present (upstream `readPiManifest` / `addManifestEntries`),
/// else the convention directories (upstream `collectResourceFiles`).
///
/// Boundary wrapper: resolves the ambient home directory the manifest's
/// `~`-prefixed filter patterns expand against.
fn collect_package_resources(package_root: &Path) -> PackageResources {
    collect_package_resources_with_home(package_root, process_home_dir().as_deref())
}

/// [`collect_package_resources`] with the home directory the manifest's
/// `~/…` filter patterns expand against passed in explicitly (`None` = no
/// home).
fn collect_package_resources_with_home(
    package_root: &Path,
    home: Option<&Path>,
) -> PackageResources {
    let mut resources = PackageResources::default();
    let manifest = std::fs::read_to_string(package_root.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|manifest| manifest.get("pi").cloned());
    let Some(manifest) = manifest.filter(|manifest| manifest.is_object()) else {
        for kind in ["extensions", "skills", "prompts", "themes"] {
            let out = match kind {
                "extensions" => &mut resources.extensions,
                "skills" => &mut resources.skills,
                "prompts" => &mut resources.prompts,
                _ => &mut resources.themes,
            };
            collect_convention_resources(package_root, kind, out);
        }
        return resources;
    };
    for (kind, out) in [
        ("extensions", &mut resources.extensions),
        ("skills", &mut resources.skills),
        ("prompts", &mut resources.prompts),
        ("themes", &mut resources.themes),
    ] {
        let Some(entries) = manifest.get(kind).and_then(Value::as_array).map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        }) else {
            continue;
        };
        // Upstream `addManifestEntries` (package-manager.ts:2384-2414): plain
        // entries are SOURCES, `+`/`-`/`!` entries are overrides applied to
        // the files those sources collect. An overrides-only array therefore
        // contributes nothing (there is no plain source to filter).
        let patterns: Vec<String> = entries
            .iter()
            .filter(|entry| is_override_pattern(entry))
            .cloned()
            .collect();
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in entries.iter().filter(|entry| !is_override_pattern(entry)) {
            collect_manifest_entry(package_root, kind, entry, &mut files);
        }
        // Keep the first occurrence order (plain-entry order, then the
        // sorted glob walk) while dropping duplicates from overlapping
        // entries, like upstream's path map.
        let mut unique = std::collections::BTreeSet::new();
        files.retain(|path| unique.insert(path.clone()));
        let filter = TopLevelResourceFilter::from_entries_with_home(&patterns, home);
        out.extend(
            files
                .into_iter()
                .filter(|path| filter.keeps(path, package_root)),
        );
    }
    resources
}

/// Whether a manifest/resources entry is a `+`/`-`/`!` override instead of a
/// source path (upstream `isOverridePattern`, package-manager.ts:300-302).
fn is_override_pattern(entry: &str) -> bool {
    entry.starts_with('!') || entry.starts_with('+') || entry.starts_with('-')
}

/// Resolve one plain manifest entry to the resource paths it names
/// (upstream `collectFilesFromManifestEntries`'s per-entry resolve + glob).
fn collect_manifest_entry(package_root: &Path, kind: &str, entry: &str, out: &mut Vec<PathBuf>) {
    let target = package_root.join(entry);
    if target.exists() {
        collect_manifest_target(&target, kind, out);
        return;
    }
    if entry.contains('*') {
        // Walk the package root and match the pattern relative to it
        // (`*` per segment, `**` across).
        let mut matches: Vec<PathBuf> = Vec::new();
        collect_glob(package_root, entry, &mut matches);
        for path in matches {
            collect_manifest_target(&path, kind, out);
        }
        return;
    }
    tracing::debug!(
        target: "pir::packages",
        manifest = %entry,
        "packages: manifest entry does not exist; skipping"
    );
}

/// Expand one resolved manifest target by resource kind (upstream
/// `collectFilesFromPaths` + `collectResourceFiles`): extensions stay a
/// file/directory the extension loader resolves itself, skills become the
/// skill directories (or a directly named file), prompts only `*.md` files,
/// themes only `*.json` files.
fn collect_manifest_target(target: &Path, kind: &str, out: &mut Vec<PathBuf>) {
    if target.is_file() {
        let keep = match kind {
            "prompts" => file_has_suffix(target, "md"),
            "themes" => file_has_suffix(target, "json"),
            _ => true,
        };
        if keep {
            out.push(target.to_path_buf());
        }
        return;
    }
    if !target.is_dir() {
        return;
    }
    match kind {
        // A manifest may name a directory of extensions; the loader owns its
        // index/manifest discovery.
        "extensions" => out.push(target.to_path_buf()),
        "skills" => collect_manifest_skill_dirs(target, true, out),
        "prompts" => collect_recursive_suffix(target, "md", out),
        "themes" => collect_recursive_suffix(target, "json", out),
        _ => {}
    }
}

/// Collect the skill directories under one manifest target (upstream
/// `collectSkillEntries`): a directory with a `SKILL.md` is one skill and is
/// not descended into; otherwise direct `.md` files at the root count, and
/// non-hidden subdirectories are searched recursively.
fn collect_manifest_skill_dirs(dir: &Path, is_root: bool, out: &mut Vec<PathBuf>) {
    if dir.join("SKILL.md").is_file() {
        out.push(dir.to_path_buf());
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut found: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    found.sort();
    for path in found {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        if path.is_dir() {
            collect_manifest_skill_dirs(&path, false, out);
        } else if is_root && file_has_suffix(&path, "md") {
            out.push(path);
        }
    }
}

/// Walk the package root and collect every path whose package-relative posix
/// form matches `pattern` (the corrected `glob_like_match` handles `**`
/// across segments and `*` within one, so a plain full-tree walk suffices —
/// no manual per-level pattern splitting).
fn collect_glob(root: &Path, pattern: &str, out: &mut Vec<PathBuf>) {
    fn walk(dir: &Path, relative: &str, pattern: &str, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut found: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        found.sort();
        for path in found {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let child_relative = if relative.is_empty() {
                name.clone()
            } else {
                format!("{relative}/{name}")
            };
            if glob_like_match(pattern, &child_relative) {
                out.push(path.clone());
            }
            if path.is_dir() {
                walk(&path, &child_relative, pattern, out);
            }
        }
    }
    walk(root, "", pattern, out);
}
