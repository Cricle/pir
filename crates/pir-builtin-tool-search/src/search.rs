//! Pure BM25/search implementation for the standalone tool-search builtin.

use serde_json::Value;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

/// Stop words dropped from queries and documents (upstream `STOP_WORDS`,
/// tool.ts:39-61: `without` is not a stop word there, `this` is).
const STOP_WORDS: [&str; 21] = [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "with",
];

/// Lowercase, split camelCase and non-alphanumeric runs, drop stop words,
/// naive-stem plurals (upstream `tokenize` / `stem`, tool.ts:73-96). Text in
/// any script is NFC-normalized and segmented with UAX #29 word boundaries,
/// so Chinese/Japanese/Thai queries and canonically equivalent accents match
/// (#11037).
pub fn tokenize(text: &str) -> Vec<String> {
    let normalized = if text.is_ascii() {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(text.nfc().collect())
    };
    let mut tokens = Vec::new();
    for raw in split_identifier_words(&normalized) {
        let lowered = raw.to_lowercase();
        if lowered.is_empty() || STOP_WORDS.contains(&lowered.as_str()) {
            continue;
        }
        // Upstream stems only ASCII words (`/^[a-z0-9]+$/`); stemming a
        // Unicode plural such as `cafés` would corrupt it.
        tokens.push(
            if lowered
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
            {
                stem(&lowered)
            } else {
                lowered
            },
        );
    }
    tokens
}

/// camelCase / snake_case / kebab-case aware word split (upstream
/// `tokenize`'s pipeline: `([a-z0-9])([A-Z])` -> `$1 $2`,
/// `([A-Z]+)([A-Z][a-z])` -> `$1 $2`, lowercase, then
/// `split(/[^\p{L}\p{M}\p{N}]+/)`).
fn split_identifier_words(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    // Pass 1: a lowercase letter or digit immediately followed by an
    // uppercase letter starts a new word (`readFile` -> `read File`).
    let mut first = String::with_capacity(text.len() + 4);
    for (index, character) in chars.iter().enumerate() {
        if index > 0
            && character.is_ascii_uppercase()
            && (chars[index - 1].is_ascii_lowercase() || chars[index - 1].is_ascii_digit())
        {
            first.push(' ');
        }
        first.push(*character);
    }
    // Pass 2: an uppercase run followed by `[A-Z][a-z]` splits before its
    // last uppercase letter (`HTTPServer` -> `HTTP Server`, `ABCd` -> `AB Cd`).
    let chars: Vec<char> = first.chars().collect();
    let mut second = String::with_capacity(first.len() + 4);
    for (index, character) in chars.iter().enumerate() {
        if index > 0
            && chars[index - 1].is_ascii_uppercase()
            && character.is_ascii_uppercase()
            && chars
                .get(index + 1)
                .is_some_and(|next| next.is_ascii_lowercase())
        {
            second.push(' ');
        }
        second.push(*character);
    }
    // Lowercase and replace non-letter/mark/digit runs with spaces. Scripts
    // without spaces are then split at word boundaries; pure-ASCII text
    // splits at its spaces, like upstream's fast path.
    let mut cleaned = String::with_capacity(second.len());
    for character in second.chars() {
        for lower in character.to_lowercase() {
            if lower.is_alphanumeric() || is_combining_mark(lower) {
                cleaned.push(lower);
            } else {
                cleaned.push(' ');
            }
        }
    }
    if cleaned.is_ascii() {
        cleaned.split_whitespace().map(str::to_owned).collect()
    } else {
        cleaned.unicode_words().map(str::to_owned).collect()
    }
}

/// Naive plural stem (upstream `stem`, tool.ts:64-69).
fn stem(token: &str) -> String {
    if token.len() > 4 && token.ends_with("ies") {
        return format!("{}y", &token[..token.len() - 3]);
    }
    if token.len() > 4
        && (token.ends_with("ches")
            || token.ends_with("shes")
            || token.ends_with("sses")
            || token.ends_with("xes")
            || token.ends_with("zes"))
    {
        return token[..token.len() - 2].to_owned();
    }
    if token.len() > 3 && token.ends_with('s') && !token.ends_with("ss") {
        return token[..token.len() - 1].to_owned();
    }
    token.to_owned()
}

/// Lowercase, with characters invalid in script identifiers as `_`, so
/// `dev-radius` matches `dev_radius` (upstream `normalizeName`,
/// tool.ts:118-123).
fn normalize_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '$' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// Whether `query` names the tool: its full name or a suffix after a `__`
/// separator, so `thread_read` and `app__thread_read` both name
/// `mcp__app__thread_read` (upstream `isNameQuery`, tool.ts:125-131).
fn is_name_query(query: &str, name: &str) -> bool {
    let normalized = normalize_name(name);
    normalized == query || normalized.ends_with(&format!("__{query}"))
}

