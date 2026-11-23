//! Differential consistency matrix (roadmap §11.3): the same fixture script
//! runs under the real Node binary found on PATH **and** under `pir js`, and
//! every deterministic `key: value` stdout line must agree.
//!
//! Ignored by default (requires a local node); run with:
//! `cargo test --test differential_node -- --ignored`
//! When no node is on PATH every case is skipped, so CI without node stays
//! green. Fixtures are written to a temp workspace at runtime (the hygiene
//! gate forbids `.js` sources in the repository).
//!
//! Scope discipline: cases whose real-Node behavior pir deliberately does not
//! chase (HANDOFF.md §2 tiering, e.g. full Intl formatting) carry a
//! `skip_reason` and are reported as skipped — never counted as failures.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Workspace {
    dir: PathBuf,
    _agent_dir: AgentDirGuard,
}

struct AgentDirGuard(PathBuf);

impl Drop for AgentDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Workspace {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-differential-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_millis()
        ));
        std::fs::create_dir_all(&dir).expect("workspace create");
        let agent_dir = dir.join(".pir-agent");
        std::fs::create_dir_all(&agent_dir).expect("agent dir create");
        Self {
            dir,
            _agent_dir: AgentDirGuard(agent_dir),
        }
    }

    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, source).expect("fixture write");
        path
    }
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn find_node() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(if cfg!(windows) { "node.exe" } else { "node" });
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Parse `key: value` lines (first `: ` splits; later duplicates overwrite).
fn parse_lines(stdout: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in stdout.lines() {
        if let Some((key, value)) = line.split_once(": ") {
            map.insert(key.trim().to_string(), value.to_string());
        }
    }
    map
}

struct Case {
    name: &'static str,
    script: &'static str,
    /// Tiering discipline: real-Node behavior pir does not chase — reported
    /// as skipped, never a failure.
    skip_reason: Option<&'static str>,
}

