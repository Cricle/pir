//! TextEncoder / TextDecoder / `node:string_decoder` native-codec parity.
//!
//! Two corpora, both compared against expectations captured by running the
//! identical script under Node v24.22.1 (no Node needed at test time):
//!
//! * `textcodec_node_parity_corpus` — the supported surface: encode /
//!   encodeInto counts, utf-8/latin1/utf-16le decode, the WHATWG label table
//!   (latin1/ascii/... canonicalize to windows-1252 with the CP1252 0x80-0x9F
//!   mapping), BOM handling (first call, split BOM, per-stream reset after a
//!   completed stream or a fatal error), every-byte-boundary streaming,
//!   truncated sequences, the replacement-character set both tiers agree on,
//!   fatal-mode TypeError shape, argument/brand-check errors, and
//!   StringDecoder chunk splits for utf8/utf16le/latin1/ascii/hex/base64.
//! * `textcodec_known_native_gaps` (ignored) — WHATWG maximal-subpart cases
//!   the in-tree Rust `utf8_decode`/`utf8_decode_chunk` fallbacks still get
//!   wrong (overlong E0/F0, surrogate ED, above-U+10FFFF F4, and a valid
//!   prefix before an invalid continuation). The JS shim routes these into
//!   that kernel; fixing the kernel is a `buffer_ops.rs` change owned by the
//!   buffer workstream, so the expectations are pinned here for that wiring.
//!
//! The shim edits are also timed by `textcodec_hot_path_timing`, which runs a
//! 1240-byte mixed-script buffer in the same dev profile before/after.

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

fn test_config() -> ExtensionConfig {
    ExtensionConfig {
        cwd: std::env::temp_dir(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
        ..ExtensionConfig::default()
    }
}

async fn new_runtime() -> ExtensionRuntime {
    ExtensionRuntime::new(test_config())
        .await
        .expect("runtime construction")
}

async fn run_fixture(rt: &ExtensionRuntime, name: &str, ts: &str, global: &str) -> Value {
    let module = format!("pir-ext:{name}.ts");
    rt.seed_module(&module, ts).expect("seed fixture");
    let loaded = rt.seeded_load(&module).await;
    assert_eq!(loaded.expect("seeded load"), 1, "fixture {name} must load");
    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors in {name}: {:?}",
        outcome.errors
    );
    rt.eval_json(&format!("JSON.stringify(globalThis.{global})"))
        .await
        .expect("read fixture results")
}

