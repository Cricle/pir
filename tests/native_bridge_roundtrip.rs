//! Native value bridge round-trip tests (external, per project rule: no
//! inline `cfg(test)` modules under `src/`).

use pir::extensions::runtime::native_bridge::{js_to_json, json_to_js};
use rquickjs::{Context, Runtime};
use serde_json::{json, Number, Value as JsonValue};

#[test]
fn json_js_round_trip() {
    let input: JsonValue = json!({
        "null": null,
        "bool": true,
        "int": 42,
        "float": 1.5,
        "string": "中\n\"quoted\"",
        "nested": {"a": [1, 2, {"b": false}]},
        "array": [null, "x", 3.25],
    });
    let rt = Runtime::new().expect("runtime");
    let ctx = Context::full(&rt).expect("context");
    ctx.with(|ctx| {
        let js = json_to_js(&ctx, &input).expect("json_to_js");
        let back = js_to_json(&js).expect("js_to_json");
        assert_eq!(back, input);
    });
}

#[test]
fn json_js_deep_tree_does_not_overflow() {
    let mut input = JsonValue::Number(Number::from(1));
    for _ in 0..512 {
        input = JsonValue::Array(vec![input]);
    }
    let rt = Runtime::new().expect("runtime");
    let ctx = Context::full(&rt).expect("context");
    ctx.with(|ctx| {
        let js = json_to_js(&ctx, &input).expect("json_to_js deep");
        let back = js_to_json(&js).expect("js_to_json deep");
        assert_eq!(back, input);
    });
}
