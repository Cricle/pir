//! Skill discovery, frontmatter validation, and prompt formatting.
//!
//! Port of upstream `packages/coding-agent/src/core/skills.ts` (Agent Skills spec):
//! a skill is either a directory containing a `SKILL.md` or a root-level `.md` file,
//! with YAML frontmatter carrying `name`, `description`, and the optional
//! `disable-model-invocation` flag.
//!
//! # Feature gate (`skills`)
//!
//! The *data plane* here — [`SkillInfo`], [`SkillFileReadTool`], the diagnostic
//! types, the pure frontmatter helpers ([`frontmatter_parts`],
//! [`strip_frontmatter`]) and the prompt renderer ([`available_skills_xml`]) —
//! is always compiled: the system prompt, the prompt-template loader and the
//! session's `skills` field all name it, and none of it touches the
//! filesystem.
//!
//! The *discovery plane* — the recursive directory walk plus the
//! `.gitignore`/`.ignore`/`.fdignore` matcher — sits behind the `skills`
//! cargo feature. With it off, the four public entry points keep their exact
//! signatures and degrade to "found nothing" (a one-time `warning` diagnostic
//! explains why), which is the same shape a machine with no skill directories
//! produces: no skills in the system prompt, and `/skill:<name>` finds nothing.

#[cfg(feature = "skills")]
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(feature = "skills")]
use serde::Deserialize;

/// Max name length per the Agent Skills spec.
const MAX_NAME_LENGTH: usize = 64;

/// Max description length per the Agent Skills spec.
const MAX_DESCRIPTION_LENGTH: usize = 1024;

/// Directories never scanned for skills.
#[cfg(feature = "skills")]
const SKIP_DIRS: [&str; 2] = ["node_modules", ".git"];

/// Ignore files honored during skill discovery (upstream skills.ts
/// `IGNORE_FILE_NAMES`).
#[cfg(feature = "skills")]
const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];

/// The notice a `skills`-feature-off build reports when something asks it to
/// discover or load skills. Skill discovery needs the filesystem walk that the
/// feature compiles in, so the honest answer is "disabled", never an empty
/// skill list that reads like "there are no skills here".
pub const SKILLS_DISABLED: &str =
    "skill discovery is disabled in this build (the `skills` cargo feature is off)";

/// The tool that loads skill files in the system prompt (upstream
/// `formatSkillsForPrompt`'s `fileReadTool`); `Indirect` names none, for a
/// reader reachable only through another tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillFileReadTool {
    Read,
    Bash,
    Indirect,
}

/// A discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    /// Path of the skill file (`SKILL.md` or the root-level `.md` file).
    pub path: PathBuf,
    /// When true the skill is excluded from the system prompt and only reachable
    /// via an explicit `/skill:name` command.
    pub disable_model_invocation: bool,
}

impl SkillInfo {
    /// The skill's directory (upstream `Skill.baseDir`, the parent of the
    /// skill file). A root-level `.md` skill's base dir is its containing
    /// directory; a path with no parent answers `.`.
    pub fn base_dir(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }

    /// Upstream `Skill.sourceInfo` (core/skills.ts:74-81): the source file of
    /// this skill. pir tracks the file path only, so the reduced
    /// [`SourceInfo`](crate::extensions::SourceInfo) carries exactly that.
    pub fn source_info(&self) -> crate::extensions::SourceInfo {
        crate::extensions::SourceInfo {
            path: self.path.display().to_string(),
            ..Default::default()
        }
    }

    /// The upstream `Skill` wire shape (`{name, description, filePath,
    /// baseDir, sourceInfo, disableModelInvocation}`), for extension-facing
    /// payloads (`systemPromptOptions.skills`) and SDK consumers.
    pub fn to_json(&self) -> serde_json::Value {
        let file_path = self.path.display().to_string();
        serde_json::json!({
            "name": self.name,
            "description": self.description,
            "filePath": file_path,
            "baseDir": self.base_dir().display().to_string(),
            "sourceInfo": { "path": file_path },
            "disableModelInvocation": self.disable_model_invocation,
        })
    }
}

