// Integration tests (kept out of src/).

use pir_builtin_context_mode::*;

#[test]
fn accepts_http_and_https_only() {
    assert!(validate_http_scheme("http://example.com/a").is_ok());
    assert!(validate_http_scheme("HTTPS://example.com").is_ok());
    for rejected in [
        "file:///etc/passwd",
        "ftp://example.com",
        "javascript:alert(1)",
        "example.com/path",
        "",
    ] {
        assert!(
            validate_http_scheme(rejected).is_err(),
            "{rejected} must be rejected"
        );
    }
}

#[test]
fn classifies_content_types() {
    assert_eq!(content_type_for("text/html; charset=utf-8"), "markdown");
    assert_eq!(content_type_for("application/xhtml+xml"), "markdown");
    assert_eq!(content_type_for("application/json"), "json");
    assert_eq!(content_type_for("application/yaml"), "yaml");
    assert_eq!(content_type_for("text/plain"), "text");
    assert_eq!(content_type_for(""), "text");
}

#[test]
fn decodes_utf8_and_falls_back_lossily() {
    assert_eq!(decode_body(b"hello".to_vec()), "hello");
    assert_eq!(decode_body(b"caf\xc3\xa9".to_vec()), "café");
    assert_eq!(decode_body(vec![b'a', 0xff, b'b']), "a\u{fffd}b");
}

#[test]
fn normalizes_html_bodies() {
    let (body, content_type) =
        normalize_response_body(b"<h1>Hi</h1><p>there</p>".to_vec(), "text/html");
    assert_eq!(content_type, "markdown");
    assert_eq!(body, "# Hi\n\nthere");
}

#[test]
fn removes_scripts_styles_templates_and_comments() {
    let html = r#"
        <h1>Docs</h1>
        <script>var hidden = "<p>no</p>";</script>
        <style>.x { color: red }</style>
        <noscript>enable js</noscript>
        <template><p>template</p></template>
        <!-- a comment with <b>markup</b> -->
        <p>Visible</p>
    "#;
    let text = html_to_text(html);
    assert_eq!(text, "# Docs\n\nVisible");
}

#[test]
fn converts_headings_with_inner_tags() {
    let text = html_to_text(
        "<h1>Top</h1><h2>Sub <em>title</em></h2><h3 class=\"x\">Deep</h3><h7>not</h7>",
    );
    assert!(text.contains("# Top"));
    assert!(text.contains("## Sub title"));
    assert!(text.contains("### Deep"));
    assert!(text.contains("not"));
}

#[test]
fn converts_blocks_breaks_and_list_items() {
    let text = html_to_text(
        "<p>one</p><div>two<br>three<br/>four</div><ul><li>alpha</li><li>beta</li></ul>",
    );
    // Each `<li>` opens a new list block, so items are separated by a blank
    // line (the same normalization the core context-mode fetch applies).
    assert_eq!(text, "one\ntwo\nthree\nfour\n\n- alpha\n\n- beta");
}

#[test]
fn decodes_common_and_numeric_entities() {
    let text = html_to_text("<p>A &amp; B &lt; C &gt; D &quot;q&quot; &#65;&#x42; &mdash;</p>");
    assert_eq!(text, "A & B < C > D \"q\" AB —");
    // Unknown entities and stray ampersands survive.
    assert_eq!(html_to_text("<p>AT&T &bogus; x</p>"), "AT&T &bogus; x");
}

#[test]
fn normalizes_whitespace() {
    let text = html_to_text("<p>  a   b  </p>\n\n\n\n<p>c</p>   <p></p>");
    assert_eq!(text, "a b\n\nc");
}

#[test]
fn plain_text_without_tags_is_preserved() {
    assert_eq!(html_to_text("just text"), "just text");
    assert_eq!(html_to_text(""), "");
}
