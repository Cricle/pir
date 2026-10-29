//! `node:assert` shim semantics — the parts the official suite pins
//! (test/parallel/test-assert*.js) but which no narrower shim test covered:
//!
//!   * the three deep-equality modes of node's
//!     lib/internal/util/comparisons.js (loose / strict / partial), including
//!     the internal-slot branding that must survive a swapped prototype or a
//!     forged `Symbol.toStringTag`
//!   * the `AssertionError` shape: the code-qualified stack head, the frames
//!     dropped via `stackStartFn`, the `[inspect.custom]` string ellipsis and
//!     the `options` argument validation
//!   * the `ERR_INVALID_ARG_TYPE` / `ERR_AMBIGUOUS_ARGUMENT` message templates
//!     of `assert.throws` / `assert.rejects`
//!
//! Fixtures are generated at runtime into per-test temp dirs: the repository
//! hygiene gate forbids tracked `.ts` files.

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

fn write_temp_fixture(test_name: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-assert-shim-{}-{test_name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    std::fs::write(dir.join("ext.ts"), source).expect("write fixture");
    dir
}

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// Runs one seeded extension and returns the JSON it leaves on
/// `globalThis.__assertShim`. The fixture installs an async `session_start`
/// handler, so everything it observes is recorded before the event resolves.
async fn run_fixture(test_name: &str, body: &str) -> Value {
    let source = format!(
        "import assert from \"node:assert\";\n\
         import {{ inspect }} from \"node:util\";\n\
         export default (pi) => {{\n  \
         pi.on(\"session_start\", async () => {{\n    \
         const out = {{}};\n    \
         function probe(fn) {{\n      \
         try {{ fn(); return {{ threw: false }}; }}\n      \
         catch (e) {{ return {{ threw: true, code: e && e.code, name: e && e.name, \
         message: String(e && e.message), stack: String(e && e.stack) }}; }}\n    \
         }}\n    \
         function capture(fn) {{\n      \
         try {{ fn(); }} catch (e) {{ return e; }}\n      \
         return null;\n    }}\n{body}\n    \
         globalThis.__assertShim = out;\n  }});\n}};\n"
    );

    let dir = write_temp_fixture(test_name, &source);
    let rt = {
        let rt = ExtensionRuntime::new(test_config_at(&dir))
            .await
            .expect("runtime construction");
        rt.allow_root(dir.clone());
        rt
    };
    rt.seed_module("pir-ext:ext.ts", &source)
        .expect("seed assert fixture");
    let loaded = rt.seeded_load("pir-ext:ext.ts").await;
    assert_eq!(loaded.expect("seeded load"), 1);

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors while exercising node:assert: {:?}",
        outcome.errors
    );

    rt.eval_json("JSON.stringify(globalThis.__assertShim)")
        .await
        .expect("read assert shim results")
}

