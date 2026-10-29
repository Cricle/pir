//! Revision diffing: the port of upstream `packages/chord/src/delta/diff.ts`,
//! plus `overlap` from `delta/index.ts`.
//!
//! Every function mirrors the upstream function of the same name and keeps its
//! control flow, so the port can be read side by side with the TypeScript. Two
//! mechanical translations are unavoidable:
//!
//! * upstream's `WeakSet<Op[]>` overflow marker becomes the per-call
//!   `Emitter::overflowed` flag, which is exactly the lifetime of one
//!   `diffRevisions` call;
//! * upstream's identity comparisons (`===`, and the `Map` keys of
//!   `identityAnchors`/`permutation`) are object *reference* identity; a
//!   [`Value`](serde_json::Value) is an owned tree with no shared identity, so
//!   deep equality stands in for it. Matching by value can only merge values
//!   upstream would keep apart, and every merged pair is interchangeable in the
//!   output, so the emitted batch still reproduces `after` exactly.
//!
//! Counts (`t` truncation, cost estimates) are Unicode scalar values, matching
//! [`crate::durable::delta::apply`], not upstream's UTF-16 code units. The two
//! differ only for characters outside the BMP.

use crate::durable::delta::{Op, Seg};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

const DEFAULT_OVERLAP_SCAN: usize = 65_536;
const MAX_DELTA_OPERATIONS: usize = 4_096;
const MAX_IDENTITY_CANDIDATES: usize = 200_000;
const MAX_SEMANTIC_CELLS: usize = 65_536;
const OVERLAP_PROBE: usize = 64;
const OVERLAP_MAX_CANDIDATES: usize = 8;
const DELTA_COST_LIMIT: usize = 65_536;
const SNAPSHOT_OVERHEAD: usize = 6;

// ─── Emission ────────────────────────────────────────────────────────────────

/// The per-call batch and its overflow flag (upstream's `operations` array and
/// the `overflowedBatches` WeakSet entry keyed by it).
struct Emitter {
    operations: Vec<Op>,
    overflowed: bool,
}

impl Emitter {
    fn new() -> Self {
        Self {
            operations: Vec::new(),
            overflowed: false,
        }
    }

    /// `emitOperation`: an overflowed batch stops growing; the operation that
    /// crosses the limit is dropped and only marks the batch.
    fn emit(&mut self, operation: Op) {
        if self.overflowed {
            return;
        }
        if self.operations.len() >= MAX_DELTA_OPERATIONS {
            self.overflowed = true;
            return;
        }
        self.operations.push(operation);
    }
}

/// `emitSet`: the root can only be replaced; every other path is a set.
fn emit_set(path: &[Seg], value: &Value, out: &mut Emitter) {
    if path.is_empty() {
        out.emit(Op::Replace(value.clone()));
    } else {
        out.emit(Op::Set(path.to_vec(), value.clone()));
    }
}

// ─── Equality ────────────────────────────────────────────────────────────────

fn is_container(value: &Value) -> bool {
    matches!(value, Value::Array(_) | Value::Object(_))
}

/// `equalJson`: order-insensitive object comparison, element-wise arrays.
fn equal_json(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Array(left), Value::Array(right)) => equal_array(left, right),
        (Value::Object(left), Value::Object(right)) => equal_object(left, right),
        _ => left == right,
    }
}

fn equal_array(left: &[Value], right: &[Value]) -> bool {
    left.len() == right.len() && left.iter().zip(right).all(|(a, b)| equal_json(a, b))
}

fn equal_object(left: &Map<String, Value>, right: &Map<String, Value>) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .all(|(key, value)| right.get(key).is_some_and(|other| equal_json(value, other)))
}

/// `sameValue`: strict identity or deep equality. There is no reference
/// identity in an owned JSON tree, so this is deep equality.
fn same_value(left: &Value, right: &Value) -> bool {
    equal_json(left, right)
}

// ─── Structural keys ─────────────────────────────────────────────────────────

