//! Chord delta operations for v8 service state updates (upstream
//! `packages/chord/src/delta/index.ts`, `wire.ts:131-139`).
//!
//! A state update carries either the v8 `{op, value}` object vocabulary pir
//! has always emitted, or the chord delta tuple vocabulary upstream clients
//! speak. This module implements the delta side: the decoded [`DeltaOp`]
//! batches, the wire encoder/decoder (path interning `["#", id, path]`,
//! numeric path refs, and the arity-shortened forms), and application to a
//! plain JSON value.
//!
//! The tuple vocabulary is:
//!
//! ```text
//! ["r", value]                        replace the whole value
//! ["s", path, value]                  set a property / array slot
//! ["d", path]                         delete a property / splice one array slot
//! ["a", path, text]                   append text to a string
//! ["t", path, count]                  drop `count` UTF-16 code units from a string
//! ["p", path, index, remove, items]   array splice
//! ["m", path, permutation]            reorder an array (`new[i] = old[permutation[i]]`)
//! ["#", id, path]                     define a path id (decoder directive, no value change)
//! ```
//!
//! Short forms omit the path and reuse the previous op's path; a numeric path
//! ref is an id defined by an earlier `["#", id, path]`. `r` clears the path
//! dictionary (a base batch is a recovery point), and a path id may only be
//! referenced after its definition. Paths are never allowed to reach
//! `__proto__`/`constructor`/`prototype`, and `s`/`d`/`a`/`t` cannot target
//! the root (only `r` replaces it).

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde_json::{Map, Value};

/// One path segment: an object key or a non-negative array index.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(untagged)]
pub enum DeltaSeg {
    Key(String),
    Index(usize),
}

impl DeltaSeg {
    fn is_reserved(&self) -> bool {
        matches!(self, DeltaSeg::Key(key) if key == "__proto__" || key == "constructor" || key == "prototype")
    }
}

/// A path through a JSON value.
pub type DeltaPath = Vec<DeltaSeg>;

/// A decoded delta operation (paths inline; `#` definitions and short forms
/// are a wire concern handled by [`DeltaDecoder`]).
#[derive(Clone, Debug, PartialEq)]
pub enum DeltaOp {
    Replace(Value),
    Set(DeltaPath, Value),
    Delete(DeltaPath),
    Append(DeltaPath, String),
    Truncate(DeltaPath, usize),
    Splice {
        path: DeltaPath,
        index: usize,
        remove: usize,
        items: Vec<Value>,
    },
    Move {
        path: DeltaPath,
        permutation: Vec<usize>,
    },
}

impl DeltaOp {
    /// The op's path (`None` for `r`, which has no path).
    pub fn path(&self) -> Option<&DeltaPath> {
        match self {
            DeltaOp::Replace(_) => None,
            DeltaOp::Set(path, _)
            | DeltaOp::Delete(path)
            | DeltaOp::Append(path, _)
            | DeltaOp::Truncate(path, _)
            | DeltaOp::Splice { path, .. }
            | DeltaOp::Move { path, .. } => Some(path),
        }
    }

    /// Whether this op replaces the whole value (a base/recovery op).
    pub fn is_replace(&self) -> bool {
        matches!(self, DeltaOp::Replace(_))
    }
}

/// Whether a decoded batch begins with a replacement (upstream `isBase`).
pub fn is_base(ops: &[DeltaOp]) -> bool {
    ops.first().is_some_and(DeltaOp::is_replace)
}

/// Why an operation or its wire form was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaError {
    /// The operation tuple (or its arity/payload) is malformed.
    Shape(String),
    /// A path segment is reserved, negative, or a string index on an array.
    UnsafePath(String),
    /// A path could not be resolved (or a path id was never defined).
    Path(String),
}

impl fmt::Display for DeltaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeltaError::Shape(message) => write!(formatter, "invalid operation: {message}"),
            DeltaError::UnsafePath(message) => write!(formatter, "unsafe path segment: {message}"),
            DeltaError::Path(message) => write!(formatter, "unresolvable path: {message}"),
        }
    }
}

impl std::error::Error for DeltaError {}

