//! Embedded bash-compatible shell (`brush`) behind the `brush` feature.
//!
//! The `pir __brush` hidden subcommand hosts a brush shell so the shell pool
//! can drive a persistent, in-process bash member instead of spawning Git
//! Bash (a ~300 ms process creation per call on Windows). Two modes:
//!
//! - `pir __brush -c <script>`: run one script and exit (bash `-c` shape, so
//!   [`crate::coding_agent::tools::bash::ShellConfig`] can name it directly).
//! - `pir __brush` (no `-c`): read commands from stdin (the pooled member;
//!   the pool's sentinel protocol is unchanged).
//!
//! `shellPath` in settings still wins over the embedded shell, and a build
//! without the feature keeps spawning the external bash exactly as before.
//!
//! # Stdin mode
//!
//! The shell pool writes one complete command script at a time and appends a
//! `printf '\036%s:<token>\036' "$rc"` sentinel line. This loop therefore
//! reads stdin line by line, accumulates lines until the buffer parses as a
//! complete program (the same completeness rule brush's own input backends
//! use: an unterminated quote/`$(...)`/`{ ... }`/`if ... fi` etc. yields an
//! "incomplete" parse error and more lines are read), then runs exactly that
//! unit with [`brush_core::Shell::run_string`]. Consecutive units share the
//! shell state, so the sentinel's `$?` observes the preceding unit's status,
//! and the `printf` itself executes as a normal command.
//!
//! Reading stdin through tokio (not a blocking `std::io::stdin()` read) keeps
//! the runtime free while the member waits for its next script, so background
//! jobs brush spawned on the runtime keep making progress.
//!
//! No prompt is ever written and no terminal control is acquired: the member
//! is driven purely over pipes, so the `brush_interactive` UI is not used.

use std::collections::VecDeque;
use std::io::Write;