/// Hash a value structurally. Object keys are combined order-independently to
/// match [`equal_json`]; collisions are resolved by comparing the values.
fn hash_value(value: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    hash_into(value, &mut hasher);
    hasher.finish()
}

fn hash_into<H: Hasher>(value: &Value, state: &mut H) {
    match value {
        Value::Null => state.write_u8(0),
        Value::Bool(flag) => {
            state.write_u8(1);
            state.write_u8(u8::from(*flag));
        }
        Value::Number(number) => {
            state.write_u8(2);
            number.to_string().hash(state);
        }
        Value::String(text) => {
            state.write_u8(3);
            text.hash(state);
        }
        Value::Array(items) => {
            state.write_u8(4);
            state.write_usize(items.len());
            for item in items {
                hash_into(item, state);
            }
        }
        Value::Object(map) => {
            state.write_u8(5);
            state.write_usize(map.len());
            let mut combined = 0u64;
            for (key, item) in map {
                let mut child = DefaultHasher::new();
                key.hash(&mut child);
                hash_into(item, &mut child);
                combined = combined.wrapping_add(child.finish());
            }
            state.write_u64(combined);
        }
    }
}

/// Value → indices, the upstream `Map<JsonValue, number[]>` used by the
/// identity matchers. Groups are found by [`equal_json`], so a value group's
/// indices are in ascending order exactly as upstream's are.
struct Positions<'a> {
    values: Vec<&'a Value>,
    groups: Vec<Vec<usize>>,
    buckets: HashMap<u64, Vec<usize>>,
}

impl<'a> Positions<'a> {
    fn new(values: &'a [Value], start: usize, end: usize) -> Self {
        let mut positions = Self {
            values: Vec::new(),
            groups: Vec::new(),
            buckets: HashMap::new(),
        };
        for (index, value) in values.iter().enumerate().skip(start).take(end - start) {
            positions.insert(index, value);
        }
        positions
    }

    fn insert(&mut self, index: usize, value: &'a Value) {
        if let Some(id) = self.find_id(value) {
            self.groups[id].push(index);
            return;
        }
        let id = self.groups.len();
        self.values.push(value);
        self.groups.push(vec![index]);
        self.buckets.entry(hash_value(value)).or_default().push(id);
    }

    fn find_id(&self, value: &Value) -> Option<usize> {
        let ids = self.buckets.get(&hash_value(value))?;
        ids.iter()
            .copied()
            .find(|&id| equal_json(self.values[id], value))
    }

    fn get(&self, value: &Value) -> Option<&[usize]> {
        self.find_id(value).map(|id| self.groups[id].as_slice())
    }
}

// ─── Overlap ─────────────────────────────────────────────────────────────────

/// Longest suffix of `a` that is a prefix of `b`, scanning at most `scan`
/// trailing characters of `a`.
///
/// Upstream probes with a long head first (few candidates, and it catches the
/// large overlaps a rolling window produces), then falls back to one character,
/// which finds any overlap at the cost of more candidates. Candidates are
/// bounded because repetitive output makes a long head match thousands of
/// times; giving up returns 0, which emits a set: larger, never wrong.
pub fn overlap(a: &str, b: &str, scan: usize) -> usize {
    overlap_with(a, b, scan, OVERLAP_PROBE, OVERLAP_MAX_CANDIDATES)
}

/// String lengths and slices in this module are **UTF-16 code units**, exactly
/// like the TypeScript originals (`String.prototype.length`, `.slice()`).
///
/// This is not cosmetic. The persisted `["t", path, count]` operation means
/// "drop `count` UTF-16 units", and the applier
/// ([`crate::durable::delta::apply`]) counts the same way; a differ that
/// emitted Unicode scalar counts would replay a log at the wrong offset for any
/// non-BMP text (emoji in shell output is common). [`String::from_utf16_lossy`]
/// is the Rust equivalent of slicing a surrogate pair apart: a lone surrogate
/// becomes U+FFFD, which is also what a lossy decode of the same `slice` gives.
fn utf16_units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn utf16_slice_from(s: &str, from: usize) -> String {
    String::from_utf16_lossy(&s.encode_utf16().skip(from).collect::<Vec<u16>>())
}