/// YAML frontmatter of a skill file.
#[cfg(feature = "skills")]
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(rename = "disable-model-invocation")]
    disable_model_invocation: Option<bool>,
}

/// The kind of a skill-discovery diagnostic (upstream
/// `ResourceDiagnostic.type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDiagnosticKind {
    /// A load or validation problem.
    Warning,
    /// A same-name duplicate whose earlier entry kept the name.
    Collision,
}

impl SkillDiagnosticKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillDiagnosticKind::Warning => "warning",
            SkillDiagnosticKind::Collision => "collision",
        }
    }
}

/// One skill-discovery diagnostic (upstream `ResourceDiagnostic`): either a
/// load/validation warning (unreadable file, malformed frontmatter in a
/// `SKILL.md`, name/description spec violations) or a same-name collision
/// carrying the kept (`winner_path`) and dropped (`path`, the loser) files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiagnostic {
    pub kind: SkillDiagnosticKind,
    pub message: String,
    /// The skill file the diagnostic is about (the loser for collisions).
    pub path: PathBuf,
    /// The kept file for collisions (upstream `collision.winnerPath`).
    pub winner_path: Option<PathBuf>,
}

impl SkillDiagnostic {
    fn warning(message: impl Into<String>, path: &Path) -> Self {
        SkillDiagnostic {
            kind: SkillDiagnosticKind::Warning,
            message: message.into(),
            path: path.to_path_buf(),
            winner_path: None,
        }
    }

    #[cfg(feature = "skills")]
    fn collision(name: &str, loser: &Path, winner: &Path) -> Self {
        SkillDiagnostic {
            kind: SkillDiagnosticKind::Collision,
            message: format!("name \"{name}\" collision"),
            path: loser.to_path_buf(),
            winner_path: Some(winner.to_path_buf()),
        }
    }
}

/// Log diagnostics that would otherwise be silent (name/description spec
/// violations, malformed frontmatter, same-name collisions). Consumers that
/// want the structured records use the `*_with_diagnostics` entry points
/// directly.
pub fn log_skill_diagnostics(diagnostics: &[SkillDiagnostic]) {
    for diagnostic in diagnostics {
        match diagnostic.kind {
            SkillDiagnosticKind::Warning => tracing::warn!(
                kind = diagnostic.kind.as_str(),
                path = %diagnostic.path.display(),
                "{}",
                diagnostic.message
            ),
            SkillDiagnosticKind::Collision => tracing::info!(
                kind = diagnostic.kind.as_str(),
                path = %diagnostic.path.display(),
                "{}",
                diagnostic.message
            ),
        }
    }
}

/// Split a markdown file into its YAML frontmatter and body.
/// Returns `None` when the file does not start with a `---` fenced block.
/// Only the `skills` feature's frontmatter parser calls this; the public
/// `frontmatter_parts`/`strip_frontmatter` helpers cover the feature-off
/// surface.
#[cfg(feature = "skills")]
fn split_frontmatter(text: &str) -> Option<&str> {
    frontmatter_parts(text).map(|(yaml, _)| yaml)
}

/// Split a markdown file into `(frontmatter_yaml, body)` (upstream
/// `parseFrontmatter`). Returns `None` when the file does not start with a
/// `---` fenced block.
pub fn frontmatter_parts(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---")?;
    // Tolerate CRLF: strip the remainder of the opening line.
    let rest = rest.strip_prefix('\r').unwrap_or(rest);
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    let mut closing = None;
    let mut offset = 0;
    for line in rest.lines() {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.trim() == "---" {
            closing = Some(offset);
            break;
        }
        offset += line.len() + 1;
    }
    let end = closing?;
    // The body starts after the closing fence line (and its newline).
    let body = &rest[end..];
    let body = body.strip_prefix("---").unwrap_or(body);
    let body = body.strip_prefix('\r').unwrap_or(body);
    let body = body.strip_prefix('\n').unwrap_or(body);
    Some((&rest[..end], body))
}

