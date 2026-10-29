//! W10-I: bounded asynchronous terminal output.
//!
//! W9-A found the only unbounded wait on the UI path: `Terminal::write` did
//! `stdout.write_all` + `flush` on the caller's thread while holding the
//! output mutex, so a console in QuickEdit/selection (or a blocked ConPTY
//! pipe) froze `app.core.render()` forever. These tests pin the replacement:
//! a dedicated `pir-tui-writer` thread, a bounded frame queue that drops its
//! OLDEST pending frame when full, a bounded drain, and a synchronous direct
//! path for teardown.
//!
//! Unregistered on purpose (lane W10-I owns only its own files): the foreman
//! adds the `tests/g_tui.rs` module registration. Run once wired with
//! `cargo test --test g_tui tui_output_writer_w10`.

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use pir::tui::core::{Terminal, TerminalSink, WRITER_QUEUE_CAPACITY};
use pir::tui::stdin_buffer::WriteLog;

/// Shared state of a recording sink. The `pir-tui-writer` thread can be
/// parked on `gate` before its first write lands, letting a test observe the
/// queue while a frame is in flight. Writes from any other thread (the
/// direct `write_sync` path) are never gated.
struct SinkState {
    bytes: Mutex<Vec<u8>>,
    /// Async writer-thread writes still to park before entering the gate.
    blocked: AtomicUsize,
    /// Set when the writer thread parked in a gated write.
    started: (Mutex<bool>, Condvar),
    /// Opened by the test to let the parked write complete.
    gate: (Mutex<bool>, Condvar),
    /// Number of `write` calls that reached this sink (both paths).
    writes: AtomicUsize,
}

#[derive(Clone)]
struct RecordingSink(Arc<SinkState>);

impl RecordingSink {
    fn new() -> Self {
        Self(Arc::new(SinkState {
            bytes: Mutex::new(Vec::new()),
            blocked: AtomicUsize::new(0),
            started: (Mutex::new(false), Condvar::new()),
            gate: (Mutex::new(false), Condvar::new()),
            writes: AtomicUsize::new(0),
        }))
    }

    /// Park the next writer-thread write until [`Self::open_gate`].
    fn block_next_writer_write(&self) {
        self.0.blocked.store(1, Ordering::SeqCst);
    }

    fn wait_until_blocked(&self, timeout: Duration) {
        let (lock, cv) = &self.0.started;
        let mut started = lock.lock().expect("started lock");
        let deadline = Instant::now() + timeout;
        while !*started {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "the pir-tui-writer thread never entered the gated write"
            );
            let (guard, _) = cv.wait_timeout(started, remaining).expect("started wait");
            started = guard;
        }
    }

    fn open_gate(&self) {
        let (lock, cv) = &self.0.gate;
        *lock.lock().expect("gate lock") = true;
        cv.notify_all();
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.bytes.lock().expect("bytes lock").clone())
            .expect("the sink only sees UTF-8")
    }

    fn writes(&self) -> usize {
        self.0.writes.load(Ordering::SeqCst)
    }
}

impl Write for RecordingSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Only the dedicated writer thread is gated; `write_sync` runs on the
        // test thread and must reach the sink even while the queue is stuck.
        if std::thread::current().name() == Some("pir-tui-writer")
            && self
                .0
                .blocked
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
        {
            {
                let (lock, cv) = &self.0.started;
                *lock.lock().expect("started lock") = true;
                cv.notify_all();
            }
            let (lock, cv) = &self.0.gate;
            let mut open = lock.lock().expect("gate lock");
            while !*open {
                open = cv.wait(open).expect("gate wait");
            }
        }
        self.0
            .bytes
            .lock()
            .expect("bytes lock")
            .extend_from_slice(buf);
        self.0.writes.fetch_add(1, Ordering::SeqCst);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn terminal_with(sink: &RecordingSink) -> Terminal {
    let make = sink.clone();
    Terminal::with_sink_factory(
        move || Box::new(make.clone()) as Box<dyn TerminalSink>,
        WriteLog::disabled(),
    )
}

fn temp_log_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "pir-tui-output-{tag}-{}-{}.log",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ))
}