#[tokio::test(flavor = "current_thread")]
async fn assert_deep_equality_modes() {
    let v = run_fixture(
        "deep_equality",
        r#"
    const date = new Date('2016');
    class MyDate extends Date {
      constructor(...args) { super(...args); this[0] = '1'; }
    }
    const date2 = new MyDate('2016');

    // Loose deepEqual is not prototype-blind: Date vs a Date subclass with an
    // extra own key must differ.
    out.looseDateEquals = probe(() => assert.deepEqual(date, date2)).threw;
    out.looseSameDate = !probe(() => assert.deepEqual(new Date('2016'), new Date('2016'))).threw;
    // ... but it still coerces primitives, and NaN/-0 keep their loose rules.
    out.loosePrimitiveCoercion = !probe(() => assert.deepEqual([1, 2], ['1', 2])).threw;
    out.looseNaN = !probe(() => assert.deepEqual([NaN], [NaN])).threw;
    out.strictNegZero = probe(() => assert.deepStrictEqual(-0, 0)).threw;

    // A forged prototype / Symbol.toStringTag must not change what a value is.
    const a = new Uint8Array(3);
    const b = new Int8Array(3);
    Object.defineProperty(b, Symbol.toStringTag, { value: 'Uint8Array' });
    Object.setPrototypeOf(b, Uint8Array.prototype);
    out.forgedTypedArray = probe(() => assert.deepStrictEqual(a, b)).threw;

    const ab = new ArrayBuffer(3);
    const view = new Uint8Array(3);
    Object.setPrototypeOf(view, ArrayBuffer.prototype);
    Object.defineProperty(view, Symbol.toStringTag, { value: 'ArrayBuffer' });
    out.forgedArrayBuffer = probe(() => assert.deepStrictEqual(ab, view)).threw;

    const buf = Buffer.from('test');
    const bufFake = Object.create(Object.getPrototypeOf(buf), Object.getOwnPropertyDescriptors(buf));
    Object.defineProperty(bufFake, Symbol.toStringTag, { value: 'Uint8Array' });
    out.forgedBuffer = probe(() => assert.deepStrictEqual(buf, bufFake)).threw;

    // Partial mode: the expected side may be a subset.
    out.partialSubsetObject = !probe(() => assert.partialDeepStrictEqual(
      { a: 1, b: 2, c: 3 }, { b: 2 })).threw;
    out.partialExtraExpected = probe(() => assert.partialDeepStrictEqual(
      { a: 1 }, { a: 1, b: 2 })).threw;
    out.partialArraySubset = !probe(() => assert.partialDeepStrictEqual([1, 2, 3], [2, 3])).threw;
    out.partialArrayTooShort = probe(() => assert.partialDeepStrictEqual([1], [1, 2])).threw;
    out.partialSetSubset = !probe(() => assert.partialDeepStrictEqual(
      new Set([1, 2, 3]), new Set([3]))).threw;
    out.partialMapSubset = !probe(() => assert.partialDeepStrictEqual(
      new Map([[1, 'a'], [2, 'b']]), new Map([[2, 'b']]))).threw;
    out.partialErrorCause = probe(() => assert.partialDeepStrictEqual(
      new Error('x'), new Error('x', { cause: new Error('y') }))).threw;
    out.partialDifferentValues = probe(() => assert.partialDeepStrictEqual(
      new Map([[[1], 1], [{}, 2]]), new Map([[[1], 2], [{}, 1]]))).threw;

    // Cycles must terminate (the previous implementation recursed until the
    // stack overflowed), and shared references stay structural.
    const c1 = {}; c1.self = c1; c1.set = new Set([['x']]);
    const c2 = {}; c2.self = c2; c2.set = new Set([['x']]);
    out.partialCircular = !probe(() => assert.partialDeepStrictEqual(c1, c2)).threw;
    out.strictCircular = !probe(() => assert.deepStrictEqual(c1, c2)).threw;

    const shared = { x: 1 };
    out.crossRootAlias = probe(() => assert.deepStrictEqual(
      { a: shared, b: { c: shared } },
      { a: { x: 1 }, b: { c: { x: 2 } } })).threw;

    // Spurious equalities the old lenient deep-equal produced.
    out.argumentsVsArray = probe(() => assert.deepStrictEqual(
      (function () { return arguments; })('a'), ['a'])).threw;
    out.sparseVsUndefined = probe(() => assert.deepStrictEqual([1, , 3], [1, undefined, 3])).threw;
    out.boxedVsPrimitive = probe(() => assert.deepStrictEqual(new Number(2), 2)).threw;
    out.weakMapInstances = probe(() => assert.deepStrictEqual(new WeakMap(), new WeakMap())).threw;
"#,
    )
    .await;

    assert_eq!(
        v["looseDateEquals"], true,
        "loose deepEqual(Date, MyDate) must reject the extra own key: {v}"
    );
    assert_eq!(
        v["looseSameDate"], true,
        "loose deepEqual of equal Dates: {v}"
    );
    assert_eq!(
        v["loosePrimitiveCoercion"], true,
        "loose deepEqual([1,2], ['1',2]) must pass: {v}"
    );
    assert_eq!(
        v["looseNaN"], true,
        "loose deepEqual(NaN, NaN) must pass: {v}"
    );
    assert_eq!(
        v["strictNegZero"], true,
        "deepStrictEqual(-0, 0) must throw (Object.is): {v}"
    );

    assert_eq!(
        v["forgedTypedArray"], true,
        "an Int8Array with a Uint8Array prototype/tag is not a Uint8Array: {v}"
    );
    assert_eq!(
        v["forgedArrayBuffer"], true,
        "a typed array is not an ArrayBuffer, whatever its prototype says: {v}"
    );
    assert_eq!(
        v["forgedBuffer"], true,
        "a Buffer clone with forged slots is not a Buffer: {v}"
    );

    assert_eq!(
        v["partialSubsetObject"], true,
        "partialDeepStrictEqual accepts a subset object: {v}"
    );
    assert_eq!(
        v["partialExtraExpected"], true,
        "partialDeepStrictEqual rejects a bigger expected object: {v}"
    );
    assert_eq!(
        v["partialArraySubset"], true,
        "partialDeepStrictEqual accepts a sub-array in order: {v}"
    );
    assert_eq!(
        v["partialArrayTooShort"], true,
        "partialDeepStrictEqual rejects a shorter actual array: {v}"
    );
    assert_eq!(
        v["partialSetSubset"], true,
        "partialDeepStrictEqual accepts a Set subset: {v}"
    );
    assert_eq!(
        v["partialMapSubset"], true,
        "partialDeepStrictEqual accepts a Map subset: {v}"
    );
    assert_eq!(
        v["partialErrorCause"], true,
        "partialDeepStrictEqual rejects a missing expected cause: {v}"
    );
    assert_eq!(
        v["partialDifferentValues"], true,
        "partialDeepStrictEqual rejects maps whose values differ: {v}"
    );
    assert_eq!(
        v["partialCircular"], true,
        "partialDeepStrictEqual must terminate on cycles: {v}"
    );
    assert_eq!(
        v["strictCircular"], true,
        "deepStrictEqual must terminate on cycles: {v}"
    );
    assert_eq!(
        v["crossRootAlias"], true,
        "deepStrictEqual must compare shared references structurally: {v}"
    );
    assert_eq!(
        v["argumentsVsArray"], true,
        "an arguments object is not an array: {v}"
    );
    assert_eq!(
        v["sparseVsUndefined"], true,
        "a sparse array differs from an explicit undefined: {v}"
    );
    assert_eq!(v["boxedVsPrimitive"], true, "new Number(2) is not 2: {v}");
    assert_eq!(
        v["weakMapInstances"], true,
        "two distinct WeakMaps are never deep-equal: {v}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn assert_error_shape_and_messages() {
    let v = run_fixture(
        "error_shape",
        r#"
    // The stack head carries the code, and the assert-internal frames are
    // dropped (node: ErrorCaptureStackTrace(this, stackStartFn)).
    out.failStack = probe(() => assert.fail('boom')).stack;
    out.nestedOkStack = probe(() => assert.ok(false)).stack;
    out.doesNotThrowStack = probe(() => assert.doesNotThrow(() => { throw new Error('x'); })).stack;

    // Missing expected exception details.
    out.missingDefault = probe(() => assert.throws(() => {})).message;
    out.missingTyped = probe(() => assert.throws(() => {}, TypeError)).message;
    out.missingWithMessage = probe(() => assert.throws(() => {}, TypeError, 'why')).message;

    // assert.rejects argument validation (ERR_INVALID_ARG_TYPE / ERR_INVALID_RETURN_VALUE).
    try { await assert.rejects('fail', {}); }
    catch (e) { out.rejectsArgType = { code: e.code, message: e.message }; }
    try { await assert.rejects(() => {}, {}); }
    catch (e) { out.rejectsReturnValue = { code: e.code, message: e.message }; }
    try { await assert.doesNotReject(123); }
    catch (e) { out.doesNotRejectArgType = { code: e.code, message: e.message }; }

    // Ambiguous error/message argument + empty validation object.
    out.ambiguousPrimitive = probe(() => assert.throws(() => { throw 'foo'; }, 'foo')).message;
    out.ambiguousMessage = probe(() => assert.throws(() => { throw new TypeError('foo'); }, 'foo')).message;
    out.emptyExpectedObject = probe(() => assert.throws(() => { throw new Error(); }, {})).code;

    // AssertionError option validation + the inspect ellipsis.
    out.optionsValidation = probe(() => new assert.AssertionError(1));
    out.optionsWorks = !probe(() => {
      const e = new assert.AssertionError({ message: 'x' });
      if (e.code !== 'ERR_ASSERTION') throw new Error('code');
    }).threw;

    const longError = capture(() => assert.strictEqual('A'.repeat(1000), ''));
    out.longMessage = String(longError.message);
    out.longMessageKeepsActual = out.longMessage.indexOf('A'.repeat(509)) !== -1;
    out.inspectTruncated = inspect(longError)
      .indexOf("actual: '" + 'A'.repeat(488) + "...'") !== -1;
    out.inspectKeys = Object.keys(capture(() => assert.strictEqual(1, 2)));
"#,
    )
    .await;

    let fail_stack = v["failStack"].as_str().unwrap_or_default();
    assert!(
        fail_stack.starts_with("AssertionError [ERR_ASSERTION]: boom"),
        "the stack head must be the code-qualified form: {v}"
    );
    assert!(
        !fail_stack.contains("(@node/assert"),
        "assert-internal frames must be dropped from the stack: {v}"
    );
    assert!(
        fail_stack.contains("ext.ts:"),
        "the caller frame must survive the stack trim: {v}"
    );

    let ok_stack = v["nestedOkStack"].as_str().unwrap_or_default();
    assert!(
        ok_stack.starts_with(
            "AssertionError [ERR_ASSERTION]: The expression evaluated to a falsy value:\n\n  \
             assert.ok(false)"
        ),
        "assert.ok(false) generated message mismatch: {v}"
    );
    let dnt_stack = v["doesNotThrowStack"].as_str().unwrap_or_default();
    assert!(
        !dnt_stack.contains("doesNotThrow"),
        "doesNotThrow must not name itself in the stack: {v}"
    );

    assert_eq!(
        v["missingDefault"], "Missing expected exception.",
        "default missing-exception message: {v}"
    );
    assert_eq!(
        v["missingTyped"], "Missing expected exception (TypeError).",
        "typed missing-exception message: {v}"
    );
    assert_eq!(
        v["missingWithMessage"], "Missing expected exception (TypeError): why",
        "missing-exception message with a custom message: {v}"
    );

    assert_eq!(
        v["rejectsArgType"]["code"], "ERR_INVALID_ARG_TYPE",
        "assert.rejects('fail') must be ERR_INVALID_ARG_TYPE: {v}"
    );
    assert_eq!(
        v["rejectsArgType"]["message"],
        "The \"promiseFn\" argument must be of type function or an instance of Promise. \
         Received type string ('fail')",
        "assert.rejects argument-type template: {v}"
    );
    assert_eq!(
        v["doesNotRejectArgType"]["message"],
        "The \"promiseFn\" argument must be of type function or an instance of Promise. \
         Received type number (123)",
        "assert.doesNotReject argument-type template: {v}"
    );
    assert_eq!(
        v["rejectsReturnValue"]["message"],
        "Expected instance of Promise to be returned from the \"promiseFn\" function but \
         got undefined.",
        "assert.rejects non-promise return template: {v}"
    );

    assert_eq!(
        v["ambiguousPrimitive"],
        "The \"error/message\" argument is ambiguous. The error \"foo\" is identical to the message.",
        "ambiguous primitive template: {v}"
    );
    assert_eq!(
        v["ambiguousMessage"],
        "The \"error/message\" argument is ambiguous. The error message \"foo\" is identical \
         to the message.",
        "ambiguous message template: {v}"
    );
    assert_eq!(
        v["emptyExpectedObject"], "ERR_INVALID_ARG_VALUE",
        "an empty validation object must be ERR_INVALID_ARG_VALUE: {v}"
    );

    assert_eq!(
        v["optionsValidation"]["code"], "ERR_INVALID_ARG_TYPE",
        "AssertionError must validate its options argument: {v}"
    );
    assert_eq!(
        v["optionsValidation"]["message"],
        "The \"options\" argument must be of type object. Received type number (1)",
        "AssertionError options validation template: {v}"
    );
    assert_eq!(
        v["optionsWorks"], true,
        "new AssertionError({{message}}) must work: {v}"
    );
    assert_eq!(
        v["longMessageKeepsActual"], true,
        "the error message keeps the full actual value: {v}"
    );
    assert_eq!(
        v["inspectTruncated"], true,
        "inspect(error) must elide long string values (node addEllipsis): {v}"
    );
    assert_eq!(
        v["inspectKeys"],
        json!([
            "generatedMessage",
            "code",
            "actual",
            "expected",
            "operator",
            "diff"
        ]),
        "AssertionError own enumerable keys must match node's: {v}"
    );
}

#[tokio::test]
async fn generated_ok_messages_use_the_call_source_line() {
    let v = run_fixture(
        "generated-messages",
        "    out.generated = (() => { const e = capture(() => assert.ok(null)); return e && e.message; })();\n    \
         out.generatedMultiline = (() => { const e = capture(() => assert.ok(\n      1 === 2\n    )); return e && e.message; })();\n    \
         out.generatedExplicit = (() => { const e = capture(() => assert.ok(false, 'boom')); return e && e.message; })();",
    )
    .await;
    assert_eq!(
        v["generated"],
        json!("The expression evaluated to a falsy value:\n\n  assert.ok(null)\n"),
        "generated message must carry the call's source expression: {v}"
    );
    assert_eq!(
        v["generatedMultiline"],
        json!("The expression evaluated to a falsy value:\n\n  assert.ok(\n"),
        "a multi-line call slices to the end of the call's own line: {v}"
    );
    assert_eq!(
        v["generatedExplicit"],
        json!("boom"),
        "an explicit message is untouched: {v}"
    );
}