/// Compare every corpus key, reporting all divergences at once.
fn assert_corpus(actual: &Value, expected_json: &str) {
    let expected: Value = serde_json::from_str(expected_json).expect("expected corpus JSON");
    let actual_map = actual.as_object().expect("corpus object");
    let expected_map = expected.as_object().expect("expected object");
    let mut failures = Vec::new();
    for (key, want) in expected_map {
        match actual_map.get(key) {
            Some(got) if got == want => {}
            Some(got) => failures.push(format!("{key}:\n  node: {want}\n  pir:  {got}")),
            None => failures.push(format!("{key}: missing (node: {want})")),
        }
    }
    for key in actual_map.keys() {
        if !expected_map.contains_key(key) {
            failures.push(format!("{key}: unexpected key (pir: {})", actual_map[key]));
        }
    }
    assert!(
        failures.is_empty(),
        "{} corpus divergences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn corpus_fixture(body: &str, global: &str) -> String {
    let mut source = String::new();
    source.push_str("import { StringDecoder } from \"node:string_decoder\";\n");
    source.push_str("export default (pi: any) => {\n");
    source.push_str("  pi.on(\"session_start\", async () => {\n");
    source.push_str("try {\n");
    source.push_str(body);
    source.push_str(&format!("\n(globalThis as any).{global} = out;\n"));
    source.push_str(&format!(
        "}} catch (error) {{ (globalThis as any).{global} = {{ error: String(error && (error.stack || error.message)) }}; }}\n"
    ));
    source.push_str("  });\n};\n");
    source
}

// ---------------------------------------------------------------------------
// Corpus (Node-verified expectations captured from node v24.22.1)
// ---------------------------------------------------------------------------

const CORPUS_BODY: &str = r#"
// Corpus body shared by node (expectation capture) and the Rust fixture.
// Only standard globals + an injected `StringDecoder` binding.
function hex(bytes) {
  let s = "";
  for (let i = 0; i < bytes.length; i++) s += (bytes[i] < 16 ? "0" : "") + bytes[i].toString(16);
  return s;
}
function cps(text) {
  const out = [];
  for (const ch of text) out.push(ch.codePointAt(0));
  return out;
}
function u8(list) {
  return Uint8Array.from(list);
}
function attempt(fn) {
  try {
    return { ok: fn() };
  } catch (e) {
    return { name: e && e.name, message: e && e.message, code: e && e.code };
  }
}
const out = {};

// ---- TextEncoder ---------------------------------------------------------
const encoder = new TextEncoder();
out.encEncoding = encoder.encoding;
out.encEmpty = hex(encoder.encode(""));
out.encAscii = hex(encoder.encode("abc"));
out.encMixed = hex(encoder.encode("é中😀"));
out.encLoneHigh = hex(encoder.encode("\uD800"));
out.encLoneHighAscii = hex(encoder.encode("\uD800x"));
out.encLoneLow = hex(encoder.encode("x\uDC00"));
out.encUndefined = hex(encoder.encode(undefined));
out.encNull = hex(encoder.encode(null));
out.encNumber = hex(encoder.encode(42));
out.encRoundTrip = new TextDecoder().decode(encoder.encode("héllo 世界 😀")) === "héllo 世界 😀";

function into(src, size, fill) {
  const target = new Uint8Array(size);
  if (fill !== undefined) target.fill(fill);
  const r = encoder.encodeInto(src, target);
  return { read: r.read, written: r.written, bytes: hex(target) };
}
out.encIntoExact = into("abc", 3, 0x2e);
out.encIntoShort = into("é", 1, 0x2e);
out.encIntoPartial = into("aé", 2, 0x2e);
out.encIntoFull = into("aé", 3, 0x2e);
out.encIntoAstral = into("😀", 3, 0x2e);
out.encIntoAstralFull = into("😀", 4, 0x2e);
out.encIntoLone = into("\uD800x", 4, 0x2e);
out.encIntoZero = into("abc", 0);
out.encIntoBuffer = (function () {
  const b = Buffer.alloc(4);
  const r = encoder.encodeInto("é", b);
  return { read: r.read, written: r.written, bytes: hex(b) };
})();
out.encIntoTargetError = attempt(function () {
  return encoder.encodeInto("a", new ArrayBuffer(2));
}).name;

// ---- TextDecoder: utf-8 --------------------------------------------------
const text = "aé中😀z";
const textBytes = encoder.encode(text);
out.decRoundTrip = new TextDecoder().decode(textBytes) === text;
out.decNoArgs = new TextDecoder().decode();
out.decEmpty = new TextDecoder().decode(u8([]));
out.decUint8 = new TextDecoder().decode(u8([0x41, 0x42]));
out.decArrayBuffer = new TextDecoder().decode(u8([0x41, 0x42]).buffer);
out.decDataView = new TextDecoder().decode(
  new DataView(u8([0x41, 0x42]).buffer)
);
out.decInt16 = new TextDecoder().decode(new Int16Array(u8([0x41, 0x42, 0x43, 0x44]).buffer, 2, 1));
out.decTypeError = attempt(function () {
  return new TextDecoder().decode("abc");
}).name;

// BOM
const bom = u8([0xef, 0xbb, 0xbf, 0x61, 0x62]);
out.bomStrip = new TextDecoder().decode(bom);
out.bomIgnore = cps(new TextDecoder("utf-8", { ignoreBOM: true }).decode(bom));
out.bomOnly = new TextDecoder().decode(u8([0xef, 0xbb, 0xbf]));
out.bomTwice = cps(new TextDecoder().decode(u8([0xef, 0xbb, 0xbf, 0xef, 0xbb, 0xbf, 0x61])));
out.bomAfterEmpty = (function () {
  const d = new TextDecoder();
  return d.decode() + d.decode(bom);
})();
out.bomEmptyStream = (function () {
  const d = new TextDecoder();
  return d.decode(undefined, { stream: true }) + d.decode(bom, { stream: true }) + d.decode();
})();
out.bomSplit = (function () {
  const d = new TextDecoder();
  return d.decode(u8([0xef]), { stream: true }) + d.decode(u8([0xbb, 0xbf, 0x61]));
})();
out.bomNotFirst = (function () {
  const d = new TextDecoder();
  return cps(d.decode(u8([0x61]), { stream: true }) + d.decode(bom));
})();

// streaming across every boundary
out.streamBoundaries = (function () {
  const d = new TextDecoder();
  let s = "";
  for (let i = 0; i < textBytes.length; i++) s += d.decode(textBytes.subarray(i, i + 1), { stream: true });
  s += d.decode();
  return cps(s).join(",") === cps(text).join(",");
})();
out.streamPairs = (function () {
  const d = new TextDecoder();
  let s = "";
  for (let i = 0; i < textBytes.length; i += 2) s += d.decode(textBytes.subarray(i, i + 2), { stream: true });
  s += d.decode();
  return cps(s).join(",") === cps(text).join(",");
})();
out.streamTruncated = (function () {
  const d = new TextDecoder();
  return [
    cps(d.decode(u8([0xe2]), { stream: true })),
    cps(d.decode(u8([0x82]), { stream: true })),
    cps(d.decode()),
  ];
})();
out.truncFinal3 = cps(new TextDecoder().decode(u8([0xe2, 0x82])));
out.truncFinal4 = cps(new TextDecoder().decode(u8([0xf0, 0x9f, 0x98])));

// invalid sequences (the replacement-semantics kernel's parity set)
out.invalidCont = cps(new TextDecoder().decode(u8([0x80])));
out.invalidFF = cps(new TextDecoder().decode(u8([0xff])));
out.invalidFEFE = cps(new TextDecoder().decode(u8([0xfe, 0xff])));
out.invalidOverlong2 = cps(new TextDecoder().decode(u8([0xc0, 0x80])));
out.invalidF5 = cps(new TextDecoder().decode(u8([0xf5, 0x80, 0x80, 0x80])));
out.invalidC2A = cps(new TextDecoder().decode(u8([0xc2, 0x41])));
out.invalidStreamSplit = (function () {
  const d = new TextDecoder();
  return [
    cps(d.decode(u8([0x80]), { stream: true })),
    cps(d.decode(u8([0xff]))),
  ];
})();

// fatal
out.fatalFF = attempt(function () {
  return new TextDecoder("utf-8", { fatal: true }).decode(u8([0xff]));
});
out.fatalTrunc = attempt(function () {
  return new TextDecoder("utf-8", { fatal: true }).decode(u8([0xe2, 0x82]));
});
out.fatalValid = new TextDecoder("utf-8", { fatal: true }).decode(textBytes) === text;
out.fatalStream = (function () {
  const d = new TextDecoder("utf-8", { fatal: true });
  const a = d.decode(u8([0xe2]), { stream: true });
  return [a, attempt(function () { return d.decode(); })];
})();

// labels
out.labels = ["utf8", "UTF-8", " unicode-1-1-utf-8 ", "utf-16le", "utf-16", "ucs-2"].map(
  function (l) { return new TextDecoder(l).encoding; }
);
out.labelBad = attempt(function () { return new TextDecoder("no-such-charset"); }).name;

// latin1 labels are WHATWG windows-1252 (node), so 0x80-0x9F are the CP1252
// punctuation; Buffer#toString("latin1") keeps the byte->U+00xx mapping.
out.latin1LowHigh = cps(
  new TextDecoder("latin1").decode(u8([0x00, 0x41, 0x7f, 0xa0, 0xe9, 0xff]))
);
out.latin1Alias = ["latin1", "iso-8859-1", "ascii", "us-ascii"].map(function (l) {
  return cps(new TextDecoder(l).decode(u8([0x41, 0xe9])));
});
out.latin1NoBom = cps(new TextDecoder("latin1").decode(u8([0xef, 0xbb, 0xbf, 0x61])));
out.latin1High = cps(new TextDecoder("latin1").decode(
  u8([0x80, 0x81, 0x8b, 0x8d, 0x8f, 0x90, 0x91, 0x9d, 0x9f])
));
out.latin1All = cps(new TextDecoder("windows-1252").decode(
  u8(Array.from({ length: 256 }, function (_, i) { return i; }))
));
out.latin1Encoding = new TextDecoder("latin1").encoding;
out.latin1Labels = [
  "latin1", "iso-8859-1", "iso8859-1", "iso88591", "iso_8859-1",
  "iso_8859-1:1987", "l1", "ascii", "us-ascii", "windows-1252", "cp1252",
  "cp819", "ibm819", "csisolatin1", "x-cp1252", "ansi_x3.4-1968",
].map(function (l) { return new TextDecoder(l).encoding; });
out.labelBadShape = attempt(function () { return new TextDecoder("bogus"); });
out.labelNull = attempt(function () { return new TextDecoder(null); });
out.labelUcs2 = attempt(function () { return new TextDecoder("ucs2"); }).name;
out.labelNonAsciiSpace = attempt(function () { return new TextDecoder("\u00a0latin1"); }).name;
out.fatalCoerce = new TextDecoder("utf-8", { fatal: "yes" }).fatal;
out.ignoreBomCoerce = new TextDecoder("utf-8", { ignoreBOM: 1 }).ignoreBOM;
out.streamInputError = attempt(function () {
  return new TextDecoder().decode("", { stream: true });
});
out.decodeThisError = attempt(function () {
  return TextDecoder.prototype.decode.call({}, u8([0x41]));
});
out.encodeThisError = attempt(function () {
  return TextEncoder.prototype.encode.call({}, "a");
});
out.encodeIntoBadDest = attempt(function () {
  return new TextEncoder().encodeInto("a", new ArrayBuffer(2));
});

// BOM state is per stream, not per instance (node resets it when a non-stream
// decode completes, and on a decode error).
out.bomPerStream = (function () {
  const d = new TextDecoder();
  return [d.decode(u8([0xef, 0xbb, 0xbf, 0x41])), d.decode(u8([0xef, 0xbb, 0xbf, 0x42]))];
})();
out.bomAfterStreamEnd = (function () {
  const d = new TextDecoder();
  d.decode(u8([0xef, 0xbb, 0xbf, 0x41]), { stream: true });
  d.decode();
  return d.decode(u8([0xef, 0xbb, 0xbf, 0x42]));
})();
out.bomAfterFatal = (function () {
  const d = new TextDecoder("utf-8", { fatal: true });
  try { d.decode(u8([0xff])); } catch (error) { /* the error ends the stream */ }
  return d.decode(u8([0xef, 0xbb, 0xbf, 0x41]));
})();
out.bomPartialPrefixEnd = (function () {
  const d = new TextDecoder();
  return d.decode(u8([0xef]), { stream: true }) + d.decode();
})();
out.utf16BomPerStream = (function () {
  const d = new TextDecoder("utf-16le");
  return [d.decode(u8([0xff, 0xfe, 0x41, 0x00])), d.decode(u8([0xff, 0xfe, 0x42, 0x00]))];
})();

// ---- TextDecoder: utf-16le ----------------------------------------------
const u16 = function (bytes) { return new TextDecoder("utf-16le").decode(u8(bytes)); };
out.u16Basic = u16([0x41, 0x00, 0x42, 0x00]);
out.u16Astral = cps(u16([0x00, 0xd8, 0x00, 0xdc]));
out.u16Bom = u16([0xff, 0xfe, 0x41, 0x00]);
out.u16BomIgnore = cps(new TextDecoder("utf-16le", { ignoreBOM: true }).decode(u8([0xff, 0xfe, 0x41, 0x00])));
out.u16Odd = cps(u16([0x41]));
out.u16HighOnly = cps(u16([0x00, 0xd8]));
out.u16HighOdd = cps(u16([0x00, 0xd8, 0x41]));
out.u16HighNonLow = cps(u16([0x00, 0xd8, 0x41, 0x00, 0x42]));
out.u16LowOnly = cps(u16([0x00, 0xdc]));
out.u16HighHigh = cps(u16([0x00, 0xd8, 0x00, 0xd8]));
out.u16HighHighLow = cps(u16([0x00, 0xd8, 0x00, 0xd8, 0x00, 0xdc]));
out.u16StreamOdd = (function () {
  const d = new TextDecoder("utf-16le");
  return [cps(d.decode(u8([0x41]), { stream: true })), cps(d.decode())];
})();
out.u16StreamOddRest = (function () {
  const d = new TextDecoder("utf-16le");
  return [
    cps(d.decode(u8([0x41]), { stream: true })),
    cps(d.decode(u8([0x00, 0x42, 0x00]))),
  ];
})();
out.u16StreamHigh = (function () {
  const d = new TextDecoder("utf-16le");
  return [
    cps(d.decode(u8([0x00, 0xd8]), { stream: true })),
    cps(d.decode(u8([0x00, 0xdc]))),
  ];
})();
out.u16StreamHighOdd = (function () {
  const d = new TextDecoder("utf-16le");
  return [
    cps(d.decode(u8([0x00, 0xd8, 0x41]), { stream: true })),
    cps(d.decode(u8([0x00, 0xdc]))),
    cps(d.decode()),
  ];
})();
function u16leBytes(s) {
  const a = [];
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    a.push(c & 0xff, (c >> 8) & 0xff);
  }
  return Uint8Array.from(a);
}
out.u16StreamBoundaries = (function () {
  const bytes = u16leBytes("A😀B");
  const d = new TextDecoder("utf-16le");
  let s = "";
  for (let i = 0; i < bytes.length; i++) s += d.decode(bytes.subarray(i, i + 1), { stream: true });
  s += d.decode();
  return [cps(s), cps(s).join(",") === cps("A😀B").join(",")];
})();
out.u16FatalLow = attempt(function () {
  return new TextDecoder("utf-16le", { fatal: true }).decode(u8([0x00, 0xdc]));
});
out.u16FatalOdd = attempt(function () {
  return new TextDecoder("utf-16le", { fatal: true }).decode(u8([0x41]));
});
out.u16FatalHighNonLow = attempt(function () {
  return new TextDecoder("utf-16le", { fatal: true }).decode(u8([0x00, 0xd8, 0x41, 0x00]));
});
out.u16FatalStreamHigh = (function () {
  const d = new TextDecoder("utf-16le", { fatal: true });
  const a = d.decode(u8([0x00, 0xd8]), { stream: true });
  const b = d.decode(u8([0x00, 0xdc]));
  return [a, b];
})();