impl From<DeltaError> for super::error::SchemaError {
    fn from(error: DeltaError) -> Self {
        super::error::SchemaError::new(error.to_string())
    }
}

fn path_text(path: &[DeltaSeg]) -> String {
    let mut text = String::new();
    for segment in path {
        text.push('/');
        match segment {
            DeltaSeg::Key(key) => text.push_str(key),
            DeltaSeg::Index(index) => text.push_str(&index.to_string()),
        }
    }
    text
}

fn assert_safe_path(path: &[DeltaSeg]) -> Result<(), DeltaError> {
    for segment in path {
        if segment.is_reserved() {
            return Err(DeltaError::UnsafePath(format!("{segment:?}")));
        }
    }
    Ok(())
}

fn assert_non_empty(path: &[DeltaSeg]) -> Result<(), DeltaError> {
    if path.is_empty() {
        return Err(DeltaError::Shape(
            "s/d/a/t cannot target the root".to_owned(),
        ));
    }
    Ok(())
}

fn assert_permutation(permutation: &[usize]) -> Result<(), DeltaError> {
    let mut seen = vec![false; permutation.len()];
    for &index in permutation {
        if index >= permutation.len() || seen[index] {
            return Err(DeltaError::Shape(
                "m permutation is not a bijection".to_owned(),
            ));
        }
        seen[index] = true;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Wire codec
// ---------------------------------------------------------------------------

fn path_key(path: &[DeltaSeg]) -> String {
    // The upstream key is `JSON.stringify(path)`; the wire form is exactly
    // what the encoder emits. Serialize the segment slice DIRECTLY: the former
    // shape built a `Value::Array` and cloned every key string just to
    // stringify it, on every delta op (one allocation per text delta).
    struct PathKey<'a>(&'a [DeltaSeg]);

    impl serde::Serialize for PathKey<'_> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeSeq as _;
            let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
            for segment in self.0 {
                match segment {
                    DeltaSeg::Key(key) => seq.serialize_element(key)?,
                    DeltaSeg::Index(index) => seq.serialize_element(index)?,
                }
            }
            seq.end()
        }
    }

    serde_json::to_string(&PathKey(path)).unwrap_or_default()
}

fn path_value(path: &[DeltaSeg]) -> Value {
    Value::Array(
        path.iter()
            .map(|segment| match segment {
                DeltaSeg::Key(key) => Value::String(key.clone()),
                DeltaSeg::Index(index) => Value::Number(serde_json::Number::from(*index)),
            })
            .collect(),
    )
}

enum PathStyle<'a> {
    /// Borrowed from the op being encoded: an inline path does not need its
    /// own `Vec<DeltaSeg>` copy.
    Inline(&'a DeltaPath),
    Id(usize),
    Short,
}

fn push_op(out: &mut Vec<Value>, op: &DeltaOp, style: PathStyle<'_>) {
    let mut tuple = Vec::with_capacity(5);
    match op {
        DeltaOp::Replace(value) => {
            tuple.push(Value::String("r".to_owned()));
            tuple.push(value.clone());
        }
        DeltaOp::Set(_, value) => {
            tuple.push(Value::String("s".to_owned()));
            push_path(&mut tuple, &style);
            tuple.push(value.clone());
        }
        DeltaOp::Delete(_) => {
            tuple.push(Value::String("d".to_owned()));
            push_path(&mut tuple, &style);
        }
        DeltaOp::Append(_, text) => {
            tuple.push(Value::String("a".to_owned()));
            push_path(&mut tuple, &style);
            tuple.push(Value::String(text.clone()));
        }
        DeltaOp::Truncate(_, count) => {
            tuple.push(Value::String("t".to_owned()));
            push_path(&mut tuple, &style);
            tuple.push(Value::Number(serde_json::Number::from(*count)));
        }
        DeltaOp::Splice {
            index,
            remove,
            items,
            ..
        } => {
            tuple.push(Value::String("p".to_owned()));
            push_path(&mut tuple, &style);
            tuple.push(Value::Number(serde_json::Number::from(*index)));
            tuple.push(Value::Number(serde_json::Number::from(*remove)));
            tuple.push(Value::Array(items.clone()));
        }
        DeltaOp::Move { permutation, .. } => {
            tuple.push(Value::String("m".to_owned()));
            push_path(&mut tuple, &style);
            tuple.push(Value::Array(
                permutation
                    .iter()
                    .map(|index| Value::Number(serde_json::Number::from(*index)))
                    .collect(),
            ));
        }
    }
    out.push(Value::Array(tuple));
}