/// A tool as the ranker sees it (upstream `ToolSearchDocument`): the name is
/// scored as its own field, `text` is the description plus the recursive
/// schema text, and `namespace_text` is the server-level text shared by every
/// tool of the namespace.
#[derive(Debug, Clone)]
pub struct Bm25Document {
    pub name: String,
    /// Description, schema descriptions, and property names.
    pub text: String,
    /// Name, description, and instructions of the tool's namespace.
    pub namespace_text: String,
}

impl Bm25Document {
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            text: text.into(),
            namespace_text: String::new(),
        }
    }

    pub fn with_namespace(mut self, namespace_text: impl Into<String>) -> Self {
        self.namespace_text = namespace_text.into();
        self
    }

    /// Build the document for one tool from its model-facing metadata.
    ///
    /// This is the adapter seam a host uses: pass the tool's name,
    /// description, JSON parameter schema, and (for grouped tools) its
    /// namespace. [`tool_document`] derives both searchable fields.
    pub fn from_tool(
        name: impl Into<String>,
        description: &str,
        parameters: Option<&Value>,
        namespace: Option<&ToolNamespace<'_>>,
    ) -> Self {
        let document = tool_document(description, parameters, namespace);
        Self::new(name, document.text).with_namespace(document.namespace_text)
    }
}

impl From<(String, String)> for Bm25Document {
    fn from((name, text): (String, String)) -> Self {
        Self::new(name, text)
    }
}

impl From<(String, String, String)> for Bm25Document {
    fn from((name, text, namespace_text): (String, String, String)) -> Self {
        Self::new(name, text).with_namespace(namespace_text)
    }
}

/// One document split into the fields the ranker weighs differently
/// (upstream `indexDocument`).
struct IndexedDocument {
    name: String,
    /// Terms of the name, e.g. `mcp`, `app`, `thread`, `read` for
    /// `mcp__app__thread_read`.
    name_terms: std::collections::BTreeSet<String>,
    /// Terms of the last `__` segment, the part models say: `thread`, `read`.
    local_name_terms: Vec<String>,
    text_counts: BTreeMap<String, usize>,
    text_length: usize,
    namespace_terms: std::collections::BTreeSet<String>,
}

fn index_document(document: Bm25Document) -> IndexedDocument {
    let mut text_counts: BTreeMap<String, usize> = BTreeMap::new();
    for term in tokenize(&document.text) {
        *text_counts.entry(term).or_default() += 1;
    }
    let text_length = text_counts.values().sum();
    let segments: Vec<&str> = document.name.split("__").collect();
    let local = segments.last().copied().unwrap_or(&document.name);
    IndexedDocument {
        name: document.name.clone(),
        name_terms: tokenize(&document.name).into_iter().collect(),
        local_name_terms: tokenize(local)
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect(),
        text_counts,
        text_length,
        namespace_terms: tokenize(&document.namespace_text).into_iter().collect(),
    }
}

/// A query term in the name counts this many times its IDF. The text part of
/// a term is at most `k1 + 1` (2.2) times its IDF and about 1 for a single
/// mention, so a name match beats any number of description mentions
/// (upstream `NAME_WEIGHT`, #11625).
const NAME_WEIGHT: f64 = 3.0;

/// A query term in the namespace text counts this many times its IDF. They
/// are shared by every tool of the server, so they help find the server but
/// not the tool in it (upstream `NAMESPACE_WEIGHT`, #11625).
const NAMESPACE_WEIGHT: f64 = 0.5;

/// Okapi BM25 over the description, with the name and namespace as separate
/// fields (upstream `Bm25Ranker`, tool.ts:133-260; k1=1.2, b=0.75):
///
/// - a tool matching more of the query's distinct terms (in any field) ranks
///   above one matching fewer, whatever the scores (#11632);
/// - a query term in the name outweighs description mentions (#11625);
/// - a tool whose whole local name is in the query gets a bonus worth one
///   more match of every query term;
/// - a query that is exactly a tool's name ranks that tool first (#11331),
///   before coverage.
///
/// The rank order is a documented total key: name query, coverage, score,
/// then tool name ascending ([`Self::rank`]).
pub struct Bm25Ranker {
    documents: Vec<IndexedDocument>,
    document_frequency: BTreeMap<String, usize>,
    average_length: f64,
}

