// Integration tests (kept out of src/).

use pir_builtin_tool_search::{
    tokenize, tool_document, tool_search_document_text, Bm25Document, Bm25Ranker, ToolNamespace,
};
use serde_json::json;

#[test]
fn tokenize_splits_identifiers_and_stems_ascii_plurals() {
    assert_eq!(tokenize("readFile"), vec!["read", "file"]);
    assert_eq!(tokenize("HTTPServer"), vec!["http", "server"]);
    assert_eq!(
        tokenize("mcp__app__thread_read"),
        vec!["mcp", "app", "thread", "read"]
    );
    assert_eq!(tokenize("dev-tool"), vec!["dev", "tool"]);
    // Stop words disappear, including `this` (upstream keeps it in
    // STOP_WORDS) and `without` is not one of them.
    assert_eq!(tokenize("the this and of"), Vec::<String>::new());
    assert_eq!(tokenize("without"), vec!["without"]);
    // Only ASCII words get the naive plural stem.
    assert_eq!(tokenize("queries"), vec!["query"]);
    assert_eq!(tokenize("boxes"), vec!["box"]);
    assert_eq!(tokenize("class"), vec!["class"]);
}

#[test]
fn bm25_ranking_is_deterministic_across_document_order() {
    fn ranker(order: [&str; 3]) -> Bm25Ranker {
        Bm25Ranker::new(
            order
                .into_iter()
                .map(|name| Bm25Document::new(name, "shared searchable text"))
                .collect::<Vec<_>>(),
        )
    }

    let forward = ranker(["zeta", "alpha", "middle"]).rank("shared", 8);
    let reversed = ranker(["middle", "alpha", "zeta"]).rank("shared", 8);
    assert_eq!(
        forward, reversed,
        "rank order and scores must not depend on document insertion order"
    );
    assert_eq!(
        forward
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "middle", "zeta"],
        "equal scores break ties on name ascending"
    );
    let scores: Vec<f64> = forward.iter().map(|(_, score)| *score).collect();
    assert!(
        scores
            .windows(2)
            .all(|pair| (pair[0] - pair[1]).abs() < 1e-12),
        "the fixture must actually tie: {scores:?}"
    );

    // The total order is applied before the limit, so the returned window
    // is stable too.
    let build = |order: [&str; 4]| {
        Bm25Ranker::new(
            order
                .into_iter()
                .map(|name| Bm25Document::new(name, "shared searchable text"))
                .collect::<Vec<_>>(),
        )
    };
    let forward = build(["delta", "charlie", "bravo", "alpha"]).rank("shared", 2);
    let reversed = build(["alpha", "bravo", "charlie", "delta"]).rank("shared", 2);
    assert_eq!(forward, reversed);
    assert_eq!(
        forward
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "bravo"]
    );

    // The primary key stays score descending; only equal scores fall
    // through to the name tie-break.
    let ranker = Bm25Ranker::new(vec![
        Bm25Document::new("zeta", "widget widget widget"),
        Bm25Document::new("alpha", "widget"),
    ]);
    assert_eq!(
        ranker.rank("widget", 8)[0].0,
        "zeta",
        "the higher BM25 score ranks first"
    );
}

#[test]
fn unicode_text_is_nfc_and_matches_in_any_script() {
    assert_eq!(
        tokenize("café"),
        tokenize("cafe\u{301}"),
        "NFC makes canonically equivalent text identical"
    );
    assert_eq!(
        tokenize("cafés"),
        vec!["cafés"],
        "only ASCII words get the naive plural stem"
    );

    let russian = tokenize("Поиск документов");
    assert!(
        russian.contains(&"поиск".to_owned()) && russian.contains(&"документов".to_owned()),
        "Cyrillic words survive tokenization: {russian:?}"
    );
    assert!(
        !tokenize("搜索仓库问题").is_empty(),
        "text without spaces must not tokenize to nothing"
    );

    for (query, text) in [
        ("Поиск документов", "поиск документов сервера"),
        ("搜索仓库问题", "搜索仓库问题"),
        ("café", "cafe\u{301} au lait"),
    ] {
        let ranker = Bm25Ranker::new(vec![
            Bm25Document::new("target", text),
            Bm25Document::new("other", "unrelated words here"),
        ]);
        let ranked = ranker.rank(query, 8);
        assert_eq!(
            ranked.first().map(|(name, _)| name.as_str()),
            Some("target"),
            "query {query:?} finds its document: {ranked:?}"
        );
    }
}