const CASES: &[Case] = &[
    Case {
        name: "path",
        script: r#"
const path = require("path");
console.log("join:", path.join("a", "b", "..", "c"));
console.log("resolve:", path.basename(path.resolve("x/y")));
console.log("basename:", path.basename("/dir/file.txt"));
console.log("dirname:", path.dirname("/dir/file.txt"));
console.log("extname:", path.extname("a/b.tar.gz"));
console.log("relative:", path.relative("/a/b", "/a/c"));
console.log("posix-join:", path.posix.join("a", "b"));
console.log("win32-join:", path.win32.join("a", "b"));
console.log("isAbsolute:", path.isAbsolute("/x"));
console.log("normalize:", path.normalize("a//b/../c/"));
console.log("win32-normalize:", path.win32.normalize("C:\\a\\..\\b\\\\c\\"));
console.log("win32-unc:", path.win32.normalize("\\\\server\\share\\x\\..\\y"));
console.log("win32-relative:", path.win32.relative("C:\\a\\b", "C:\\a\\c"));
console.log("win32-reserved:", path.win32.normalize("C:\\CON"));
console.log("posix-normalize:", path.posix.normalize("/a//b/../c/"));
console.log("posix-relative:", path.posix.relative("/a/b/c", "/a/b/d/e"));
console.log("posix-resolve:", path.posix.resolve("/a", "b", "..", "c"));
console.log("posix-join-dup:", path.posix.join("a/", "/b", "//c"));
"#,
        skip_reason: None,
    },
    Case {
        name: "util",
        script: r#"
const util = require("util");
console.log("format-pct:", util.format("%d %s %j", 42, "str", { a: 1 }));
console.log("format-plain:", util.format("plain", 1, null));
console.log("inspect-object:", util.inspect({ a: 1 }));
console.log("inspect-nested:", util.inspect({ list: [1, 2], when: null }));
console.log("types-isdate:", util.types.isDate(new Date()));
console.log("types-isarray:", util.types.isUint8Array(new Uint8Array(2)));
console.log("promisify:", typeof util.promisify(setTimeout) === "function");
"#,
        skip_reason: None,
    },
    Case {
        name: "url",
        script: r##"
const url = require("url");
const u = new URL("https://example.com:8443/p?q=1#frag");
console.log("protocol:", u.protocol);
console.log("pathname:", u.pathname);
console.log("search:", u.search);
console.log("hash:", u.hash);
console.log("param:", u.searchParams.get("q"));
console.log("parsed-format:", url.format(url.parse("http://a.b/c?d=e")));
console.log("resolve-file:", url.resolve("file:///a/x/y", "z"));
console.log("resolve-http:", url.resolve("https://a.b/c/d", "e"));
console.log("resolve-http-abs:", url.resolve("https://a.b/c/d", "/f?q"));
console.log("pathtourl-proto:", url.pathToFileURL("a b.txt").protocol);
console.log("filetopath-end:", url.fileURLToPath(url.pathToFileURL("a b.txt")).endsWith("a b.txt"));
console.log("httpbase-abs:", new URL("/path", "https://example.com/sub/").href);
console.log("httpbase-rel:", new URL("path", "https://example.com/sub/").href);
console.log("httpbase-dotdot:", new URL("../x", "https://example.com/a/b/c").href);
console.log("httpbase-empty:", new URL("", "https://example.com/p?q=1").href);
console.log("httpbase-query:", new URL("?y=2", "https://example.com/p?q=1").href);
console.log("httpbase-frag:", new URL("#f2", "https://example.com/p?q=1#f1").href);
console.log("httpbase-creds:", new URL("/p", "https://u:p@example.com:8443/sub/").href);
console.log("httpbase-barehost:", new URL("x", "https://example.com").href);
console.log("httpbase-proto-rel:", new URL("//other.com/p", "https://a.com/").href);
// Corpus: every component, including the shapes the native fast path must
// match (credentials, default ports, IDN, IPv6, dot segments, empty
// query/fragment, percent-encoding, whitespace).
const corpus = [
  "http://example.com",
  "https://example.com:8443/p/q?x=1&y=2#f",
  "https://u:p@example.com:8443/a/../b/./c?q=%E4%B8%AD#h",
  "https://example.com:443/x",
  "http://example.com:80/x",
  "HTTP://EXAMPLE.COM/Path?Q=1",
  "https://[::1]:8080/x?y",
  "https://bücher.example/straße?q=1",
  "http://user:pa%20ss@example.com/",
  "https://example.com/",
  "https://example.com/?",
  "https://example.com/#",
  "https://example.com/p?",
  "https://example.com/p#",
  "https://example.com/a/b/../../../c",
  "https://example.com/a%2Fb/c?d=%2F",
  "  https://example.com/trim  ",
  "http://example.com:0/x",
];
for (const raw of corpus) {
  try {
    const v = new URL(raw);
    console.log("corpus:", raw, "=>",
      [v.href, v.protocol, v.username, v.password, v.host, v.hostname, v.port, v.pathname, v.search, v.hash].join("|"));
  } catch (error) {
    console.log("corpus:", raw, "=> ERR", error.code || error.name);
  }
}
const bases = ["https://example.com/a/b/c?q=1#f", "http://u:p@example.com:8080/dir/"];
const rels = ["", ".", "..", "./x", "../x", "../../y", "?q=2", "#g", "//other.com/z", "/abs", "rel", "a/../b"];
for (const base of bases) {
  for (const rel of rels) {
    const v = new URL(rel, base);
    console.log("base:", base, "+", rel, "=>", v.href);
  }
}
        "##,
        skip_reason: None,
    },
    Case {
        name: "querystring",
        script: r#"
const qs = require("querystring");
console.log("stringify:", qs.stringify({ a: "1 2", b: "&=" }));
const parsed = qs.parse("x=1%20&y=%26z");
console.log("parse-x:", parsed.x);
console.log("parse-y:", parsed.y);
console.log("escape:", qs.escape("a b/c"));
"#,
        skip_reason: None,
    },
    Case {
        name: "url_corpus_indexed",
        script: r##"
// The `url` case logs repeated `corpus:`/`base:` keys (later lines overwrite
// earlier ones in the harness map). This variant indexes every line so the
// whole WHATWG corpus and the relative-reference matrix are actually compared.
const corpus = [
  "http://example.com",
  "https://example.com:8443/p/q?x=1&y=2#f",
  "https://u:p@example.com:8443/a/../b/./c?q=%E4%B8%AD#h",
  "https://example.com:443/x",
  "http://example.com:80/x",
  "HTTP://EXAMPLE.COM/Path?Q=1",
  "https://[::1]:8080/x?y",
  "https://bücher.example/straße?q=1",
  "http://user:pa%20ss@example.com/",
  "https://example.com/",
  "https://example.com/?",
  "https://example.com/#",
  "https://example.com/p?",
  "https://example.com/p#",
  "https://example.com/a/b/../../../c",
  "https://example.com/a%2Fb/c?d=%2F",
  "  https://example.com/trim  ",
  "http://example.com:0/x",
  "data:text/plain,hi",
  "file:///tmp/a b#x?y",
  "ftp://Example.COM:21/a",
  "ws://example.com:80/x",
  "wss://example.com:443/x",
  "blob:https://example.com/uuid",
  "http://",
];
for (let i = 0; i < corpus.length; i++) {
  const raw = corpus[i];
  try {
    const v = new URL(raw);
    console.log("corpus" + i + ":",
      [v.href, v.protocol, v.username, v.password, v.host, v.hostname, v.port,
       v.pathname, v.search, v.hash, v.origin].join("|"));
  } catch (error) {
    console.log("corpus" + i + ":", "ERR " + (error.code || error.name));
  }
}
const bases = ["https://example.com/a/b/c?q=1#f", "http://u:p@example.com:8080/dir/"];
const rels = ["", ".", "..", "./x", "../x", "../../y", "?q=2", "#g", "//other.com/z", "/abs", "rel", "a/../b"];
let n = 0;
for (const base of bases) {
  for (const rel of rels) {
    console.log("baseline" + n + ":", new URL(rel, base).href);
    n += 1;
  }
}
"##,
        skip_reason: None,
    },
    Case {
        name: "url_components",
        script: r##"
const u = new URL("https://user:pa%20ss@example.com:8443/a/../b?q=1&r=2#frag");
console.log("u-href:", u.href);
console.log("u-origin:", u.origin);
console.log("u-protocol:", u.protocol);
console.log("u-username:", u.username);
console.log("u-password:", u.password);
console.log("u-host:", u.host);
console.log("u-hostname:", u.hostname);
console.log("u-port:", u.port);
console.log("u-pathname:", u.pathname);
console.log("u-search:", u.search);
console.log("u-hash:", u.hash);
console.log("u-json:", JSON.stringify(u));
console.log("u-string:", u.toString());
console.log("u-ownkeys:", Object.keys(u).length);
console.log("u-canparse:", URL.canParse("https://x/"), URL.canParse("nope"));
console.log("u-parse-static:", URL.parse("https://x/y?a=1").href, URL.parse("nope"));
console.log("u-rel:", new URL("../x?q=2#g", "https://example.com/a/b/c?q=1#f").href);
console.log("u-empty-path:", new URL("http://x").href, new URL("http://x").pathname);
console.log("u-empty-query:", JSON.stringify(new URL("https://example.com/?").search), new URL("https://example.com/?").href);
console.log("u-empty-hash:", JSON.stringify(new URL("https://example.com/#").hash), new URL("https://example.com/#").href);
console.log("u-origin-file:", new URL("file:///tmp/x").origin);
console.log("u-origin-data:", new URL("data:text/plain,hi").origin);
console.log("u-origin-ftp:", new URL("ftp://example.com/a").origin);
console.log("u-origin-ws:", new URL("ws://example.com/x").origin);
console.log("u-origin-wss:", new URL("wss://example.com:443/x").origin);
console.log("u-origin-blob:", new URL("blob:https://example.com/id").origin);
const s = new URL("https://example.com/a/b?q=1#f");
s.pathname = "/c/d";
console.log("u-set-pathname:", s.href);
s.search = "?z=9";
console.log("u-set-search:", s.href);
s.hash = "#h2";
console.log("u-set-hash:", s.href);
s.protocol = "http:";
console.log("u-set-protocol:", s.href);
console.log("u-baseobj:", new URL("x", new URL("https://a.com/d/e/")).href);
try { new URL("http://x", null); } catch (error) { console.log("u-null-base:", error.code, JSON.stringify(error.base)); }
try { new URL("./x"); } catch (error) { console.log("u-rel-nobase:", error.code, JSON.stringify(error.message), JSON.stringify(error.input)); }
try { new URL("http://"); } catch (error) { console.log("u-invalid:", error.code, JSON.stringify(error.message), JSON.stringify(error.input)); }
try { new URL("https://x/", "not a url"); } catch (error) { console.log("u-bad-base:", error.code, JSON.stringify(error.input), JSON.stringify(error.base)); }
console.log("u-tag:", Object.prototype.toString.call(u));
console.log("u-tojson-type:", typeof u.toJSON());
"##,
        skip_reason: None,
    },
    Case {
        name: "url_search_params",
        script: r#"
const sp = new URLSearchParams("a=1&b=x+y&a=3&c=%E4%B8%AD");
console.log("sp-get:", sp.get("a"), sp.get("missing"));
console.log("sp-getall:", JSON.stringify(sp.getAll("a")));
console.log("sp-size:", sp.size);
console.log("sp-has:", sp.has("a"), sp.has("a", "3"), sp.has("a", "1"));
console.log("sp-tostring:", sp.toString());
console.log("sp-entries:", JSON.stringify([...sp.entries()]));
console.log("sp-keys:", JSON.stringify([...sp.keys()]));
console.log("sp-values:", JSON.stringify([...sp.values()]));
console.log("sp-spread:", JSON.stringify([...sp]));
const forEachOut = [];
sp.forEach((value, key, obj) => { forEachOut.push(key + "=" + value + ":" + (obj === sp)); });
console.log("sp-foreach:", forEachOut.join(","));
const s2 = new URLSearchParams("b=1&a=2&b=3");
s2.set("b", "9");
s2.append("d", "4");
console.log("sp-set:", s2.toString());
s2.sort();
console.log("sp-sort:", s2.toString());
s2.delete("a");
console.log("sp-delete:", s2.toString());
s2.delete("b", "9");
console.log("sp-delete-value:", s2.toString());
console.log("sp-iter-ident:", URLSearchParams.prototype[Symbol.iterator] === URLSearchParams.prototype.entries);
console.log("sp-tostringtag:", Object.prototype.toString.call(s2));
console.log("sp-encode-set:", new URLSearchParams([["k", "~!*'() -._"], ["a b", "c+d"], ["%", "%"]]).toString());
console.log("sp-invalid-pct:", new URLSearchParams("a=%E4%B8%AD%ZZ").get("a"));
console.log("sp-bad-utf8:", new URLSearchParams("a=%E4%B8").get("a"));
console.log("sp-qmark:", new URLSearchParams("?a=1").toString());
console.log("sp-two-qmark:", new URLSearchParams("??a=1").toString());
console.log("sp-noeq:", new URLSearchParams("abc").toString());
console.log("sp-empty-name:", new URLSearchParams("=v").toString());
console.log("sp-dups:", new URLSearchParams([["k", "1"], ["k", "2"]]).toString());
console.log("sp-record:", new URLSearchParams({ a: 1, b: "x y" }).toString());
console.log("sp-num:", new URLSearchParams(5).toString());
console.log("sp-null:", new URLSearchParams(null).toString());
console.log("sp-lone-get:", JSON.stringify(new URLSearchParams("a=\uD800").get("a")));
console.log("sp-lone-tostring:", new URLSearchParams([["a", "\uD800"]]).toString());
const u = new URL("https://example.com/p?a=1&b=2");
console.log("sp-url:", u.searchParams.get("b"), u.searchParams === u.searchParams);
console.log("u-lone-path:", new URL("https://x/\uD800").href);
try { new URL("http://a\uD800b/"); } catch (error) { console.log("u-lone-host:", error.code); }
"#,
        skip_reason: None,
    },
    Case {
        name: "querystring_details",
        script: r#"
const qs = require("querystring");
console.log("qs-parse-basic:", JSON.stringify(qs.parse("a=1&b=2&a=3")));
console.log("qs-parse-plus:", JSON.stringify(qs.parse("a=x+y&b=%2B")));
console.log("qs-parse-noeq:", JSON.stringify(qs.parse("abc&d=")));
console.log("qs-parse-empty:", JSON.stringify(qs.parse("")));
console.log("qs-parse-nonstr:", JSON.stringify(qs.parse(5)));
console.log("qs-parse-multi-eq:", JSON.stringify(qs.parse("a=b=c")));
console.log("qs-parse-empty-chunks:", JSON.stringify(qs.parse("&&a=1&&")));
console.log("qs-parse-invalid:", JSON.stringify(qs.parse("a=%2Q&b=%E4%B8")));
console.log("qs-parse-max0:", JSON.stringify(qs.parse("a=1&b=2", "&", "=", { maxKeys: 0 })));
console.log("qs-parse-max1:", JSON.stringify(qs.parse("a=1&b=2&c=3", "&", "=", { maxKeys: 1 })));
console.log("qs-parse-maxneg:", JSON.stringify(qs.parse("a=1&b=2&c=3", "&", "=", { maxKeys: -1 })));
console.log("qs-parse-max-empty-first:", JSON.stringify(qs.parse("&&a=1", "&", "=", { maxKeys: 1 })));
console.log("qs-parse-sep:", JSON.stringify(qs.parse("a=1;b=2", ";")));
console.log("qs-parse-eq:", JSON.stringify(qs.parse("a:1&b:2", "&", ":")));
console.log("qs-parse-multi-sep:", JSON.stringify(qs.parse("a=1||b=2||c=3", "||")));
console.log("qs-parse-partial:", JSON.stringify(qs.parse("a=1|x|b=2", "||")));
console.log("qs-parse-custom-decode:", JSON.stringify(qs.parse("a=1&b=2", "&", "=", { decodeURIComponent: (s) => "[" + s + "]" })));
console.log("qs-proto-null:", Object.getPrototypeOf(qs.parse("a=1")) === null);
const dup = qs.parse("a=1&a=2");
console.log("qs-dup-array:", Array.isArray(dup.a), dup.a.join("|"));
console.log("qs-stringify-basic:", qs.stringify({ a: 1, b: "x y", c: [1, 2] }));
console.log("qs-stringify-empty-array:", qs.stringify({ a: [], b: 1 }));
console.log("qs-stringify-nan:", qs.stringify({ a: NaN, b: Infinity }));
console.log("qs-stringify-bigint:", qs.stringify({ a: 10n }));
console.log("qs-stringify-1e21:", qs.stringify({ a: 1e21, b: 1e20 }));
console.log("qs-stringify-sep:", qs.stringify({ a: 1, b: 2 }, ";", ":"));
console.log("qs-stringify-custom:", qs.stringify({ a: "x y" }, "&", "=", { encodeURIComponent: (s) => "[" + s + "]" }));
console.log("qs-stringify-null:", qs.stringify(null));
console.log("qs-escape:", qs.escape("a b/c"), qs.escape("!'()*~"), qs.escape("中"));
console.log("qs-unescape:", qs.unescape("a%20b"), qs.unescape("a+b"), qs.unescape("%2Q"), qs.unescape("%E4%B8%AD%ZZ"), qs.unescape("%E4%B8"));
console.log("qs-decode-alias:", qs.decode === qs.parse, qs.encode === qs.stringify);
try { qs.escape("\uD800"); } catch (error) { console.log("qs-escape-lone:", error.code); }
console.log("qs-override:", (() => {
  const original = qs.unescape;
  qs.unescape = (s) => "[" + s + "]";
  const result = qs.parse("a=1").a;
  qs.unescape = original;
  return result;
})());
"#,
        skip_reason: None,
    },
    Case {
        name: "string_decoder",
        script: r#"
const { StringDecoder } = require("string_decoder");
const dec = new StringDecoder("utf8");
// A 3-byte UTF-8 character split across two chunks must reassemble.
const whole = Buffer.from("é中", "utf8");
const mid = whole.length === 4 ? 2 : 1;
console.log("split:", dec.write(whole.subarray(0, mid)) + dec.write(whole.subarray(mid)) + dec.end());
console.log("ascii:", new StringDecoder("utf8").write(Buffer.from("plain")) + new StringDecoder("utf8").end());
"#,
        // base64 StringDecoder support is outside the decoder shim's tier
        // (TextDecoder pad: utf-8/latin1/utf-16le only) — utf8-across-chunks
        // is the documented compatibility target.
        skip_reason: None,
    },
    Case {
        name: "buffer",
        script: r#"
const b = Buffer.from("héllo", "utf8");
console.log("hex:", b.toString("hex"));
console.log("base64:", b.toString("base64"));
console.log("latin1-roundtrip:", Buffer.from(b.toString("latin1"), "latin1").length === b.length);
console.log("utf16le:", Buffer.from("ab", "utf16le").toString("hex"));
console.log("bytelen:", Buffer.byteLength("中", "utf8"));
console.log("alloc-fill:", Buffer.alloc(3, 7).toString("hex"));
console.log("concat:", Buffer.concat([Buffer.from("ab"), Buffer.from("cd")]).toString());
console.log("isbuffer:", Buffer.isBuffer(b));
console.log("compare:", Buffer.compare(Buffer.from("abc"), Buffer.from("abd")), Buffer.compare(Buffer.from("abc"), Buffer.from("abc")), Buffer.compare(Buffer.from("abc"), Buffer.from("abcd")));
console.log("swap16:", Buffer.from([1, 2, 3, 4]).swap16().toString("hex"));
console.log("swap32:", Buffer.from([1, 2, 3, 4]).swap32().toString("hex"));
console.log("swap64:", Buffer.from([1, 2, 3, 4, 5, 6, 7, 8]).swap64().toString("hex"));
"#,
        skip_reason: None,
    },
    Case {
        name: "crypto",
        script: r#"
const crypto = require("crypto");
console.log("sha256:", crypto.createHash("sha256").update("abc").digest("hex"));
console.log("md5:", crypto.createHash("md5").update("abc").digest("hex"));
console.log("hmac:", crypto.createHmac("sha256", "key").update("message").digest("hex"));
console.log("random-len:", crypto.randomBytes(16).length);
console.log("uuid-shape:", /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(crypto.randomUUID()));
"#,
        skip_reason: None,
    },
    Case {
        name: "zlib",
        script: r#"
const zlib = require("zlib");
const input = Buffer.from("hello-zlib-roundtrip", "utf8");
const gz = zlib.gzipSync(input);
const back = zlib.gunzipSync(gz);
console.log("gzip-roundtrip:", back.toString() === input.toString());
console.log("gzip-shrunk:", gz.length < 64);
const deflated = zlib.deflateSync(input);
console.log("deflate-roundtrip:", zlib.inflateSync(deflated).toString() === input.toString());
"#,
        skip_reason: None,
    },
    Case {
        name: "os",
        script: r#"
const os = require("os");
console.log("platform-is-string:", typeof os.platform() === "string");
console.log("arch:", os.arch());
console.log("endianness:", os.endianness());
console.log("eol:", JSON.stringify(os.EOL));
console.log("homedir-nonempty:", os.homedir().length > 0);
console.log("tmpdir-nonempty:", os.tmpdir().length > 0);
console.log("cpus-count:", os.cpus().length > 0);
console.log("freemem-number:", typeof os.freemem() === "number");
"#,
        skip_reason: None,
    },
    Case {
        name: "events",
        script: r#"
const { EventEmitter } = require("events");
const emitter = new EventEmitter();
const order = [];
emitter.on("tick", () => order.push("first"));
emitter.on("tick", () => order.push("second"));
emitter.once("once", () => order.push("only-once"));
emitter.emit("tick");
emitter.emit("once");
emitter.emit("once");
console.log("order:", order.join(","));
console.log("listener-count:", emitter.listenerCount("tick"));
emitter.removeAllListeners("tick");
console.log("after-remove:", emitter.listenerCount("tick"));
"#,
        skip_reason: None,
    },
    Case {
        name: "stream",
        script: r#"
const { Readable } = require("stream");
const r = new Readable({ read() {} });
let acc = "";
let ended = false;
r.on("data", (chunk) => { acc += chunk.toString(); });
// The end listener is attached before the stream is terminated: the pir
// minimal stream pad emits "end" synchronously on push(null) (documented
// minimal-pad timing divergence), while real node defers it — attaching
// early is correct on both.
r.on("end", () => {
  ended = true;
  console.log("stream-acc:", acc);
  console.log("stream-ended:", ended);
  process.exit(0);
});
r.push("chunk-one-");
r.push("chunk-two");
r.push(null);
"#,
        skip_reason: None,
    },
    Case {
        name: "child_process",
        script: r#"
const { spawnSync } = require("child_process");
if (process.env.PIR_DIFF_CHILD === "1") {
  console.log("child-echo");
  process.exit(0);
}
const base = process.execPath.split(/[\\/]/).pop() || "";
const isPir = /pir/i.test(base);
const args = isPir ? ["js", __filename] : [__filename];
const result = spawnSync(process.execPath, args, {
  env: Object.assign({}, process.env, { PIR_DIFF_CHILD: "1" }),
});
console.log("child-status:", result.status);
console.log("child-stdout:", String(result.stdout).trim());
"#,
        skip_reason: None,
    },
    Case {
        name: "timers",
        script: r#"
const { setImmediate } = require("timers");
const order = [];
setTimeout(() => order.push("t20"), 20);
setTimeout(() => order.push("t5"), 5);
let intervalTicks = 0;
const interval = setInterval(() => { intervalTicks += 1; if (intervalTicks === 3) clearInterval(interval); }, 5);
setImmediate(() => order.push("immediate"));
setTimeout(() => {
  console.log("timer-order:", order.join(","));
  console.log("interval-ticks:", intervalTicks);
  process.exit(0);
}, 60);
"#,
        skip_reason: None,
    },
    Case {
        name: "vm",
        script: r#"
const vm = require("vm");
const sb = { x: 1, emit(v) { return v * 2; } };
const ctx = vm.createContext(sb);
console.log("vm-completion:", vm.runInNewContext("1+1"));
console.log("vm-read:", vm.runInContext("x + 1", ctx));
console.log("vm-member:", vm.runInContext("emit(21)", ctx));
vm.runInContext("globalThis.newG = 5", ctx);
console.log("vm-write:", sb.newG);
console.log("vm-isolated-process:", vm.runInContext("typeof process", ctx));
console.log("vm-isolated-buffer:", vm.runInContext("typeof Buffer", ctx));
console.log("vm-json:", vm.runInContext("JSON.stringify({a:1})", ctx));
console.log("vm-is-context:", vm.isContext(sb) && !vm.isContext({}));
let topReturn = "accepted";
try { vm.runInNewContext("return 1"); } catch (e) { topReturn = e.constructor.name; }
console.log("vm-top-return:", topReturn);
console.log("vm-script:", new vm.Script("x * 10").runInContext(ctx));
console.log("vm-create-script:", vm.createScript("2+3").runInThisContext());
"#,
        skip_reason: None,
    },
    Case {
        name: "http",
        script: r#"
const http = require("http");
const server = http.createServer((req, res) => {
  res.setHeader("x-diff", "yes");
  res.end("pong:" + (req.url || "/"));
});
server.listen(0, "127.0.0.1", () => {
  const port = server.address().port;
  http.get({ host: "127.0.0.1", port, path: "/ping" }, (res) => {
    let body = "";
    res.on("data", (c) => { body += c; });
    res.on("end", () => {
      console.log("http-status:", res.statusCode);
      console.log("http-header:", res.headers["x-diff"]);
      console.log("http-body:", body);
      server.close(() => process.exit(0));
    });
  });
});
"#,
        skip_reason: None,
    },
    Case {
        name: "intl",
        script: r#"
console.log("intl-shape:", typeof Intl === "object");
"#,
        // Full Intl.NumberFormat/DateTimeFormat formatting is a documented
        // non-goal (HANDOFF.md §2.6): the engine only carries a Segmenter pad,
        // so anything beyond the intrinsic shape is out of tier.
        skip_reason: Some("Intl 完整格式化为分级不追面(仅 Segmenter 码点近似垫)"),
    },
];

