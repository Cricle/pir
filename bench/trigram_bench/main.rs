// Trigram tokenizer migration evaluation harness (PIR-BIN-021 follow-up).
//
// Build:
//   cargo build --release --no-default-features --features session --example trigram_bench
// (see bench/README.md for the exact command used)
//
// What it measures on a real 60-file corpus (~100-200 KiB of English technical
// prose, in the corpus/ dir next to this file):
//   1. Index: identical chunks inserted into two DBs —
//        A: chunks_fts using `tokenize='porter unicode61'` (current schema)
//        B: chunks_fts using `tokenize='trigram'` (candidate schema)
//      DB size, chunk count, wall time.
//   2. Query latency + recall for three query classes:
//        - substring (CJK) queries: current path = LIKE fallback (O(n) scan);
//          trigram path = FTS MATCH (works for >=3-char patterns)
//        - 2-char CJK queries: trigram cannot index them -> LIKE still needed
//        - ASCII queries: both go through FTS (porter vs trigram recall diff)
//   3. Index size delta (the storage cost of trigram).

use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection};

// Production ranks with `bm25(chunks_fts, 5.0, 1.0)`: a title hit is worth
// five content hits. The paths measured below compare raw latency and recall
// (unranked), so the weights are printed in the header instead of applied in
// the timed SQL.
const BM25_WEIGHTS: (f64, f64) = (5.0, 1.0);

fn open_db(path: &Path, tokenize: &str) -> Connection {
    let _ = std::fs::remove_file(path);
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = OFF;")
        .unwrap();
    connection
        .execute_batch(&format!(
            "CREATE TABLE chunks (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT,\
                 source TEXT NOT NULL,\
                 title TEXT NOT NULL,\
                 content TEXT NOT NULL\
             );\
             CREATE VIRTUAL TABLE chunks_fts USING fts5(\
                 title, content, tokenize='{tokenize}'\
             );"
        ))
        .unwrap();
    connection
}

fn insert_chunks(connection: &Connection, chunks: &[(String, String, String)]) -> Duration {
    let start = Instant::now();
    let transaction = connection.unchecked_transaction().unwrap();
    let mut insert_chunk = transaction
        .prepare("INSERT INTO chunks (source, title, content) VALUES (?1, ?2, ?3)")
        .unwrap();
    let mut insert_fts = transaction
        .prepare("INSERT INTO chunks_fts (rowid, title, content) VALUES (?1, ?2, ?3)")
        .unwrap();
    for (source, title, content) in chunks {
        insert_chunk
            .execute(params![source, title, content])
            .unwrap();
        let id = transaction.last_insert_rowid();
        insert_fts.execute(params![id, title, content]).unwrap();
    }
    drop(insert_chunk);
    drop(insert_fts);
    transaction.commit().unwrap();
    start.elapsed()
}

/// The current production LIKE fallback (mirrors `like_fallback_search`).
fn like_fallback_search(
    connection: &Connection,
    terms: &[String],
) -> (Vec<(i64, String)>, Duration) {
    let likes = terms
        .iter()
        .map(|term| {
            format!(
                "%{}%",
                term.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            )
        })
        .collect::<Vec<_>>();
    let conditions = terms
        .iter()
        .map(|_| "(title LIKE ? ESCAPE '\\' OR content LIKE ? ESCAPE '\\')")
        .collect::<Vec<_>>()
        .join(" OR ");
    let sql = format!("SELECT id, title FROM chunks WHERE ({conditions}) LIMIT 200");
    let start = Instant::now();
    let mut statement = connection.prepare_cached(&sql).unwrap();
    let bindings: Vec<rusqlite::types::Value> = likes
        .iter()
        .flat_map(|like| [like.clone().into(), like.clone().into()])
        .collect();
    let mut rows = statement
        .query(rusqlite::params_from_iter(bindings.iter()))
        .unwrap();
    let mut hits = Vec::new();
    while let Some(row) = rows.next().unwrap() {
        hits.push((row.get(0).unwrap(), row.get::<_, String>(1).unwrap()));
    }
    (hits, start.elapsed())
}

/// FTS MATCH over the trigram index (>=3 chars required).
fn trigram_match(connection: &Connection, query: &str) -> (Vec<(i64, String)>, Duration) {
    let sql = "SELECT c.id, c.title FROM chunks_fts f JOIN chunks c ON c.id = f.rowid \
               WHERE chunks_fts MATCH ?1 LIMIT 200";
    let start = Instant::now();
    let mut statement = connection.prepare_cached(sql).unwrap();
    let mut rows = statement.query(params![query]).unwrap();
    let mut hits = Vec::new();
    while let Some(row) = rows.next().unwrap() {
        hits.push((row.get(0).unwrap(), row.get::<_, String>(1).unwrap()));
    }
    (hits, start.elapsed())
}

fn median(values: &mut [Duration]) -> Duration {
    values.sort();
    values[values.len() / 2]
}