fn push_path(tuple: &mut Vec<Value>, style: &PathStyle<'_>) {
    match style {
        PathStyle::Inline(path) => tuple.push(path_value(path)),
        PathStyle::Id(id) => tuple.push(Value::Number(serde_json::Number::from(*id))),
        PathStyle::Short => {}
    }
}

/// The encoder of one independent state stream (upstream `encoder()`).
///
/// A decoder must observe exactly the batches this encoder produced,
/// beginning with the state's base. Path ids persist across batches (they are
/// explicit in the wire form); the arity-shortening "previous path" does not.
#[derive(Debug, Default)]
pub struct DeltaEncoder {
    seen: HashSet<String>,
    ids: HashMap<String, usize>,
    next_id: usize,
}

impl DeltaEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Encode one batch.
    pub fn encode(&mut self, ops: &[DeltaOp]) -> Vec<Value> {
        let mut out = Vec::new();
        let mut previous: Option<String> = None;
        for op in ops {
            if let DeltaOp::Replace(_) = op {
                push_op(&mut out, op, PathStyle::Short);
                // A base batch is a recovery point; ids after it must be
                // self-contained for a reader that replays from here.
                self.seen.clear();
                self.ids.clear();
                self.next_id = 0;
                previous = None;
                continue;
            }
            let Some(path) = op.path() else {
                // `Replace` was handled above; every other op has a path.
                continue;
            };
            let key = path_key(path);
            if previous.as_deref() == Some(key.as_str()) {
                push_op(&mut out, op, PathStyle::Short);
                continue;
            }
            let style = if let Some(id) = self.ids.get(&key) {
                PathStyle::Id(*id)
            } else if self.seen.contains(&key) {
                let id = self.next_id;
                self.next_id += 1;
                self.ids.insert(key.clone(), id);
                out.push(Value::Array(vec![
                    Value::String("#".to_owned()),
                    Value::Number(serde_json::Number::from(id)),
                    path_value(path),
                ]));
                PathStyle::Id(id)
            } else {
                self.seen.insert(key.clone());
                PathStyle::Inline(path)
            };
            push_op(&mut out, op, style);
            previous = Some(key);
        }
        out
    }
}

/// Hard ceiling on the number of path ids one delta stream may define. Path
/// ids are wire-driven state: without a bound a malicious (or buggy) producer
/// can grow the decoder's dictionary for as long as the stream lives. A base
/// batch (`r`) clears the dictionary, so a legitimate stream that needs more
/// ids can always reset and replay.
pub const MAX_DELTA_PATH_IDS: usize = 4096;

/// The decoder of one independent state stream (upstream `decoder()`).
#[derive(Debug, Default)]
pub struct DeltaDecoder {
    paths: HashMap<usize, DeltaPath>,
}

fn decode_segments(value: &Value, context: &str) -> Result<DeltaPath, DeltaError> {
    let array = value
        .as_array()
        .ok_or_else(|| DeltaError::Shape(format!("{context}: path is not an array")))?;
    let mut path = Vec::with_capacity(array.len());
    for segment in array {
        match segment {
            Value::String(key) => {
                let key = DeltaSeg::Key(key.clone());
                if key.is_reserved() {
                    return Err(DeltaError::UnsafePath(format!("{key:?}")));
                }
                path.push(key);
            }
            Value::Number(number) => {
                let index = number.as_u64().ok_or_else(|| {
                    DeltaError::UnsafePath(format!("{context}: negative path index"))
                })?;
                let index = usize::try_from(index).map_err(|_| {
                    DeltaError::UnsafePath(format!("{context}: path index is out of range"))
                })?;
                path.push(DeltaSeg::Index(index));
            }
            _ => {
                return Err(DeltaError::UnsafePath(format!(
                    "{context}: path segment is not a string or integer"
                )))
            }
        }
    }
    Ok(path)
}