#[test]
fn schema_text_follows_local_refs_once_and_ignores_remote_refs() {
    let parameters = json!({
        "type": "object",
        "properties": {
            "options": { "$ref": "#/$defs/options" },
            "options_again": { "$ref": "#/$defs/options" },
            "tree": { "$ref": "#/$defs/node" },
            "escaped": { "$ref": "#/$defs/a~1b" },
            "tilde": { "$ref": "#/$defs/a~0b" },
            "encoded": { "$ref": "#/$defs/space%20name" },
            "remote": { "$ref": "https://example.invalid/schema" },
            "root_ref": { "$ref": "#" },
            "array_items": { "items": [ { "description": "array-item-token" } ] },
            "variant": { "anyOf": [ { "description": "variant-token" } ] }
        },
        "$defs": {
            "options": {
                "description": "reconciliation",
                "properties": { "mode": { "type": "string" } }
            },
            "node": {
                "description": "recursive-node",
                "properties": { "child": { "$ref": "#/$defs/node" } }
            },
            "a/b": { "description": "slash definition" },
            "a~b": { "description": "tilde definition" },
            "space name": { "description": "percent definition" },
            "unused": { "description": "unreferenced-token" }
        }
    });

    let text = tool_search_document_text("Inspect records", &parameters);
    assert!(text.starts_with("Inspect records"), "{text}");
    for expected in [
        "options",
        "reconciliation",
        "recursive-node",
        "slash definition",
        "tilde definition",
        "percent definition",
        "variant-token",
    ] {
        assert!(text.contains(expected), "missing {expected:?}: {text}");
    }
    assert_eq!(
        text.matches("reconciliation").count(),
        1,
        "a shared definition is read once: {text}"
    );
    assert_eq!(
        text.matches("recursive-node").count(),
        1,
        "a recursive definition terminates and is read once: {text}"
    );
    assert!(
        !text.contains("array-item-token"),
        "array-valued `items` contributes nothing: {text}"
    );
    assert!(
        !text.contains("unreferenced-token"),
        "unreferenced definitions stay out: {text}"
    );
    assert!(
        !text.contains("example.invalid"),
        "remote refs stay out: {text}"
    );
}

#[test]
fn tool_document_combines_description_schema_and_namespace() {
    let parameters = json!({
        "type": "object",
        "properties": { "query": { "description": "search terms" } }
    });
    let namespace = ToolNamespace::new("mcp__docs")
        .with_description("Documentation server")
        .with_instructions("Call docs__read first");

    let document = tool_document("Read documentation", Some(&parameters), Some(&namespace));
    assert!(document.text.starts_with("Read documentation"));
    assert!(document.text.contains("search terms"), "{}", document.text);
    assert_eq!(
        document.namespace_text,
        "mcp__docs Documentation server Call docs__read first"
    );

    let built = Bm25Document::from_tool(
        "mcp__docs__read",
        "Read documentation",
        Some(&parameters),
        Some(&namespace),
    );
    assert_eq!(built.name, "mcp__docs__read");
    assert_eq!(built.text, document.text);
    assert_eq!(built.namespace_text, document.namespace_text);

    let blank = ToolNamespace::new("solo").with_description("   ");
    assert_eq!(
        tool_document("", None, Some(&blank)).namespace_text,
        "solo",
        "empty namespace parts are skipped"
    );
}