/// The file content with its frontmatter block removed (upstream
/// `stripFrontmatter`): the body after the closing `---` fence, or the whole
/// text when no frontmatter block is present.
pub fn strip_frontmatter(text: &str) -> &str {
    frontmatter_parts(text)
        .map(|(_, body)| body)
        .unwrap_or(text)
}

/// Parse skill frontmatter from file contents. Returns an error message on malformed YAML.
#[cfg(feature = "skills")]
fn parse_frontmatter(text: &str) -> Result<SkillFrontmatter, String> {
    match split_frontmatter(text) {
        Some(yaml) => serde_yaml::from_str::<SkillFrontmatter>(yaml)
            .map_err(|e| format!("invalid frontmatter: {e}")),
        None => Ok(SkillFrontmatter::default()),
    }
}

/// Validate a skill name per the Agent Skills spec; returns validation errors.
pub fn validate_name(name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if name.chars().count() > MAX_NAME_LENGTH {
        errors.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({})",
            name.chars().count()
        ));
    }
    // The empty name must warn like upstream's `/^[a-z0-9-]+$/` regex (whose
    // `+` fails on an empty string); `chars().all(...)` alone is vacuously
    // true on an empty iterator and would stay silent.
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_string());
    }
    errors
}

/// Validate a skill description per the Agent Skills spec; returns validation errors.
pub fn validate_description(description: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if description.trim().is_empty() {
        errors.push("description is required".to_string());
    } else if description.chars().count() > MAX_DESCRIPTION_LENGTH {
        errors.push(format!(
            "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
            description.chars().count()
        ));
    }
    errors
}

/// Load one skill file. Returns `Err(message)` when the skill cannot be loaded
/// (unreadable, missing/empty description, malformed frontmatter in a SKILL.md).
#[cfg(feature = "skills")]
pub fn load_skill_from_file(path: &Path) -> Result<SkillInfo, String> {
    let (skill, diagnostics) = load_skill_from_file_with_diagnostics(path);
    skill.ok_or_else(|| {
        diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .next()
            .unwrap_or_else(|| "failed to load skill file".to_string())
    })
}

/// Feature-off twin of [`load_skill_from_file`]: skill loading is not compiled
/// into this build, so the caller gets the documented "disabled" error instead
/// of a silently-empty skill.
#[cfg(not(feature = "skills"))]
pub fn load_skill_from_file(path: &Path) -> Result<SkillInfo, String> {
    let _ = path;
    Err(SKILLS_DISABLED.to_string())
}