use brush_core::{
    extensions::DefaultShellExtensions, parser::ParseError, ExecutionControlFlow,
    ProfileLoadBehavior, RcLoadBehavior, Shell, SourceInfo,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

/// Largest command script the stdin loop will accumulate before declaring the
/// member unusable. A script that never forms a complete program (an
/// unterminated here-document, quote, or compound command) used to grow the
/// pending buffer without bound; the pool recovers from a dead member by
/// falling back to a per-call spawn, so aborting is safe and bounded.
const MAX_PENDING_UNIT_BYTES: usize = 8 * 1024 * 1024;

/// Number of lines accumulated before a unit the incremental gate believes is
/// incomplete is parsed anyway. This is a correctness backstop for the gate
/// (the parser remains authoritative); every failed forced parse doubles the
/// interval so a genuinely long here-document/compound command costs only
/// O(log n) full parses instead of one per line.
const FORCE_PARSE_FIRST_LINES: usize = 256;

/// Incremental, allocation-free-in-the-common-case completeness gate for the
/// stdin loop.
///
/// [`is_complete`] is authoritative but re-parses the whole accumulated buffer
/// and clones it into brush's parse cache, so calling it after every line is
/// O(n²) in the size of a multi-line script (the pooled command wrapper alone
/// is a multi-line `( ... )` unit). This gate tracks just enough shell lexical
/// state to prove that the buffer is still incomplete (open quote/escape,
/// here-document body, unclosed `( )`/`{ }`/`[[ ]]`, open `if`/loop/`case`
/// block, trailing `|`/`&&`/`||`) and lets the loop skip the parse for those
/// lines. When it cannot prove incompleteness the parser decides, so a gate
/// false-negative only costs a parse; a gate false-positive is bounded by
/// [`FORCE_PARSE_FIRST_LINES`] and the unit-terminator trigger.
///
/// The scanner is deliberately approximate around exotic syntax (nested
/// command substitutions inside double quotes, arithmetic). Approximations
/// only ever make the gate *less* sure, which routes the line to the real
/// parser; the parse result, never the gate, decides completeness.
struct CompletenessGate {
    quote: QuoteState,
    /// The previous character was an unquoted backslash.
    escaped: bool,
    /// A backslash-newline continued the current logical line.
    continued_by_backslash: bool,
    /// The word being scanned (only bare words can be reserved words).
    word: String,
    word_quoted: bool,
    word_command_start: bool,
    /// `( … )` / `$( … )` nesting. A pooled command wrapper is one level.
    paren_depth: usize,
    /// `{ … }` group nesting.
    brace_depth: usize,
    /// `${ … }` parameter-expansion nesting (closed without command position).
    dollar_brace_depth: usize,
    /// `$(( … ))` / `(( … ))` arithmetic nesting.
    arithmetic_depth: usize,
    arithmetic_paren_depth: usize,
    /// `[[ … ]]` nesting.
    bracket_depth: usize,
    /// Open `if` / `for|while|until|select` / `case` constructs.
    blocks: Vec<Block>,
    /// The next word starts a command (so reserved words are recognized).
    command_start: bool,
    /// The previous significant token was `)` (function-definition `foo() {`).
    last_was_close_paren: bool,
    /// A `function` keyword was seen and its body's `{` may follow.
    expect_function_body: bool,
    /// The last significant token was `|`, `||`, or `&&`.
    last_token_continues: bool,
    /// Here-documents whose bodies have not started yet.
    heredoc_queue: VecDeque<Heredoc>,
    /// The here-document body currently being consumed.
    heredoc_body: Option<Heredoc>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum QuoteState {
    #[default]
    Normal,
    Single,
    Double,
    AnsiC,
    Backtick,
    Comment,
}

#[derive(Debug)]
enum Block {
    If,
    Loop,
    Case(CaseState),
}

#[derive(Debug)]
struct CaseState {
    paren_depth_at_entry: usize,
    in_body: bool,
    at_pattern_start: bool,
    /// The pattern started with the optional `(`; that paren is part of the
    /// pattern, not a nested subshell.
    pattern_paren_prefix: bool,
}

#[derive(Debug, Clone)]
struct Heredoc {
    delimiter: String,
    strip_tabs: bool,
}

impl Default for CompletenessGate {
    fn default() -> Self {
        Self {
            quote: QuoteState::Normal,
            escaped: false,
            continued_by_backslash: false,
            word: String::new(),
            word_quoted: false,
            word_command_start: false,
            paren_depth: 0,
            brace_depth: 0,
            dollar_brace_depth: 0,
            arithmetic_depth: 0,
            arithmetic_paren_depth: 0,
            bracket_depth: 0,
            blocks: Vec::new(),
            command_start: true,
            last_was_close_paren: false,
            expect_function_body: false,
            last_token_continues: false,
            heredoc_queue: VecDeque::new(),
            heredoc_body: None,
        }
    }
}

impl CompletenessGate {
    /// Reset after one unit has been executed.
    fn reset(&mut self) {
        *self = Self::default();
    }

    /// True when the accumulated text is provably still an incomplete program.
    fn is_definitely_incomplete(&self) -> bool {
        self.quote != QuoteState::Normal
            || self.escaped
            || self.continued_by_backslash
            || self.paren_depth > 0
            || self.brace_depth > 0
            || self.dollar_brace_depth > 0
            || self.arithmetic_depth > 0
            || self.bracket_depth > 0
            || !self.blocks.is_empty()
            || self.last_token_continues
            || self.heredoc_body.is_some()
            || !self.heredoc_queue.is_empty()
    }

    /// Feed one complete line (including its newline, when there is one).
    fn push_line(&mut self, line: &str) {
        // Here-document bodies are opaque: only the delimiter line matters.
        if let Some(tag) = &self.heredoc_body {
            let mut body = line.strip_suffix('\n').unwrap_or(line);
            if let Some(rest) = body.strip_suffix('\r') {
                body = rest;
            }
            let matched = if tag.strip_tabs {
                body.trim_start_matches('\t') == tag.delimiter.as_str()
            } else {
                body == tag.delimiter.as_str()
            };
            if matched {
                self.heredoc_body = self.heredoc_queue.pop_front();
                if self.heredoc_body.is_none() {
                    self.command_start = true;
                    self.last_was_close_paren = false;
                }
            }
            return;
        }

        let mut chars = line.chars().peekable();
        while let Some(character) = chars.next() {
            match self.quote {
                QuoteState::Single => {
                    if character == '\'' {
                        self.quote = QuoteState::Normal;
                    }
                }
                QuoteState::AnsiC => {
                    if self.escaped {
                        self.escaped = false;
                    } else if character == '\\' {
                        self.escaped = true;
                    } else if character == '\'' {
                        self.quote = QuoteState::Normal;
                    }
                }
                QuoteState::Backtick => {
                    if self.escaped {
                        self.escaped = false;
                    } else if character == '\\' {
                        self.escaped = true;
                    } else if character == '`' {
                        self.quote = QuoteState::Normal;
                    }
                }
                QuoteState::Double => {
                    if self.escaped {
                        self.escaped = false;
                        continue;
                    }
                    match character {
                        '\\' => self.escaped = true,
                        '"' => {
                            self.quote = QuoteState::Normal;
                            self.word_quoted = true;
                        }
                        // Keep double-quoted text from touching the global
                        // nesting counters (`")"` must not close the wrapper).
                        '$' => match chars.peek() {
                            Some('(') => {
                                chars.next();
                            }
                            Some('{') => {
                                chars.next();
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
                QuoteState::Comment => {
                    if character == '\n' {
                        self.quote = QuoteState::Normal;
                        self.end_of_line();
                    }
                }
                QuoteState::Normal => {
                    if self.escaped {
                        self.escaped = false;
                        if character == '\n' {
                            // Backslash-newline: the logical line continues.
                            self.continued_by_backslash = true;
                        } else {
                            self.push_word_char(character);
                        }
                        continue;
                    }
                    if self.arithmetic_depth > 0 {
                        // Arithmetic content is opaque except for its own
                        // parentheses; `<<`, `|`, `&&` there are operators,
                        // not redirections/pipelines.
                        match character {
                            '(' => self.arithmetic_paren_depth += 1,
                            ')' => {
                                if self.arithmetic_paren_depth > 0 {
                                    self.arithmetic_paren_depth -= 1;
                                } else if chars.peek() == Some(&')') {
                                    chars.next();
                                    self.arithmetic_depth -= 1;
                                    self.command_start = false;
                                    self.last_token_continues = false;
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }
                    match character {
                        '\\' => {
                            self.escaped = true;
                            self.word_quoted = true;
                            self.ensure_word();
                        }
                        '\'' => {
                            self.start_quoted_word();
                            self.quote = QuoteState::Single;
                        }
                        '"' => {
                            self.start_quoted_word();
                            self.quote = QuoteState::Double;
                        }
                        '`' => {
                            self.start_quoted_word();
                            self.quote = QuoteState::Backtick;
                        }
                        '$' => match chars.peek() {
                            Some('\'') => {
                                chars.next();
                                self.start_quoted_word();
                                self.quote = QuoteState::AnsiC;
                            }
                            Some('"') => {
                                chars.next();
                                self.start_quoted_word();
                                self.quote = QuoteState::Double;
                            }
                            Some('(') => {
                                chars.next();
                                self.ensure_word();
                                if chars.peek() == Some(&'(') {
                                    chars.next();
                                    self.arithmetic_depth += 1;
                                    self.arithmetic_paren_depth = 0;
                                } else {
                                    self.paren_depth += 1;
                                }
                                self.command_start = true;
                                self.last_was_close_paren = false;
                                self.last_token_continues = false;
                            }
                            Some('{') => {
                                chars.next();
                                self.start_quoted_word();
                                self.dollar_brace_depth += 1;
                            }
                            _ => self.push_word_char('$'),
                        },
                        '#' if self.word.is_empty() => {
                            self.quote = QuoteState::Comment;
                        }
                        '\n' => {
                            self.finish_word();
                            self.end_of_line();
                        }
                        character if character.is_whitespace() => self.finish_word(),
                        ';' => {
                            self.finish_word();
                            let mut case_next = false;
                            if chars.peek() == Some(&';') {
                                chars.next();
                                case_next = true;
                                if chars.peek() == Some(&'&') {
                                    chars.next();
                                }
                            } else if chars.peek() == Some(&'&') {
                                chars.next();
                                case_next = true;
                            }
                            if case_next {
                                self.next_case_pattern();
                            }
                            self.command_start = !case_next;
                            self.last_was_close_paren = false;
                            self.last_token_continues = false;
                        }
                        '&' => {
                            self.finish_word();
                            self.last_token_continues = chars.peek() == Some(&'&');
                            if self.last_token_continues {
                                chars.next();
                            }
                            self.command_start = true;
                            self.last_was_close_paren = false;
                        }
                        '|' => {
                            self.finish_word();
                            if chars.peek() == Some(&'|') {
                                chars.next();
                            }
                            self.last_token_continues = true;
                            self.command_start = true;
                            self.last_was_close_paren = false;
                        }
                        '(' => {
                            self.finish_word();
                            if self.arithmetic_depth > 0 {
                                self.arithmetic_paren_depth += 1;
                            } else if self.command_start
                                && self.word.is_empty()
                                && chars.peek() == Some(&'(')
                            {
                                // `(( … ))`: arithmetic command, not a
                                // subshell-inside-a-subshell.
                                chars.next();
                                self.arithmetic_depth += 1;
                                self.arithmetic_paren_depth = 0;
                            } else if !self.pattern_prefix_paren() {
                                self.paren_depth += 1;
                            }
                            self.command_start = true;
                            self.last_was_close_paren = false;
                            self.last_token_continues = false;
                        }
                        ')' => {
                            self.finish_word();
                            if self.arithmetic_depth > 0 {
                                if self.arithmetic_paren_depth > 0 {
                                    self.arithmetic_paren_depth -= 1;
                                } else if chars.peek() == Some(&')') {
                                    chars.next();
                                    self.arithmetic_depth -= 1;
                                }
                            } else if self.case_pattern_terminator() {
                                // Consumed as a `case` pattern's closing paren.
                            } else {
                                self.paren_depth = self.paren_depth.saturating_sub(1);
                            }
                            self.command_start = false;
                            self.last_was_close_paren = true;
                            self.last_token_continues = false;
                        }
                        '{' if self.standalone_brace(&mut chars) => {
                            if self.in_case_pattern() {
                                self.push_word_char('{');
                            } else if self.command_start
                                || self.last_was_close_paren
                                || self.expect_function_body
                            {
                                self.finish_word();
                                self.brace_depth += 1;
                                self.expect_function_body = false;
                                self.command_start = true;
                                self.last_was_close_paren = false;
                                self.last_token_continues = false;
                            } else {
                                self.push_word_char('{');
                            }
                        }
                        '}' if self.dollar_brace_depth > 0 => {
                            self.dollar_brace_depth -= 1;
                        }
                        '}' if self.command_start
                            && self.brace_depth > 0
                            && !self.in_case_pattern() =>
                        {
                            self.finish_word();
                            self.brace_depth -= 1;
                            self.command_start = false;
                            self.last_was_close_paren = false;
                            self.last_token_continues = false;
                        }
                        '<' => {
                            self.finish_word();
                            self.scan_redirection(&mut chars);
                            self.command_start = false;
                            self.last_was_close_paren = false;
                            self.last_token_continues = false;
                        }
                        '>' => {
                            self.finish_word();
                            if chars.peek() == Some(&'>') {
                                chars.next();
                            }
                            self.command_start = false;
                            self.last_was_close_paren = false;
                            self.last_token_continues = false;
                        }
                        '[' if self.command_start && self.word.is_empty() => {
                            if chars.peek() == Some(&'[') {
                                chars.next();
                                self.bracket_depth += 1;
                                self.command_start = false;
                            } else {
                                self.push_word_char('[');
                            }
                        }
                        ']' if self.bracket_depth > 0 && chars.peek() == Some(&']') => {
                            chars.next();
                            self.bracket_depth -= 1;
                            self.command_start = false;
                            self.last_was_close_paren = false;
                            self.last_token_continues = false;
                        }
                        character => self.push_word_char(character),
                    }
                }
            }
        }

        if !line.ends_with('\n') {
            self.finish_word();
        }
    }

    fn end_of_line(&mut self) {
        self.continued_by_backslash = false;
        self.command_start = true;
        self.last_was_close_paren = false;
        self.expect_function_body = false;
        if self.heredoc_body.is_none() {
            self.heredoc_body = self.heredoc_queue.pop_front();
        }
    }

    fn ensure_word(&mut self) {
        if self.word.is_empty() {
            self.word_command_start = self.command_start;
        }
    }

    /// Start/continue a quoted word. The sentinel byte keeps an otherwise
    /// empty quoted word non-empty, so it is classified as a (non-reserved)
    /// word and the following token is not mistaken for a command start.
    fn start_quoted_word(&mut self) {
        self.word_quoted = true;
        self.ensure_word();
        if self.word.is_empty() {
            self.word.push('\u{0}');
        }
    }

    fn push_word_char(&mut self, character: char) {
        self.ensure_word();
        self.word.push(character);
        self.last_was_close_paren = false;
        self.last_token_continues = false;
    }

    fn finish_word(&mut self) {
        if self.word.is_empty() {
            self.word_quoted = false;
            return;
        }
        let word = std::mem::take(&mut self.word);
        let quoted = self.word_quoted;
        let word_command_start = self.word_command_start;
        self.word_quoted = false;
        self.word_command_start = false;
        self.last_token_continues = false;
        self.last_was_close_paren = false;

        if self.in_case_pattern() && word != "esac" {
            // Pattern words are not commands: no reserved-word recognition.
            self.command_start = false;
            return;
        }
        if quoted {
            self.command_start = false;
            return;
        }
        // `in` terminates a `case` word list and `esac` terminates the case
        // body; both are reserved in those positions even though the scanner's
        // command-position flag is false there.
        if word == "in" {
            if let Some(Block::Case(state)) = self.blocks.last_mut() {
                if !state.in_body {
                    state.in_body = true;
                    state.at_pattern_start = true;
                    state.pattern_paren_prefix = false;
                    self.command_start = false;
                    return;
                }
            }
        }
        if word == "esac" && matches!(self.blocks.last(), Some(Block::Case(_))) {
            self.blocks.pop();
            self.command_start = true;
            return;
        }
        if !word_command_start {
            self.command_start = false;
            return;
        }
        self.classify_reserved_word(&word);
    }

    fn classify_reserved_word(&mut self, word: &str) {
        match word {
            "if" => {
                self.blocks.push(Block::If);
                self.command_start = true;
            }
            "then" | "elif" | "else" => self.command_start = true,
            "fi" => {
                if matches!(self.blocks.last(), Some(Block::If)) {
                    self.blocks.pop();
                }
                self.command_start = true;
            }
            "for" | "while" | "until" | "select" => {
                self.blocks.push(Block::Loop);
                self.command_start = false;
            }
            "do" => self.command_start = true,
            "done" => {
                if matches!(self.blocks.last(), Some(Block::Loop)) {
                    self.blocks.pop();
                }
                self.command_start = true;
            }
            "case" => {
                self.blocks.push(Block::Case(CaseState {
                    paren_depth_at_entry: self.paren_depth,
                    in_body: false,
                    at_pattern_start: false,
                    pattern_paren_prefix: false,
                }));
                self.command_start = false;
            }
            "esac" => {
                if matches!(self.blocks.last(), Some(Block::Case(_))) {
                    self.blocks.pop();
                }
                self.command_start = true;
            }
            "in" => {
                if let Some(Block::Case(state)) = self.blocks.last_mut() {
                    if !state.in_body {
                        state.in_body = true;
                        state.at_pattern_start = true;
                        state.pattern_paren_prefix = false;
                    }
                }
                self.command_start = false;
            }
            "function" => {
                self.expect_function_body = true;
                self.command_start = false;
            }
            "time" | "!" => self.command_start = true,
            _ => self.command_start = false,
        }
    }

    fn in_case_pattern(&self) -> bool {
        matches!(
            self.blocks.last(),
            Some(Block::Case(state)) if state.in_body && state.at_pattern_start
        )
    }

    /// A `)` closing a `case` pattern (at the pattern's base paren depth).
    fn case_pattern_terminator(&mut self) -> bool {
        if let Some(Block::Case(state)) = self.blocks.last_mut() {
            if state.in_body
                && state.at_pattern_start
                && self.paren_depth == state.paren_depth_at_entry
            {
                state.at_pattern_start = false;
                state.pattern_paren_prefix = false;
                return true;
            }
        }
        false
    }

    /// The optional leading `(` of a `case` pattern is not a subshell.
    fn pattern_prefix_paren(&mut self) -> bool {
        if let Some(Block::Case(state)) = self.blocks.last_mut() {
            if state.in_body
                && state.at_pattern_start
                && !state.pattern_paren_prefix
                && self.paren_depth == state.paren_depth_at_entry
                && self.word.is_empty()
            {
                state.pattern_paren_prefix = true;
                return true;
            }
        }
        false
    }

    fn next_case_pattern(&mut self) {
        if let Some(Block::Case(state)) = self.blocks.last_mut() {
            if state.in_body {
                state.at_pattern_start = true;
                state.pattern_paren_prefix = false;
            }
        }
    }

    /// Is the `{` at the cursor a standalone brace token (not `{a,b}`)?
    fn standalone_brace<I: Iterator<Item = char>>(
        &self,
        chars: &mut std::iter::Peekable<I>,
    ) -> bool {
        chars
            .peek()
            .is_none_or(|next| next.is_whitespace() || ";&|<>(){}".contains(*next))
    }

    /// Consume a redirection; `<<`/`<<-` additionally queue a here-document.
    fn scan_redirection<I: Iterator<Item = char>>(&mut self, chars: &mut std::iter::Peekable<I>) {
        if chars.peek() != Some(&'<') {
            return;
        }
        chars.next();
        if chars.peek() == Some(&'<') {
            // `<<<` here-string: no here-document body follows.
            chars.next();
            return;
        }
        let strip_tabs = if chars.peek() == Some(&'-') {
            chars.next();
            true
        } else {
            false
        };
        while matches!(chars.peek(), Some(' ' | '\t')) {
            chars.next();
        }
        let mut delimiter = String::new();
        while let Some(&next) = chars.peek() {
            if next.is_whitespace() || ";&|<>()".contains(next) {
                break;
            }
            chars.next();
            match next {
                '\'' => {
                    for quoted in chars.by_ref() {
                        if quoted == '\'' {
                            break;
                        }
                        delimiter.push(quoted);
                    }
                }
                '"' => {
                    while let Some(quoted) = chars.next() {
                        if quoted == '"' {
                            break;
                        }
                        if quoted == '\\' {
                            if let Some(escaped) = chars.next() {
                                delimiter.push(escaped);
                            }
                        } else {
                            delimiter.push(quoted);
                        }
                    }
                }
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        delimiter.push(escaped);
                    }
                }
                _ => delimiter.push(next),
            }
        }
        if !delimiter.is_empty() {
            self.heredoc_queue.push_back(Heredoc {
                delimiter,
                strip_tabs,
            });
        }
    }
}

/// Lines that close a bare compound program unit (`fi`, `done`, `esac`, `}`),
/// used as a parse trigger when the gate is unsure. Lines inside a `( … )`
/// wrapper are handled by the closing `)` line, so these only fire at the
/// outer level.
fn looks_like_keyword_terminator(line: &str) -> bool {
    let trimmed = line.trim();
    matches!(trimmed, "fi" | "done" | "esac" | "}")
        || ["fi", "done", "esac", "}"].iter().any(|keyword| {
            trimmed
                .strip_prefix(keyword)
                .is_some_and(|rest| rest.starts_with(';') || rest.starts_with(char::is_whitespace))
        })
}

/// Read one line with a hard byte ceiling, so a newline-free stream cannot
/// grow the pending buffer past [`MAX_PENDING_UNIT_BYTES`] before the loop
/// notices. Returns the bytes read (`0` at EOF), like `read_line`.
async fn read_line_capped<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> std::io::Result<usize> {
    line.clear();
    let limit = u64::try_from(MAX_PENDING_UNIT_BYTES.saturating_add(2)).unwrap_or(u64::MAX);
    let mut limited = reader.take(limit);
    let read = limited.read_until(b'\n', line).await?;
    if read >= MAX_PENDING_UNIT_BYTES && !line.ends_with(b"\n") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("input line exceeds the {MAX_PENDING_UNIT_BYTES}-byte command limit"),
        ));
    }
    Ok(read)
}

/// The program a shell spawn site uses for the embedded shell: the current
/// executable, spawned with `__brush` as the first argument. `None` when the
/// binary path cannot be resolved, or when this process is not the `pir`
/// binary itself (the caller then keeps the external shell).
///
/// The host check matters: `current_exe()` inside a test binary or an SDK
/// embedder is *not* `pir`, and spawning it with `__brush` would hand pir's
/// argv to a program that does not understand it.
#[cfg(feature = "brush")]
pub fn embedded_shell_program() -> Option<String> {
    if !HOST_IS_PIR.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    std::env::current_exe()
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

/// Whether this process is the `pir` binary, i.e. whether `current_exe()`
/// understands the hidden `__brush` entry.
#[cfg(feature = "brush")]
static HOST_IS_PIR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Marks this process as the `pir` binary. The binary's entry point calls
/// this once before dispatch; library users (tests, SDK embedders) do not,
/// so they keep spawning the external shell.
#[cfg(feature = "brush")]
pub fn mark_host_binary() {
    HOST_IS_PIR.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Flags accepted in the short-option cluster before `-c` (`-euc`, `-xc`,
/// ...). Only the ones that change execution are applied; the rest are
/// accepted and ignored so common bash invocations still start.
#[derive(Clone, Copy, Default)]
struct ShellFlags {
    exit_on_nonzero: bool,
    unset_as_error: bool,
    no_exec: bool,
    xtrace: bool,
    verbose: bool,
}

impl ShellFlags {
    fn apply_cluster(self, cluster: &str) -> Self {
        let mut flags = self;
        for flag in cluster.chars() {
            match flag {
                'e' => flags.exit_on_nonzero = true,
                'u' => flags.unset_as_error = true,
                'n' => flags.no_exec = true,
                'x' => flags.xtrace = true,
                'v' => flags.verbose = true,
                // `-i`, `-l`, `-s`, `-a`, ...: accepted, no behavioral change
                // for a pipe-driven member.
                _ => {}
            }
        }
        flags
    }
}

/// How this invocation was parsed.
enum Mode {
    /// `-c <script> [name [args...]]`.
    Command {
        script: String,
        name: Option<String>,
        args: Vec<String>,
    },
    /// No `-c`: commands arrive on stdin until EOF.
    Stdin,
}

/// Parse the `__brush` argument list. Supported shapes:
///
/// - `-c <script> [name [args...]]`
/// - `-<flags>c <script> [name [args...]]` (`-ec`, `-euc`, ...)
/// - `-c -- <script>` (bash treats `--` as the `-c` option terminator)
/// - no arguments / `-s` / `-` / `--`: stdin mode
fn parse_mode(args: &[String]) -> crate::Result<(Mode, ShellFlags)> {
    let Some(first) = args.first() else {
        return Ok((Mode::Stdin, ShellFlags::default()));
    };

    if first == "-s" || first == "-" || first == "--" {
        return Ok((Mode::Stdin, ShellFlags::default()));
    }

    let Some(rest) = first.strip_prefix('-') else {
        return Err(anyhow::anyhow!(
            "unexpected argument '{first}': expected `-c <script>` or no arguments"
        ));
    };
    if rest.is_empty() || rest.starts_with('-') {
        return Err(anyhow::anyhow!(
            "unsupported option '{first}': expected `-c <script>` or no arguments"
        ));
    }

    let Some(flag_cluster) = rest.strip_suffix('c') else {
        // Pure flag cluster with no `-c`: stdin mode (`-e`, `-u`, ...).
        if rest.chars().all(|flag| "sil".contains(flag)) {
            return Ok((Mode::Stdin, ShellFlags::default().apply_cluster(rest)));
        }
        return Err(anyhow::anyhow!(
            "unsupported option '{first}': expected `-c <script>` or no arguments"
        ));
    };

    // `-c -- <script>`: bash skips the option terminator before the script.
    let script_index = if args.get(1).map(String::as_str) == Some("--") {
        2
    } else {
        1
    };
    let script = args
        .get(script_index)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("option `-c` requires a command string argument"))?;
    let name = args.get(script_index + 1).cloned();
    let positional = args
        .get(script_index + 2..)
        .map(<[String]>::to_vec)
        .unwrap_or_default();

    Ok((
        Mode::Command {
            script,
            name,
            args: positional,
        },
        ShellFlags::default().apply_cluster(flag_cluster),
    ))
}

/// Build a non-interactive brush shell: parent environment inherited, no
/// profile/rc files, bash-mode builtins.
async fn build_shell(
    flags: ShellFlags,
    command_mode: bool,
    name: Option<String>,
    args: Vec<String>,
) -> crate::Result<Shell> {
    let mut builder = Shell::builder()
        .profile(ProfileLoadBehavior::Skip)
        .rc(RcLoadBehavior::Skip)
        .builtins(brush_builtins::default_builtins::<DefaultShellExtensions>(
            brush_builtins::BuiltinSet::BashMode,
        ))
        .command_string_mode(command_mode)
        .read_commands_from_stdin(!command_mode)
        .exit_on_nonzero_command_exit(flags.exit_on_nonzero)
        .treat_unset_variables_as_error(flags.unset_as_error)
        .do_not_execute_commands(flags.no_exec)
        .verbose(flags.verbose)
        .maybe_shell_name(name)
        .shell_args(args)
        .shell_version(env!("CARGO_PKG_VERSION").to_string());
    if flags.xtrace {
        builder = builder.enable_option("xtrace");
    }

    Ok(builder.build().await?)
}

/// Is `buffer` a syntactically complete program? Mirrors brush's own input
/// backends: an incomplete tokenizer error or a parse error at end of input
/// means "read more"; anything else (including a successful parse) is final.
/// This is the authoritative verdict; [`CompletenessGate`] only avoids calling
/// it on lines that cannot possibly complete the unit.
fn is_complete(shell: &Shell, buffer: &str) -> bool {
    match shell.parse_string(buffer) {
        Err(ParseError::Tokenizing { inner, .. }) if inner.is_incomplete() => false,
        Err(ParseError::ParsingAtEndOfInput) => false,
        _ => true,
    }
}

/// Run one accumulated program unit. Returns `Some(exit_code)` when the unit
/// asked the shell to exit (`exit`/`exit N`), `None` to keep reading.
async fn execute_unit(shell: &mut Shell, unit: String) -> Option<i32> {
    let params = shell.default_exec_params();
    let source_info = SourceInfo::from("stdin");

    match shell.run_string(unit, &source_info, &params).await {
        Ok(result) => {
            if matches!(result.next_control_flow, ExecutionControlFlow::ExitShell) {
                return Some(i32::from(shell.last_exit_status()));
            }
            None
        }
        Err(error) => {
            // `run_string` normally converts execution/parse errors into a
            // result already; reaching here means even error reporting
            // failed. Keep the member alive: the pool falls back per call if
            // it ever loses the protocol.
            {
                let mut stderr = shell.stderr();
                let _ = shell.display_error(&mut stderr, &error);
                let _ = stderr.flush();
            }
            shell.set_last_exit_status(1);
            None
        }
    }
}

/// The stdin command loop: accumulate lines to a complete program, execute,
/// repeat until EOF. Returns the process exit code.
///
/// The incremental [`CompletenessGate`] keeps the per-line cost O(line) for
/// multi-line units; only a line that can finish the unit (gate says the
/// lexical state is closed, a compound terminator line arrives, or the forced
/// backstop fires) reaches the authoritative whole-buffer parse. The pending
/// buffer and a single input line are both capped at
/// [`MAX_PENDING_UNIT_BYTES`].
async fn run_stdin_loop(shell: &mut Shell) -> crate::Result<i32> {
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut buffer = String::new();
    let mut line = Vec::new();
    let mut gate = CompletenessGate::default();
    let mut lines_since_parse = 0usize;
    let mut force_parse_after = FORCE_PARSE_FIRST_LINES;

    loop {
        let read = match read_line_capped(&mut reader, &mut line).await {
            Ok(read) => read,
            Err(error) => {
                eprintln!("pir __brush: failed to read stdin: {error}");
                return Ok(1);
            }
        };
        if read == 0 {
            break;
        }
        let text = match std::str::from_utf8(&line) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("pir __brush: stdin is not valid UTF-8: {error}");
                return Ok(1);
            }
        };
        if buffer.len().saturating_add(text.len()) > MAX_PENDING_UNIT_BYTES {
            eprintln!(
                "pir __brush: pending command exceeded the {MAX_PENDING_UNIT_BYTES}-byte limit without forming a complete program; exiting"
            );
            return Ok(1);
        }
        buffer.push_str(text);
        gate.push_line(text);
        lines_since_parse += 1;

        let should_parse = !gate.is_definitely_incomplete()
            || text.trim_start().starts_with(')')
            || (gate.paren_depth == 0
                && gate.brace_depth == 0
                && looks_like_keyword_terminator(text))
            || lines_since_parse >= force_parse_after;
        if !should_parse {
            continue;
        }
        if !is_complete(shell, &buffer) {
            lines_since_parse = 0;
            if gate.is_definitely_incomplete() {
                force_parse_after = force_parse_after.saturating_mul(2);
            }
            continue;
        }

        lines_since_parse = 0;
        force_parse_after = FORCE_PARSE_FIRST_LINES;
        gate.reset();
        let unit = std::mem::take(&mut buffer);
        if let Some(code) = execute_unit(shell, unit).await {
            let _ = shell.on_exit().await;
            return Ok(code);
        }
    }

    // A trailing unit without a newline still runs (bash behavior), even if
    // it is syntactically incomplete: `run_string` reports the parse error.
    if !buffer.trim().is_empty() {
        if let Some(code) = execute_unit(shell, std::mem::take(&mut buffer)).await {
            let _ = shell.on_exit().await;
            return Ok(code);
        }
    }

    let _ = shell.on_exit().await;
    Ok(i32::from(shell.last_exit_status()))
}

async fn run_shell(mode: Mode, flags: ShellFlags) -> crate::Result<i32> {
    match mode {
        Mode::Command { script, name, args } => {
            let mut shell =
                build_shell(flags, true, name.or_else(default_shell_name), args).await?;
            shell.run_dash_c_command(script).await?;
            Ok(i32::from(shell.last_exit_status()))
        }
        Mode::Stdin => {
            let mut shell = build_shell(flags, false, default_shell_name(), Vec::new()).await?;
            run_stdin_loop(&mut shell).await
        }
    }
}

/// `$0` when the invocation names no shell: the running executable, matching
/// how bash reports its own invocation path.
fn default_shell_name() -> Option<String> {
    std::env::current_exe()
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

/// Entry point for the hidden `__brush` subcommand: `-c <script>` runs one
/// script, otherwise commands are read from stdin until EOF.
///
/// Exits the process directly so the shell's status (`exit 7`, a failed
/// command, ...) reaches the parent; errors are returned to `run_mode.rs`,
/// which reports them on stderr and exits non-zero.
pub fn run_brush_main(args: &[String]) -> crate::Result<()> {
    let (mode, flags) = parse_mode(args)?;

    // `__brush` is dispatched from `run()`, which already runs inside the
    // process-wide tokio runtime. Building a second runtime on that thread and
    // blocking on it panics ("Cannot start a runtime from within a runtime"),
    // so the shell always gets its own OS thread: no runtime context, a clean
    // current-thread runtime, and a large stack for deeply nested scripts.
    let code = std::thread::Builder::new()
        .name("pir-brush".to_owned())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || -> crate::Result<i32> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| anyhow::anyhow!("failed to create tokio runtime: {error}"))?;
            runtime.block_on(run_shell(mode, flags))
        })
        .map_err(|error| anyhow::anyhow!("failed to start the shell thread: {error}"))?
        .join()
        .map_err(|_| anyhow::anyhow!("the embedded shell thread panicked"))??;

    // Make sure output written through the shell's std handles is on the
    // wire before the process disappears (brush flushes per builtin, but a
    // final partial line could still sit in the Rust stdout buffer).
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}

#[doc(hidden)]
/// Test seam for the stdin completeness gate (`tests/brush_embed_gate.rs`):
/// inline test modules are forbidden in `src/`. Feeds every line to a fresh
/// gate and reports whether it still proves the program incomplete.
pub fn completeness_gate_is_incomplete_for_test(lines: &[&str]) -> bool {
    let mut gate = CompletenessGate::default();
    for line in lines {
        gate.push_line(line);
    }
    gate.is_definitely_incomplete()
}

#[doc(hidden)]
/// Test seam for the compound-line parse trigger (see
/// [`looks_like_keyword_terminator`]).
pub fn keyword_terminator_for_test(line: &str) -> bool {
    looks_like_keyword_terminator(line)
}