fn overlap_with(a: &str, b: &str, scan: usize, probe: usize, max_candidates: usize) -> usize {
    if a.is_empty() || b.is_empty() || scan == 0 {
        return 0;
    }
    let a_units = utf16_units(a);
    let b_units = utf16_units(b);
    let tail = &a_units[a_units.len().saturating_sub(scan)..];

    for h in [probe.min(b_units.len()), 1] {
        let head = &b_units[..h];
        let mut tried = 0usize;
        let mut from = 0usize;
        while let Some(k) = find_units(tail, head, from) {
            from = k + 1;
            tried += 1;
            if tried > max_candidates {
                break;
            }
            let n = tail.len() - k;
            if n <= b_units.len() && tail[k..] == b_units[..n] {
                return n;
            }
        }
        if h == 1 {
            break;
        }
    }
    0
}

fn find_units(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let end = haystack.len() - needle.len();
    (from..=end).find(|&index| &haystack[index..index + needle.len()] == needle)
}

// ─── Strings ─────────────────────────────────────────────────────────────────

fn emit_string(before: &str, after: &str, path: &[Seg], out: &mut Emitter) {
    if before == after {
        return;
    }
    let before_len = utf16_len(before);
    let after_len = utf16_len(after);
    if after_len > before_len && after.starts_with(before) {
        out.emit(Op::Append(
            path.to_vec(),
            utf16_slice_from(after, before_len),
        ));
        return;
    }
    let shared = overlap(before, after, DEFAULT_OVERLAP_SCAN);
    if shared == 0 {
        out.emit(Op::Set(path.to_vec(), Value::String(after.to_owned())));
        return;
    }
    out.emit(Op::Truncate(path.to_vec(), before_len - shared));
    if after_len > shared {
        out.emit(Op::Append(path.to_vec(), utf16_slice_from(after, shared)));
    }
}

// ─── Arrays ──────────────────────────────────────────────────────────────────

/// `permutation`: a reordering of the same multiset of values. Upstream keys
/// its map by identity; see the module docs for the deep-equality stand-in.
fn permutation(before: &[Value], after: &[Value]) -> Option<Vec<usize>> {
    if before.len() != after.len() {
        return None;
    }
    let positions = Positions::new(before, 0, before.len());
    let mut used = vec![0usize; positions.groups.len()];
    let mut result = Vec::with_capacity(after.len());
    for value in after {
        let id = positions.find_id(value)?;
        let group = &positions.groups[id];
        if used[id] == group.len() {
            return None;
        }
        result.push(group[used[id]]);
        used[id] += 1;
    }
    Some(result)
}

fn lower_bound(values: &[usize], value: usize) -> usize {
    values.partition_point(|&candidate| candidate < value)
}

/// The LCS table is filled bottom-up by index; iterating the indices is the
/// algorithm, not a missed iterator.
#[allow(clippy::needless_range_loop)]
fn lcs_matches(
    before: &[Value],
    after: &[Value],
    equal: fn(&Value, &Value) -> bool,
    max_cells: usize,
) -> Option<Vec<(usize, usize)>> {
    if before.is_empty() || after.is_empty() {
        return Some(Vec::new());
    }
    if before.len().saturating_mul(after.len()) > max_cells {
        return None;
    }
    let width = after.len() + 1;
    let mut lengths = vec![0u32; (before.len() + 1) * width];
    for left in (0..before.len()).rev() {
        for right in (0..after.len()).rev() {
            let at = left * width + right;
            lengths[at] = if equal(&before[left], &after[right]) {
                lengths[(left + 1) * width + right + 1] + 1
            } else {
                lengths[(left + 1) * width + right].max(lengths[left * width + right + 1])
            };
        }
    }
    let mut matches = Vec::new();
    let (mut left, mut right) = (0usize, 0usize);
    while left < before.len() && right < after.len() {
        if equal(&before[left], &after[right])
            && lengths[left * width + right] == lengths[(left + 1) * width + right + 1] + 1
        {
            matches.push((left, right));
            left += 1;
            right += 1;
        } else if lengths[(left + 1) * width + right] >= lengths[left * width + right + 1] {
            left += 1;
        } else {
            right += 1;
        }
    }
    Some(matches)
}