/// A sink blocked inside `write_all` cannot stall the caller: `write` only
/// enqueues, and even the explicit drain gives up at its bound.
#[test]
fn slow_sink_does_not_block_write_beyond_a_bound() {
    let sink = RecordingSink::new();
    sink.block_next_writer_write();
    let terminal = terminal_with(&sink);

    let started = Instant::now();
    terminal.write("[frame-0]");
    terminal.write("[frame-1]");
    terminal.write("[frame-2]");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "write must not wait for the sink: blocked for {elapsed:?}"
    );
    assert_eq!(terminal.write_sequence(), 3);

    sink.wait_until_blocked(Duration::from_secs(5));
    // Frame 0 is stuck inside the sink, so the queue cannot drain.
    let flush_started = Instant::now();
    assert!(!terminal.flush_bounded(Duration::from_millis(75)));
    let waited = flush_started.elapsed();
    assert!(
        waited >= Duration::from_millis(50),
        "flush_bounded returned before its bound: {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(2),
        "flush_bounded must never wait indefinitely: {waited:?}"
    );
    assert_eq!(terminal.pending_frames(), 2);

    sink.open_gate();
    assert!(terminal.flush_bounded(Duration::from_secs(5)));
    assert_eq!(sink.text(), "[frame-0][frame-1][frame-2]");
}

/// When the queue is full, the OLDEST pending frame is dropped and the newest
/// frame always survives (and is delivered by the bounded flush).
#[test]
fn full_queue_drops_oldest_and_newest_wins() {
    let sink = RecordingSink::new();
    sink.block_next_writer_write();
    let terminal = terminal_with(&sink);

    // Park the writer on frame 0, then fill the queue while it cannot pop.
    terminal.write("[frame-0]");
    sink.wait_until_blocked(Duration::from_secs(5));

    let overflow = 3usize;
    for i in 1..=(WRITER_QUEUE_CAPACITY + overflow) {
        terminal.write(&format!("[frame-{i}]"));
    }
    assert_eq!(terminal.dropped_frames(), overflow as u64);
    assert_eq!(terminal.pending_frames(), WRITER_QUEUE_CAPACITY);
    assert_eq!(
        terminal.write_sequence(),
        (1 + WRITER_QUEUE_CAPACITY + overflow) as u64
    );

    sink.open_gate();
    assert!(terminal.flush_bounded(Duration::from_secs(5)));

    // Frames 1..=overflow are the oldest pending ones: dropped. Frame 0 was
    // already in flight and every later frame survives, newest last.
    let mut expected = String::from("[frame-0]");
    for i in 1..=(WRITER_QUEUE_CAPACITY + overflow) {
        if i <= overflow {
            continue;
        }
        expected.push_str(&format!("[frame-{i}]"));
    }
    assert_eq!(sink.text(), expected);
}

/// A dropped frame never reaches the terminal, so the renderer's diff
/// baseline no longer describes the screen: `drop_epoch` must advance exactly
/// once per dropped frame and the renderer must treat a change as "repaint
/// everything".
#[test]
fn dropped_frames_advance_the_repaint_epoch() {
    let sink = RecordingSink::new();
    sink.block_next_writer_write();
    let terminal = terminal_with(&sink);

    terminal.write("[frame-0]");
    sink.wait_until_blocked(Duration::from_secs(5));
    assert_eq!(terminal.drop_epoch(), 0, "nothing dropped yet");

    let overflow = 3usize;
    for i in 1..=(WRITER_QUEUE_CAPACITY + overflow) {
        terminal.write(&format!("[frame-{i}]"));
    }
    assert_eq!(terminal.dropped_frames(), overflow as u64);
    assert_eq!(
        terminal.drop_epoch(),
        overflow as u64,
        "every dropped frame must invalidate the renderer baseline"
    );

    sink.open_gate();
    assert!(terminal.flush_bounded(Duration::from_secs(5)));
    assert_eq!(
        terminal.drop_epoch(),
        overflow as u64,
        "draining the queue does not rewrite history"
    );
}

