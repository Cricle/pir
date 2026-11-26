// Integration tests (kept out of src/).

use pir_builtin_context_mode::*;
use pir_plugin_api::BuiltinPlugin;

#[test]
fn truncate_utf8_keeps_char_boundary() {
    let text = "aé"; // é is two bytes; byte index 2 is inside it
    assert_eq!(truncate_utf8(text, 2), "a");
    assert_eq!(truncate_utf8(text, 3), text);
    assert_eq!(truncate_utf8("", 4), "");
}

#[test]
fn truncate_chars_counts_chars_not_bytes() {
    assert_eq!(truncate_chars("aébc", 2), "aé");
    assert_eq!(truncate_chars("short", 10), "short");
    assert_eq!(truncate_chars("abcdef", 3), "abc");
    assert_eq!(truncate_chars("a b ", 3), "a b");
}

#[test]
fn plugin_uses_builtin_path() {
    let plugin = ContextModePlugin::new();
    assert_eq!(plugin.info().name, "context-mode");
    assert_eq!(plugin.info().path, "builtin:context-mode");
    assert!(!plugin.info().replaceable);
}
