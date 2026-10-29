//! In-flight tool-call argument mirrors.
//!
//! The streaming adapters accumulate the raw argument JSON of a call per
//! delta and mirror a best-effort parse back into the message block so stream
//! consumers (the TUI's pending tool block, the extension `message_update`
//! bridge) can read the arguments accumulated so far.
//!
//! Two strategies live here:
//!
//! - [`MirrorCadence`] is the bounded-cadence fallback used by the adapters
//!   whose argument fragments are parsed as a whole at each refresh: once for
//!   the first fragment, whenever a container structurally closes, and
//!   otherwise only every [`MIRROR_REFRESH_BYTES`] appended bytes. A stale
//!   mirror is always the last well-formed whole-document parse, and the
//!   FINAL arguments are always parsed fresh from the complete raw buffer at
//!   `toolcall_end`.
//! - `StreamingJsonMirror` is the incremental parser used by the OpenAI
//!   Completions adapter (upstream Unreleased `StreamingJsonParser` /
//!   `ToolCallArgumentsStream`). It reads every fragment once and reports the
//!   structural changes the fragment made, so the mirror is refreshed after
//!   EVERY delta without re-parsing the accumulated text: O(args) total per
//!   call instead of the O(args²) whole-document parse per fragment. The
//!   conversation from changes to the block's `BTreeMap` is
//!   `apply_json_changes`.
//!
//! In both cases the final `toolcall_end` parse runs strictly over the
//! complete raw buffer, so the mirror's intermediate behavior cannot change
//! the finalized call.

/// Bytes of appended argument JSON between two mirrored refreshes (after the
/// mandatory first one). 8 KiB bounds the parse work to `total/8KiB` full
/// parses while keeping the TUI's live block within one fragment batch of the
/// raw stream.
pub(crate) const MIRROR_REFRESH_BYTES: usize = 8 * 1024;

/// Per-call refresh cadence for the mirrored argument map.
///
/// The close signal is STRUCTURAL, not textual: the scanner tracks whether it
/// is inside a JSON string (with escape state across fragments) and the open
/// container depth, so braces/brackets that are merely string content — for
/// example a `write` payload containing `}` or `]` — never count as a
/// container closing. A 1 MiB argument payload streamed in 20-byte fragments
/// therefore refreshes at the byte cadence (~128 times), not once per fragment
/// whose tail happens to end in a brace (~52k).
#[derive(Debug, Clone, Default)]
pub(crate) struct MirrorCadence {
    /// Raw argument bytes appended since the last mirror refresh.
    appended_since_refresh: usize,
    /// Whether the first (mandatory) refresh already happened.
    refreshed: bool,
    /// The accumulated raw JSON currently has an unterminated string literal.
    in_string: bool,
    /// The previous byte was an unescaped `\` inside a string literal; the
    /// next byte is escaped and cannot terminate the string.
    escaped: bool,
    /// Open `{`/`[` container depth of the accumulated raw JSON (bytes inside
    /// string literals never affect it).
    depth: usize,
}

impl MirrorCadence {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record `appended` raw argument bytes (the size of the fragment just
    /// accumulated). Returns `true` when the caller should now rebuild the
    /// mirrored arguments: on the first NON-EMPTY fragment, whenever this
    /// fragment closes a top-level container (or ends on a structural closing
    /// brace outside a string), and otherwise once per
    /// [`MIRROR_REFRESH_BYTES`] appended. An empty fragment changes nothing,
    /// so it never consumes the first refresh.
    pub(crate) fn record(&mut self, appended: impl AsRef<str>) -> bool {
        let appended = appended.as_ref();
        if appended.is_empty() {
            return false;
        }
        self.appended_since_refresh = self.appended_since_refresh.saturating_add(appended.len());
        // Structural completion signal: the common "the call is complete"
        // case, delivered without waiting for the byte cadence (the final
        // parse at `toolcall_end` runs regardless).
        let completes = self.scan(appended);
        if self.refreshed && !completes && self.appended_since_refresh < MIRROR_REFRESH_BYTES {
            return false;
        }
        self.appended_since_refresh = 0;
        self.refreshed = true;
        true
    }

    /// Advance the JSON lexical state over one fragment.
    ///
    /// Returns `true` when the fragment completes a top-level container
    /// (depth returns to zero) or its last structural character — outside any
    /// string literal — is a closing `}`/`]`. String content and escapes are
    /// tracked across calls, so a brace inside a string can never trigger a
    /// refresh.
    fn scan(&mut self, fragment: &str) -> bool {
        let mut closed_top_level = false;
        let mut last_structural_close = false;
        for byte in fragment.bytes() {
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                } else if byte == b'\\' {
                    self.escaped = true;
                } else if byte == b'"' {
                    self.in_string = false;
                    last_structural_close = false;
                }
                continue;
            }
            match byte {
                b' ' | b'\t' | b'\n' | b'\r' => {}
                b'"' => {
                    self.in_string = true;
                    last_structural_close = false;
                }
                b'{' | b'[' => {
                    self.depth = self.depth.saturating_add(1);
                    last_structural_close = false;
                }
                b'}' | b']' => {
                    if self.depth > 0 {
                        self.depth -= 1;
                        closed_top_level |= self.depth == 0;
                    }
                    last_structural_close = true;
                }
                _ => last_structural_close = false,
            }
        }
        closed_top_level || last_structural_close
    }
}

