//! Immutable JSON operation batches (the subset of `@earendil-works/chord/delta`
//! the durable storage needs).
//!
//! Documents store a base value plus deltas. Upstream imports `Op` and
//! `applyImmutableBatches` from the chord package; this crate has no chord
//! runtime, so the same tuple vocabulary and application semantics are
//! implemented here over [`serde_json::Value`].
//!
//! The wire form is the upstream tuple array:
//!
//! ```text
//! ["r", value]                  replace the whole value
//! ["s", path, value]            set a property / array slot
//! ["d", path]                   delete a property / splice one array slot
//! ["a", path, text]             append text to a string
//! ["t", path, count]            drop `count` characters from a string's start
//! ["p", path, index, remove, items]   array splice
//! ["m", path, permutation]      reorder an array
//! ```
//!
//! Paths are non-empty for `s`/`d`/`a`/`t` (the root can only be replaced),
//! and segments must be non-negative integers (array indices) or strings other
//! than `__proto__`, `constructor` and `prototype`.
//!
//! Application is copy-on-write: [`apply_immutable_batches`] clones the target
//! once per call and mutates the clone, which yields the same value as
//! upstream's container-sharing replay without exposing intermediate revisions.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;

/// One path segment: an object key or an array index.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Seg {
    /// An object key.
    Key(String),
    /// A non-negative array index.
    Index(usize),
}

impl Seg {
    /// The reserved object keys the upstream vocabulary forbids.
    fn is_reserved(&self) -> bool {
        matches!(self, Self::Key(key) if key == "__proto__" || key == "constructor" || key == "prototype")
    }
}

/// A path through a JSON value.
pub type Path = Vec<Seg>;

/// A decoded operation.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// Replace the whole value.
    Replace(Value),
    /// Set a property or array slot.
    Set(Path, Value),
    /// Delete a property or splice one array slot.
    Delete(Path),
    /// Append text to a string.
    Append(Path, String),
    /// Drop characters from the start of a string (`slice(count)` upstream).
    Truncate(Path, usize),
    /// Splice an array.
    Splice {
        /// Array path (empty for the root).
        path: Path,
        /// Insert position.
        index: usize,
        /// Number of elements to remove.
        remove: usize,
        /// Elements to insert.
        items: Vec<Value>,
    },
    /// Reorder an array: `new[i] = old[permutation[i]]`.
    Move {
        /// Array path (empty for the root).
        path: Path,
        /// The permutation.
        permutation: Vec<usize>,
    },
}

/// Why an operation batch was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeltaError {
    /// The operation tuple is malformed.
    #[error("invalid operation: {0}")]
    Shape(String),
    /// A path segment is reserved or negative.
    #[error("unsafe path segment: {0}")]
    UnsafePath(String),
    /// A path could not be resolved.
    #[error("unresolvable path: {0}")]
    Path(String),
}