/// Load one skill file with the upstream diagnostic surface
/// (`loadSkillFromFile`): spec violations of the name and description are
/// recorded as warnings while the skill still loads; a missing/empty
/// description, an unreadable file, or malformed frontmatter in a `SKILL.md`
/// yields no skill.
#[cfg(feature = "skills")]
pub fn load_skill_from_file_with_diagnostics(
    path: &Path,
) -> (Option<SkillInfo>, Vec<SkillDiagnostic>) {
    let mut diagnostics: Vec<SkillDiagnostic> = Vec::new();
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => {
            diagnostics.push(SkillDiagnostic::warning(
                format!("failed to read skill file: {error}"),
                path,
            ));
            return (None, diagnostics);
        }
    };
    // Strip a UTF-8 BOM if present.
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let is_declared_skill = path.file_name().is_some_and(|n| n == "SKILL.md");

    let frontmatter = if is_declared_skill {
        match parse_frontmatter(raw) {
            Ok(frontmatter) => frontmatter,
            Err(message) => {
                diagnostics.push(SkillDiagnostic::warning(message, path));
                return (None, diagnostics);
            }
        }
    } else {
        // Undeclared root-level files: ignore malformed frontmatter entirely.
        parse_frontmatter(raw).unwrap_or_default()
    };

    // Validate the description per the Agent Skills spec (upstream
    // `validateDescription`); the messages are surfaced either way, but a
    // missing/empty description still rejects the skill.
    let description = frontmatter.description.unwrap_or_default();
    for error in validate_description(&description) {
        diagnostics.push(SkillDiagnostic::warning(error, path));
    }
    if description.trim().is_empty() {
        return (None, diagnostics);
    }

    // Name: the frontmatter value falls back to the parent directory name
    // when missing, empty, or whitespace-only (upstream
    // `frontmatterName || parentDirName`, whose `||` already falls back on
    // the empty string; the whitespace case is treated the same here).
    let name = frontmatter
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| {
            path.parent()
                .and_then(|dir| dir.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    // Validate the name per the Agent Skills spec (upstream `validateName`);
    // the skill still loads with the warnings attached.
    for error in validate_name(&name) {
        diagnostics.push(SkillDiagnostic::warning(error, path));
    }

    (
        Some(SkillInfo {
            name,
            description,
            path: path.to_path_buf(),
            disable_model_invocation: frontmatter.disable_model_invocation == Some(true),
        }),
        diagnostics,
    )
}

/// Recursively scan `dir` for skills.
///
/// Discovery rules (upstream `loadSkillsFromDir`):
/// - if the directory contains a `SKILL.md`, treat it as a skill root and do not recurse
/// - otherwise, load direct `.md` children when `include_root_files` is set
/// - recurse into non-hidden subdirectories (skipping `node_modules`)
///
/// `.gitignore` / `.ignore` / `.fdignore` files filter the walk (upstream
/// `addIgnoreRules` + the `ignore` crate matcher).
#[cfg(feature = "skills")]
pub fn load_skills_from_dir(dir: &Path, include_root_files: bool) -> Vec<SkillInfo> {
    let (skills, diagnostics) = load_skills_from_dir_with_diagnostics(dir, include_root_files);
    log_skill_diagnostics(&diagnostics);
    skills
}

/// Feature-off twin of [`load_skills_from_dir`]: no directory is scanned.
#[cfg(not(feature = "skills"))]
pub fn load_skills_from_dir(dir: &Path, include_root_files: bool) -> Vec<SkillInfo> {
    load_skills_from_dir_with_diagnostics(dir, include_root_files).0
}

/// [`load_skills_from_dir`] with the diagnostic surface (upstream
/// `loadSkillsFromDir` returns `{ skills, diagnostics }`).
#[cfg(feature = "skills")]
pub fn load_skills_from_dir_with_diagnostics(
    dir: &Path,
    include_root_files: bool,
) -> (Vec<SkillInfo>, Vec<SkillDiagnostic>) {
    let mut matcher = IgnoreMatcher::default();
    load_skills_from_dir_internal(dir, include_root_files, &mut matcher, dir)
}

/// Feature-off twin of [`load_skills_from_dir_with_diagnostics`]: reports the
/// disabled notice instead of an unexplained empty list.
#[cfg(not(feature = "skills"))]
pub fn load_skills_from_dir_with_diagnostics(
    dir: &Path,
    include_root_files: bool,
) -> (Vec<SkillInfo>, Vec<SkillDiagnostic>) {
    let _ = include_root_files;
    (
        Vec::new(),
        vec![SkillDiagnostic::warning(SKILLS_DISABLED, dir)],
    )
}

#[cfg(feature = "skills")]
fn load_skills_from_dir_internal(
    dir: &Path,
    include_root_files: bool,
    matcher: &mut IgnoreMatcher,
    root: &Path,
) -> (Vec<SkillInfo>, Vec<SkillDiagnostic>) {
    let mut skills = Vec::new();
    let mut diagnostics: Vec<SkillDiagnostic> = Vec::new();
    if !dir.is_dir() {
        return (skills, diagnostics);
    }
    matcher.add_ignore_rules(dir, root);

    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return (skills, diagnostics),
    };
    let mut files: Vec<PathBuf> = Vec::new();
    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut skill_md: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Dotfiles (including the ignore files, loaded via addIgnoreRules)
        // and dependency directories are never scanned.
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        // Upstream `statSync` semantics: classify through metadata, which
        // follows symlinks (a symlinked skill directory or SKILL.md is
        // scanned; `entry.file_type()` would not follow). A broken symlink is
        // skipped (upstream `catch { continue }`).
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.is_dir() {
            subdirs.push(path);
        } else if metadata.is_file() {
            if name.as_ref() == "SKILL.md" {
                skill_md = Some(path);
            } else {
                files.push(path);
            }
        }
    }

    // A SKILL.md makes this directory a skill root; stop descending (upstream
    // checks the ignore matcher against the SKILL.md path).
    if let Some(skill_md) = skill_md {
        let relative = relative_posix(root, &skill_md);
        if !matcher.is_ignored(&relative, false) {
            let (skill, mut skill_diagnostics) = load_skill_from_file_with_diagnostics(&skill_md);
            diagnostics.append(&mut skill_diagnostics);
            if let Some(skill) = skill {
                skills.push(skill);
            }
        }
        return (skills, diagnostics);
    }

    if include_root_files {
        let mut root_md: Vec<&PathBuf> = files
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        root_md.sort();
        for path in root_md {
            let relative = relative_posix(root, path);
            if matcher.is_ignored(&relative, false) {
                continue;
            }
            let (skill, mut skill_diagnostics) = load_skill_from_file_with_diagnostics(path);
            diagnostics.append(&mut skill_diagnostics);
            if let Some(skill) = skill {
                skills.push(skill);
            }
        }
    }

    subdirs.sort();
    for subdir in subdirs {
        // Upstream tests directory entries against the matcher (with a
        // trailing separator) before recursing.
        let relative = relative_posix(root, &subdir);
        if matcher.is_ignored(&relative, true) {
            continue;
        }
        let (sub_skills, mut sub_diagnostics) =
            load_skills_from_dir_internal(&subdir, false, matcher, root);
        skills.extend(sub_skills);
        diagnostics.append(&mut sub_diagnostics);
    }
    (skills, diagnostics)
}

