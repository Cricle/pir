//! Scalar `ctx.sessionManager` accessors (`extensions/api/js/prelude.js.txt`).
//!
//! `getSessionId` / `getSessionPath` / `getSessionFile` / `getCwd` /
//! `getHeader` / `getLeafId` / `getLabel` used to parse the ENTIRE
//! `getSessionManagerJson` envelope (context entries, tree text, projection,
//! session listing) just to read one field. They now read the host's live
//! snapshot fields through dedicated host functions.
//!
//! These tests pin the wire contract: each accessor returns exactly what the
//! former `JSON.parse(getSessionManagerJson())` path returned (`undefined` for
//! absent/null), and - with `getSessionManagerJson` replaced by a counting
//! stub - none of them calls the big JSON host function.

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionHandle, SessionContextSnapshot};
use serde_json::{json, Value};

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-scalar-accessors-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn extension_config(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// The probe derives the baseline through the old `getSessionManagerJson()`
/// path, then counts calls while exercising every scalar accessor. The big
/// host function is restored afterwards so the value stays readable.
const SCALAR_PROBE: &str = r#"
export default function (pi) {
  pi.on("session_start", (_event, ctx) => {
    const sm = ctx.sessionManager;
    // Baseline: the former implementation (parse the whole envelope).
    const original = __pir_host.getSessionManagerJson;
    const parsed = JSON.parse(original());
    const baseline = {
      sessionId: parsed.sessionId ?? undefined,
      sessionPath: parsed.sessionPath ?? undefined,
      cwd: parsed.cwd ?? undefined,
      header: parsed.header ?? undefined,
      leafId: parsed.leafId ?? undefined,
      label: (parsed.labels ?? {})["entry-1"] ?? undefined,
      missingLabel: (parsed.labels ?? {})["nope"] ?? undefined,
    };

    let bigCalls = 0;
    __pir_host.getSessionManagerJson = () => {
      bigCalls += 1;
      return original();
    };
    const actual = {
      sessionId: sm.getSessionId(),
      sessionPath: sm.getSessionPath(),
      sessionFile: sm.getSessionFile(),
      cwd: sm.getCwd(),
      header: sm.getHeader(),
      leafId: sm.getLeafId(),
      label: sm.getLabel("entry-1"),
      missingLabel: sm.getLabel("nope"),
    };
    __pir_host.getSessionManagerJson = original;

    const jsonEq = (a, b) => JSON.stringify(a) === JSON.stringify(b);
    globalThis.__scalarProbe = {
      bigCalls,
      sessionIdMatches: Object.is(actual.sessionId, baseline.sessionId),
      sessionPathMatches: Object.is(actual.sessionPath, baseline.sessionPath),
      sessionFileMatches: Object.is(actual.sessionFile, baseline.sessionPath),
      cwdMatches: Object.is(actual.cwd, baseline.cwd),
      headerMatches: jsonEq(actual.header, baseline.header),
      leafIdMatches: Object.is(actual.leafId, baseline.leafId),
      labelMatches: Object.is(actual.label, baseline.label),
      missingLabelMatches:
        actual.missingLabel === undefined && baseline.missingLabel === undefined,
      values: {
        sessionId: actual.sessionId === undefined ? null : actual.sessionId,
        sessionPath: actual.sessionPath === undefined ? null : actual.sessionPath,
        cwd: actual.cwd === undefined ? null : actual.cwd,
        leafId: actual.leafId === undefined ? null : actual.leafId,
        label: actual.label === undefined ? null : actual.label,
        headerId: actual.header ? actual.header.id : null,
      },
    };
  });
}
"#;

async fn run_probe(temp: &TempDir, snapshot: SessionContextSnapshot) -> Value {
    let handle = ExtensionHandle::start(extension_config(temp.path())).expect("extension handle");
    handle.update_session_context(snapshot);
    let path = temp.path().join("scalar-probe.ts");
    std::fs::write(&path, SCALAR_PROBE).expect("write probe");
    let loaded = handle
        .load(std::slice::from_ref(&path))
        .await
        .expect("load probe");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    let mut payload = json!({ "type": "session_start", "reason": "test" });
    handle
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    handle
        .eval_json("JSON.stringify(globalThis.__scalarProbe ?? null)")
        .await
        .expect("read probe")
}

fn full_snapshot() -> SessionContextSnapshot {
    SessionContextSnapshot {
        model: json!({ "id": "fake-model", "provider": "fake" }),
        entries: json!([{ "role": "user", "content": [{ "type": "text", "text": "hi" }] }]),
        sessions: json!([{ "id": "sess-1", "path": "C:/sessions/sess-1.jsonl" }]),
        session_id: Some("sess-1".into()),
        session_path: Some("C:/sessions/sess-1.jsonl".into()),
        context_tokens: Some(10),
        context_window: Some(4096),
        has_pending_messages: false,
        project_trusted: true,
        cwd: Some("C:/repo".into()),
        header: json!({ "type": "session", "id": "sess-1", "cwd": "C:/repo" }),
        branch: json!([]),
        tree_json: "[]".into(),
        leaf_id: Some("entry-2".into()),
        labels: json!({ "entry-1": "checkpoint" }),
        projection: json!({ "entries": [], "messages": [] }),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn scalar_getters_match_the_full_snapshot_without_parsing_it() {
    let temp = TempDir::new("full");
    let probe = run_probe(&temp, full_snapshot()).await;

    assert_eq!(probe["bigCalls"], json!(0), "{probe}");
    for key in [
        "sessionIdMatches",
        "sessionPathMatches",
        "sessionFileMatches",
        "cwdMatches",
        "headerMatches",
        "leafIdMatches",
        "labelMatches",
        "missingLabelMatches",
    ] {
        assert_eq!(probe[key], json!(true), "{key} must match: {probe}");
    }
    assert_eq!(
        probe["values"],
        json!({
            "sessionId": "sess-1",
            "sessionPath": "C:/sessions/sess-1.jsonl",
            "cwd": "C:/repo",
            "leafId": "entry-2",
            "label": "checkpoint",
            "headerId": "sess-1",
        })
    );
}

/// Absent cells keep the documented `undefined` shape (`null` on the host side
/// maps through `?? undefined`), matching the former parsed-envelope path.
#[tokio::test(flavor = "current_thread")]
async fn scalar_getters_preserve_undefined_for_absent_cells() {
    let temp = TempDir::new("absent");
    let snapshot = SessionContextSnapshot {
        session_id: None,
        session_path: None,
        cwd: None,
        header: Value::Null,
        leaf_id: None,
        labels: Value::Null,
        ..SessionContextSnapshot::default()
    };
    let probe = run_probe(&temp, snapshot).await;

    assert_eq!(probe["bigCalls"], json!(0), "{probe}");
    for key in [
        "sessionIdMatches",
        "sessionPathMatches",
        "sessionFileMatches",
        "cwdMatches",
        "headerMatches",
        "leafIdMatches",
        "labelMatches",
        "missingLabelMatches",
    ] {
        assert_eq!(probe[key], json!(true), "{key} must match: {probe}");
    }
    assert_eq!(
        probe["values"],
        json!({
            "sessionId": null,
            "sessionPath": null,
            "cwd": null,
            "leafId": null,
            "label": null,
            "headerId": null,
        }),
        "{probe}"
    );
}