// ---- StringDecoder -------------------------------------------------------
const sdText = "aé中😀z";
const sdBytes = Array.from(encoder.encode(sdText));
out.sdUtf8Bytes = (function () {
  const d = new StringDecoder("utf8");
  const parts = [];
  let s = "";
  for (let i = 0; i < sdBytes.length; i++) {
    const p = d.write(Buffer.from([sdBytes[i]]));
    parts.push(cps(p));
    s += p;
  }
  s += d.end();
  return { parts: parts, same: cps(s).join(",") === cps(sdText).join(",") };
})();
out.sdUtf8Splits = (function () {
  const res = [];
  for (let cut = 0; cut <= sdBytes.length; cut++) {
    const d = new StringDecoder("utf8");
    const s =
      d.write(Buffer.from(sdBytes.slice(0, cut))) +
      d.write(Buffer.from(sdBytes.slice(cut))) +
      d.end();
    res.push(cps(s).join(",") === cps(sdText).join(","));
  }
  return res;
})();
out.sdUtf8Held = (function () {
  const d = new StringDecoder("utf8");
  const a = d.write(Buffer.from([0x61, 0xf0, 0x9f]));
  return [cps(a), hex(d.lastChar), d.lastNeed, d.lastTotal, cps(d.end())];
})();
out.sdUtf8Invalid = [
  [0xf1, 0xcc, 0xb8],
  [0xed, 0xa0, 0xb5, 0xed, 0xb0, 0x8d],
  [0xe0, 0x80, 0x80],
  [0xe1, 0x80, 0x41],
].map(function (a) {
  const d = new StringDecoder("utf8");
  return cps(d.write(Buffer.from(a)) + d.end());
});
out.sdUtf8InvalidSplits = [
  [0xf1, 0xcc, 0xb8],
  [0xed, 0xa0, 0xb5, 0xed, 0xb0, 0x8d],
  [0xe0, 0x80, 0x80],
  [0xf0, 0x80, 0x80, 0x80],
  [0xf4, 0x90, 0x80, 0x80],
].map(function (a) {
  const res = [];
  for (let cut = 0; cut <= a.length; cut++) {
    const d = new StringDecoder("utf8");
    res.push(
      cps(d.write(Buffer.from(a.slice(0, cut))) + d.write(Buffer.from(a.slice(cut))) + d.end()).join(",")
    );
  }
  return res;
});
out.sdString = cps(new StringDecoder("utf8").write("aé中"));
out.sdLatin1 = cps(new StringDecoder("latin1").write(Buffer.from([0x41, 0xe9, 0xff])));
out.sdAscii = cps(new StringDecoder("ascii").write(Buffer.from([0x41, 0xe9, 0xff])));
out.sdHex = new StringDecoder("hex").write(Buffer.from([0x00, 0x0f, 0xff]));
out.sdBase64 = (function () {
  const d = new StringDecoder("base64");
  return [d.write(Buffer.from([0x61, 0x62, 0x63, 0x64, 0x65])), d.end()];
})();
out.sdBase64Splits = [1, 2, 3, 4].map(function (n) {
  const d = new StringDecoder("base64");
  return (
    d.write(Buffer.from([0x61, 0x62, 0x63, 0x64, 0x65].slice(0, n))) +
    d.write(Buffer.from([0x61, 0x62, 0x63, 0x64, 0x65].slice(n))) +
    d.end()
  );
});
out.sdU16 = (function () {
  const d = new StringDecoder("utf16le");
  return [
    cps(d.write(Buffer.from([0x41]))),
    cps(d.write(Buffer.from([0x00, 0x42, 0x00]))),
    cps(d.end()),
  ];
})();
out.sdU16Surrogate = (function () {
  const d = new StringDecoder("utf16le");
  return [
    cps(d.write(Buffer.from([0x00, 0xd8]))),
    cps(d.write(Buffer.from([0x00, 0xdc]))),
    cps(d.end()),
  ];
})();
out.sdU16Astral = cps(new StringDecoder("utf16le").write(Buffer.from([0x3d, 0xd8, 0x00, 0xde])));
"#;