fn semantically_aligned(left: &Value, right: &Value) -> bool {
    if same_value(left, right) {
        return true;
    }
    match (left, right) {
        (Value::Array(left), Value::Array(right)) => {
            if left.len() != right.len() {
                return false;
            }
            left.iter()
                .zip(right)
                .any(|(value, counterpart)| is_container(value) && same_value(value, counterpart))
        }
        (Value::Object(left), Value::Object(right)) => left.iter().any(|(key, value)| {
            is_container(value)
                && right
                    .get(key)
                    .is_some_and(|counterpart| same_value(value, counterpart))
        }),
        _ => false,
    }
}

/// The matcher walks two index ranges at once, so index loops are the clearest
/// form here.
#[allow(clippy::needless_range_loop)]
fn identity_subsequence(
    before: &[Value],
    before_start: usize,
    before_end: usize,
    after: &[Value],
    after_start: usize,
    after_end: usize,
) -> Option<Vec<(usize, usize)>> {
    let before_count = before_end - before_start;
    let after_count = after_end - after_start;
    let mut matches = Vec::new();
    if after_count < before_count {
        let mut before_index = before_start;
        for after_index in after_start..after_end {
            while before_index < before_end
                && !same_value(&before[before_index], &after[after_index])
            {
                before_index += 1;
            }
            if before_index == before_end {
                return None;
            }
            matches.push((before_index, after_index));
            before_index += 1;
        }
        return Some(matches);
    }
    if before_count < after_count {
        let mut after_index = after_start;
        for before_index in before_start..before_end {
            while after_index < after_end && !same_value(&before[before_index], &after[after_index])
            {
                after_index += 1;
            }
            if after_index == after_end {
                return None;
            }
            matches.push((before_index, after_index));
            after_index += 1;
        }
        return Some(matches);
    }
    None
}

fn greedy_identity_anchors(
    positions: &Positions<'_>,
    after: &[Value],
    after_start: usize,
    after_end: usize,
) -> Vec<(usize, usize)> {
    let mut matches = Vec::new();
    let mut previous: Option<usize> = None;
    for (offset, value) in after[after_start..after_end].iter().enumerate() {
        let after_index = after_start + offset;
        let Some(candidates) = positions.get(value) else {
            continue;
        };
        let floor = previous.map_or(0, |index| index + 1);
        let at = lower_bound(candidates, floor);
        let Some(&before_index) = candidates.get(at) else {
            continue;
        };
        matches.push((before_index, after_index));
        previous = Some(before_index);
    }
    matches
}

