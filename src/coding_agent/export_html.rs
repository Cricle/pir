//! Session → self-contained HTML export.
//!
//! Faithful equivalent of upstream `core/export-html/` (upstream embeds a
//! client-side JS template; pir renders the same content server-side into one
//! static file): a header with the session metadata and stats, user/assistant
//! messages with markdown converted to HTML, tool execution blocks with their
//! (ANSI-stripped) output, and inline CSS. No external scripts, fonts, or
//! assets.

use std::collections::BTreeMap;
use std::path::Path;

use crate::ai::types::{ContentBlock, Message, Usage};
use crate::session_manager::{types::FileEntry, SessionManager};

// ---------------------------------------------------------------------------
// HTML helpers
// ---------------------------------------------------------------------------

/// Escape text for HTML body/attribute contexts (upstream template.js
/// `escapeHtml`).
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Strip ANSI escape sequences for tool output (upstream renders custom tool
/// output through TUI→HTML; pir strips the codes and keeps the plain text).
fn ansi_safe(text: &str) -> String {
    crate::text::strip_ansi(text)
}

// ---------------------------------------------------------------------------
// Minimal markdown → HTML (hand-rolled subset of upstream's `marked`)
// ---------------------------------------------------------------------------

/// Whether `url` uses a scheme safe to emit as an href (upstream
/// `sanitizeMarkdownUrl`).
fn safe_link_url(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return false;
    }
    let Some(colon) = trimmed.find(':') else {
        return true; // relative link
    };
    let scheme = &trimmed[..colon];
    !scheme.is_empty()
        && scheme
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '+' || ch == '.' || ch == '-')
        && matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "mailto" | "tel" | "ftp"
        )
}