const EXPECTED_CORPUS: &str = r#"{"encEncoding":"utf-8","encEmpty":"","encAscii":"616263","encMixed":"c3a9e4b8adf09f9880","encLoneHigh":"efbfbd","encLoneHighAscii":"efbfbd78","encLoneLow":"78efbfbd","encUndefined":"","encNull":"6e756c6c","encNumber":"3432","encRoundTrip":true,"encIntoExact":{"read":3,"written":3,"bytes":"616263"},"encIntoShort":{"read":0,"written":0,"bytes":"2e"},"encIntoPartial":{"read":1,"written":1,"bytes":"612e"},"encIntoFull":{"read":2,"written":3,"bytes":"61c3a9"},"encIntoAstral":{"read":0,"written":0,"bytes":"2e2e2e"},"encIntoAstralFull":{"read":2,"written":4,"bytes":"f09f9880"},"encIntoLone":{"read":2,"written":4,"bytes":"efbfbd78"},"encIntoZero":{"read":0,"written":0,"bytes":""},"encIntoBuffer":{"read":1,"written":2,"bytes":"c3a90000"},"encIntoTargetError":"TypeError","decRoundTrip":true,"decNoArgs":"","decEmpty":"","decUint8":"AB","decArrayBuffer":"AB","decDataView":"AB","decInt16":"CD","decTypeError":"TypeError","bomStrip":"ab","bomIgnore":[65279,97,98],"bomOnly":"","bomTwice":[65279,97],"bomAfterEmpty":"ab","bomEmptyStream":"ab","bomSplit":"a","bomNotFirst":[97,65279,97,98],"streamBoundaries":true,"streamPairs":true,"streamTruncated":[[],[],[65533]],"truncFinal3":[65533],"truncFinal4":[65533],"invalidCont":[65533],"invalidFF":[65533],"invalidFEFE":[65533,65533],"invalidOverlong2":[65533,65533],"invalidF5":[65533,65533,65533,65533],"invalidC2A":[65533,65],"invalidStreamSplit":[[65533],[65533]],"fatalFF":{"name":"TypeError","message":"The encoded data was not valid for encoding utf-8","code":"ERR_ENCODING_INVALID_ENCODED_DATA"},"fatalTrunc":{"name":"TypeError","message":"The encoded data was not valid for encoding utf-8","code":"ERR_ENCODING_INVALID_ENCODED_DATA"},"fatalValid":true,"fatalStream":["",{"name":"TypeError","message":"The encoded data was not valid for encoding utf-8","code":"ERR_ENCODING_INVALID_ENCODED_DATA"}],"labels":["utf-8","utf-8","utf-8","utf-16le","utf-16le","utf-16le"],"labelBad":"RangeError","latin1LowHigh":[0,65,127,160,233,255],"latin1Alias":[[65,233],[65,233],[65,233],[65,233]],"latin1NoBom":[239,187,191,97],"latin1High":[8364,129,8249,141,143,144,8216,157,376],"latin1All":[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,64,65,66,67,68,69,70,71,72,73,74,75,76,77,78,79,80,81,82,83,84,85,86,87,88,89,90,91,92,93,94,95,96,97,98,99,100,101,102,103,104,105,106,107,108,109,110,111,112,113,114,115,116,117,118,119,120,121,122,123,124,125,126,127,8364,129,8218,402,8222,8230,8224,8225,710,8240,352,8249,338,141,381,143,144,8216,8217,8220,8221,8226,8211,8212,732,8482,353,8250,339,157,382,376,160,161,162,163,164,165,166,167,168,169,170,171,172,173,174,175,176,177,178,179,180,181,182,183,184,185,186,187,188,189,190,191,192,193,194,195,196,197,198,199,200,201,202,203,204,205,206,207,208,209,210,211,212,213,214,215,216,217,218,219,220,221,222,223,224,225,226,227,228,229,230,231,232,233,234,235,236,237,238,239,240,241,242,243,244,245,246,247,248,249,250,251,252,253,254,255],"latin1Encoding":"windows-1252","latin1Labels":["windows-1252","windows-1252","windows-1252","windows-1252","windows-1252","windows-1252","windows-1252","windows-1252","windows-1252","windows-1252","windows-1252","windows-1252","windows-1252","windows-1252","windows-1252","windows-1252"],"labelBadShape":{"name":"RangeError","message":"The \"bogus\" encoding is not supported","code":"ERR_ENCODING_NOT_SUPPORTED"},"labelNull":{"name":"RangeError","message":"The \"null\" encoding is not supported","code":"ERR_ENCODING_NOT_SUPPORTED"},"labelUcs2":"RangeError","labelNonAsciiSpace":"RangeError","fatalCoerce":true,"ignoreBomCoerce":true,"streamInputError":{"name":"TypeError","message":"The \"input\" argument must be an instance of ArrayBuffer or ArrayBufferView. Received type string ('')","code":"ERR_INVALID_ARG_TYPE"},"decodeThisError":{"name":"TypeError","message":"Value of \"this\" must be of type TextDecoder","code":"ERR_INVALID_THIS"},"encodeThisError":{"name":"TypeError","message":"Value of \"this\" must be of type TextEncoder","code":"ERR_INVALID_THIS"},"encodeIntoBadDest":{"name":"TypeError","message":"The \"dest\" argument must be an instance of Uint8Array. Received an instance of ArrayBuffer","code":"ERR_INVALID_ARG_TYPE"},"bomPerStream":["A","B"],"bomAfterStreamEnd":"B","bomAfterFatal":"A","bomPartialPrefixEnd":"�","utf16BomPerStream":["A","B"],"u16Basic":"AB","u16Astral":[65536],"u16Bom":"A","u16BomIgnore":[65279,65],"u16Odd":[65533],"u16HighOnly":[65533],"u16HighOdd":[65533],"u16HighNonLow":[65533,65,65533],"u16LowOnly":[65533],"u16HighHigh":[65533,65533],"u16HighHighLow":[65533,65536],"u16StreamOdd":[[],[65533]],"u16StreamOddRest":[[],[65,66]],"u16StreamHigh":[[],[65536]],"u16StreamHighOdd":[[],[65533,65,65533],[]],"u16StreamBoundaries":[[65,128512,66],true],"u16FatalLow":{"name":"TypeError","message":"The encoded data was not valid for encoding utf-16le","code":"ERR_ENCODING_INVALID_ENCODED_DATA"},"u16FatalOdd":{"name":"TypeError","message":"The encoded data was not valid for encoding utf-16le","code":"ERR_ENCODING_INVALID_ENCODED_DATA"},"u16FatalHighNonLow":{"name":"TypeError","message":"The encoded data was not valid for encoding utf-16le","code":"ERR_ENCODING_INVALID_ENCODED_DATA"},"u16FatalStreamHigh":["","𐀀"],"sdUtf8Bytes":{"parts":[[97],[],[233],[],[],[20013],[],[],[],[128512],[122]],"same":true},"sdUtf8Splits":[true,true,true,true,true,true,true,true,true,true,true,true],"sdUtf8Held":[[97],"f09f0000",2,4,[65533]],"sdUtf8Invalid":[[65533,824],[65533,65533,65533,65533,65533,65533],[65533,65533,65533],[65533,65]],"sdUtf8InvalidSplits":[["65533,824","65533,824","65533,824","65533,824"],["65533,65533,65533,65533,65533,65533","65533,65533,65533,65533,65533,65533","65533,65533,65533,65533,65533,65533","65533,65533,65533,65533,65533,65533","65533,65533,65533,65533,65533,65533","65533,65533,65533,65533,65533,65533","65533,65533,65533,65533,65533,65533"],["65533,65533,65533","65533,65533,65533","65533,65533,65533","65533,65533,65533"],["65533,65533,65533,65533","65533,65533,65533,65533","65533,65533,65533,65533","65533,65533,65533,65533","65533,65533,65533,65533"],["65533,65533,65533,65533","65533,65533,65533,65533","65533,65533,65533,65533","65533,65533,65533,65533","65533,65533,65533,65533"]],"sdString":[97,233,20013],"sdLatin1":[65,233,255],"sdAscii":[65,105,127],"sdHex":"000fff","sdBase64":["YWJj","ZGU="],"sdBase64Splits":["YWJjZGU=","YWJjZGU=","YWJjZGU=","YWJjZGU="],"sdU16":[[],[65,66],[]],"sdU16Surrogate":[[],[65536],[]],"sdU16Astral":[128512]}"#;

