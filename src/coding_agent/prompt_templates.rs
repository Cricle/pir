//! Prompt templates: markdown files with `description`/`argument-hint`
//! frontmatter registered as slash commands (upstream
//! `packages/coding-agent/src/core/prompt-templates.ts`).
//!
//! A template is a `.md` file whose name (without the extension) is the
//! slash-command name; submitting `/name args` expands the template body
//! with the arguments (`$1..$@`, `${N:-default}`, `${@:N:L}`) and sends the
//! result as the prompt.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// A prompt template loaded from a markdown file (upstream `PromptTemplate`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTemplate {
    /// Command name: the file name without the `.md` extension.
    pub name: String,
    /// Frontmatter `description`, else the first non-empty body line
    /// (truncated to 60 chars, upstream `loadTemplateFromFile`).
    pub description: String,
    /// Frontmatter `argument-hint`, when present.
    pub argument_hint: Option<String>,
    /// The template body (frontmatter removed).
    pub content: String,
    /// Absolute path to the template file.
    pub file_path: PathBuf,
}

impl PromptTemplate {
    /// Upstream `PromptTemplate.sourceInfo` (`core/prompt-templates.ts:12-19`):
    /// the template's source file. pir tracks the file path only, so the
    /// reduced [`SourceInfo`](crate::extensions::SourceInfo) carries exactly
    /// that.
    pub fn source_info(&self) -> crate::extensions::SourceInfo {
        crate::extensions::SourceInfo {
            path: self.file_path.display().to_string(),
            ..Default::default()
        }
    }

    /// The upstream `PromptTemplate` wire shape (`{name, description,
    /// argumentHint?, content, sourceInfo, filePath}`).
    pub fn to_json(&self) -> serde_json::Value {
        let file_path = self.file_path.display().to_string();
        serde_json::json!({
            "name": self.name,
            "description": self.description,
            "argumentHint": self.argument_hint,
            "content": self.content,
            "sourceInfo": { "path": file_path },
            "filePath": file_path,
        })
    }
}

/// Template frontmatter fields pir consumes.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TemplateFrontmatter {
    description: Option<String>,
    #[serde(rename = "argument-hint")]
    argument_hint: Option<String>,
}

/// Parse command arguments respecting quoted strings, bash-style (upstream
/// `parseCommandArgs`).
pub fn parse_command_args(args_string: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut current = String::new();
    // Tracks whether a token has started, so quoted empty strings ("" or '')
    // are kept as arguments (upstream #10867).
    let mut has_token = false;
    let mut in_quote: Option<char> = None;

    for char in args_string.chars() {
        if let Some(quote) = in_quote {
            if char == quote {
                in_quote = None;
            } else {
                current.push(char);
            }
        } else if char == '"' || char == '\'' {
            in_quote = Some(char);
            has_token = true;
        } else if char.is_whitespace() {
            if has_token {
                args.push(std::mem::take(&mut current));
                has_token = false;
            }
        } else {
            current.push(char);
            has_token = true;
        }
    }

    if has_token {
        args.push(current);
    }

    args
}

/// Substitute argument placeholders in template content (upstream
/// `substituteArgs`). Supported:
/// - `$1`, `$2`, ... positional args (missing -> empty string)
/// - `$@` / `$ARGUMENTS` — all args joined by spaces
/// - `${N:-default}` / `${@:-default}` / `${ARGUMENTS:-default}` — value or
///   default when missing/empty
/// - `${@:N}` — args from the Nth onwards; `${@:N:L}` — L args from the Nth
///
/// Replacement happens on the template only; argument and default values are
/// never recursively substituted.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all_args = args.join(" ");
    let mut out = String::with_capacity(content.len());
    let bytes = content.as_bytes();
    let mut index = 0usize;

    while index < bytes.len() {
        if bytes[index] != b'$' {
            let ch = content[index..].chars().next().unwrap_or('\u{fffd}');
            out.push(ch);
            index += ch.len_utf8();
            continue;
        }

        // `${...}` forms.
        if bytes.get(index + 1) == Some(&b'{') {
            if let Some((replacement, consumed)) =
                parse_braced(&content[index + 2..], args, &all_args)
            {
                out.push_str(&replacement);
                index += 2 + consumed;
                continue;
            }
        }

        // `$ARGUMENTS` (longest simple form first), then `$@`, then `$N`.
        let rest = &content[index + 1..];
        if rest.starts_with("ARGUMENTS") {
            out.push_str(&all_args);
            index += 1 + "ARGUMENTS".len();
            continue;
        }
        if rest.starts_with('@') {
            out.push_str(&all_args);
            index += 2;
            continue;
        }
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 {
            let number: usize = rest[..digits].parse().unwrap_or(0);
            let value = if number == 0 {
                String::new()
            } else {
                args.get(number - 1).cloned().unwrap_or_default()
            };
            out.push_str(&value);
            index += 1 + digits;
            continue;
        }

        // A bare `$` with no recognized form is literal.
        out.push('$');
        index += 1;
    }
    out
}

