use pir_builtin_memory::*;
use pir_plugin_api::BuiltinPlugin;

#[test]
fn memory_text_is_escaped_inside_the_envelope() {
    let block = render_memory_context("a & b <script>");
    assert!(block.starts_with("<memory-context"));
    assert!(block.contains("a &amp; b &lt;script&gt;"));
    assert!(block.ends_with("</memory-context>"));
    assert!(block.contains("trust=\"untrusted-project-memory\""));
}

#[test]
fn brief_uses_its_own_tag() {
    let brief = render_memory_brief("note");
    assert!(brief.starts_with("<memory-brief"));
    assert!(brief.ends_with("</memory-brief>"));
}

#[test]
fn append_keeps_original_text() {
    assert_eq!(append_to_prompt("", "block"), "block");
    assert_eq!(append_to_prompt("hi", "block"), "hi\n\nblock");
    assert_eq!(append_to_prompt("hi   ", "block"), "hi\n\nblock");
}

#[test]
fn xml_escape_covers_all_three_metacharacters() {
    assert_eq!(xml_escape("<&>"), "&lt;&amp;&gt;");
}

#[test]
fn plugin_identity() {
    assert_eq!(MemoryPlugin::new().info().path, "builtin:memory");
    assert_eq!(TOOL_NAME, "memory");
    assert_eq!(COMMAND_NAME, "memory");
}