const GAP_BODY: &str = r#"
// Known native-tier gaps: WHATWG replacement semantics that the Rust
// `utf8_decode` / `utf8_decode_chunk` fallbacks get wrong today.
function cps(text) { const out = []; for (const ch of text) out.push(ch.codePointAt(0)); return out; }
function u8(list) { return Uint8Array.from(list); }
function attempt(fn) { try { return { ok: fn() }; } catch (e) { return { name: e && e.name, message: e && e.message, code: e && e.code }; } }
const out = {};
const cases = [
  [0xe0, 0x80, 0x80],
  [0xed, 0xa0, 0x80],
  [0xf4, 0x90, 0x80, 0x80],
  [0xf0, 0x80, 0x80, 0x80],
  [0xe1, 0x80, 0x41],
  [0xe1, 0x41],
  [0xf1, 0xcc, 0xb8],
  [0xed, 0xa0, 0xb5, 0xed, 0xb0, 0x8d],
];
out.cases = cases.map(function (bytes) {
  return cps(new TextDecoder().decode(u8(bytes)));
});
out.fatalOverlong = attempt(function () {
  return new TextDecoder("utf-8", { fatal: true }).decode(u8([0xe0, 0x80, 0x80]));
});
out.streamOverlong = (function () {
  const d = new TextDecoder();
  return [
    cps(d.decode(u8([0xe0]), { stream: true })),
    cps(d.decode(u8([0x80]), { stream: true })),
    cps(d.decode(u8([0x80]))),
  ];
})();
"#;

