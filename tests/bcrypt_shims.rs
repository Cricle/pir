//! `bcrypt` virtual package shim (`js/bcrypt_shim.js.txt`) + the host
//! EksBlowfish backend (`host_functions/bcrypt_fns.rs`): the standard bcrypt
//! test vectors run through the real shim, so a drift in either the JS face or
//! the Rust algorithm fails this gate.
//!
//! Vector provenance: the `$2a$`/`$2b$` set is OpenBSD's bcrypt test set (the
//! jBCrypt/crypt_blowfish vectors the `bcrypt` npm package and the Rust
//! `bcrypt` crate both carry — empty password, single chars, the full
//! lowercase alphabet, punctuation, the `U*U` family and the `C`/`X`/`.`-salted
//! cases); the long-password entries pin bcrypt's 72-byte truncation. Every
//! value below was re-verified on 2026-10-12 against both the native
//! `bcrypt@6.0.0` addon (`npm i bcrypt`, win32-x64 prebuild) and `bcryptjs`.

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// See tests/sqlite_shims.rs: fixtures are written at runtime (repository
/// hygiene gate forbids checked-in fixture artifacts).
fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-bcrypt-shims-{}-{test_name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    for (name, content) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture subdir");
        }
        std::fs::write(&path, content).expect("write fixture");
    }
    dir
}