impl Serialize for Op {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        match self {
            Self::Replace(value) => {
                let mut seq = serializer.serialize_seq(Some(2))?;
                seq.serialize_element("r")?;
                seq.serialize_element(value)?;
                seq.end()
            }
            Self::Set(path, value) => {
                let mut seq = serializer.serialize_seq(Some(3))?;
                seq.serialize_element("s")?;
                seq.serialize_element(path)?;
                seq.serialize_element(value)?;
                seq.end()
            }
            Self::Delete(path) => {
                let mut seq = serializer.serialize_seq(Some(2))?;
                seq.serialize_element("d")?;
                seq.serialize_element(path)?;
                seq.end()
            }
            Self::Append(path, text) => {
                let mut seq = serializer.serialize_seq(Some(3))?;
                seq.serialize_element("a")?;
                seq.serialize_element(path)?;
                seq.serialize_element(text)?;
                seq.end()
            }
            Self::Truncate(path, count) => {
                let mut seq = serializer.serialize_seq(Some(3))?;
                seq.serialize_element("t")?;
                seq.serialize_element(path)?;
                seq.serialize_element(count)?;
                seq.end()
            }
            Self::Splice {
                path,
                index,
                remove,
                items,
            } => {
                let mut seq = serializer.serialize_seq(Some(5))?;
                seq.serialize_element("p")?;
                seq.serialize_element(path)?;
                seq.serialize_element(index)?;
                seq.serialize_element(remove)?;
                seq.serialize_element(items)?;
                seq.end()
            }
            Self::Move { path, permutation } => {
                let mut seq = serializer.serialize_seq(Some(3))?;
                seq.serialize_element("m")?;
                seq.serialize_element(path)?;
                seq.serialize_element(permutation)?;
                seq.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Op {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Vec::<Value>::deserialize(deserializer)?;
        let verb = raw
            .first()
            .and_then(Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("op is not a tuple"))?;
        let shape = |what: &str| serde::de::Error::custom(format!("invalid {verb} op: {what}"));
        let decode_path = |value: &Value| -> Result<Path, D::Error> {
            let path: Path = serde_json::from_value(value.clone()).map_err(|_| shape("path"))?;
            for seg in &path {
                if seg.is_reserved() {
                    return Err(serde::de::Error::custom(format!(
                        "unsafe path segment: {seg:?}"
                    )));
                }
            }
            Ok(path)
        };
        match verb {
            "r" if raw.len() == 2 => Ok(Self::Replace(raw[1].clone())),
            "s" if raw.len() == 3 => Ok(Self::Set(decode_path(&raw[1])?, raw[2].clone())),
            "d" if raw.len() == 2 => Ok(Self::Delete(decode_path(&raw[1])?)),
            "a" if raw.len() == 3 => {
                let text = raw[2].as_str().ok_or_else(|| shape("text"))?;
                Ok(Self::Append(decode_path(&raw[1])?, text.to_owned()))
            }
            "t" if raw.len() == 3 => {
                let count = raw[2]
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| shape("count"))?;
                Ok(Self::Truncate(decode_path(&raw[1])?, count))
            }
            "p" if raw.len() == 5 => {
                let index = raw[2]
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| shape("index"))?;
                let remove = raw[3]
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| shape("remove"))?;
                let items = raw[4].as_array().ok_or_else(|| shape("items"))?.clone();
                Ok(Self::Splice {
                    path: decode_path(&raw[1])?,
                    index,
                    remove,
                    items,
                })
            }
            "m" if raw.len() == 3 => {
                let permutation: Vec<usize> =
                    serde_json::from_value(raw[2].clone()).map_err(|_| shape("permutation"))?;
                assert_permutation(&permutation).map_err(serde::de::Error::custom)?;
                Ok(Self::Move {
                    path: decode_path(&raw[1])?,
                    permutation,
                })
            }
            other => Err(shape(other)),
        }
    }
}

impl fmt::Display for Seg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Key(key) => f.write_str(key),
            Self::Index(index) => write!(f, "{index}"),
        }
    }
}

fn path_text(path: &[Seg]) -> String {
    let mut text = String::new();
    for seg in path {
        text.push('/');
        text.push_str(&seg.to_string());
    }
    text
}

fn assert_permutation(permutation: &[usize]) -> Result<(), String> {
    let mut seen = vec![false; permutation.len()];
    for &index in permutation {
        if index >= permutation.len() || seen[index] {
            return Err("m permutation is not a bijection".to_owned());
        }
        seen[index] = true;
    }
    Ok(())
}

fn assert_non_empty(path: &[Seg]) -> Result<(), DeltaError> {
    if path.is_empty() {
        return Err(DeltaError::Shape(
            "s/d/a/t cannot target the root".to_owned(),
        ));
    }
    Ok(())
}

fn resolve_mut<'a>(root: &'a mut Value, path: &[Seg]) -> Result<&'a mut Value, DeltaError> {
    let mut node = root;
    for seg in path {
        match (node, seg) {
            (Value::Object(map), Seg::Key(key)) => {
                node = map
                    .get_mut(key)
                    .ok_or_else(|| DeltaError::Path(path_text(path)))?;
            }
            (Value::Array(items), Seg::Index(index)) => {
                node = items
                    .get_mut(*index)
                    .ok_or_else(|| DeltaError::Path(path_text(path)))?;
            }
            (Value::Array(_), Seg::Key(_)) => {
                return Err(DeltaError::UnsafePath(seg.to_string()));
            }
            _ => return Err(DeltaError::Path(path_text(path))),
        }
    }
    Ok(node)
}