const EXPECTED_GAP: &str = r#"{"cases":[[65533,65533,65533],[65533,65533,65533],[65533,65533,65533,65533],[65533,65533,65533,65533],[65533,65],[65533,65],[65533,824],[65533,65533,65533,65533,65533,65533]],"fatalOverlong":{"name":"TypeError","message":"The encoded data was not valid for encoding utf-8","code":"ERR_ENCODING_INVALID_ENCODED_DATA"},"streamOverlong":[[],[65533,65533],[65533]]}"#;

// ---------------------------------------------------------------------------
// Main corpus
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn textcodec_node_parity_corpus() {
    let rt = new_runtime().await;
    let fixture = corpus_fixture(CORPUS_BODY, "__tcCorpus");
    let actual = run_fixture(&rt, "corpus", &fixture, "__tcCorpus").await;
    assert_corpus(&actual, EXPECTED_CORPUS);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "buffer_ops utf8_decode/utf8_decode_chunk replacement-semantics fix pending (lead wiring)"]
async fn textcodec_known_native_gaps() {
    let rt = new_runtime().await;
    let fixture = corpus_fixture(GAP_BODY, "__tcGap");
    let actual = run_fixture(&rt, "gaps", &fixture, "__tcGap").await;
    assert_corpus(&actual, EXPECTED_GAP);
}