fn wire_verb(op: &[Value]) -> Result<&str, DeltaError> {
    op.first()
        .and_then(Value::as_str)
        .ok_or_else(|| DeltaError::Shape("op is not a tuple".to_owned()))
}

fn wire_len(op: &[Value], expected: usize) -> Result<(), DeltaError> {
    if op.len() == expected {
        Ok(())
    } else {
        Err(DeltaError::Shape(format!(
            "{} arity: expected {expected}, got {}",
            wire_verb(op)?.to_owned(),
            op.len()
        )))
    }
}

fn wire_short(op: &[Value], verb: &str) -> bool {
    match verb {
        "d" => op.len() == 1,
        "p" => op.len() == 4,
        _ => op.len() == 2,
    }
}

impl DeltaDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one batch, resolving and validating every wire form.
    pub fn decode(&mut self, wire: &[Value]) -> Result<Vec<DeltaOp>, DeltaError> {
        let mut out = Vec::with_capacity(wire.len());
        let mut previous: Option<DeltaPath> = None;
        for op in wire {
            let array = op
                .as_array()
                .ok_or_else(|| DeltaError::Shape("op is not a tuple".to_owned()))?;
            let verb = wire_verb(array)?.to_owned();
            if verb == "#" {
                wire_len(array, 3)?;
                let id = array[1]
                    .as_u64()
                    .ok_or_else(|| DeltaError::Shape("# id".to_owned()))?;
                let path = decode_segments(&array[2], "#")?;
                assert_safe_path(&path)?;
                let id = usize::try_from(id)
                    .map_err(|_| DeltaError::Shape(format!("# id {id} is out of range")))?;
                // Bound the dictionary: a new id past the cap is a structured
                // error (the producer must replay from a base batch, which
                // clears the dictionary).
                if !self.paths.contains_key(&id) && self.paths.len() >= MAX_DELTA_PATH_IDS {
                    return Err(DeltaError::Shape(format!(
                        "# path dictionary exceeds {MAX_DELTA_PATH_IDS} ids; replay from a base batch"
                    )));
                }
                self.paths.insert(id, path);
                continue;
            }
            if verb == "r" {
                wire_len(array, 2)?;
                out.push(DeltaOp::Replace(array[1].clone()));
                self.paths.clear();
                previous = None;
                continue;
            }
            if !matches!(verb.as_str(), "s" | "d" | "a" | "t" | "p" | "m") {
                return Err(DeltaError::Shape(format!("unknown op verb: {verb}")));
            }
            let short = wire_short(array, &verb);
            let path = if short {
                previous
                    .clone()
                    .ok_or_else(|| DeltaError::Path("[]".to_owned()))?
            } else {
                let reference = &array[1];
                let path = if let Value::Number(number) = reference {
                    let id = number
                        .as_u64()
                        .ok_or_else(|| DeltaError::Path(reference.to_string()))?;
                    self.paths
                        .get(&(id as usize))
                        .cloned()
                        .ok_or_else(|| DeltaError::Path(id.to_string()))?
                } else {
                    decode_segments(reference, &verb)?
                };
                assert_safe_path(&path)?;
                previous = Some(path.clone());
                path
            };
            if !matches!(verb.as_str(), "p" | "m") && path.is_empty() {
                return Err(DeltaError::Path("[]".to_owned()));
            }
            let value_index = if short { 1 } else { 2 };
            let op = match verb.as_str() {
                "s" => {
                    wire_len(array, value_index + 1)?;
                    DeltaOp::Set(path, array[value_index].clone())
                }
                "d" => {
                    if short {
                        wire_len(array, 1)?;
                    } else {
                        wire_len(array, 2)?;
                    }
                    DeltaOp::Delete(path)
                }
                "a" => {
                    wire_len(array, value_index + 1)?;
                    let text = array[value_index]
                        .as_str()
                        .ok_or_else(|| DeltaError::Shape("a value".to_owned()))?;
                    DeltaOp::Append(path, text.to_owned())
                }
                "t" => {
                    wire_len(array, value_index + 1)?;
                    let count = array[value_index]
                        .as_u64()
                        .ok_or_else(|| DeltaError::Shape("t count".to_owned()))?;
                    DeltaOp::Truncate(path, usize::try_from(count).unwrap_or(usize::MAX))
                }
                "p" => {
                    let [index, remove, items] = if short {
                        wire_len(array, 4)?;
                        [&array[1], &array[2], &array[3]]
                    } else {
                        wire_len(array, 5)?;
                        [&array[2], &array[3], &array[4]]
                    };
                    let index = index
                        .as_u64()
                        .ok_or_else(|| DeltaError::Shape("p index".to_owned()))?;
                    let remove = remove
                        .as_u64()
                        .ok_or_else(|| DeltaError::Shape("p remove".to_owned()))?;
                    let items = items
                        .as_array()
                        .ok_or_else(|| DeltaError::Shape("p items".to_owned()))?
                        .clone();
                    DeltaOp::Splice {
                        path,
                        index: usize::try_from(index).unwrap_or(usize::MAX),
                        remove: usize::try_from(remove).unwrap_or(usize::MAX),
                        items,
                    }
                }
                "m" => {
                    let permutation = if short {
                        wire_len(array, 2)?;
                        &array[1]
                    } else {
                        wire_len(array, 3)?;
                        &array[2]
                    };
                    let permutation: Vec<usize> = serde_json::from_value(permutation.clone())
                        .map_err(|_| DeltaError::Shape("m permutation".to_owned()))?;
                    assert_permutation(&permutation)?;
                    DeltaOp::Move { path, permutation }
                }
                // The guard above rejects every other verb, so this arm is a
                // defensive total case rather than a panic point: decoder
                // input is network-driven, and a malformed batch must always
                // surface as a typed error.
                _ => {
                    return Err(DeltaError::Shape(format!("unknown op verb: {verb}")));
                }
            };
            out.push(op);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Applier
// ---------------------------------------------------------------------------

fn resolve_mut<'a>(root: &'a mut Value, path: &[DeltaSeg]) -> Result<&'a mut Value, DeltaError> {
    let mut node = root;
    for segment in path {
        match (node, segment) {
            (Value::Object(map), DeltaSeg::Key(key)) => {
                node = map
                    .get_mut(key)
                    .ok_or_else(|| DeltaError::Path(path_text(path)))?;
            }
            (Value::Array(items), DeltaSeg::Index(index)) => {
                node = items
                    .get_mut(*index)
                    .ok_or_else(|| DeltaError::Path(path_text(path)))?;
            }
            (Value::Array(_), DeltaSeg::Key(_)) => {
                return Err(DeltaError::UnsafePath(segment.to_string()));
            }
            _ => return Err(DeltaError::Path(path_text(path))),
        }
    }
    Ok(node)
}