/// Apply one decoded operation batch to a value, in place.
pub fn apply(root: &mut Value, ops: &[Op]) -> Result<(), DeltaError> {
    for op in ops {
        match op {
            Op::Replace(value) => *root = value.clone(),
            Op::Set(path, value) => {
                assert_non_empty(path)?;
                let (last, parents) = path
                    .split_last()
                    .ok_or_else(|| DeltaError::Shape("s cannot target the root".to_owned()))?;
                let parent = resolve_mut(&mut *root, parents)?;
                match (parent, last) {
                    (Value::Object(map), Seg::Key(key)) => {
                        map.insert(key.clone(), value.clone());
                    }
                    (Value::Array(items), Seg::Index(index)) => {
                        if *index > items.len() {
                            return Err(DeltaError::UnsafePath(index.to_string()));
                        }
                        if *index == items.len() {
                            items.push(value.clone());
                        } else {
                            items[*index] = value.clone();
                        }
                    }
                    (Value::Array(_), Seg::Key(_)) => {
                        return Err(DeltaError::UnsafePath(last.to_string()));
                    }
                    _ => return Err(DeltaError::Path(path_text(path))),
                }
            }
            Op::Delete(path) => {
                assert_non_empty(path)?;
                let (last, parents) = path
                    .split_last()
                    .ok_or_else(|| DeltaError::Shape("d cannot target the root".to_owned()))?;
                let parent = resolve_mut(&mut *root, parents)?;
                match (parent, last) {
                    (Value::Object(map), Seg::Key(key)) => {
                        map.remove(key);
                    }
                    (Value::Array(items), Seg::Index(index)) => {
                        if *index >= items.len() {
                            return Err(DeltaError::Path(path_text(path)));
                        }
                        items.remove(*index);
                    }
                    (Value::Array(_), Seg::Key(_)) => {
                        return Err(DeltaError::UnsafePath(last.to_string()));
                    }
                    _ => return Err(DeltaError::Path(path_text(path))),
                }
            }
            Op::Append(path, text) => {
                assert_non_empty(path)?;
                let node = resolve_mut(&mut *root, path)?;
                let Value::String(current) = node else {
                    return Err(DeltaError::Path(path_text(path)));
                };
                current.push_str(text);
            }
            Op::Truncate(path, count) => {
                assert_non_empty(path)?;
                let node = resolve_mut(&mut *root, path)?;
                let Value::String(current) = node else {
                    return Err(DeltaError::Path(path_text(path)));
                };
                // Upstream `current.slice(count)` counts **UTF-16 code units**,
                // and the differ emits UTF-16 lengths (`before.length - shared`).
                // Counting scalars here would replay a TypeScript-written log at
                // the wrong offset for any non-BMP text (emoji in shell output
                // is common). A count that splits a surrogate pair cannot be
                // represented in a Rust `String`; the lone surrogate becomes
                // U+FFFD, which is what a lossy decode of the same `slice` gives.
                let truncated: String = String::from_utf16_lossy(
                    &current.encode_utf16().skip(*count).collect::<Vec<u16>>(),
                );
                *current = truncated;
            }
            Op::Splice {
                path,
                index,
                remove,
                items,
            } => {
                let node = if path.is_empty() {
                    &mut *root
                } else {
                    resolve_mut(&mut *root, path)?
                };
                let Value::Array(array) = node else {
                    return Err(DeltaError::Path(path_text(path)));
                };
                let start = (*index).min(array.len());
                // `remove` comes from external op JSON, so a corrupt log can
                // carry a value that overflows `start + remove`; saturate
                // instead of panicking in debug or wrapping in release.
                let end = start.saturating_add(*remove).min(array.len());
                if end < start {
                    return Err(DeltaError::Shape("p splice range is invalid".to_owned()));
                }
                array.splice(start..end, items.iter().cloned());
            }
            Op::Move { path, permutation } => {
                assert_permutation(permutation).map_err(DeltaError::Shape)?;
                let node = if path.is_empty() {
                    &mut *root
                } else {
                    resolve_mut(&mut *root, path)?
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

/// Apply decoded batches as one final-result-only replay.
///
/// The target is cloned once; the caller's value is never mutated.
pub fn apply_immutable_batches<I>(target: &Value, batches: I) -> Result<Value, DeltaError>
where
    I: IntoIterator<Item = Vec<Op>>,
{
    let mut root = target.clone();
    for ops in batches {
        apply(&mut root, &ops)?;
    }
    Ok(root)
}

/// A quick validation of a whole batch without applying it.
pub fn validate(ops: &[Op]) -> Result<(), DeltaError> {
    let mut probe = Value::Null;
    apply(&mut probe, ops)
}

/// Reserved path segments, exported for callers that build paths themselves.
pub fn reserved_segments() -> HashSet<&'static str> {
    HashSet::from(["__proto__", "constructor", "prototype"])
}