/// Incremental, tolerant JSON reader for streamed tool-call arguments
/// (port of upstream Unreleased `packages/ai/src/utils/streaming-json.ts`).
///
/// [`StreamingJsonMirror::append`] reads one argument fragment and returns the
/// [`JsonChange`]s it made; [`apply_json_changes`] applies them to the block's
/// parsed `BTreeMap` in order. The raw fragment text is never re-read, so a
/// call costs O(arguments) in total; the old whole-document
/// `parse_partial_json` refresh cost O(arguments) per refresh.
///
/// Partial-state rules (matched to upstream's incremental parser, which
/// deliberately differs from the all-or-nothing `parseStreamingJson` fallback
/// on some malformed prefixes):
///
/// - an unterminated string shows the characters read so far; an incomplete
///   escape (`\`, `\u`, `\u00`) is held back until it is complete;
/// - an unterminated container shows its members/elements read so far, and a
///   member whose key or value has not started is omitted (so `{"a":` still
///   mirrors `{}` and `{"a":1,` keeps `{"a":1}` rather than dropping it);
/// - an unterminated literal shows its value once the token is a prefix of
///   `true` / `false` / `null`; an unterminated number shows its value while
///   the token is a complete JSON number (an exponent without digits shows
///   the mantissa). A token that later turns invalid (for example `1.`) leaves
///   the last shown value in place until a later value replaces it — partial
///   snapshots are monotonic, never regress;
/// - invalid escapes such as `\d` and a `\u` without four hex digits stay
///   literal text (upstream `repairJson` behavior); valid `\uXXXX` escapes
///   are decoded, including surrogate pairs split across fragments. A lone
///   surrogate (invalid UTF-8 in Rust) shows as U+FFFD;
/// - missing commas/colons are tolerated, stray closers and values in invalid
///   positions are read and dropped, and text after the root value is ignored;
/// - paths deeper than [`MAX_MIRRORED_DEPTH`] are parsed but not mirrored, so
///   a pathological input cannot grow change paths quadratically. The strict
///   final parse at `toolcall_end` remains authoritative for such input.
///
/// The parser never panics on malformed input (a malformed fragment just
/// produces no or fewer changes).
#[cfg(feature = "provider-openai")]
pub(crate) mod streaming_json {
    use std::collections::BTreeMap;

    use serde_json::Value;

    /// Deepest member/element path mirrored into the block. Deeper values are
    /// still parsed (so quotes and escapes stay balanced) but produce no
    /// changes; the final strict parse handles their validity.
    const MAX_MIRRORED_DEPTH: usize = 32;

    /// Longest unterminated number/literal shown in a partial mirror. A longer
    /// token stays hidden until it ends (upstream
    /// `MAX_PENDING_SCALAR_LENGTH`).
    const MAX_PENDING_SCALAR_BYTES: usize = 64;

    /// U+FFFD: the visible stand-in for a lone UTF-16 surrogate (Rust strings
    /// cannot hold one).
    const REPLACEMENT: &str = "\u{FFFD}";

