//! The `edit` tool's whole-file source cap is enforced on the bytes a backend
//! actually returns, not only on a prior `stat`.
//!
//! Every arm that materializes the file applies the same refusal:
//!
//! * the local arm streams through `read_confined`, which stops at the cap;
//! * the injected-`EditOperations` arm checks the returned buffer (this test);
//! * the remote arm used to trust `file_info`'s size and then read the whole
//!   file, so a file that grew between the stat and the read could bypass the
//!   bound. It now re-checks the bytes it read (`source_limit_refusal`).
//!
//! The cap is process-wide (`PIR_TOOL_MAX_SOURCE_BYTES`), which is why this
//! lives in its own integration-test binary with a single test: the variable
//! is set once, before any tool runs.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};
use pir::coding_agent::tools::{EditOperations, EditTool};

struct FakeEdit {
    content: Vec<u8>,
    writes: Mutex<usize>,
}

impl EditOperations for FakeEdit {
    fn read_file<'a>(
        &'a self,
        _absolute_path: &'a Path,
    ) -> BoxFuture<'a, std::io::Result<Vec<u8>>> {
        Box::pin(async move { Ok(self.content.clone()) })
    }

    fn write_file<'a>(
        &'a self,
        _absolute_path: &'a Path,
        _content: &'a str,
    ) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            *self.writes.lock().expect("writes") += 1;
            Ok(())
        })
    }

    fn access<'a>(&'a self, _absolute_path: &'a Path) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

fn temp_workspace(label: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "pir-edit-limit-{label}-{nonce}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create workspace");
    root
}

#[tokio::test]
async fn the_edit_source_cap_applies_to_the_bytes_a_backend_returns() {
    // Set once for this binary (see the module docs).
    std::env::set_var("PIR_TOOL_MAX_SOURCE_BYTES", "64");
    let root = temp_workspace("cap");

    // Over the cap: refused, and the backend never sees a write.
    let oversized = Arc::new(FakeEdit {
        content: vec![b'a'; 200],
        writes: Mutex::new(0),
    });
    let refused = EditTool::with_operations(oversized.clone())
        .run(
            &serde_json::json!({
                "path": "big.txt",
                "edits": [{ "oldText": "a", "newText": "b" }]
            }),
            &ToolContext::new(&root),
        )
        .await;
    assert!(refused.is_error, "{}", refused.content);
    assert!(
        refused
            .content
            .contains("exceeds the 64 byte source-size limit"),
        "the refusal must name the configured cap: {}",
        refused.content
    );
    assert_eq!(
        *oversized.writes.lock().expect("writes"),
        0,
        "a refused edit must never reach the backend's write"
    );

    // Within the cap: the same code path still applies the edit.
    let small = Arc::new(FakeEdit {
        content: b"hello world\n".to_vec(),
        writes: Mutex::new(0),
    });
    let applied = EditTool::with_operations(small.clone())
        .run(
            &serde_json::json!({
                "path": "small.txt",
                "edits": [{ "oldText": "world", "newText": "there" }]
            }),
            &ToolContext::new(&root),
        )
        .await;
    assert!(!applied.is_error, "{}", applied.content);
    assert_eq!(*small.writes.lock().expect("writes"), 1);

    let _ = std::fs::remove_dir_all(&root);
}