/// A path relative to the discovery root, with posix separators (upstream
/// `toPosixPath(relative(root, fullPath))`). Empty when `path == root`.
#[cfg(feature = "skills")]
fn relative_posix(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().replace('\\', "/"),
    }
}

// ---------------------------------------------------------------------------
// Ignore matching (upstream `addIgnoreRules` + the `ignore` crate)
// ---------------------------------------------------------------------------

/// One parsed ignore pattern.
#[cfg(feature = "skills")]
struct IgnoreRule {
    negated: bool,
    /// Pattern ended with `/`: only directories match.
    dir_only: bool,
    regex: regex::Regex,
}

/// A gitignore-style matcher over the discovery walk. Rules accumulate in
/// file order across directories; the LAST matching rule wins (the `ignore`
/// crate's semantics).
#[derive(Default)]
#[cfg(feature = "skills")]
struct IgnoreMatcher {
    rules: Vec<IgnoreRule>,
}

#[cfg(feature = "skills")]
impl IgnoreMatcher {
    /// Load `.gitignore` / `.ignore` / `.fdignore` from `dir`, prefixing each
    /// pattern with the directory's path relative to the root (upstream
    /// `prefixIgnorePattern`).
    fn add_ignore_rules(&mut self, dir: &Path, root: &Path) {
        let relative = relative_posix(root, dir);
        let prefix = if relative.is_empty() {
            String::new()
        } else {
            format!("{relative}/")
        };
        for filename in IGNORE_FILE_NAMES {
            let ignore_path = dir.join(filename);
            let Ok(content) = fs::read_to_string(&ignore_path) else {
                continue;
            };
            for line in content.lines() {
                if let Some(pattern) = prefix_ignore_pattern(line, &prefix) {
                    if let Some(rule) = parse_ignore_pattern(&pattern) {
                        self.rules.push(rule);
                    }
                }
            }
        }
    }