    /// One path segment from the root object to a value.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum JsonPathSegment {
        Key(String),
        Index(usize),
    }

    /// One structural change a fragment made to the mirrored arguments.
    #[derive(Debug, Clone, PartialEq)]
    pub(crate) enum JsonChange {
        /// Create or replace the value at `path` (a container starts empty and
        /// is then extended by later changes).
        Set {
            path: Vec<JsonPathSegment>,
            value: Value,
        },
        /// Extend the string value at `path` with `text`.
        Append {
            path: Vec<JsonPathSegment>,
            text: String,
        },
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Phase {
        Key,
        Colon,
        Value,
        Comma,
    }

    /// Where a value that starts now goes. `Invalid` values are parsed and
    /// dropped (upstream `#valueSlot` returning `undefined`).
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Slot {
        Root,
        Member(String),
        Element(usize),
        Invalid,
    }

    #[derive(Debug)]
    struct Frame {
        is_object: bool,
        /// The container's own path, `None` when it is not mirrored (invalid
        /// position or deeper than [`MAX_MIRRORED_DEPTH`]).
        base_path: Option<Vec<JsonPathSegment>>,
        /// Members for objects, elements for arrays (the next array index).
        size: usize,
        /// Object key read last; the member value's destination.
        key: Option<String>,
        phase: Phase,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum StringTarget {
        Key,
        Value,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Escape {
        None,
        /// A `\` was read; the next character decides the escape.
        Backslash,
        /// `\u` plus the hex digits read so far (0..3).
        Unicode {
            digits: String,
        },
    }

    #[derive(Debug)]
    struct OpenString {
        target: StringTarget,
        /// Where a value string sits; `None` for keys and dropped values.
        path: Option<Vec<JsonPathSegment>>,
        /// Key text, accumulated until the closing quote. Value text lives in
        /// the mirrored block (via `Append` changes) instead of being kept
        /// here twice.
        text: String,
        escape: Escape,
        /// A decoded high surrogate waiting for its low surrogate (which may
        /// arrive in a later fragment).
        pending_high: Option<u16>,
    }

    /// Incremental streaming JSON reader; see the module-level rules.
    #[derive(Debug, Default)]
    pub(crate) struct StreamingJsonMirror {
        stack: Vec<Frame>,
        root_started: bool,
        done: bool,
        string: Option<OpenString>,
        scalar: String,
        scalar_slot: Option<Slot>,
        scalar_path: Option<Vec<JsonPathSegment>>,
        changes: Vec<JsonChange>,
    }

    impl StreamingJsonMirror {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// Read one argument fragment and return the changes it made to the
        /// mirrored value, in order.
        pub(crate) fn append(&mut self, text: &str) -> Vec<JsonChange> {
            let mut index = 0;
            while index < text.len() && !self.done {
                if self.string.is_some() {
                    index = self.read_string(text, index);
                } else if !self.scalar.is_empty() {
                    index = self.read_scalar(text, index);
                } else {
                    let Some(value) = text[index..].chars().next() else {
                        break;
                    };
                    self.structural(value);
                    index += value.len_utf8();
                }
            }
            self.emit_pending_scalar();
            std::mem::take(&mut self.changes)
        }

        /// Handle one character outside strings/scalars.
        fn structural(&mut self, value: char) {
            match value {
                ' ' | '\t' | '\n' | '\r' => {}
                '"' => self.start_string(),
                '{' => self.open_container(true),
                '[' => self.open_container(false),
                '}' | ']' => self.close_container(),
                ':' => {
                    if let Some(frame) = self.stack.last_mut() {
                        if frame.is_object && frame.phase == Phase::Colon {
                            frame.phase = Phase::Value;
                        }
                    }
                }
                ',' => {
                    if let Some(frame) = self.stack.last_mut() {
                        frame.phase = if frame.is_object {
                            Phase::Key
                        } else {
                            Phase::Value
                        };
                    }
                }
                _ => {
                    self.scalar.clear();
                    self.scalar.push(value);
                    let slot = self.value_slot();
                    self.scalar_path = self.slot_path(&slot);
                    self.scalar_slot = Some(slot);
                }
            }
        }

        /// Read string characters from `index` until the string closes or the
        /// fragment ends; returns the next index.
        fn read_string(&mut self, text: &str, start: usize) -> usize {
            let Some(mut open) = self.string.take() else {
                return start;
            };
            let mut index = start;
            let mut closed = false;
            while index < text.len() {
                let Some(value) = text[index..].chars().next() else {
                    break;
                };
                let escape = std::mem::replace(&mut open.escape, Escape::None);
                match escape {
                    Escape::None => {
                        if value == '"' {
                            index += value.len_utf8();
                            closed = true;
                            break;
                        }
                        if value == '\\' {
                            open.escape = Escape::Backslash;
                            index += value.len_utf8();
                            continue;
                        }
                        let run_start = index;
                        index += value.len_utf8();
                        while index < text.len() {
                            let Some(next) = text[index..].chars().next() else {
                                break;
                            };
                            if next == '"' || next == '\\' {
                                break;
                            }
                            index += next.len_utf8();
                        }
                        flush_pending_high(&mut open, &mut self.changes);
                        push_string_text(&mut open, &mut self.changes, &text[run_start..index]);
                    }
                    Escape::Backslash => {
                        index += value.len_utf8();
                        if let Some(decoded) = simple_escape(value) {
                            flush_pending_high(&mut open, &mut self.changes);
                            push_string_text(&mut open, &mut self.changes, decoded);
                        } else if value == 'u' {
                            open.escape = Escape::Unicode {
                                digits: String::new(),
                            };
                        } else {
                            // Invalid escape: keep the backslash as literal
                            // text and read the character normally.
                            flush_pending_high(&mut open, &mut self.changes);
                            push_string_text(&mut open, &mut self.changes, "\\");
                            index -= value.len_utf8();
                        }
                    }
                    Escape::Unicode { mut digits } => {
                        if value.is_ascii_hexdigit() {
                            digits.push(value);
                            index += value.len_utf8();
                            if digits.len() == 4 {
                                let code = digits
                                    .bytes()
                                    .fold(0u16, |code, byte| code * 16 + hex_value(byte));
                                complete_unicode(&mut open, &mut self.changes, code);
                            } else {
                                open.escape = Escape::Unicode { digits };
                            }
                        } else {
                            // `\u` without four hex digits: keep the escape
                            // text literal and read the character normally.
                            let mut literal = String::with_capacity(2 + digits.len());
                            literal.push_str("\\u");
                            literal.push_str(&digits);
                            flush_pending_high(&mut open, &mut self.changes);
                            push_string_text(&mut open, &mut self.changes, &literal);
                            // Do not consume `value`.
                        }
                    }
                }
            }
            if closed {
                flush_pending_high(&mut open, &mut self.changes);
                self.finish_string(open);
            } else {
                self.string = Some(open);
            }
            index
        }

        /// Read a number/literal token until a delimiter or the fragment end.
        fn read_scalar(&mut self, text: &str, start: usize) -> usize {
            let mut index = start;
            while index < text.len() {
                let Some(value) = text[index..].chars().next() else {
                    break;
                };
                if is_delimiter(value) {
                    break;
                }
                self.scalar.push(value);
                index += value.len_utf8();
            }
            if index < text.len() {
                self.end_scalar();
            }
            index
        }

        /// A scalar token ended at a delimiter: move it to its slot when it is
        /// a complete literal/number, then mark the value ended.
        fn end_scalar(&mut self) {
            let token = std::mem::take(&mut self.scalar);
            let slot = self.scalar_slot.take().unwrap_or(Slot::Invalid);
            let path = self.scalar_path.take();
            if let Some(value) = scalar_value(&token) {
                if matches!(slot, Slot::Root) {
                    self.root_started = true;
                }
                if matches!(slot, Slot::Element(_)) {
                    if let Some(frame) = self.stack.last_mut() {
                        frame.size += 1;
                    }
                }
                if let Some(path) = path {
                    self.changes.push(JsonChange::Set { path, value });
                }
            }
            self.value_ended();
        }

        /// At the end of a fragment, show an unterminated scalar when the
        /// prefix is a displayable value.
        fn emit_pending_scalar(&mut self) {
            if self.done || self.scalar.is_empty() || self.scalar.len() > MAX_PENDING_SCALAR_BYTES {
                return;
            }
            let Some(path) = self.scalar_path.clone() else {
                return;
            };
            if let Some(value) = pending_scalar_value(&self.scalar) {
                self.changes.push(JsonChange::Set { path, value });
            }
        }

        fn start_string(&mut self) {
            let is_key = self.stack.last().is_some_and(|frame| {
                frame.is_object && matches!(frame.phase, Phase::Key | Phase::Comma)
            });
            if is_key {
                self.string = Some(OpenString {
                    target: StringTarget::Key,
                    path: None,
                    text: String::new(),
                    escape: Escape::None,
                    pending_high: None,
                });
                return;
            }
            let slot = self.value_slot();
            let path = self.slot_path(&slot);
            if matches!(slot, Slot::Root) {
                self.root_started = true;
            }
            if matches!(slot, Slot::Element(_)) {
                if let Some(frame) = self.stack.last_mut() {
                    frame.size += 1;
                }
            }
            if let Some(path) = &path {
                self.changes.push(JsonChange::Set {
                    path: path.clone(),
                    value: Value::String(String::new()),
                });
            }
            self.string = Some(OpenString {
                target: StringTarget::Value,
                path,
                text: String::new(),
                escape: Escape::None,
                pending_high: None,
            });
        }

        fn finish_string(&mut self, open: OpenString) {
            match open.target {
                StringTarget::Key => {
                    if let Some(frame) = self.stack.last_mut() {
                        if frame.is_object {
                            frame.key = Some(open.text);
                            frame.phase = Phase::Colon;
                        }
                    }
                }
                StringTarget::Value => self.value_ended(),
            }
        }

        fn open_container(&mut self, is_object: bool) {
            let slot = self.value_slot();
            let path = self.slot_path(&slot);
            if matches!(slot, Slot::Root) {
                self.root_started = true;
            }
            if matches!(slot, Slot::Element(_)) {
                if let Some(frame) = self.stack.last_mut() {
                    frame.size += 1;
                }
            }
            if let Some(path) = &path {
                let value = if is_object {
                    Value::Object(serde_json::Map::new())
                } else {
                    Value::Array(Vec::new())
                };
                self.changes.push(JsonChange::Set {
                    path: path.clone(),
                    value,
                });
            }
            self.stack.push(Frame {
                is_object,
                base_path: path,
                size: 0,
                key: None,
                phase: if is_object { Phase::Key } else { Phase::Value },
            });
        }

        fn close_container(&mut self) {
            // A stray closer outside any container is ignored; container
            // kind mismatches (`{` closed by `]`) are tolerated like upstream.
            if self.stack.pop().is_some() {
                self.value_ended();
            }
        }

        fn value_ended(&mut self) {
            if let Some(frame) = self.stack.last_mut() {
                frame.phase = Phase::Comma;
            } else if self.root_started {
                self.done = true;
            }
        }

        /// Where a value starting now goes, if anywhere.
        fn value_slot(&self) -> Slot {
            match self.stack.last() {
                None => {
                    if self.root_started {
                        Slot::Invalid
                    } else {
                        Slot::Root
                    }
                }
                Some(frame) if frame.is_object => {
                    // A missing colon is tolerated: `{"a" 1}` reads as
                    // `{"a": 1}`.
                    if matches!(frame.phase, Phase::Value | Phase::Colon) {
                        match &frame.key {
                            Some(key) => Slot::Member(key.clone()),
                            None => Slot::Invalid,
                        }
                    } else {
                        Slot::Invalid
                    }
                }
                Some(frame) => Slot::Element(frame.size),
            }
        }

        /// The full path of a value at `slot`, or `None` when it is not
        /// mirrored (invalid position or beyond the mirrored depth).
        fn slot_path(&self, slot: &Slot) -> Option<Vec<JsonPathSegment>> {
            let path = match slot {
                Slot::Root => Some(Vec::new()),
                Slot::Member(key) => {
                    let frame = self.stack.last()?;
                    let mut path = frame.base_path.clone()?;
                    path.push(JsonPathSegment::Key(key.clone()));
                    Some(path)
                }
                Slot::Element(index) => {
                    let frame = self.stack.last()?;
                    let mut path = frame.base_path.clone()?;
                    path.push(JsonPathSegment::Index(*index));
                    Some(path)
                }
                Slot::Invalid => None,
            };
            path.filter(|path| path.len() <= MAX_MIRRORED_DEPTH)
        }
    }

    /// Apply the changes of one fragment to the block's parsed arguments.
    /// Changes that do not fit the root object (a non-object root, a path
    /// into a value that a malformed stream never created) are ignored.
    pub(crate) fn apply_json_changes(
        target: &mut BTreeMap<String, Value>,
        changes: Vec<JsonChange>,
    ) {
        for change in changes {
            match change {
                JsonChange::Set { path, value } => apply_set(target, &path, value),
                JsonChange::Append { path, text } => apply_append(target, &path, &text),
            }
        }
    }

    fn apply_set(target: &mut BTreeMap<String, Value>, path: &[JsonPathSegment], value: Value) {
        let Some((first, rest)) = path.split_first() else {
            // The root value itself: the mirrored arguments are always the
            // root object, and a root container starts empty already.
            return;
        };
        let JsonPathSegment::Key(first_key) = first else {
            // A non-object root (array/scalar arguments) has no mirror shape.
            return;
        };
        if rest.is_empty() {
            target.insert(first_key.clone(), value);
            return;
        }
        let Some(node) = target.get_mut(first_key) else {
            return;
        };
        let Some(parent) = node_at(node, &rest[..rest.len() - 1]) else {
            return;
        };
        let last = &rest[rest.len() - 1];
        match (parent, last) {
            (Value::Object(map), JsonPathSegment::Key(key)) => {
                map.insert(key.clone(), value);
            }
            (Value::Array(items), JsonPathSegment::Index(index)) => {
                if *index < items.len() {
                    items[*index] = value;
                } else if *index == items.len() {
                    items.push(value);
                }
            }
            _ => {}
        }
    }

    fn apply_append(target: &mut BTreeMap<String, Value>, path: &[JsonPathSegment], text: &str) {
        let Some((first, rest)) = path.split_first() else {
            return;
        };
        let JsonPathSegment::Key(first_key) = first else {
            return;
        };
        let Some(node) = target.get_mut(first_key) else {
            return;
        };
        if rest.is_empty() {
            append_string(node, text);
            return;
        }
        let Some(parent) = node_at(node, &rest[..rest.len() - 1]) else {
            return;
        };
        let last = &rest[rest.len() - 1];
        let value = match (parent, last) {
            (Value::Object(map), JsonPathSegment::Key(key)) => map.get_mut(key),
            (Value::Array(items), JsonPathSegment::Index(index)) => items.get_mut(*index),
            _ => None,
        };
        if let Some(value) = value {
            append_string(value, text);
        }
    }

    fn node_at<'a>(mut node: &'a mut Value, path: &[JsonPathSegment]) -> Option<&'a mut Value> {
        for segment in path {
            node = match (node, segment) {
                (Value::Object(map), JsonPathSegment::Key(key)) => map.get_mut(key)?,
                (Value::Array(items), JsonPathSegment::Index(index)) => items.get_mut(*index)?,
                _ => return None,
            };
        }
        Some(node)
    }

    fn append_string(value: &mut Value, text: &str) {
        if let Value::String(string) = value {
            string.push_str(text);
        }
    }

    fn push_string_text(open: &mut OpenString, changes: &mut Vec<JsonChange>, text: &str) {
        if text.is_empty() {
            return;
        }
        match open.target {
            StringTarget::Key => open.text.push_str(text),
            StringTarget::Value => {
                if let Some(path) = &open.path {
                    changes.push(JsonChange::Append {
                        path: path.clone(),
                        text: text.to_string(),
                    });
                }
            }
        }
    }

    /// Flush a high surrogate that was not followed by its low surrogate.
    fn flush_pending_high(open: &mut OpenString, changes: &mut Vec<JsonChange>) {
        if open.pending_high.take().is_some() {
            push_string_text(open, changes, REPLACEMENT);
        }
    }

    /// Append one decoded `\uXXXX` code unit, pairing surrogates across
    /// fragments and mapping lone surrogates to U+FFFD.
    fn complete_unicode(open: &mut OpenString, changes: &mut Vec<JsonChange>, code: u16) {
        if let Some(high) = open.pending_high.take() {
            if (0xDC00..=0xDFFF).contains(&code) {
                let scalar =
                    0x1_0000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(code) - 0xDC00);
                if let Some(decoded) = char::from_u32(scalar) {
                    let mut buffer = [0u8; 4];
                    push_string_text(open, changes, decoded.encode_utf8(&mut buffer));
                } else {
                    push_string_text(open, changes, REPLACEMENT);
                }
                return;
            }
            // The high surrogate was not part of a pair.
            push_string_text(open, changes, REPLACEMENT);
        }
        if (0xD800..=0xDBFF).contains(&code) {
            open.pending_high = Some(code);
        } else if (0xDC00..=0xDFFF).contains(&code) {
            push_string_text(open, changes, REPLACEMENT);
        } else if let Some(decoded) = char::from_u32(u32::from(code)) {
            let mut buffer = [0u8; 4];
            push_string_text(open, changes, decoded.encode_utf8(&mut buffer));
        }
    }

    fn hex_value(byte: u8) -> u16 {
        match byte {
            b'0'..=b'9' => u16::from(byte - b'0'),
            b'a'..=b'f' => u16::from(byte - b'a') + 10,
            _ => u16::from(byte - b'A') + 10,
        }
    }

    /// One-character JSON escapes (`\u` is handled by the four-hex-digit
    /// state).
    fn simple_escape(value: char) -> Option<&'static str> {
        match value {
            '"' => Some("\""),
            '\\' => Some("\\"),
            '/' => Some("/"),
            'b' => Some("\u{0008}"),
            'f' => Some("\u{000C}"),
            'n' => Some("\n"),
            'r' => Some("\r"),
            't' => Some("\t"),
            _ => None,
        }
    }

    fn is_delimiter(value: char) -> bool {
        matches!(
            value,
            ' ' | '\t' | '\n' | '\r' | ',' | ':' | '{' | '}' | '[' | ']' | '"'
        )
    }

    /// The value of a complete JSON number or literal token, `None` for
    /// anything else (upstream `scalarValue`).
    fn scalar_value(token: &str) -> Option<Value> {
        match token {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            "null" => Some(Value::Null),
            _ if is_json_number(token) => serde_json::from_str::<Value>(token)
                .ok()
                .filter(Value::is_number),
            _ => None,
        }
    }

    /// The value an unterminated token shows (upstream
    /// `pendingScalarValue`): a prefix of a literal, a complete number, or
    /// the mantissa of a number whose exponent digits have not arrived yet.
    fn pending_scalar_value(token: &str) -> Option<Value> {
        if token.is_empty() {
            return None;
        }
        if "true".starts_with(token) {
            return Some(Value::Bool(true));
        }
        if "false".starts_with(token) {
            return Some(Value::Bool(false));
        }
        if "null".starts_with(token) {
            return Some(Value::Null);
        }
        if is_json_number(token) {
            return serde_json::from_str::<Value>(token)
                .ok()
                .filter(Value::is_number);
        }
        for suffix in ["e+", "e-", "E+", "E-", "e", "E"] {
            if let Some(mantissa) = token.strip_suffix(suffix) {
                if is_json_mantissa(mantissa) {
                    return serde_json::from_str::<Value>(mantissa)
                        .ok()
                        .filter(Value::is_number);
                }
                return None;
            }
        }
        None
    }

    /// `-?(0|[1-9][0-9]*)(\.[0-9]+)?` — a JSON number without its exponent.
    fn is_json_mantissa(token: &str) -> bool {
        let bytes = token.as_bytes();
        let mut index = 0;
        if bytes.first() == Some(&b'-') {
            index += 1;
        }
        if index >= bytes.len() {
            return false;
        }
        match bytes[index] {
            b'0' => index += 1,
            b'1'..=b'9' => {
                index += 1;
                while index < bytes.len() && bytes[index].is_ascii_digit() {
                    index += 1;
                }
            }
            _ => return false,
        }
        if index < bytes.len() && bytes[index] == b'.' {
            index += 1;
            let fraction_start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            if index == fraction_start {
                return false;
            }
        }
        index == bytes.len()
    }

    /// A complete JSON number (`-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`).
    fn is_json_number(token: &str) -> bool {
        let bytes = token.as_bytes();
        let mut index = 0;
        if bytes.first() == Some(&b'-') {
            index += 1;
        }
        if index >= bytes.len() {
            return false;
        }
        match bytes[index] {
            b'0' => index += 1,
            b'1'..=b'9' => {
                index += 1;
                while index < bytes.len() && bytes[index].is_ascii_digit() {
                    index += 1;
                }
            }
            _ => return false,
        }
        if index < bytes.len() && bytes[index] == b'.' {
            index += 1;
            let fraction_start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            if index == fraction_start {
                return false;
            }
        }
        if index < bytes.len() && (bytes[index] == b'e' || bytes[index] == b'E') {
            index += 1;
            if index < bytes.len() && (bytes[index] == b'+' || bytes[index] == b'-') {
                index += 1;
            }
            let exponent_start = index;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            if index == exponent_start {
                return false;
            }
        }
        index == bytes.len()
    }
}

#[cfg(feature = "provider-openai")]
pub(crate) use streaming_json::{apply_json_changes, JsonChange, StreamingJsonMirror};