fn identity_anchors(
    before: &[Value],
    before_start: usize,
    before_end: usize,
    after: &[Value],
    after_start: usize,
    after_end: usize,
) -> Vec<(usize, usize)> {
    let positions = Positions::new(before, before_start, before_end);
    let mut candidate_count = 0usize;
    for value in &after[after_start..after_end] {
        candidate_count += positions.get(value).map_or(0, |group| group.len());
        if candidate_count > MAX_IDENTITY_CANDIDATES {
            return greedy_identity_anchors(&positions, after, after_start, after_end);
        }
    }
    if candidate_count == 0 {
        return Vec::new();
    }

    struct Candidate {
        before: usize,
        after: usize,
        previous: Option<usize>,
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    // `tails` stores candidate indices, `tail_values` the before index each
    // tail ends at; both stay sorted, mirroring upstream's parallel arrays.
    let mut tails: Vec<usize> = Vec::new();
    let mut tail_values: Vec<usize> = Vec::new();
    for (offset, value) in after[after_start..after_end].iter().enumerate() {
        let after_index = after_start + offset;
        let Some(before_positions) = positions.get(value) else {
            continue;
        };
        for &before_index in before_positions.iter().rev() {
            let at = lower_bound(&tail_values, before_index);
            let candidate_index = candidates.len();
            let previous = if at == 0 { None } else { Some(tails[at - 1]) };
            candidates.push(Candidate {
                before: before_index,
                after: after_index,
                previous,
            });
            if at == tails.len() {
                tails.push(candidate_index);
                tail_values.push(before_index);
            } else {
                tails[at] = candidate_index;
                tail_values[at] = before_index;
            }
        }
    }

    let mut matches = Vec::new();
    let mut candidate_index = tails.last().copied();
    while let Some(index) = candidate_index {
        let candidate = &candidates[index];
        matches.push((candidate.before, candidate.after));
        candidate_index = candidate.previous;
    }
    matches.reverse();
    matches
}

#[allow(clippy::too_many_arguments)]
fn process_array_matches(
    before: &[Value],
    after: &[Value],
    path: &mut Vec<Seg>,
    out: &mut Emitter,
    before_start: usize,
    before_end: usize,
    after_start: usize,
    after_end: usize,
    output_start: usize,
    matches: &[(usize, usize)],
) {
    let mut before_at = before_start;
    let mut after_at = after_start;
    let mut output_at = output_start;
    for &(before_match, after_match) in matches {
        if out.overflowed {
            return;
        }
        diff_array_region(
            before,
            after,
            path,
            out,
            before_at,
            before_match,
            after_at,
            after_match,
            output_at,
        );
        output_at += after_match - after_at;
        if !same_value(&before[before_match], &after[after_match]) {
            path.push(Seg::Index(output_at));
            diff_value(&before[before_match], &after[after_match], path, out);
            path.pop();
        }
        output_at += 1;
        before_at = before_match + 1;
        after_at = after_match + 1;
    }
    if !out.overflowed {
        diff_array_region(
            before, after, path, out, before_at, before_end, after_at, after_end, output_at,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn diff_array_region(
    before: &[Value],
    after: &[Value],
    path: &mut Vec<Seg>,
    out: &mut Emitter,
    mut before_start: usize,
    mut before_end: usize,
    mut after_start: usize,
    mut after_end: usize,
    mut output_start: usize,
) {
    if out.overflowed {
        return;
    }
    while before_start < before_end
        && after_start < after_end
        && same_value(&before[before_start], &after[after_start])
    {
        before_start += 1;
        after_start += 1;
        output_start += 1;
    }
    while before_start < before_end
        && after_start < after_end
        && same_value(&before[before_end - 1], &after[after_end - 1])
    {
        before_end -= 1;
        after_end -= 1;
    }
    let before_count = before_end - before_start;
    let after_count = after_end - after_start;
    if before_count == 0 && after_count == 0 {
        return;
    }
    if before_count == 0 || after_count == 0 {
        out.emit(Op::Splice {
            path: path.clone(),
            index: output_start,
            remove: before_count,
            items: after[after_start..after_end].to_vec(),
        });
        return;
    }

    if before_count == after_count {
        let positional: Vec<(usize, usize)> = (0..before_count)
            .filter(|&offset| {
                same_value(&before[before_start + offset], &after[after_start + offset])
            })
            .map(|offset| (before_start + offset, after_start + offset))
            .collect();
        if !positional.is_empty() {
            process_array_matches(
                before,
                after,
                path,
                out,
                before_start,
                before_end,
                after_start,
                after_end,
                output_start,
                &positional,
            );
            return;
        }
    }

    if let Some(subsequence) = identity_subsequence(
        before,
        before_start,
        before_end,
        after,
        after_start,
        after_end,
    )
    .filter(|matches| !matches.is_empty())
    {
        process_array_matches(
            before,
            after,
            path,
            out,
            before_start,
            before_end,
            after_start,
            after_end,
            output_start,
            &subsequence,
        );
        return;
    }

    let identity = identity_anchors(
        before,
        before_start,
        before_end,
        after,
        after_start,
        after_end,
    );
    if !identity.is_empty() {
        process_array_matches(
            before,
            after,
            path,
            out,
            before_start,
            before_end,
            after_start,
            after_end,
            output_start,
            &identity,
        );
        return;
    }

    let semantic = lcs_matches(
        &before[before_start..before_end],
        &after[after_start..after_end],
        semantically_aligned,
        MAX_SEMANTIC_CELLS,
    )
    .filter(|matches| !matches.is_empty());
    if let Some(semantic) = semantic {
        let absolute: Vec<(usize, usize)> = semantic
            .iter()
            .map(|&(before_index, after_index)| {
                (before_start + before_index, after_start + after_index)
            })
            .collect();
        process_array_matches(
            before,
            after,
            path,
            out,
            before_start,
            before_end,
            after_start,
            after_end,
            output_start,
            &absolute,
        );
        return;
    }

    if before_count == 1 && after_count == 1 {
        path.push(Seg::Index(output_start));
        diff_value(&before[before_start], &after[after_start], path, out);
        path.pop();
        return;
    }
    out.emit(Op::Splice {
        path: path.clone(),
        index: output_start,
        remove: before_count,
        items: after[after_start..after_end].to_vec(),
    });
}

fn diff_array(before: &[Value], after: &[Value], path: &mut Vec<Seg>, out: &mut Emitter) {
    if equal_array(before, after) {
        return;
    }
    if before.len() == after.len()
        && before.len() > 1
        && !same_value(&before[0], &after[0])
        && !same_value(&before[before.len() - 1], &after[after.len() - 1])
    {
        if let Some(permutation) = permutation(before, after) {
            out.emit(Op::Move {
                path: path.clone(),
                permutation,
            });
            return;
        }
    }
    diff_array_region(before, after, path, out, 0, before.len(), 0, after.len(), 0);
}

// ─── Objects ─────────────────────────────────────────────────────────────────

fn is_reserved_segment(key: &str) -> bool {
    // Mirrors `delta::reserved_segments`; kept local because this check runs
    // once per object key and building a HashSet per object diff would dwarf it.
    matches!(key, "__proto__" | "constructor" | "prototype")
}

fn diff_object(
    before: &Map<String, Value>,
    after: &Map<String, Value>,
    path: &mut Vec<Seg>,
    out: &mut Emitter,
) {
    if before
        .keys()
        .chain(after.keys())
        .any(|key| is_reserved_segment(key.as_str()))
    {
        if !equal_object(before, after) {
            emit_set(path, &Value::Object(after.clone()), out);
        }
        return;
    }
    for (key, after_value) in after {
        if out.overflowed {
            return;
        }
        path.push(Seg::Key(key.clone()));
        match before.get(key.as_str()) {
            Some(before_value) => diff_value(before_value, after_value, path, out),
            None => emit_set(path, after_value, out),
        }
        path.pop();
    }
    for key in before.keys() {
        if out.overflowed {
            return;
        }
        if !after.contains_key(key.as_str()) {
            path.push(Seg::Key(key.clone()));
            out.emit(Op::Delete(path.clone()));
            path.pop();
        }
    }
}

// ─── Values ──────────────────────────────────────────────────────────────────

fn diff_value(before: &Value, after: &Value, path: &mut Vec<Seg>, out: &mut Emitter) {
    if equal_json(before, after) || out.overflowed {
        return;
    }
    match (before, after) {
        (Value::String(before_text), Value::String(after_text)) if !path.is_empty() => {
            emit_string(before_text, after_text, path, out);
        }
        (Value::Array(before_items), Value::Array(after_items)) => {
            diff_array(before_items, after_items, path, out);
        }
        (Value::Object(before_map), Value::Object(after_map)) => {
            diff_object(before_map, after_map, path, out);
        }
        _ => emit_set(path, after, out),
    }
}

// ─── Cost model ──────────────────────────────────────────────────────────────

fn json_cost(value: &Value) -> usize {
    match value {
        // Cost is an estimate the differ only compares against itself; UTF-16
        // units keep it on the same scale as the operation costs it is weighed
        // against (upstream `string.length`).
        Value::Null => 4,
        Value::String(text) => utf16_len(text) + 2,
        Value::Number(number) => number.to_string().len(),
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Array(items) => array_cost(items.iter()),
        Value::Object(map) => {
            let mut cost = 2;
            for (index, (key, value)) in map.iter().enumerate() {
                cost += utf16_len(key) + 3 + json_cost(value) + usize::from(index != 0);
            }
            cost
        }
    }
}

fn array_cost<'a>(items: impl Iterator<Item = &'a Value>) -> usize {
    let mut cost = 2;
    for (index, item) in items.enumerate() {
        cost += json_cost(item) + usize::from(index != 0);
    }
    cost
}
fn path_cost(path: &[Seg]) -> usize {
    let mut cost = 2;
    for (index, segment) in path.iter().enumerate() {
        let segment_cost = match segment {
            Seg::Key(key) => utf16_len(key) + 2,
            Seg::Index(value) => value.to_string().len(),
        };
        cost += segment_cost + usize::from(index != 0);
    }
    cost
}

fn permutation_cost(permutation: &[usize]) -> usize {
    let mut cost = 2;
    for (index, value) in permutation.iter().enumerate() {
        cost += value.to_string().len() + usize::from(index != 0);
    }
    cost
}

fn operation_cost(operation: &Op) -> usize {
    match operation {
        Op::Replace(value) => 6 + json_cost(value),
        Op::Set(path, value) => 7 + path_cost(path) + json_cost(value),
        Op::Delete(path) => 6 + path_cost(path),
        Op::Append(path, text) => 7 + path_cost(path) + utf16_len(text) + 2,
        Op::Truncate(path, count) => 7 + path_cost(path) + count.to_string().len(),
        Op::Splice {
            path,
            index,
            remove,
            items,
        } => {
            10 + path_cost(path)
                + index.to_string().len()
                + remove.to_string().len()
                + array_cost(items.iter())
        }
        Op::Move { path, permutation } => 7 + path_cost(path) + permutation_cost(permutation),
    }
}

/// Compute a compact operation batch from two immutable JSON revisions.
///
/// The result replays `before` into `after` under
/// [`crate::durable::delta::apply`].
pub fn diff_revisions(before: &Value, after: &Value) -> Vec<Op> {
    let mut out = Emitter::new();
    let mut path = Vec::new();
    diff_value(before, after, &mut path, &mut out);
    finish(out, || after.clone())
}

/// [`diff_revisions`] for two objects that are already maps.
///
/// A durable document value is a `Map<String, Value>` everywhere it is held
/// (the loaded baseline, the staged draft), so wrapping both sides in a
/// `Value::Object` first would deep-clone the whole document twice per diff —
/// O(state) on a path that must be O(delta). This entry point borrows the maps
/// directly and only copies when the batch collapses to a root replacement.
pub fn diff_objects(before: &Map<String, Value>, after: &Map<String, Value>) -> Vec<Op> {
    let mut out = Emitter::new();
    let mut path = Vec::new();
    diff_object(before, after, &mut path, &mut out);
    finish(out, || Value::Object(after.clone()))
}

/// Apply the overflow/cost rules to an emitted batch.
///
/// `snapshot` builds the whole `after` revision lazily: it is only called when
/// the batch collapses to (or overflows into) a root replacement.
fn finish(out: Emitter, snapshot: impl FnOnce() -> Value) -> Vec<Op> {
    if out.overflowed {
        return vec![Op::Replace(snapshot())];
    }
    let operations = out.operations;
    if operations.is_empty() || matches!(operations.first(), Some(Op::Replace(_))) {
        return operations;
    }
    let mut delta_cost = 2;
    for operation in &operations {
        delta_cost += operation_cost(operation) + 1;
    }
    if delta_cost < DELTA_COST_LIMIT {
        return operations;
    }
    let after = snapshot();
    let snapshot_cost = json_cost(&after) + SNAPSHOT_OVERHEAD;
    if delta_cost >= snapshot_cost {
        vec![Op::Replace(after)]
    } else {
        operations
    }
}