impl Bm25Ranker {
    pub fn new<D: Into<Bm25Document>>(documents: Vec<D>) -> Self {
        let documents: Vec<IndexedDocument> = documents
            .into_iter()
            .map(|document| index_document(document.into()))
            .collect();
        let mut document_frequency: BTreeMap<String, usize> = BTreeMap::new();
        for document in &documents {
            let mut terms: std::collections::BTreeSet<&String> =
                document.name_terms.iter().collect();
            terms.extend(document.text_counts.keys());
            terms.extend(document.namespace_terms.iter());
            for term in terms {
                *document_frequency.entry(term.clone()).or_default() += 1;
            }
        }
        let average_length = if documents.is_empty() {
            0.0
        } else {
            documents
                .iter()
                .map(|document| document.text_length)
                .sum::<usize>() as f64
                / documents.len() as f64
        };
        Self {
            documents,
            document_frequency,
            average_length,
        }
    }

    /// Ranked ids, truncated to `limit` (upstream `rank`).
    pub fn rank(&self, query: &str, limit: usize) -> Vec<(String, f64)> {
        let terms: Vec<String> = {
            let mut seen = std::collections::BTreeSet::new();
            tokenize(query)
                .into_iter()
                .filter(|token| seen.insert(token.clone()))
                .collect()
        };
        if terms.is_empty() || self.documents.is_empty() || limit == 0 {
            return Vec::new();
        }
        let count = self.documents.len() as f64;
        let k1 = 1.2_f64;
        let b = 0.75_f64;
        let idf: BTreeMap<&str, f64> = terms
            .iter()
            .map(|term| {
                let frequency = self.document_frequency.get(term).copied().unwrap_or(0) as f64;
                (
                    term.as_str(),
                    (1.0 + (count - frequency + 0.5) / (frequency + 0.5)).ln(),
                )
            })
            .collect();
        let full_name_bonus: f64 = idf.values().sum();
        let name_query = normalize_name(query);
        let average_length = self.average_length.max(1.0);
        // (name, score, covered, exact-name query)
        let mut scored: Vec<(String, f64, usize, bool)> = Vec::new();
        for document in &self.documents {
            let norm = k1 * (1.0 - b + b * document.text_length as f64 / average_length);
            let mut score = 0.0;
            let mut covered = 0usize;
            for term in &terms {
                let frequency = document.text_counts.get(term).copied().unwrap_or(0) as f64;
                let weight = f64::from(document.name_terms.contains(term)) * NAME_WEIGHT
                    + f64::from(document.namespace_terms.contains(term)) * NAMESPACE_WEIGHT
                    + (frequency * (k1 + 1.0)) / (frequency + norm);
                if weight > 0.0 {
                    covered += 1;
                }
                score += idf.get(term.as_str()).copied().unwrap_or(0.0) * weight;
            }
            if score == 0.0 {
                continue;
            }
            if !document.local_name_terms.is_empty()
                && document
                    .local_name_terms
                    .iter()
                    .all(|term| terms.contains(term))
            {
                score += full_name_bonus;
            }
            scored.push((
                document.name.clone(),
                score,
                covered,
                is_name_query(&name_query, &document.name),
            ));
        }
        // Deterministic ordering (upstream's sort is by `exact`, coverage and
        // score and is stable, so ties keep its registration-ordered document
        // list; pir's candidate source is a HashMap-backed registry, whose
        // iteration order varies per run). The documented total key is:
        //
        //   primary:   query names the tool, first
        //   secondary: coverage, descending (more query terms matched)
        //   tertiary:  score, descending
        //   tie-break: tool name, ascending (byte order)
        //
        // `sort_by` is a stable sort, and names are unique in the merged
        // registry, so the result is independent of document/input order —
        // never of hashing.
        scored.sort_by(
            |(id_a, score_a, covered_a, exact_a), (id_b, score_b, covered_b, exact_b)| {
                exact_b
                    .cmp(exact_a)
                    .then_with(|| covered_b.cmp(covered_a))
                    .then_with(|| {
                        score_b
                            .partial_cmp(score_a)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .then_with(|| id_a.cmp(id_b))
            },
        );
        scored.truncate(limit);
        scored
            .into_iter()
            .map(|(id, score, _, _)| (id, score))
            .collect()
    }
}

/// Borrowed namespace metadata shared by every tool of a namespace.
///
/// The algorithm only reads these fields, so the crate takes borrowed
/// strings and stays independent of any host/engine tool type. The host
/// adapter maps its own namespace record onto this view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolNamespace<'a> {
    /// Namespace identifier, for example `mcp__docs`.
    pub name: &'a str,
    /// Short summary shared by the namespace's tools.
    pub description: Option<&'a str>,
    /// Longer usage guidance shared by the namespace's tools.
    pub instructions: Option<&'a str>,
}

impl<'a> ToolNamespace<'a> {
    pub fn new(name: &'a str) -> Self {
        Self {
            name,
            description: None,
            instructions: None,
        }
    }

