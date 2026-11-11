// PIR-TUI round61 spill-preview benchmark (not shipped): compares the
// mmap+offset preview against the old whole-file `read_to_string` approach
// on the same file. Compiled ad hoc with rustc against target-gnu's libpir.
use std::path::Path;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    let path = args.next().unwrap_or_default();
    let loops: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(1);
    let before = pir::tui::alloc_trace::snapshot();
    let start = Instant::now();
    let mut rows = 0usize;
    match mode.as_str() {
        "read" => {
            for _ in 0..loops {
                let text = std::fs::read_to_string(&path).expect("read");
                rows = text.lines().count();
                // Old expanded-render shape: scan every line and copy the
                // bounded tail into a preview buffer.
                let mut preview = String::new();
                let all: Vec<&str> = text.lines().collect();
                for line in all.iter().rev().take(2048).rev() {
                    preview.push_str(line);
                    preview.push('\n');
                }
                std::hint::black_box(preview);
            }
        }
        "mmap" => {
            for _ in 0..loops {
                let lines = pir::tui::spill_preview::MappedLines::open(Path::new(&path))
                    .expect("mmap preview");
                rows = lines.line_count();
                let body = lines
                    .tail_preview(2048, 2 * 1024 * 1024)
                    .expect("bounded preview");
                std::hint::black_box(body);
            }
        }
        other => panic!("unknown mode {other}"),
    }
    let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
    let after = pir::tui::alloc_trace::snapshot();
    println!(
        "{{\"mode\":\"{mode}\",\"loops\":{loops},\"rows\":{rows},\"wall_ms\":{:.3},\"alloc_bytes\":{},\"alloc_count\":{},\"live_end\":{},\"peak_live\":{}}}",
        wall_ms,
        after.allocated_bytes.saturating_sub(before.allocated_bytes),
        after.allocations.saturating_sub(before.allocations),
        after.current_bytes,
        after.peak_bytes,
    );
}