#[test]
#[ignore = "differential matrix: requires a real node on PATH"]
fn differential_matrix_against_real_node() {
    let node = match find_node() {
        Some(node) => node,
        None => {
            // No node on PATH: the whole matrix skips (CI without node).
            println!("[skip] no node on PATH — differential matrix skipped");
            return;
        }
    };
    let pir = env!("CARGO_BIN_EXE_pir");

    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for case in CASES {
        if let Some(reason) = case.skip_reason {
            skipped += 1;
            println!("[skip] {}: {reason}", case.name);
            continue;
        }
        let ws = Workspace::new(case.name);
        ws.write("case.cjs", case.script);

        let node_output = Command::new(&node)
            .arg("case.cjs")
            .current_dir(&ws.dir)
            .output()
            .expect("node should start");
        let pir_output = Command::new(pir)
            .env("PIR_AGENT_DIR", &ws._agent_dir.0)
            .args(["js", "case.cjs"])
            .current_dir(&ws.dir)
            .output()
            .expect("pir binary should start");

        if !node_output.status.success() {
            failed += 1;
            failures.push(format!(
                "{}: node itself failed ({}):\n{}",
                case.name,
                node_output.status,
                stderr_of(&node_output)
            ));
            println!("[fail] {} (node harness error)", case.name);
            continue;
        }
        if !pir_output.status.success() {
            failed += 1;
            failures.push(format!(
                "{}: pir js failed ({}):\n--- stderr ---\n{}\n--- node stdout ---\n{}\n--- pir stdout ---\n{}",
                case.name,
                pir_output.status,
                stderr_of(&pir_output),
                stdout_of(&node_output),
                stdout_of(&pir_output)
            ));
            println!("[fail] {} (pir exit)", case.name);
            continue;
        }

        let node_lines = parse_lines(&stdout_of(&node_output));
        let pir_lines = parse_lines(&stdout_of(&pir_output));
        let mut case_failures: Vec<String> = Vec::new();
        for (key, want) in &node_lines {
            match pir_lines.get(key) {
                Some(got) if got == want => {}
                Some(got) => {
                    case_failures.push(format!("  {key}:\n    node: {want}\n    pir:  {got}"))
                }
                None => case_failures.push(format!(
                    "  {key}:\n    node: {want}\n    pir:  <line missing>"
                )),
            }
        }
        for key in pir_lines.keys() {
            if !node_lines.contains_key(key) {
                case_failures.push(format!(
                    "  {key}:\n    node: <line missing>\n    pir:  {}",
                    pir_lines[key]
                ));
            }
        }
        if case_failures.is_empty() {
            passed += 1;
            println!("[pass] {}", case.name);
        } else {
            failed += 1;
            failures.push(format!(
                "{} ({} mismatches):\n{}\n--- raw node stdout ---\n{}\n--- raw pir stdout ---\n{}",
                case.name,
                case_failures.len(),
                case_failures.join("\n"),
                stdout_of(&node_output),
                stdout_of(&pir_output)
            ));
            println!("[fail] {}", case.name);
        }
    }

    println!(
        "\ndifferential first-round: {} passed, {} failed, {} skipped (of {} cases)",
        passed,
        failed,
        skipped,
        CASES.len()
    );
    if !failures.is_empty() {
        panic!("differential mismatches:\n\n{}", failures.join("\n\n"));
    }
}