impl fmt::Display for DeltaSeg {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeltaSeg::Key(key) => formatter.write_str(key),
            DeltaSeg::Index(index) => write!(formatter, "{index}"),
        }
    }
}

/// Apply one decoded batch to a plain JSON value, in place.
pub fn apply_delta(root: &mut Value, ops: &[DeltaOp]) -> Result<(), DeltaError> {
    for op in ops {
        if let DeltaOp::Replace(value) = op {
            *root = value.clone();
            continue;
        }
        if let Some(path) = op.path() {
            assert_safe_path(path)?;
        }
        match op {
            // `Replace` is handled by the `if let` above (it is the recovery
            // point and never goes through path validation). Keep the match
            // total without panicking: a decoder-facing function must report
            // a typed error even if that control flow is ever refactored.
            DeltaOp::Replace(_) => {
                return Err(DeltaError::Shape(
                    "replace operation was not handled as a recovery point".to_owned(),
                ));
            }
            DeltaOp::Set(path, value) => {
                assert_non_empty(path)?;
                let (last, parents) = path
                    .split_last()
                    .ok_or_else(|| DeltaError::Shape("s cannot target the root".to_owned()))?;
                let parent = resolve_mut(root, parents)?;
                match (parent, last) {
                    (Value::Object(map), DeltaSeg::Key(key)) => {
                        map.insert(key.clone(), value.clone());
                    }
                    (Value::Array(items), DeltaSeg::Index(index)) => {
                        if *index > items.len() {
                            return Err(DeltaError::UnsafePath(index.to_string()));
                        }
                        if *index == items.len() {
                            items.push(value.clone());
                        } else {
                            items[*index] = value.clone();
                        }
                    }
                    (Value::Array(_), DeltaSeg::Key(_)) => {
                        return Err(DeltaError::UnsafePath(last.to_string()));
                    }
                    _ => return Err(DeltaError::Path(path_text(path))),
                }
            }
            DeltaOp::Delete(path) => {
                assert_non_empty(path)?;
                let (last, parents) = path
                    .split_last()
                    .ok_or_else(|| DeltaError::Shape("d cannot target the root".to_owned()))?;
                let parent = resolve_mut(root, parents)?;
                match (parent, last) {
                    (Value::Object(map), DeltaSeg::Key(key)) => {
                        map.remove(key);
                    }
                    (Value::Array(items), DeltaSeg::Index(index)) => {
                        if *index >= items.len() {
                            return Err(DeltaError::Path(path_text(path)));
                        }
                        items.remove(*index);
                    }
                    (Value::Array(_), DeltaSeg::Key(_)) => {
                        return Err(DeltaError::UnsafePath(last.to_string()));
                    }
                    _ => return Err(DeltaError::Path(path_text(path))),
                }
            }
            DeltaOp::Append(path, text) => {
                assert_non_empty(path)?;
                let node = resolve_mut(root, path)?;
                let Value::String(current) = node else {
                    return Err(DeltaError::Path(path_text(path)));
                };
                current.push_str(text);
            }
            DeltaOp::Truncate(path, count) => {
                assert_non_empty(path)?;
                let node = resolve_mut(root, path)?;
                let Value::String(current) = node else {
                    return Err(DeltaError::Path(path_text(path)));
                };
                // Upstream `current.slice(count)` counts UTF-16 code units.
                // Counting scalars would replay a TypeScript-written batch at
                // the wrong offset for any non-BMP text; a count that splits a
                // surrogate pair becomes U+FFFD, as a lossy decode of the same
                // `slice` does.
                let truncated = String::from_utf16_lossy(
                    &current.encode_utf16().skip(*count).collect::<Vec<u16>>(),
                );
                *current = truncated;
            }
            DeltaOp::Splice {
                path,
                index,
                remove,
                items,
            } => {
                let node = if path.is_empty() {
                    &mut *root
                } else {
                    resolve_mut(root, path)?
                };
                let Value::Array(array) = node else {
                    return Err(DeltaError::Path(path_text(path)));
                };
                let start = (*index).min(array.len());
                let end = start.saturating_add(*remove).min(array.len());
                if end < start {
                    return Err(DeltaError::Shape("p splice range is invalid".to_owned()));
                }
                array.splice(start..end, items.iter().cloned());
            }
            DeltaOp::Move { path, permutation } => {
                assert_permutation(permutation)?;
                let node = if path.is_empty() {
                    &mut *root
                } else {
                    resolve_mut(root, path)?
                };
                let Value::Array(array) = node else {
                    return Err(DeltaError::Path(path_text(path)));
                };
                if array.len() != permutation.len() {
                    return Err(DeltaError::Path(path_text(path)));
                }
                let previous = array.clone();
                for (index, source) in permutation.iter().enumerate() {
                    array[index] = previous[*source].clone();
                }
            }
        }
    }
    Ok(())
}

/// Apply decoded batches to a clone of `target`, returning the final value.
pub fn apply_delta_immutable<I>(target: &Value, batches: I) -> Result<Value, DeltaError>
where
    I: IntoIterator<Item = Vec<DeltaOp>>,
{
    let mut root = target.clone();
    for ops in batches {
        apply_delta(&mut root, &ops)?;
    }
    Ok(root)
}

/// Validate a batch by applying it to an empty object (no external mutation).
pub fn validate_delta(ops: &[DeltaOp]) -> Result<(), DeltaError> {
    let mut probe = Value::Object(Map::new());
    apply_delta(&mut probe, ops)
}