    pub fn with_description(mut self, description: &'a str) -> Self {
        self.description = Some(description);
        self
    }

    pub fn with_instructions(mut self, instructions: &'a str) -> Self {
        self.instructions = Some(instructions);
        self
    }
}

/// The two searchable fields derived from one tool.
///
/// `text` is the description plus the recursive schema text; namespace text
/// is the server-level text shared by every tool of the namespace and is
/// scored as its own, lower-weighted field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolDocumentText {
    pub text: String,
    pub namespace_text: String,
}

/// The searchable text and namespace text (upstream
/// `createToolSearchDocument`, tool.ts:103-116): the description and the
/// recursive schema text are scored separately from the namespace's
/// name/description/instructions.
pub fn tool_document(
    description: &str,
    parameters: Option<&Value>,
    namespace: Option<&ToolNamespace<'_>>,
) -> ToolDocumentText {
    let mut text = description.to_owned();
    if let Some(schema) = parameters {
        let mut seen = HashSet::from([schema as *const Value as usize]);
        append_schema_text(schema, schema, &mut text, &mut seen);
    }
    let mut namespace_text = String::new();
    if let Some(namespace) = namespace {
        for part in [
            Some(namespace.name),
            namespace.description,
            namespace.instructions,
        ]
        .into_iter()
        .flatten()
        {
            if !part.trim().is_empty() {
                if !namespace_text.is_empty() {
                    namespace_text.push(' ');
                }
                namespace_text.push_str(part);
            }
        }
    }
    ToolDocumentText {
        text,
        namespace_text,
    }
}

/// Recursive schema text (upstream `schemaText`, tool.ts:86-101): only object
/// schemas contribute — a description, property names plus their subtrees, an
/// object-valued `items`, and the `anyOf`/`oneOf`/`allOf` variant arrays.
/// Array-valued `items` and non-object variants contribute nothing, exactly
/// like upstream's `isObject` guard.
///
/// Local `$ref`s are followed (upstream #11850): MCP/Pydantic tools put
/// nested models in `$defs`, and without this their descriptions are not
/// searchable. Each referenced subschema is read once, so recursive and
/// shared definitions neither loop nor inflate the BM25 term counts;
/// unreferenced `$defs` and remote refs stay out.
fn append_schema_text(
    schema: &Value,
    root: &Value,
    document: &mut String,
    seen: &mut HashSet<usize>,
) {
    let Value::Object(map) = schema else {
        return;
    };
    if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
        if let Some(target) = resolve_local_schema_ref(root, reference) {
            let key = target as *const Value as usize;
            if seen.insert(key) {
                append_schema_text(target, root, document, seen);
            }
        }
    }
    if let Some(description) = map.get("description").and_then(Value::as_str) {
        document.push(' ');
        document.push_str(description);
    }
    if let Some(properties) = map.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            document.push(' ');
            document.push_str(name);
            append_schema_text(property, root, document, seen);
        }
    }
    if let Some(items) = map.get("items") {
        append_schema_text(items, root, document, seen);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(variants) = map.get(key).and_then(Value::as_array) {
            for variant in variants {
                append_schema_text(variant, root, document, seen);
            }
        }
    }
}

/// Resolve one local JSON Pointer (`#/$defs/x`) against `root` (upstream
/// `resolveSchemaRef`): `~1`/`~0` escapes, array indices, and percent
/// decoding; a remote ref, a missing target, or a malformed escape resolves
/// to nothing.
fn resolve_local_schema_ref<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    if pointer.is_empty() {
        return Some(root);
    }
    let pointer = pointer.strip_prefix('/')?;
    let mut current = root;
    for segment in pointer.split('/') {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        let segment = percent_decode(&segment)?;
        current = match current {
            Value::Object(map) => map.get(&segment)?,
            Value::Array(array) => array.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

/// Decode `%xx` escapes in a JSON Pointer segment; invalid UTF-8 or an
/// incomplete escape resolves to nothing instead of throwing.
fn percent_decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = segment.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The searchable text for one tool schema, without namespace metadata.
///
/// Convenience wrapper over [`tool_document`] for callers that need only the
/// description-plus-schema text.
pub fn tool_search_document_text(description: &str, parameters: &Value) -> String {
    tool_document(description, Some(parameters), None).text
}
