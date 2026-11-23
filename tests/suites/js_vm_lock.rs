//! QuickJS 会话的并发安全：多个任务共享一个 PythonSession 时，
//! 每个调用都应返回各自正确的结果，且不发生死锁。

use std::sync::Arc;
use std::time::Duration;

use pir::plugins::builtin::PythonSession;
use pir::plugins::builtin::js_vm::PythonJob;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn js_session_concurrent_exec_is_safe() {
    let session = Arc::new(PythonSession::new());
    let mut tasks = Vec::new();
    for i in 0..4 {
        let s = Arc::clone(&session);
        tasks.push(tokio::spawn(async move {
            let job = PythonJob::for_python_tool(format!("1 + {}", i), None);
            s.exec(job, Duration::from_secs(5)).await
        }));
    }
    for (i, task) in tasks.into_iter().enumerate() {
        let outcome = task.await.expect("任务不应 panic");
        assert!(!outcome.error, "调用 {} 不应出错: {}", i, outcome.stderr);
        assert_eq!(outcome.eval_repr.as_deref(), Some(&(i + 1).to_string()[..]));
    }
}
