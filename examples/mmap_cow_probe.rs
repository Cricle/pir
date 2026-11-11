//! SDK session JSONL mmap / COW prototype probe (R54, PROTOCOL v6).
//!
//! Compares, on the SAME large session file and in one process:
//!
//! 1. heap baseline: `std::fs::read` -> `String` -> `Vec<&str>` -> parse;
//! 2. mmap prototype: `memmap2::Mmap` read-only + a `Vec<u32>` line-offset
//!    index; every line is parsed from its mapped byte slice (no whole-file
//!    `Vec<u8>`/`String`);
//! 3. logical COW: `Arc<Mmap>` + `Arc<[u32]>` prefix handles — view clones and
//!    fork/branch sharing cost zero heap allocations, and `Arc::make_mut`
//!    copies only when the tail handle is actually shared;
//! 4. Windows semantics: append while mapped, the old map's fixed length,
//!    remap visibility, and the share-mode behavior of the append open.
//!
//! Counters: heap allocation count/bytes (counting global allocator), Private
//! Bytes / Working Set / page faults (Win32_PerfRawData_PerfProc_Process),
//! wall time. Mapped file pages are page-cache backed: Working Set growth is
//! reported separately from Private Bytes so it is not mistaken for heap RSS.
//!
//! ```text
//! cargo run --example mmap_cow_probe -- <session.jsonl>
//! ```

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use memmap2::Mmap;

struct CountingAlloc;

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: probe-only counting allocator; every method delegates to
// `std::alloc::System` unchanged.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for CountingAlloc {
    #[allow(unsafe_code)]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: delegate with the same layout.
        unsafe { System.alloc(layout) }
    }

    #[allow(unsafe_code)]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System.alloc`/`System.realloc` above.
        unsafe { System.dealloc(ptr, layout) }
    }

    #[allow(unsafe_code)]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        // SAFETY: delegate with the same arguments.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

#[derive(Clone, Copy, Debug, Default)]
struct ProcMem {
    private_bytes: u64,
    working_set: u64,
    page_faults: u64,
}