fn main() {
    let corpus_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "corpus".to_owned());
    let out_dir = std::env::args().nth(2).unwrap_or_else(|| "out".to_owned());
    std::fs::create_dir_all(&out_dir).unwrap();

    // ---- 1. Load corpus: real files, chunked like the plugin does ----
    let mut files: Vec<(String, String)> = Vec::new(); // (name, body)
    let mut entries: Vec<_> = std::fs::read_dir(&corpus_dir)
        .unwrap_or_else(|error| panic!("corpus dir {corpus_dir}: {error}"))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    for path in entries {
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let body = std::fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        files.push((name, body));
    }
    let total_bytes: usize = files.iter().map(|(_, body)| body.len()).sum();
    println!("corpus: {} files, {} bytes total", files.len(), total_bytes);

    // Split markdown at headings the same way chunk_markdown does (simplified:
    // the bench corpus is uniform, so the heading split is what matters).
    let mut chunks: Vec<(String, String, String)> = Vec::new();
    for (name, body) in &files {
        let mut title = String::new();
        let mut content = String::new();
        for line in body.split_inclusive('\n') {
            if line.starts_with('#') && !content.is_empty() {
                chunks.push((
                    name.clone(),
                    std::mem::take(&mut title),
                    std::mem::take(&mut content),
                ));
            }
            if title.is_empty() && line.starts_with('#') {
                title = line.trim().trim_start_matches('#').trim().to_owned();
            }
            content.push_str(line);
        }
        if !content.trim().is_empty() {
            chunks.push((name.clone(), title, content));
        }
    }
    println!("chunks: {}", chunks.len());
    println!(
        "production rank weights: bm25(title={}, content={})",
        BM25_WEIGHTS.0, BM25_WEIGHTS.1
    );

    // ---- 2. Index into both schemas ----
    let db_unicode = Path::new(&out_dir).join("unicode61.db");
    let db_trigram = Path::new(&out_dir).join("trigram.db");
    let connection_unicode = open_db(&db_unicode, "porter unicode61");
    let insert_time_unicode = insert_chunks(&connection_unicode, &chunks);
    let connection_trigram = open_db(&db_trigram, "trigram");
    let insert_time_trigram = insert_chunks(&connection_trigram, &chunks);
    // Checkpoint WAL so file sizes are comparable.
    for (path, connection) in [
        (&db_unicode, &connection_unicode),
        (&db_trigram, &connection_trigram),
    ] {
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        let size = std::fs::metadata(path).unwrap().len();
        let wal = std::fs::metadata(format!("{}-wal", path.display()))
            .map(|m| m.len())
            .unwrap_or(0);
        println!("db {} size={} wal={}", path.display(), size, wal);
    }

    // ---- 3. Query classes ----
    // CJK substrings that DO appear in the corpus (>=3 chars: trigram OK)
    let cjk3 = [
        "缓存数据库连接池",
        "自动化部署流水线",
        "监控告警聚合报告",
        "水平分区路由策略",
        "批处理任务调度器",
    ];
    // CJK 2-char substrings (trigram CANNOT match; LIKE still works)
    let cjk2 = ["缓存", "部署", "监控", "分区", "调度"];
    // ASCII queries (both FTS paths)
    let ascii = [
        "connection pool",
        "deployment pipeline",
        "alert aggregation",
        "rate limit",
        "garbage collection",
    ];

    let runs = 15;
    println!("\n=== CJK substring queries (>=3 chars) ===");
    println!("query | like_fallback_med_ms | trigram_med_ms | like_hits | trigram_hits");
    for query in cjk3 {
        let mut like_times = Vec::new();
        let mut like_hits = Vec::new();
        for _ in 0..runs {
            let (hits, time) = like_fallback_search(&connection_unicode, &[query.to_owned()]);
            like_times.push(time);
            like_hits = hits;
        }
        let expression = format!("\"{query}\"");
        let mut tri_times = Vec::new();
        let mut tri_hits = Vec::new();
        for _ in 0..runs {
            let (hits, time) = trigram_match(&connection_trigram, &expression);
            tri_times.push(time);
            tri_hits = hits;
        }
        println!(
            "{query} | {:.3} | {:.3} | {} | {}",
            median(&mut like_times).as_secs_f64() * 1e3,
            median(&mut tri_times).as_secs_f64() * 1e3,
            like_hits.len(),
            tri_hits.len(),
        );
    }

    println!("\n=== CJK 2-char queries (trigram cannot index) ===");
    println!("query | like_fallback_med_ms | trigram_med_ms | like_hits | trigram_hits");
    for query in cjk2 {
        let mut like_times = Vec::new();
        let mut like_hits = Vec::new();
        for _ in 0..runs {
            let (hits, time) = like_fallback_search(&connection_unicode, &[query.to_owned()]);
            like_times.push(time);
            like_hits = hits;
        }
        let expression = format!("\"{query}\"");
        let (tri_hits, tri_time) = trigram_match(&connection_trigram, &expression);
        println!(
            "{query} | {:.3} | {:.3} | {} | {}",
            median(&mut like_times).as_secs_f64() * 1e3,
            tri_time.as_secs_f64() * 1e3,
            like_hits.len(),
            tri_hits.len(),
        );
    }

    println!("\n=== ASCII queries (porter unicode61 vs trigram FTS) ===");
    println!("query | unicode61_med_ms | trigram_med_ms | unicode61_hits | trigram_hits");
    for query in ascii {
        let expression = query
            .split_whitespace()
            .map(|term| format!("\"{term}\""))
            .collect::<Vec<_>>()
            .join(" OR ");
        let mut uni_times = Vec::new();
        let mut uni_hits = Vec::new();
        for _ in 0..runs {
            let (hits, time) = trigram_match(&connection_unicode, &expression);
            uni_times.push(time);
            uni_hits = hits;
        }
        let mut tri_times = Vec::new();
        let mut tri_hits = Vec::new();
        for _ in 0..runs {
            let (hits, time) = trigram_match(&connection_trigram, &expression);
            tri_times.push(time);
            tri_hits = hits;
        }
        println!(
            "{query} | {:.3} | {:.3} | {} | {}",
            median(&mut uni_times).as_secs_f64() * 1e3,
            median(&mut tri_times).as_secs_f64() * 1e3,
            uni_hits.len(),
            tri_hits.len(),
        );
    }

    println!(
        "\ninsert: unicode61 {:.1}ms vs trigram {:.1}ms",
        insert_time_unicode.as_secs_f64() * 1e3,
        insert_time_trigram.as_secs_f64() * 1e3
    );
}