/// Parse the inside of a `${...}` placeholder starting AFTER `${`. Returns
/// `(replacement, bytes_of_rest_consumed_including_the_closing_brace)` or
/// `None` when the form is not recognized / not terminated.
fn parse_braced(rest: &str, args: &[String], all_args: &str) -> Option<(String, usize)> {
    let close = rest.find('}')?;
    let inner = &rest[..close];
    let consumed = close + 1;

    let (target, default_value) = match inner.split_once(":-") {
        Some((target, default_value)) => (target, Some(default_value)),
        None => (inner, None),
    };

    // `${N:-default}` / `${@:-default}` / `${ARGUMENTS:-default}`.
    if let Some(default_value) = default_value {
        let value = match target {
            "@" | "ARGUMENTS" => all_args.to_string(),
            other => {
                let number: usize = other.parse().ok()?;
                if number == 0 {
                    String::new()
                } else {
                    args.get(number - 1).cloned().unwrap_or_default()
                }
            }
        };
        let replacement = if value.is_empty() {
            default_value.to_string()
        } else {
            value
        };
        return Some((replacement, consumed));
    }

    // `${@:N}` / `${@:N:L}` bash-style slicing (1-indexed; `${@:0}` is
    // treated as `${@:1}` per the bash convention).
    if let Some(slice) = inner.strip_prefix("@:") {
        let mut parts = slice.split(':');
        let start: usize = parts.next()?.trim().parse().ok()?;
        let length: Option<usize> = match parts.next() {
            Some(text) => Some(text.trim().parse().ok()?),
            None => None,
        };
        let start = start.saturating_sub(1);
        let selected: Vec<String> = match length {
            Some(length) => args.iter().skip(start).take(length).cloned().collect(),
            None => args.iter().skip(start).cloned().collect(),
        };
        return Some((selected.join(" "), consumed));
    }

    None
}

/// Load one template file (upstream `loadTemplateFromFile`). Returns
/// `Err(warning)` when the file cannot be read or its frontmatter fails to
/// parse — the file is rejected either way, with the warning surfaced
/// upstream as a `ResourceDiagnostic` (prompt-templates.ts:121-127).
pub fn load_template_file(path: &Path) -> Result<PromptTemplate, String> {
    let raw = fs::read_to_string(path).map_err(|e| {
        format!(
            "failed to read prompt template file: {e} ({})",
            path.display()
        )
    })?;
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let (frontmatter_yaml, body) = match crate::skills::frontmatter_parts(raw) {
        Some((yaml, body)) => (yaml, body),
        None => ("", raw),
    };
    // A malformed frontmatter block rejects the file (upstream records a
    // warning and skips it); no frontmatter at all is fine.
    let frontmatter: TemplateFrontmatter = if frontmatter_yaml.trim().is_empty() {
        TemplateFrontmatter::default()
    } else {
        serde_yaml::from_str::<TemplateFrontmatter>(frontmatter_yaml).map_err(|e| {
            format!(
                "failed to parse prompt template file: {e} ({})",
                path.display()
            )
        })?
    };

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = name.strip_suffix(".md").unwrap_or(&name).to_string();

    // Description from frontmatter, else the first non-empty body line
    // (truncated to 60 chars with an ellipsis, upstream behavior).
    let mut description = frontmatter
        .description
        .filter(|description| !description.trim().is_empty())
        .unwrap_or_default();
    if description.is_empty() {
        if let Some(first_line) = body.lines().find(|line| !line.trim().is_empty()) {
            description = first_line.chars().take(60).collect();
            if first_line.chars().count() > 60 {
                description.push_str("...");
            }
        }
    }

    Ok(PromptTemplate {
        name,
        description,
        argument_hint: frontmatter
            .argument_hint
            .filter(|hint| !hint.trim().is_empty()),
        content: body.to_string(),
        file_path: path.to_path_buf(),
    })
}

/// Scan a directory for `.md` files (non-recursive) and load them as prompt
/// templates (upstream `loadTemplatesFromDir`). Returns the templates plus
/// the load/parse warnings of rejected files.
pub fn load_templates_from_dir(dir: &Path) -> (Vec<PromptTemplate>, Vec<String>) {
    let mut templates: Vec<PromptTemplate> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return (templates, warnings);
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    files.sort();
    for path in files {
        match load_template_file(&path) {
            Ok(template) => templates.push(template),
            Err(warning) => warnings.push(warning),
        }
    }
    (templates, warnings)
}

/// Expand a `/name args` command against the loaded templates (upstream
/// `expandPromptTemplate`): returns the expanded content, or the original
/// text when it is not a template command or the name is unknown.
pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
    if !text.starts_with('/') {
        return text.to_string();
    }
    // `/name` or `/name args...` (upstream `^\/([^\s]+)(?:\s+([\s\S]*))?$`).
    let body = &text[1..];
    let (name, args_string) = match body.find(char::is_whitespace) {
        Some(space_index) => (&body[..space_index], body[space_index..].trim_start()),
        None => (body, ""),
    };
    if name.is_empty() {
        return text.to_string();
    }
    let Some(template) = templates.iter().find(|template| template.name == name) else {
        return text.to_string();
    };
    let args = parse_command_args(args_string);
    substitute_args(&template.content, &args)
}