fn proc_mem() -> ProcMem {
    let pid = std::process::id();
    let script = format!(
        "$p = Get-CimInstance Win32_Process -Filter \"ProcessId={pid}\"; \
         if ($p) {{ '{{0}},{{1}},{{2}}' -f $p.PrivatePageCount,$p.WorkingSetSize,$p.PageFaults }}"
    );
    let Ok(output) = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
    else {
        return ProcMem::default();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let values: Vec<u64> = text
        .trim()
        .split(',')
        .filter_map(|value| value.trim().parse().ok())
        .collect();
    if values.len() == 3 {
        ProcMem {
            private_bytes: values[0],
            working_set: values[1],
            page_faults: values[2],
        }
    } else {
        ProcMem::default()
    }
}

fn print_mem(label: &str, before: ProcMem, after: ProcMem) {
    println!(
        "MEM,{label},private_bytes={},private_delta_bytes={},working_set_bytes={},\
         working_set_delta_bytes={},page_faults={},page_fault_delta={}",
        after.private_bytes,
        after.private_bytes as i64 - before.private_bytes as i64,
        after.working_set,
        after.working_set as i64 - before.working_set as i64,
        after.page_faults,
        after.page_faults.saturating_sub(before.page_faults),
    );
}

fn counters() -> (u64, u64) {
    (
        ALLOC_COUNT.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
    )
}

fn main() {
    let Some(path) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: mmap_cow_probe <session.jsonl>");
        std::process::exit(1);
    };
    let file_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!(
        "PROBE,file={},bytes={file_bytes},pid={}",
        path.display(),
        std::process::id()
    );

    // --- 1. Heap baseline: whole file into Vec<u8>/String + line vec. -------
    let mem0 = proc_mem();
    let (count0, bytes0) = counters();
    let started = std::time::Instant::now();
    let raw = std::fs::read(&path).expect("read file");
    let text = String::from_utf8(raw).expect("utf8 session");
    let lines: Vec<&str> = text.lines().collect();
    let mut parsed = 0usize;
    for line in &lines {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line).expect("json line");
        if value.is_object() {
            parsed += 1;
        }
    }
    let heap_wall = started.elapsed();
    let mem1 = proc_mem();
    let (count1, bytes1) = counters();
    println!(
        "PHASE,heap-read+parse,lines={},parsed={parsed},wall_ms={},alloc_count={},alloc_bytes={}",
        lines.len(),
        heap_wall.as_millis(),
        count1 - count0,
        bytes1 - bytes0,
    );
    print_mem("heap-read+parse", mem0, mem1);
    let retained = text.len() + lines.len() * std::mem::size_of::<&str>();
    println!("HEAP,retained_bytes_after_phase={retained}");
    drop(lines);
    drop(text);
    let mem_drop = proc_mem();
    print_mem("heap-after-drop", mem1, mem_drop);

    // --- 2. mmap + u32 line-offset index (no whole-file heap buffer). ------
    let file = std::fs::File::open(&path).expect("open for mmap");
    // SAFETY: read-only map of an append-only session file; never written
    // through this handle. The file stays open for the map lifetime.
    let map = unsafe { Mmap::map(&file) }.expect("mmap");
    let mem2 = proc_mem();
    let (count2, bytes2) = counters();
    let started = std::time::Instant::now();
    let mut offsets: Vec<u32> = Vec::with_capacity(map.len() / 128);
    let mut line_start = 0usize;
    for (index, byte) in map.iter().enumerate() {
        if *byte == b'\n' {
            offsets.push(line_start as u32);
            line_start = index + 1;
        }
    }
    if line_start < map.len() {
        offsets.push(line_start as u32);
    }
    let mut mmap_parsed = 0usize;
    for index in 0..offsets.len() {
        let start = offsets[index] as usize;
        let end = if index + 1 < offsets.len() {
            (offsets[index + 1] as usize).saturating_sub(1)
        } else {
            map.len()
        };
        if start >= end {
            continue;
        }
        let line = std::str::from_utf8(&map[start..end]).expect("utf8 line");
        let value: serde_json::Value = serde_json::from_str(line).expect("json line");
        if value.is_object() {
            mmap_parsed += 1;
        }
    }
    let mmap_wall = started.elapsed();
    let mem3 = proc_mem();
    let (count3, bytes3) = counters();
    println!(
        "PHASE,mmap-offset+parse,lines={},parsed={mmap_parsed},wall_ms={},alloc_count={},\
         alloc_bytes={},index_bytes={}",
        offsets.len(),
        mmap_wall.as_millis(),
        count3 - count2,
        bytes3 - bytes2,
        offsets.len() * std::mem::size_of::<u32>(),
    );
    // The parse above touched every page (the offset scan iterates the whole
    // map), so this delta is the mapped-file page cost, not heap.
    print_mem("mmap-offset+parse", mem2, mem3);

    // --- 3. Logical COW: Arc<Mmap>/Arc<[u32]> prefix sharing. --------------
    let prefix_map: Arc<Mmap> = Arc::new(map);
    let prefix_offsets: Arc<[u32]> = Arc::from(offsets);
    let mut views: Vec<(Arc<Mmap>, Arc<[u32]>, usize)> = Vec::with_capacity(10_000);
    let (count4, bytes4) = counters();
    let started = std::time::Instant::now();
    for tail in 0..10_000usize {
        views.push((Arc::clone(&prefix_map), Arc::clone(&prefix_offsets), tail));
    }
    let share_wall = started.elapsed();
    let (count5, bytes5) = counters();
    println!(
        "PHASE,arc-prefix-share,views={},wall_us={},alloc_count={},alloc_bytes={}",
        views.len(),
        share_wall.as_micros(),
        count5 - count4,
        bytes5 - bytes4,
    );
    drop(views);

    // Arc::make_mut: unique -> no copy; shared -> exactly one copy.
    let mut unique = Arc::new(vec![7u8; 1 << 20]);
    let (count6, bytes6) = counters();
    Arc::make_mut(&mut unique)[0] = 9;
    let (count7, bytes7) = counters();
    println!(
        "PHASE,make_mut-unique,alloc_count={},alloc_bytes={}",
        count7 - count6,
        bytes7 - bytes6
    );
    let shared = Arc::new(vec![7u8; 1 << 20]);
    let mut derived = Arc::clone(&shared);
    let (count8, bytes8) = counters();
    Arc::make_mut(&mut derived)[0] = 9;
    let (count9, bytes9) = counters();
    println!(
        "PHASE,make_mut-shared,alloc_count={},alloc_bytes={}",
        count9 - count8,
        bytes9 - bytes8
    );

    // --- 4. Windows append/remap semantics while the map is alive. ---------
    let marker = b"{\"type\":\"probe_append\",\"id\":\"mmap-tail\"}";
    let old_len = prefix_map.len();
    let mut appender = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("append open while mapped (default Windows share mode)");
    let mut line = Vec::from(&marker[..]);
    line.push(b'\n');
    appender.write_all(&line).expect("append while mapped");
    appender.flush().expect("flush append");
    let new_meta_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let old_map_len_after = prefix_map.len();
    let old_map_sees_append = old_map_len_after > old_len;
    let remap_file = std::fs::File::open(&path).expect("reopen for remap");
    // SAFETY: read-only map; the file is append-only.
    let remapped = unsafe { Mmap::map(&remap_file) }.expect("remap");
    let remap_sees_append = remapped.len() == new_meta_len as usize
        && remapped[old_len..]
            .windows(marker.len())
            .any(|window| window == marker);
    println!(
        "WINDOWS,append_while_mapped=ok,old_len={old_len},new_file_len={new_meta_len},\
         old_map_len={old_map_len_after},old_map_sees_append={old_map_sees_append},\
         remap_sees_append={remap_sees_append}"
    );
    drop(remapped);
    drop(remap_file);
    drop(appender);
    drop(prefix_map);
    drop(prefix_offsets);
    drop(file);
    println!("DONE");
}