    /// Whether `relative_path` (posix separators) is ignored. `is_dir` gates
    /// dir-only patterns. The last matching rule decides.
    fn is_ignored(&self, relative_path: &str, is_dir: bool) -> bool {
        let mut ignored = false;
        for rule in &self.rules {
            if rule.dir_only && !is_dir {
                continue;
            }
            if rule.regex.is_match(relative_path) {
                ignored = !rule.negated;
            }
        }
        ignored
    }
}

/// Upstream `prefixIgnorePattern`: skip blanks and comments, carry negation,
/// strip a leading `/`, and prepend the directory prefix.
#[cfg(feature = "skills")]
fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('#') && !trimmed.starts_with("\\#") {
        return None;
    }

    let mut pattern = trimmed.to_owned();
    let mut negated = false;
    if let Some(rest) = pattern.strip_prefix('!') {
        negated = true;
        pattern = rest.to_owned();
    } else if let Some(rest) = pattern.strip_prefix("\\!") {
        pattern = rest.to_owned();
    }
    let pattern = pattern.strip_prefix('/').unwrap_or(&pattern);
    let prefixed = format!("{prefix}{pattern}");
    Some(if negated {
        format!("!{prefixed}")
    } else {
        prefixed
    })
}

/// Parse one (already prefixed) ignore pattern into a matching rule.
#[cfg(feature = "skills")]
fn parse_ignore_pattern(pattern: &str) -> Option<IgnoreRule> {
    let mut pattern = pattern;
    let negated = if let Some(rest) = pattern.strip_prefix('!') {
        pattern = rest;
        true
    } else {
        false
    };
    if pattern.is_empty() {
        return None;
    }
    let dir_only = pattern.ends_with('/');
    let pattern = pattern.trim_end_matches('/');
    if pattern.is_empty() {
        return None;
    }
    // A pattern with an embedded separator is anchored to the root.
    let anchored = pattern.contains('/');
    let core = glob_to_regex(pattern, anchored)?;
    let regex = regex::Regex::new(&core).ok()?;
    Some(IgnoreRule {
        negated,
        dir_only,
        regex,
    })
}

/// Convert a gitignore glob into a fully anchored regex body.
#[cfg(feature = "skills")]
fn glob_to_regex(pattern: &str, anchored: bool) -> Option<String> {
    let mut regex = String::from("^");
    if !anchored {
        // Unanchored patterns match at any depth.
        regex.push_str("(?:.*/)?");
    }
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    // `**/` at the start, `/**/` in the middle, `/**` at the
                    // end, otherwise a `**` segment crosses separators.
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        regex.push_str("(?:[^/]+/)*");
                    } else {
                        regex.push_str(".*");
                    }
                } else {
                    regex.push_str("[^/]*");
                }
            }
            '?' => regex.push_str("[^/]"),
            '[' => {
                // Copy the character class through, translating a leading
                // `!` negation to regex `^`.
                let mut class = String::from("[");
                let mut closed = false;
                while let Some(&inner) = chars.peek() {
                    chars.next();
                    if inner == ']' {
                        closed = true;
                        break;
                    }
                    if class.len() == 1 && inner == '!' {
                        class.push('^');
                    } else if inner == '\\' || inner == '^' {
                        class.push('\\');
                        class.push(inner);
                    } else {
                        class.push(inner);
                    }
                }
                if closed && class.len() > 1 {
                    class.push(']');
                    regex.push_str(&class);
                } else {
                    // Unterminated class: treat `[` as a literal.
                    regex.push_str("\\[");
                    regex.push_str(&class[1..]);
                }
            }
            other => {
                if !other.is_ascii_alphanumeric() && !matches!(other, '_' | '-' | '/' | '.' | ' ') {
                    regex.push('\\');
                }
                regex.push(other);
            }
        }
    }
    regex.push('$');
    Some(regex)
}

/// Discover skills across the given directories, deduplicating by name (first wins).
#[cfg(feature = "skills")]
pub fn discover_skills(dirs: &[PathBuf]) -> Vec<SkillInfo> {
    let (skills, diagnostics) = discover_skills_with_diagnostics(dirs);
    log_skill_diagnostics(&diagnostics);
    skills
}