/// Inline formatting on already-escaped text: inline code, bold, italic, and
/// links.
fn markdown_inline(escaped: &str) -> String {
    let mut out = String::with_capacity(escaped.len());
    let chars = escaped.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '`' {
            if let Some(close) = (index + 1..chars.len()).find(|&i| chars[i] == '`') {
                let code: String = chars[index + 1..close].iter().collect();
                out.push_str("<code>");
                out.push_str(&code);
                out.push_str("</code>");
                index = close + 1;
                continue;
            }
        }
        if chars[index] == '*' {
            let doubles = chars.get(index + 1) == Some(&'*');
            let (marker_len, tag) = if doubles { (2, "strong") } else { (1, "em") };
            if let Some(close) = find_inline_close(&chars, index + marker_len, doubles) {
                let inner: String = chars[index + marker_len..close].iter().collect();
                out.push('<');
                out.push_str(tag);
                out.push('>');
                out.push_str(&markdown_inline(&inner));
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
                index = close + marker_len;
                continue;
            }
        }
        if chars[index] == '[' {
            if let Some((label, url, end)) = parse_link(&chars, index) {
                if safe_link_url(&url) {
                    out.push_str("<a href=\"");
                    out.push_str(&escape_html(url.trim()));
                    out.push_str("\">");
                    out.push_str(&markdown_inline(&escape_html(&label)));
                    out.push_str("</a>");
                } else {
                    out.push_str(&escape_html(&label));
                }
                index = end;
                continue;
            }
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

/// Find the closing marker for an emphasis span starting at `start`
/// (non-greedy: the first unescaped marker of the same arity).
fn find_inline_close(chars: &[char], start: usize, doubles: bool) -> Option<usize> {
    let mut index = start;
    while index < chars.len() {
        if chars[index] == '*' {
            if doubles {
                if chars.get(index + 1) == Some(&'*') {
                    return Some(index);
                }
            } else {
                return Some(index);
            }
        }
        index += 1;
    }
    None
}

/// Parse `[label](url)` starting at an opening bracket; returns
/// `(label, url, index-after-close-paren)`.
fn parse_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let mut index = start + 1;
    let mut label = String::new();
    while index < chars.len() && chars[index] != ']' {
        label.push(chars[index]);
        index += 1;
    }
    if index >= chars.len() || chars.get(index + 1) != Some(&'(') {
        return None;
    }
    index += 2;
    let mut url = String::new();
    while index < chars.len() && chars[index] != ')' {
        url.push(chars[index]);
        index += 1;
    }
    if index >= chars.len() {
        return None;
    }
    Some((label, url, index + 1))
}

/// Convert a markdown subset to HTML: fenced code blocks, headings, unordered
/// and ordered lists, and paragraphs with inline formatting. Everything else
/// is escaped verbatim (upstream uses `marked`; pir hand-rolls this subset).
pub fn markdown_to_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    let mut lines = text.lines().peekable();
    let mut paragraph: Vec<String> = Vec::new();
    let mut list_kind: Option<ListKind> = None;

    enum ListKind {
        Unordered,
        Ordered,
    }

    fn heading_level(line: &str) -> Option<usize> {
        let level = line.chars().take_while(|ch| *ch == '#').count();
        if (1..=6).contains(&level) && line[level..].starts_with(' ') {
            Some(level)
        } else {
            None
        }
    }

    fn list_item(line: &str) -> Option<(ListKind, &str)> {
        if let Some(rest) = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("+ "))
        {
            return Some((ListKind::Unordered, rest));
        }
        let digits = line.chars().take_while(|ch| ch.is_ascii_digit()).count();
        if digits > 0 {
            let rest = &line[digits..];
            if let Some(rest) = rest.strip_prefix(". ") {
                return Some((ListKind::Ordered, rest));
            }
        }
        None
    }

    fn flush_paragraph(out: &mut String, paragraph: &mut Vec<String>) {
        if paragraph.is_empty() {
            return;
        }
        out.push_str("<p>");
        let joined = paragraph.join("\n");
        out.push_str(&markdown_inline(&escape_html(&joined)));
        out.push_str("</p>\n");
        paragraph.clear();
    }

    fn close_list(out: &mut String, list_kind: &mut Option<ListKind>) {
        match list_kind.take() {
            Some(ListKind::Unordered) => out.push_str("</ul>\n"),
            Some(ListKind::Ordered) => out.push_str("</ol>\n"),
            None => {}
        }
    }

    while let Some(line) = lines.next() {
        // Fenced code block.
        if line.trim_start().starts_with("```") {
            flush_paragraph(&mut out, &mut paragraph);
            close_list(&mut out, &mut list_kind);
            out.push_str("<pre><code>");
            for code_line in lines.by_ref() {
                if code_line.trim_start().starts_with("```") {
                    break;
                }
                out.push_str(&escape_html(code_line));
                out.push('\n');
            }
            out.push_str("</code></pre>\n");
            continue;
        }
        // Headings.
        if let Some(level) = heading_level(line) {
            flush_paragraph(&mut out, &mut paragraph);
            close_list(&mut out, &mut list_kind);
            let content = line[level..].trim();
            out.push_str(&format!("<h{level}>"));
            out.push_str(&markdown_inline(&escape_html(content)));
            out.push_str(&format!("</h{level}>\n"));
            continue;
        }
        // Lists.
        if let Some((kind, content)) = list_item(line) {
            flush_paragraph(&mut out, &mut paragraph);
            if list_kind
                .as_ref()
                .map(|current| std::mem::discriminant(current) != std::mem::discriminant(&kind))
                .unwrap_or(false)
            {
                close_list(&mut out, &mut list_kind);
            }
            match &kind {
                ListKind::Unordered if list_kind.is_none() => out.push_str("<ul>\n"),
                ListKind::Ordered if list_kind.is_none() => out.push_str("<ol>\n"),
                _ => {}
            }
            list_kind = Some(kind);
            out.push_str("<li>");
            out.push_str(&markdown_inline(&escape_html(content)));
            out.push_str("</li>\n");
            continue;
        }
        // Blank line: paragraph/list boundary.
        if line.trim().is_empty() {
            flush_paragraph(&mut out, &mut paragraph);
            close_list(&mut out, &mut list_kind);
            continue;
        }
        close_list(&mut out, &mut list_kind);
        paragraph.push(line.to_owned());
    }
    flush_paragraph(&mut out, &mut paragraph);
    close_list(&mut out, &mut list_kind);
    out
}

// ---------------------------------------------------------------------------
// Transcript rendering
// ---------------------------------------------------------------------------