fn test_config(cwd: &Path) -> ExtensionConfig {
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

async fn new_runtime_at(cwd: &Path) -> ExtensionRuntime {
    ExtensionRuntime::new(test_config(cwd))
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
    if global.is_empty() {
        return Value::Null;
    }
    rt.eval_json(&format!("JSON.stringify(globalThis.{global})"))
        .await
        .expect("read fixture results")
}

/// (password, salt, expected hash). The trailing four entries are the same
/// 72-byte password at three lengths plus an 80-byte one: bcrypt truncates the
/// key at 72 bytes, so all of them hash like their 72-byte prefix.
fn bcrypt_vectors() -> Vec<(String, &'static str, &'static str)> {
    let mut vectors: Vec<(String, &'static str, &'static str)> = vec![
        (
            String::new(),
            "$2a$06$DCq7YPn5Rq63x1Lad4cll.",
            "$2a$06$DCq7YPn5Rq63x1Lad4cll.TV4S6ytwfsfvkgY8jIucDrjc8deX1s.",
        ),
        (
            String::new(),
            "$2a$08$HqWuK6/Ng6sg9gQzbLrgb.",
            "$2a$08$HqWuK6/Ng6sg9gQzbLrgb.Tl.ZHfXLhvt/SgVyWhQqgqcZ7ZuUtye",
        ),
        (
            String::new(),
            "$2a$10$k1wbIrmNyFAPwPVPSVa/ze",
            "$2a$10$k1wbIrmNyFAPwPVPSVa/zecw2BCEnBwVS2GbrmgzxFUOqW9dk4TCW",
        ),
        (
            "a".to_string(),
            "$2a$06$m0CrhHm10qJ3lXRY.5zDGO",
            "$2a$06$m0CrhHm10qJ3lXRY.5zDGO3rS2KdeeWLuGmsfGlMfOxih58VYVfxe",
        ),
        (
            "abc".to_string(),
            "$2a$06$If6bvum7DFjUnE9p2uDeDu",
            "$2a$06$If6bvum7DFjUnE9p2uDeDu0YHzrHM6tf.iqN8.yx.jNN1ILEf7h0i",
        ),
        (
            "abcdefghijklmnopqrstuvwxyz".to_string(),
            "$2a$06$.rCVZVOThsIa97pEDOxvGu",
            "$2a$06$.rCVZVOThsIa97pEDOxvGuRRgzG64bvtJ0938xuqzv18d3ZpQhstC",
        ),
        (
            "~!@#$%^&*()      ~!@#$%^&*()PNBFRD".to_string(),
            "$2a$06$fPIsBO8qRqkjj273rfaOI.",
            "$2a$06$fPIsBO8qRqkjj273rfaOI.HtSV9jLDpTbZn782DC6/t7qT67P6FfO",
        ),
        (
            "U*U".to_string(),
            "$2a$05$CCCCCCCCCCCCCCCCCCCCC.",
            "$2a$05$CCCCCCCCCCCCCCCCCCCCC.E5YPO9kmyuRGyh0XouQYb4YMJKvyOeW",
        ),
        (
            "U*U*".to_string(),
            "$2b$05$CCCCCCCCCCCCCCCCCCCCC.",
            "$2b$05$CCCCCCCCCCCCCCCCCCCCC.VGOzA784oUp/Z0DY336zx7pLYAy0lwK",
        ),
        (
            "U*U*U".to_string(),
            "$2b$05$XXXXXXXXXXXXXXXXXXXXXO",
            "$2b$05$XXXXXXXXXXXXXXXXXXXXXOAcXxm9kjPGEMsLznoKqmqw7tc8WCx4a",
        ),
        (
            String::new(),
            "$2b$04$......................",
            "$2b$04$......................w74bL5gU7LSJClZClCa.Pkz14aTv/XO",
        ),
        (
            "abcdefghijklmnopqrstuvwxyz".to_string(),
            "$2b$12$.rCVZVOThsIa97pEDOxvGu",
            "$2b$12$.rCVZVOThsIa97pEDOxvGuOCRAorGxPEbhNQU0qLKvTFFoG70RtXC",
        ),
        (
            "pässwörd".to_string(),
            "$2a$06$If6bvum7DFjUnE9p2uDeDu",
            "$2a$06$If6bvum7DFjUnE9p2uDeDuI3QSqBqdSZ6ah4S51cfvPJDtLJU/ZS2",
        ),
        (
            "中文密码".to_string(),
            "$2b$04$.rCVZVOThsIa97pEDOxvGu",
            "$2b$04$.rCVZVOThsIa97pEDOxvGu0wjtIM8w.A9u85jWz9wJi8gjXQvyqIu",
        ),
    ];
    let seventy_two = "x".repeat(72);
    vectors.push((
        seventy_two.clone(),
        "$2a$05$CCCCCCCCCCCCCCCCCCCCC.",
        "$2a$05$CCCCCCCCCCCCCCCCCCCCC.D7RDOYXakZhsFmh/LdqQNx0kQ424AFS",
    ));
    vectors.push((
        "x".repeat(73),
        "$2a$05$CCCCCCCCCCCCCCCCCCCCC.",
        "$2a$05$CCCCCCCCCCCCCCCCCCCCC.D7RDOYXakZhsFmh/LdqQNx0kQ424AFS",
    ));
    vectors.push((
        "x".repeat(80),
        "$2b$05$CCCCCCCCCCCCCCCCCCCCC.",
        "$2b$05$CCCCCCCCCCCCCCCCCCCCC.D7RDOYXakZhsFmh/LdqQNx0kQ424AFS",
    ));
    vectors.push((
        "y".repeat(72),
        "$2b$05$CCCCCCCCCCCCCCCCCCCCC.",
        "$2b$05$CCCCCCCCCCCCCCCCCCCCC.4eQhVFG83RE7Ujg6.w.JlWjHZKEWAHC",
    ));
    vectors
}

#[tokio::test]
async fn bcrypt_standard_vectors_match_the_reference() {
    let dir = write_temp_fixture("vectors", &[]);
    let rt = new_runtime_at(&dir).await;
    let vectors = bcrypt_vectors();
    let json_vectors = serde_json::to_string(&vectors).expect("serialize vectors");
    let expected: Vec<&str> = vectors.iter().map(|entry| entry.2).collect();

    const FIXTURE: &str = r#"
import bcrypt from "bcrypt";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const vectors: any[] = __PIR_BCRYPT_VECTORS__;
    out.count = vectors.length;
    out.hashes = vectors.map((v) => bcrypt.hashSync(v[0], v[1]));
    out.compares = vectors.map((v, i) => bcrypt.compareSync(v[0], v[2]) === true);
    out.rounds = vectors.map((v) => bcrypt.getRounds(v[2]));
    // A wrong password never verifies, whichever vector it is checked
    // against (empty-password vector keeps the check free of the 72-byte
    // truncation semantics).
    out.mismatch = bcrypt.compareSync("definitely-not-the-password", vectors[0][2]);
    out.badHash = bcrypt.compareSync(vectors[0][0], "not-a-hash");
    // The census probe: hashSync("pw", 4) must mint a $2-prefixed hash.
    out.census = bcrypt.hashSync("pw", 4).startsWith("$2");
    out.genSaltShape = /^\$2b\$\d\d\$[./A-Za-z0-9]{22}$/.test(bcrypt.genSaltSync(4));
    globalThis.__bcryptVectors = out;
  });
};
"#;
    let fixture = FIXTURE.replace("__PIR_BCRYPT_VECTORS__", &json_vectors);
    let value = run_fixture(&rt, "bcrypt_vectors", &fixture, "__bcryptVectors").await;

    assert_eq!(value["count"], 18, "vector table must stay populated");
    assert_eq!(
        value["hashes"].as_array().map(Vec::len),
        Some(expected.len())
    );
    for (index, entry) in vectors.iter().enumerate() {
        assert_eq!(
            value["hashes"][index],
            json!(entry.2),
            "vector {index} ({:?} / {}) hash mismatch",
            entry.0,
            entry.1
        );
    }
    assert_eq!(
        value["compares"],
        json!(vec![true; expected.len()]),
        "every vector must verify through compareSync"
    );
    assert_eq!(
        value["rounds"],
        json!([6, 8, 10, 6, 6, 6, 6, 5, 5, 5, 4, 12, 6, 4, 5, 5, 5, 5])
    );
    assert_eq!(value["mismatch"], false);
    assert_eq!(value["badHash"], false);
    assert_eq!(value["census"], true);
    assert_eq!(value["genSaltShape"], true);
}