/// A healthy sink receives every chunk, byte-identical and in order.
#[test]
fn healthy_sink_receives_every_chunk_in_order() {
    let sink = RecordingSink::new();
    let terminal = terminal_with(&sink);

    // Stay below capacity: even if the writer thread has not run yet, no
    // coalescing can apply.
    let frames = WRITER_QUEUE_CAPACITY / 2;
    let mut expected = String::new();
    for i in 0..frames {
        let chunk = format!("[chunk-{i}]");
        expected.push_str(&chunk);
        terminal.write(&chunk);
    }
    assert!(terminal.flush_bounded(Duration::from_secs(5)));
    assert_eq!(sink.text(), expected);
    assert_eq!(sink.writes(), frames);
    assert_eq!(terminal.dropped_frames(), 0);
}

/// The synchronous direct path bypasses a blocked queue: teardown bytes land
/// immediately, ahead of the frame still stuck in the sink.
#[test]
fn write_sync_bypasses_a_blocked_queue() {
    let sink = RecordingSink::new();
    sink.block_next_writer_write();
    let terminal = terminal_with(&sink);

    terminal.write("[frame]");
    sink.wait_until_blocked(Duration::from_secs(5));
    assert_eq!(sink.text(), "");

    let started = Instant::now();
    terminal.write_sync("[restore]");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "write_sync must not queue behind the blocked writer: {elapsed:?}"
    );
    assert_eq!(sink.text(), "[restore]");
    assert!(!terminal.flush_bounded(Duration::from_millis(20)));

    sink.open_gate();
    assert!(terminal.flush_bounded(Duration::from_secs(5)));
    assert_eq!(sink.text(), "[restore][frame]");
}

/// `PI_TUI_WRITE_LOG` keeps its upstream semantics on both paths: every
/// chunk is appended at write-call time, queue or direct.
#[test]
fn write_log_captures_async_and_direct_paths() {
    let path = temp_log_path("log");
    let _ = std::fs::remove_file(&path);
    let sink = RecordingSink::new();
    let make = sink.clone();
    let terminal = Terminal::with_sink_factory(
        move || Box::new(make.clone()) as Box<dyn TerminalSink>,
        WriteLog::at(&path),
    );

    terminal.write("[queued]");
    terminal.write_sync("[direct]");
    let logged = std::fs::read_to_string(&path).expect("write log exists");
    assert_eq!(
        logged, "[queued][direct]",
        "both paths append to PI_TUI_WRITE_LOG in call order"
    );

    assert!(terminal.flush_bounded(Duration::from_secs(5)));
    let delivered = sink.text();
    assert!(delivered.contains("[queued]"));
    assert!(delivered.contains("[direct]"));
    let _ = std::fs::remove_file(&path);
}

/// The write log keeps its file handle open across appends and stops at its
/// cap: it writes exactly the bytes that fit, then one truncation marker, and
/// later chunks do not grow the file (the old shape opened/closed the path on
/// every frame and had no bound at all).
#[test]
fn tui_write_log_stops_at_the_cap_and_keeps_the_file_handle() {
    let path = temp_log_path("cap");
    let _ = std::fs::remove_file(&path);
    let log = WriteLog::at_with_cap(&path, 32);

    log.append("0123456789abcdef");
    log.append("0123456789abcdef");
    log.append("0123456789abcdef");
    let text = std::fs::read_to_string(&path).expect("write log exists");
    assert!(
        text.starts_with("0123456789abcdef0123456789abcdef"),
        "{text}"
    );
    assert!(
        text.contains("PI_TUI_WRITE_LOG truncated"),
        "the cap leaves one marker: {text}"
    );
    let capped_len = std::fs::metadata(&path).expect("metadata").len();
    assert!(
        (32..32 + 256).contains(&capped_len),
        "the log holds the cap plus one marker, not more: {capped_len}"
    );

    // Later chunks are dropped without reopening (or regrowing) the file.
    log.append("this must not be written");
    assert_eq!(
        std::fs::metadata(&path).expect("metadata").len(),
        capped_len,
        "appends past the cap must not grow the log"
    );
    let text = std::fs::read_to_string(&path).expect("write log exists");
    assert!(!text.contains("this must not be written"), "{text}");
    let _ = std::fs::remove_file(&path);
}
