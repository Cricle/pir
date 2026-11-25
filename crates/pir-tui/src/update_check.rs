//! Version comparison helpers (upstream `utils/version-check.ts` semver
//! rules). pir deliberately performs NO startup network version check: the
//! update prompt was removed by user request; `pir update` stays available
//! as an explicit command.

use std::cmp::Ordering;

use semver::Version;

/// Parse a version the way upstream's `semver.valid` does
/// (`version-check.ts:35-36`): surrounding whitespace is trimmed and one
/// leading lowercase `v` is accepted (node-semver's `FULL` grammar is
/// `v?<major>.<minor>.<patch>[-prerelease][+build]`). Everything else must be
/// strict SemVer 2.0.0 — in particular `1.2` and `1.2.3.4` are not versions
/// for upstream either (`semver.valid("1.2") === null`), so they stay `None`
/// here and the caller's documented fallback applies. Build metadata parses
/// but never affects precedence.
pub fn parse_version(version: &str) -> Option<Version> {
    let trimmed = version.trim();
    let trimmed = trimmed.strip_prefix('v').unwrap_or(trimmed);
    Version::parse(trimmed).ok()
}

/// Semver precedence comparison (upstream `comparePackageVersions` =
/// `semver.compare`). `None` when either side is not a version (see
/// [`parse_version`]).
///
/// Precedence ignores build metadata (`1.2.3+a` == `1.2.3+b`), so this uses
/// [`Version::cmp_precedence`] rather than `Ord`; prerelease identifiers
/// compare per the semver spec (numeric identifiers numerically, so
/// `5.0.0-beta.20` > `5.0.0-beta.9`).
pub fn compare_versions(left: &str, right: &str) -> Option<Ordering> {
    let left = parse_version(left)?;
    let right = parse_version(right)?;
    Some(left.cmp_precedence(&right))
}

/// Whether `candidate` is a strictly newer release than `current` (upstream
/// `isNewerPackageVersion`): semver-ordered when both parse, falling back to
/// a trimmed string inequality (matching upstream's non-semver fallback).
pub fn is_newer_version(candidate: &str, current: &str) -> bool {
    match compare_versions(candidate, current) {
        Some(ordering) => ordering == Ordering::Greater,
        None => candidate.trim() != current.trim(),
    }
}