/// Flattened text of a message's text blocks (upstream `extractContent`).
fn text_of(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One tool result's renderable payload, keyed by tool call id (upstream
/// `findToolResult` returns the whole result message).
struct ToolResultView {
    /// ANSI-stripped text content.
    text: String,
    /// Inline images as `(base64 data, mime type)` pairs.
    images: Vec<(String, String)>,
    is_error: bool,
}

/// The tool results keyed by tool call id (upstream `findToolResult`).
fn tool_results_by_call(entries: &[Message]) -> BTreeMap<String, ToolResultView> {
    let mut map = BTreeMap::new();
    for message in entries {
        if let Message::ToolResult(result) = message {
            let text = ansi_safe(&text_of(&result.content));
            let images = result
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Image(image) => {
                        Some((image.data.clone(), image.mime_type.clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            map.insert(
                result.tool_call_id.clone(),
                ToolResultView {
                    text,
                    images,
                    is_error: result.is_error,
                },
            );
        }
    }
    map
}

/// Shortened display path (upstream `shortenPath`: `~` for the home prefix).
fn shorten_path(path: &str) -> String {
    if let Some(home) = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
    {
        let home = home.to_string_lossy().replace('\\', "/");
        let normalized = path.replace('\\', "/");
        if normalized.starts_with(&home) {
            return format!("~{}", &normalized[home.len()..]);
        }
    }
    path.to_owned()
}

/// Parse a line number like upstream `parseLineNumber` (render-utils.ts):
/// finite numbers and numeric strings only.
fn parse_line_number(value: &serde_json::Value) -> Option<f64> {
    let number = match value {
        serde_json::Value::Number(number) => number.as_f64()?,
        serde_json::Value::String(text) if !text.trim().is_empty() => {
            text.trim().parse::<f64>().ok()?
        }
        _ => return None,
    };
    number.is_finite().then_some(number)
}

/// `formatReadLineRange` (render-utils.ts:38-49): shown whenever either
/// `offset` or `limit` is non-null; a lone `limit` starts at line 1, and a
/// malformed range is omitted rather than guessed at.
fn format_read_line_range(arguments: &serde_json::Value) -> String {
    let offset = arguments.get("offset").filter(|value| !value.is_null());
    let limit = arguments.get("limit").filter(|value| !value.is_null());
    if offset.is_none() && limit.is_none() {
        return String::new();
    }
    let start = match offset {
        Some(value) => match parse_line_number(value) {
            Some(start) => start,
            None => return String::new(),
        },
        None => 1.0,
    };
    match limit {
        Some(value) => {
            let Some(count) = parse_line_number(value) else {
                return String::new();
            };
            let end = start + count - 1.0;
            // Upstream interpolates the number, so an integral 1.0 renders as
            // `1`; an end of 0 is falsy in JS and drops the range.
            if end != 0.0 {
                format!(":{start}-{end}")
            } else {
                format!(":{start}")
            }
        }
        None => format!(":{start}"),
    }
}

/// One-line tool-call summary (upstream `formatToolCall`).
fn format_tool_call(name: &str, arguments: &serde_json::Value) -> String {
    let arg = |key: &str| {
        arguments
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    match name {
        "read" => {
            // Upstream `formatReadCall`: `args.path || args.file_path`.
            let path = match arg("path") {
                path if !path.is_empty() => path,
                _ => arg("file_path"),
            };
            format!(
                "[read: {}{}]",
                shorten_path(&path),
                format_read_line_range(arguments)
            )
        }
        "write" | "edit" => format!("[{name}: {}]", shorten_path(&arg("path"))),
        "bash" => {
            let raw = arg("command");
            let collapsed = raw.replace(['\n', '\t'], " ");
            let mut cmd: String = collapsed.trim().chars().take(50).collect();
            if raw.chars().count() > 50 {
                cmd.push_str("...");
            }
            format!("[bash: {cmd}]")
        }
        "grep" | "find" => format!(
            "[{name}: {} in {}]",
            arg("pattern"),
            shorten_path(&arg("path"))
        ),
        "ls" => format!("[ls: {}]", shorten_path(&arg("path"))),
        _ => {
            let args = serde_json::to_string(arguments).unwrap_or_default();
            let mut display: String = args.chars().take(40).collect();
            if args.chars().count() > 40 {
                display.push_str("...");
            }
            format!("[{name}: {display}]")
        }
    }
}

/// Tool-call HTML: a header, the (pretty) arguments, and the linked result
/// output/images when present (upstream `renderToolCall`).
fn render_tool_call_html(
    call: &crate::ai::types::ToolCall,
    results: &BTreeMap<String, ToolResultView>,
) -> String {
    let args_json = serde_json::to_value(&call.arguments).unwrap_or(serde_json::Value::Null);
    let result = results.get(&call.id);
    let status = match result {
        Some(result) => {
            if result.is_error {
                "error"
            } else {
                "success"
            }
        }
        None => "pending",
    };
    let mut html = format!("<div class=\"tool-execution {status}\">");
    html.push_str(&format!(
        "<div class=\"tool-header\"><span class=\"tool-name\">{}</span></div>",
        escape_html(&format_tool_call(&call.name, &args_json))
    ));
    let args_pretty = serde_json::to_string_pretty(&args_json).unwrap_or_default();
    if !args_pretty.is_empty() && args_pretty != "null" {
        html.push_str(&format!(
            "<div class=\"tool-args\"><pre>{}</pre></div>",
            escape_html(&ansi_safe(&args_pretty))
        ));
    }
    if let Some(result) = result {
        for (data, mime) in &result.images {
            html.push_str(&format!(
                "<img src=\"data:{};base64,{}\" class=\"tool-image\" alt=\"tool result image\" />",
                escape_html(mime),
                escape_html(data)
            ));
        }
        if !result.text.trim().is_empty() {
            html.push_str(&format!(
                "<div class=\"tool-output\"><pre>{}</pre></div>",
                escape_html(&result.text)
            ));
        }
    }
    html.push_str("</div>");
    html
}

/// Render one transcript message (upstream `renderEntry`).
fn render_message_html(message: &Message, results: &BTreeMap<String, ToolResultView>) -> String {
    match message {
        Message::User(user) => {
            let mut html = String::from("<div class=\"user-message\">");
            for block in &user.content {
                if let ContentBlock::Image(image) = block {
                    html.push_str(&format!(
                        "<img src=\"data:{};base64,{}\" class=\"message-image\" alt=\"attached image\" />",
                        escape_html(&image.mime_type),
                        escape_html(&image.data)
                    ));
                }
            }
            let text = text_of(&user.content);
            if !text.trim().is_empty() {
                html.push_str(&format!(
                    "<div class=\"markdown-content\">{}</div>",
                    markdown_to_html(&text)
                ));
            }
            html.push_str("</div>");
            html
        }
        Message::Assistant(assistant) => {
            let mut html = String::from("<div class=\"assistant-message\">");
            for block in &assistant.content {
                match block {
                    ContentBlock::Thinking(thinking) => {
                        html.push_str(&format!(
                            "<details class=\"thinking-block\"><summary>Thinking</summary><div class=\"thinking-text\">{}</div></details>",
                            escape_html(&ansi_safe(&thinking.thinking))
                        ));
                    }
                    ContentBlock::Text(text) => {
                        html.push_str(&format!(
                            "<div class=\"assistant-text markdown-content\">{}</div>",
                            markdown_to_html(&text.text)
                        ));
                    }
                    ContentBlock::Image(_) => {}
                    ContentBlock::ToolCall(call) => {
                        html.push_str(&render_tool_call_html(call, results));
                    }
                }
            }
            if let Some(error) = &assistant.error_message {
                html.push_str(&format!(
                    "<div class=\"error-text\">Error: {}</div>",
                    escape_html(&ansi_safe(error))
                ));
            }
            html.push_str("</div>");
            html
        }
        Message::ToolResult(_) => String::new(),
        Message::System(system) => format!(
            "<div class=\"system-message\"><pre>{}</pre></div>",
            escape_html(&ansi_safe(&text_of(&system.content)))
        ),
        Message::Developer(developer) => format!(
            "<div class=\"system-message\"><pre>{}</pre></div>",
            escape_html(&ansi_safe(&text_of(&developer.content)))
        ),
    }
}

/// Format a token count (upstream `formatTokens`).
fn format_tokens(count: u64) -> String {
    if count < 1000 {
        count.to_string()
    } else if count < 1_000_000 {
        format!("{:.1}k", count as f64 / 1000.0)
    } else {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    }
}

/// Build the self-contained HTML document (upstream `generateHtml`, rendered
/// server-side instead of through the JS template).
fn generate_html(
    header: Option<&crate::session_manager::types::SessionHeader>,
    entries: &[FileEntry],
) -> String {
    // Project entries onto messages on the leaf path (the exported branch).
    let messages: Vec<Message> = entries
        .iter()
        .flat_map(crate::session_manager::projection::session_entry_to_context_messages)
        .collect();
    let results = tool_results_by_call(&messages);

    // Stats (upstream `computeStats`).
    let mut user_messages = 0u64;
    let mut assistant_messages = 0u64;
    let mut tool_results = 0u64;
    let mut tool_calls = 0u64;
    let mut tokens = Usage::default();
    let mut models: Vec<String> = Vec::new();
    for message in &messages {
        if let Message::Assistant(assistant) = message {
            assistant_messages += 1;
            tool_calls += assistant
                .content
                .iter()
                .filter(|block| matches!(block, ContentBlock::ToolCall(_)))
                .count() as u64;
            let model = format!("{}/{}", assistant.provider, assistant.model);
            if !models.contains(&model) {
                models.push(model);
            }
            tokens.input += assistant.usage.input;
            tokens.output += assistant.usage.output;
            tokens.cache_read += assistant.usage.cache_read;
            tokens.cache_write += assistant.usage.cache_write;
        } else if matches!(message, Message::User(user) if user.role == crate::ai::types::UserRole::User)
        {
            user_messages += 1;
        }
    }
    for entry in entries {
        if let FileEntry::Message(entry) = entry {
            if matches!(
                entry.message,
                crate::session_manager::types::StoredMessage::ToolResult(_)
            ) {
                tool_results += 1;
            }
        }
    }

    // Header.
    let mut html = String::from("<div class=\"header\">");
    html.push_str(&format!(
        "<h1>Session: {}</h1>",
        escape_html(header.map(|h| h.id.as_str()).unwrap_or("unknown"))
    ));
    html.push_str("<div class=\"header-info\">");
    if let Some(header) = header {
        html.push_str(&format!(
            "<div class=\"info-item\"><span class=\"info-label\">Date:</span><span class=\"info-value\">{}</span></div>",
            escape_html(&header.timestamp)
        ));
        html.push_str(&format!(
            "<div class=\"info-item\"><span class=\"info-label\">Project:</span><span class=\"info-value\">{}</span></div>",
            escape_html(&header.cwd)
        ));
    }
    html.push_str(&format!(
        "<div class=\"info-item\"><span class=\"info-label\">Models:</span><span class=\"info-value\">{}</span></div>",
        escape_html(&models.join(", "))
    ));
    html.push_str(&format!(
        "<div class=\"info-item\"><span class=\"info-label\">Messages:</span><span class=\"info-value\">{user_messages} user, {assistant_messages} assistant, {tool_results} tool results</span></div>"
    ));
    html.push_str(&format!(
        "<div class=\"info-item\"><span class=\"info-label\">Tool Calls:</span><span class=\"info-value\">{tool_calls}</span></div>"
    ));
    html.push_str(&format!(
        "<div class=\"info-item\"><span class=\"info-label\">Tokens:</span><span class=\"info-value\">↑{} ↓{} R{} W{}</span></div>",
        format_tokens(tokens.input),
        format_tokens(tokens.output),
        format_tokens(tokens.cache_read),
        format_tokens(tokens.cache_write)
    ));
    html.push_str("</div></div>");

    // Messages and notice entries in file order.
    for entry in entries {
        match entry {
            FileEntry::Message(message_entry) => {
                let stored = &message_entry.message;
                let message: Message = stored.clone().into();
                if matches!(message, Message::ToolResult(_)) {
                    continue;
                }
                html.push_str(&render_message_html(&message, &results));
            }
            FileEntry::ModelChange(change) => {
                html.push_str(&format!(
                    "<div class=\"model-change\">Switched to model: <span class=\"model-name\">{}/{}</span></div>",
                    escape_html(&change.provider),
                    escape_html(&change.model_id)
                ));
            }
            FileEntry::Compaction(compaction) => {
                html.push_str(&format!(
                    "<div class=\"compaction\"><div class=\"compaction-label\">[compaction]</div><div class=\"compaction-content\"><strong>Compacted from {} tokens</strong></div><pre>{}</pre></div>",
                    compaction.tokens_before,
                    escape_html(&ansi_safe(&compaction.summary))
                ));
            }
            FileEntry::BranchSummary(summary) => {
                html.push_str(&format!(
                    "<div class=\"branch-summary\"><div class=\"branch-summary-header\">Branch Summary</div><div class=\"markdown-content\">{}</div></div>",
                    markdown_to_html(&ansi_safe(&summary.summary))
                ));
            }
            FileEntry::CustomMessage(custom) => {
                if !custom.display {
                    continue;
                }
                let content_text = match &custom.content {
                    serde_json::Value::String(text) => text.clone(),
                    other => serde_json::to_string_pretty(other).unwrap_or_default(),
                };
                html.push_str(&format!(
                    "<div class=\"hook-message\"><div class=\"hook-type\">[{}]</div><div class=\"markdown-content\">{}</div></div>",
                    escape_html(&custom.custom_type),
                    markdown_to_html(&ansi_safe(&content_text))
                ));
            }
            _ => {}
        }
    }

    wrap_document(&html)
}

/// Inline CSS: compact dark monospace theme with the upstream export class
/// names (upstream template.css, hand-rolled subset). The page/card/info
/// surfaces read the theme `export` colors through CSS custom properties
/// (upstream theme schema `export`: pageBg/cardBg/infoBg); the literals are
/// the defaults used when the active theme does not define them.
const EXPORT_CSS: &str = "\
* { margin: 0; padding: 0; box-sizing: border-box; }
body { font-family: ui-monospace, 'Cascadia Code', 'Source Code Pro', Menlo, Consolas, monospace; font-size: 12px; line-height: 1.5; color: #ececf1; background: var(--pir-export-page-bg, #101014); padding: 24px 12px; }
.container { max-width: 840px; margin: 0 auto; }
.header { background: var(--pir-export-card-bg, #1e1e24); border: 1px solid #33333c; border-radius: 6px; padding: 14px 16px; margin-bottom: 18px; }
.header h1 { font-size: 15px; margin-bottom: 10px; }
.header-info { display: flex; flex-wrap: wrap; gap: 4px 18px; }
.info-item { color: #b7b7c2; }
.info-label { color: #8a8a95; margin-right: 4px; }
.user-message { background: var(--pir-export-card-bg, #2a2a35); border-radius: 8px; padding: 10px 14px; margin: 14px 0 14px auto; max-width: 88%; width: fit-content; white-space: pre-wrap; word-break: break-word; }
.assistant-message { margin: 16px 0; }
.assistant-text { padding: 4px 0; }
.markdown-content p { margin: 6px 0; white-space: pre-wrap; word-break: break-word; }
.markdown-content h1, .markdown-content h2, .markdown-content h3, .markdown-content h4, .markdown-content h5, .markdown-content h6 { margin: 12px 0 6px; }
.markdown-content pre { background: #18181d; border: 1px solid #33333c; border-radius: 4px; padding: 8px 10px; overflow-x: auto; margin: 8px 0; }
.markdown-content code { font-family: inherit; background: #18181d; border-radius: 3px; padding: 1px 4px; }
.markdown-content pre code { padding: 0; background: transparent; }
.markdown-content ul, .markdown-content ol { margin: 6px 0 6px 22px; }
.markdown-content a { color: #7ab8ff; }
.thinking-block { border-left: 2px solid #3d3d4a; margin: 8px 0; padding: 2px 10px; color: #9a9aa6; }
.thinking-block summary { cursor: pointer; }
.thinking-text { white-space: pre-wrap; margin-top: 6px; }
.tool-execution { background: var(--pir-export-card-bg, #1a1a20); border: 1px solid #2c2c36; border-radius: 6px; padding: 8px 12px; margin: 10px 0; }
.tool-execution.error { border-color: #6e3238; }
.tool-execution.pending { border-style: dashed; }
.tool-header { color: #9fe0a9; margin-bottom: 4px; }
.tool-execution.error .tool-header { color: #ff9aa5; }
.tool-args pre, .tool-output pre { background: #141419; border-radius: 4px; padding: 6px 8px; overflow-x: auto; white-space: pre-wrap; word-break: break-word; margin: 4px 0; }
.tool-image, .message-image { max-width: 480px; max-height: 360px; border-radius: 6px; margin: 6px 0; display: block; }
.message-image { width: fit-content; }
.error-text { color: #ff9aa5; margin: 6px 0; }
.system-message { color: #8a8a95; margin: 8px 0; }
.model-change { color: #8a8a95; margin: 12px 0; }
.model-name { color: #c9c9d4; }
.compaction, .branch-summary, .hook-message { background: var(--pir-export-info-bg, #3a3428); border: 1px solid #55503f; border-radius: 6px; padding: 8px 12px; margin: 12px 0; color: #d8d2c0; }
.compaction-label, .hook-type, .branch-summary-header { font-weight: bold; margin-bottom: 4px; }
.compaction pre, .branch-summary pre { white-space: pre-wrap; word-break: break-word; }
";

/// Wrap rendered content in the static document shell (upstream
/// template.html; no scripts — everything is pre-rendered). The theme's
/// `export` colors (pageBg/cardBg/infoBg of the active theme) override the
/// page/card/info surface colors via CSS custom properties.
fn wrap_document(body: &str) -> String {
    #[cfg(feature = "tui")]
    let mut variables = String::new();
    #[cfg(not(feature = "tui"))]
    let variables = String::new();
    // The theme's export colors are a TUI surface (the active theme lives in
    // the TUI's theme registry); without the TUI the export keeps the
    // stylesheet's built-in surface colors.
    #[cfg(feature = "tui")]
    {
        let export = crate::tui::theme::theme_export_colors(None);
        if let Some(page_bg) = &export.page_bg {
            variables.push_str(&format!("--pir-export-page-bg: {page_bg};"));
        }
        if let Some(card_bg) = &export.card_bg {
            variables.push_str(&format!("--pir-export-card-bg: {card_bg};"));
        }
        if let Some(info_bg) = &export.info_bg {
            variables.push_str(&format!("--pir-export-info-bg: {info_bg};"));
        }
    }
    let overrides = if variables.is_empty() {
        String::new()
    } else {
        format!(
            ":root {{ {} }}\n",
            variables
                .split(";")
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        )
    };
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"UTF-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n\
         <title>Session Export</title>\n<style>\n{overrides}{EXPORT_CSS}</style>\n</head>\n<body>\n\
         <div class=\"container\">\n{body}\n</div>\n</body>\n</html>\n"
    )
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Default output path next to cwd: `pir-session-<session-file-stem>.html`
/// (upstream `${APP_NAME}-session-<basename>.html`).
fn default_output_path(session_path: &Path) -> std::path::PathBuf {
    let stem = session_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".to_owned());
    std::path::PathBuf::from(format!("pir-session-{stem}.html"))
}

/// Export a session file to a self-contained HTML page; returns the written
/// output path. `output_path` defaults to [`default_output_path`] (upstream
/// `exportFromFile`).
pub fn export_session_to_html(
    session_path: &Path,
    output_path: Option<&Path>,
) -> crate::Result<std::path::PathBuf> {
    if !session_path.exists() {
        anyhow::bail!("File not found: {}", session_path.display());
    }
    let manager = SessionManager::open(session_path)?;
    let header = manager.header().cloned();
    let entries = manager.entries();
    let html = generate_html(header.as_ref(), &entries);

    let output = output_path
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| default_output_path(session_path));
    std::fs::write(&output, html)?;
    Ok(output)
}