// ---------------------------------------------------------------------------
// Hot-path timing (same-profile before/after; ignored by default)
// ---------------------------------------------------------------------------

const BENCH_TS: &str = r#"
import { performance } from "node:perf_hooks";
import { StringDecoder } from "node:string_decoder";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const text = "héllo wörld — 你好世界 ".repeat(40);
    const encoder = new TextEncoder();
    const bytes = Buffer.from(text, "utf8");
    const view = new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const decoder = new TextDecoder();
    const sd = new StringDecoder("utf8");
    const results: any = { byteLength: bytes.length };
    function bench(name: string, fn: any) {
      for (let i = 0; i < 500; i++) fn();
      let n = 16;
      let dt = 0;
      for (let round = 0; round < 25; round++) {
        const t0 = performance.now();
        for (let i = 0; i < n; i++) fn();
        dt = performance.now() - t0;
        if (dt >= 40) break;
        n *= 2;
      }
      let best = dt * 1e6 / n;
      for (let run = 0; run < 5; run++) {
        n = Math.max(16, Math.floor(n / 2));
        const t0 = performance.now();
        for (let i = 0; i < n; i++) fn();
        const sample = (performance.now() - t0) * 1e6 / n;
        if (sample < best) best = sample;
      }
      results[name] = best;
    }
    bench("decode", () => decoder.decode(bytes));
    bench("decode-u8", () => decoder.decode(view));
    bench("encode", () => encoder.encode(text));
    bench("write", () => sd.write(bytes));
    (globalThis as any).__tcBench = results;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
#[ignore = "timing comparison; run with --include-ignored --nocapture"]
async fn textcodec_hot_path_timing() {
    let rt = new_runtime().await;
    let v = run_fixture(&rt, "bench", BENCH_TS, "__tcBench").await;
    println!("textcodec bench: {v}");
    assert!(v["decode"].as_f64().unwrap_or(0.0) > 0.0);
}