#[tokio::test]
async fn bcrypt_shim_surface_matches_the_package() {
    let dir = write_temp_fixture("surface", &[]);
    let rt = new_runtime_at(&dir).await;

    const FIXTURE: &str = r#"
import bcrypt from "bcrypt";
import * as ns from "bcrypt";
import { createRequire } from "node:module";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    // Module surface: the seven exports of bcrypt 6.0.0.
    out.keys = Object.keys(bcrypt).sort();
    out.namedExports = [
      typeof ns.hashSync, typeof ns.compareSync, typeof ns.genSaltSync,
      typeof ns.hash, typeof ns.compare, typeof ns.genSalt, typeof ns.getRounds,
    ];

    // CJS require path (the census probe shape). The bridge hands out a
    // plain-prototype copy of the namespace, so the seven functions are
    // checked by shape rather than by exact key set.
    const require = createRequire(import.meta.url);
    const required = require("bcrypt");
    out.cjsFunctions = ["genSaltSync", "genSalt", "hashSync", "hash", "compareSync", "compare", "getRounds"]
      .map((key) => typeof (required as any)[key] === "function");
    out.censusHash = required.hashSync("pw", 4).startsWith("$2");
    out.cjsCallable = required.hashSync === bcrypt.hashSync;

    // genSaltSync: $2b$ default version, 22-char bcrypt-base64 salt, cost
    // clamps (3 -> 4, 40 -> 31) like the addon.
    const salt = bcrypt.genSaltSync(4);
    out.salt = [salt.length, salt.substring(0, 7), bcrypt.getRounds(salt)];
    out.saltA = bcrypt.genSaltSync(4, "a").substring(0, 7);
    out.saltDefaults = bcrypt.genSaltSync().substring(0, 7);
    out.saltClampLow = bcrypt.hashSync("pw", 3).substring(0, 7);
    out.saltClampHigh = bcrypt.genSaltSync(40).substring(0, 7);

    // Password bytes: Buffer input and a non-ASCII string agree with their
    // 60-char hash shape.
    out.bufferHash = bcrypt.hashSync(Buffer.from("abc"), "$2a$06$If6bvum7DFjUnE9p2uDeDu");
    out.bufferHashLength = out.bufferHash.length;

    // Error wordings (bcrypt 6.0.0 / node-bcrypt native).
    const messages: any = {};
    const capture = (key: string, fn: Function) => {
      try { fn(); messages[key] = "no-throw"; } catch (e: any) { messages[key] = e.message; }
    };
    capture("noArgs", () => bcrypt.hashSync());
    capture("badData", () => bcrypt.hashSync(42, "$2b$04$xxxxxxxxxxxxxxxxxxxxxx"));
    capture("badSalt", () => bcrypt.hashSync("pw", "nope"));
    capture("roundsType", () => bcrypt.genSaltSync("x"));
    capture("minor", () => bcrypt.genSaltSync(4, "c"));
    capture("roundsHash", () => bcrypt.getRounds(5));
    capture("compareArgs", () => bcrypt.compareSync(null, "x"));
    capture("compareHashType", () => bcrypt.compareSync(5, "$2b$04$xxxxxxxxxxxxxxxxxxxxxx"));
    out.messages = messages;

    // Rejecting Promise when the callback is not callable.
    let rejected = "";
    try {
      await bcrypt.hash("pw", 4, "not a function" as any);
    } catch (e: any) {
      rejected = e.message;
    }
    out.rejected = rejected;

    // Promise forms + callback forms.
    const hash = await bcrypt.hash("abc", "$2a$06$If6bvum7DFjUnE9p2uDeDu");
    out.promiseHash = hash;
    out.promiseCompare = await bcrypt.compare("abc", hash);
    out.promiseSalt = (await bcrypt.genSalt(4)).substring(0, 7);
    const viaCallback = await new Promise<string>((resolve, reject) => {
      bcrypt.hash("abc", 4, (err: any, value: string) => (err ? reject(err) : resolve(value.substring(0, 7))));
    });
    out.callbackHash = viaCallback;
    const hashError = await new Promise<string>((resolve) => {
      bcrypt.hash("abc", "nope", (err: any) => resolve(err ? err.message : "no-error"));
    });
    out.callbackError = hashError;

    // Boot registry: require("bcrypt") resolves to the virtual module and the
    // boot sweep reported no failure for it.
    const mod = (globalThis as any).__pirNodeModules["bcrypt"];
    out.registry = typeof mod === "object" && mod !== null && typeof mod.default === "object";
    out.bootErrors = ((globalThis as any).__pirBootErrors || []).filter(
      (line: string) => line.indexOf("bcrypt") !== -1
    );

    globalThis.__bcryptSurface = out;
  });
};
"#;
    let value = run_fixture(&rt, "bcrypt_surface", FIXTURE, "__bcryptSurface").await;

    let seven = json!([
        "compare",
        "compareSync",
        "genSalt",
        "genSaltSync",
        "getRounds",
        "hash",
        "hashSync"
    ]);
    assert_eq!(value["keys"], seven);
    assert_eq!(
        value["cjsFunctions"],
        json!([true, true, true, true, true, true, true])
    );
    assert_eq!(
        value["namedExports"],
        json!(["function", "function", "function", "function", "function", "function", "function"])
    );
    assert_eq!(value["censusHash"], true);
    assert_eq!(value["cjsCallable"], true);
    assert_eq!(value["salt"], json!([29, "$2b$04$", 4]));
    assert_eq!(value["saltA"], "$2a$04$");
    assert_eq!(value["saltDefaults"], "$2b$10$");
    assert_eq!(value["saltClampLow"], "$2b$04$");
    assert_eq!(value["saltClampHigh"], "$2b$31$");
    assert_eq!(
        value["bufferHash"],
        json!("$2a$06$If6bvum7DFjUnE9p2uDeDu0YHzrHM6tf.iqN8.yx.jNN1ILEf7h0i")
    );
    assert_eq!(value["bufferHashLength"], 60);
    let messages = &value["messages"];
    assert_eq!(messages["noArgs"], "data and salt arguments required");
    assert_eq!(
        messages["badData"],
        "data must be a string or Buffer and salt must either be a salt string or a number of rounds"
    );
    assert_eq!(
        messages["badSalt"],
        "Invalid salt. Salt must be in the form of: $Vers$log2(NumRounds)$saltvalue"
    );
    assert_eq!(messages["roundsType"], "rounds must be a number");
    assert_eq!(messages["minor"], "minor must be either \"a\" or \"b\"");
    assert_eq!(messages["roundsHash"], "hash must be a string");
    assert_eq!(messages["compareArgs"], "data and hash arguments required");
    assert_eq!(
        messages["compareHashType"],
        "data must be a string or Buffer and hash must be a string"
    );
    assert_eq!(
        value["rejected"],
        "cb must be a function or null to return a Promise"
    );
    assert_eq!(
        value["promiseHash"],
        json!("$2a$06$If6bvum7DFjUnE9p2uDeDu0YHzrHM6tf.iqN8.yx.jNN1ILEf7h0i")
    );
    assert_eq!(value["promiseCompare"], true);
    assert_eq!(value["promiseSalt"], "$2b$04$");
    assert_eq!(value["callbackHash"], "$2b$04$");
    assert_eq!(
        value["callbackError"],
        "Invalid salt. Salt must be in the form of: $Vers$log2(NumRounds)$saltvalue"
    );
    assert_eq!(value["registry"], true);
    assert_eq!(value["bootErrors"], json!([]));
}