/// Feature-off twin of [`discover_skills`]: nothing is discovered.
#[cfg(not(feature = "skills"))]
pub fn discover_skills(dirs: &[PathBuf]) -> Vec<SkillInfo> {
    discover_skills_with_diagnostics(dirs).0
}

/// [`discover_skills`] with the diagnostic surface (upstream `loadSkills`
/// returns `{ skills, diagnostics }`): name/description spec violations and
/// load warnings flow out as `warning` records, and a same-name duplicate is
/// recorded as a `collision` carrying the winner (earlier) and loser (later)
/// paths. The exact same file reached through a symlink is skipped silently
/// (upstream `canonicalizePath` dedupe).
#[cfg(feature = "skills")]
pub fn discover_skills_with_diagnostics(
    dirs: &[PathBuf],
) -> (Vec<SkillInfo>, Vec<SkillDiagnostic>) {
    let mut skills: Vec<SkillInfo> = Vec::new();
    let mut diagnostics: Vec<SkillDiagnostic> = Vec::new();
    let mut seen_names = std::collections::HashSet::new();
    let mut seen_real_paths = std::collections::HashSet::new();
    for dir in dirs {
        let (loaded, mut dir_diagnostics) = load_skills_from_dir_with_diagnostics(dir, true);
        diagnostics.append(&mut dir_diagnostics);
        for skill in loaded {
            // Skip silently when the same file was already loaded through a
            // symlink (upstream realPathSet).
            if !seen_real_paths.insert(canonical_path(&skill.path)) {
                continue;
            }
            if seen_names.insert(skill.name.clone()) {
                skills.push(skill);
            } else if let Some(existing) = skills.iter().find(|s| s.name == skill.name) {
                diagnostics.push(SkillDiagnostic::collision(
                    &skill.name,
                    &skill.path,
                    &existing.path,
                ));
            }
        }
    }
    (skills, diagnostics)
}

/// Feature-off twin of [`discover_skills_with_diagnostics`].
#[cfg(not(feature = "skills"))]
pub fn discover_skills_with_diagnostics(
    dirs: &[PathBuf],
) -> (Vec<SkillInfo>, Vec<SkillDiagnostic>) {
    let mut diagnostics = Vec::new();
    if let Some(first) = dirs.first() {
        diagnostics.push(SkillDiagnostic::warning(SKILLS_DISABLED, first));
    }
    (Vec::new(), diagnostics)
}

/// Resolve symlinks for duplicate detection, falling back to the literal path
/// when canonicalization fails (upstream `canonicalizePath`).
#[cfg(feature = "skills")]
fn canonical_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Format skills for inclusion in the system prompt, using the Agent Skills XML shape.
/// Skills with `disable_model_invocation` are excluded (command-only).
/// `file_read_tool` names the tool that loads skill files; `SkillFileReadTool::Indirect`
/// names none, for a reader reachable only through another tool (upstream
/// `formatSkillsForPrompt`).
pub fn available_skills_xml(skills: &[SkillInfo], file_read_tool: SkillFileReadTool) -> String {
    let visible: Vec<&SkillInfo> = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }

    let read_hint = match file_read_tool {
        SkillFileReadTool::Read => {
            "Use the read tool to load a skill's file when the task matches its description."
        }
        SkillFileReadTool::Bash => {
            "Use bash to load a skill's file when the task matches its description."
        }
        SkillFileReadTool::Indirect => "Load a skill's file when the task matches its description.",
    };
    let mut lines = vec![
        String::new(),
        String::new(),
        "The following skills provide specialized instructions for specific tasks.".to_string(),
        read_hint.to_string(),
        "When a skill file references a relative path, resolve it against the skill directory \
(parent of SKILL.md / dirname of the path) and use that absolute path in tool commands."
            .to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];
    for skill in visible {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.path.to_string_lossy())
        ));
        lines.push("  </skill>".to_string());
    }
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}
