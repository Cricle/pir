//! `listFiles`: the files and directories below a directory, as `fd --hidden --exclude .git` lists them (without
//! following symbolic links), skipping what git ignores. Uses the `ignore` crate, which fd and ripgrep use.

use crate::env_daemon::errors::Failure;
use crate::env_daemon::frame::MAX_PAYLOAD;
use ignore::WalkBuilder;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path};
use std::time::{Duration, Instant};

const DEFAULT_MAX_ENTRIES: u64 = 50_000;
const DEFAULT_MAX_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 10_000;

pub struct Options {
    /// Include names starting with `.` (and, on Windows, hidden files).
    pub hidden: bool,
    /// Skip what git ignores inside git work trees: `.gitignore` files, `.git/info/exclude`, the global excludes file.
    pub gitignore: bool,
    /// Names skipped with everything below them, in addition to `.git`.
    pub exclude: Vec<OsString>,
    pub max_entries: u64,
    /// Bytes of the listing payload: per entry one kind byte, the path, and a NUL.
    pub max_bytes: u64,
    pub timeout: Duration,
}

impl Options {
    pub fn from_json(json: &Value) -> Result<Options, Failure> {
        let count = |key: &str, default: u64| -> Result<u64, Failure> {
            match &json[key] {
                Value::Null => Ok(default),
                value => value
                    .as_u64()
                    .ok_or_else(|| Failure::new("EINVAL", format!("invalid field {key}"))),
            }
        };
        let exclude = match &json["exclude"] {
            Value::Null => Vec::new(),
            Value::Array(names) => names
                .iter()
                .map(|name| name.as_str().map(OsString::from))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| Failure::new("EINVAL", "invalid field exclude"))?,
            _ => return Err(Failure::new("EINVAL", "invalid field exclude")),
        };
        Ok(Options {
            hidden: json["hidden"].as_bool().unwrap_or(true),
            gitignore: json["gitignore"].as_bool().unwrap_or(true),
            exclude,
            max_entries: count("maxEntries", DEFAULT_MAX_ENTRIES)?,
            max_bytes: count("maxBytes", DEFAULT_MAX_BYTES)?.min(MAX_PAYLOAD as u64),
            timeout: Duration::from_millis(count("timeoutMs", DEFAULT_TIMEOUT_MS)?),
        })
    }
}

pub struct Listing {
    /// Records of a kind byte (`f`, `d`, `l`), the path relative to the root with `/` separators, and a NUL.
    pub payload: Vec<u8>,
    pub count: u64,
    pub truncated: bool,
}

/// The path of `path` below `root`, its components joined with `/`; names that are not UTF-8 get replacement
/// characters, as `readdir` reports them.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let rest = path.strip_prefix(root).ok()?;
    let mut text = String::new();
    for component in rest.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        if !text.is_empty() {
            text.push('/');
        }
        text.push_str(&name.to_string_lossy());
    }
    (!text.is_empty()).then_some(text)
}

/// List `root` depth first, siblings in byte order of their names. Stops with `truncated` at the first entry beyond
/// a bound; fails with `aborted` once `aborted()` is true. Entries that cannot be read are skipped.
pub fn list(root: &str, options: &Options, aborted: &dyn Fn() -> bool) -> Result<Listing, Failure> {
    let metadata = fs::metadata(root).map_err(|error| Failure::io(&error, "scandir", root))?;
    if !metadata.is_dir() {
        return Err(Failure::new(
            "ENOTDIR",
            format!("ENOTDIR: not a directory, scandir '{root}'"),
        )
        .path(root));
    }
    let root_path = Path::new(root);
    let exclude = options.exclude.clone();
    let walk = WalkBuilder::new(root_path)
        .hidden(!options.hidden)
        .ignore(false)
        .parents(options.gitignore)
        .git_ignore(options.gitignore)
        .git_exclude(options.gitignore)
        .git_global(options.gitignore)
        .require_git(true)
        .follow_links(false)
        // The global excludes file is found relative to the listed directory, not the daemon's.
        .current_dir(root_path)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(move |entry| {
            let name = entry.file_name();
            name != ".git" && !exclude.iter().any(|excluded| excluded == name)
        })
        .build();
    let started = Instant::now();
    let mut listing = Listing {
        payload: Vec::new(),
        count: 0,
        truncated: false,
    };
    for entry in walk {
        if aborted() {
            return Err(Failure::new("aborted", "aborted"));
        }
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 {
            continue;
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        let kind = if file_type.is_symlink() {
            b'l'
        } else if file_type.is_dir() {
            b'd'
        } else if file_type.is_file() {
            b'f'
        } else {
            continue;
        };
        let Some(path) = relative(root_path, entry.path()) else {
            continue;
        };
        let size = path.len() as u64 + 2;
        if listing.count >= options.max_entries
            || listing.payload.len() as u64 + size > options.max_bytes
            || started.elapsed() > options.timeout
        {
            listing.truncated = true;
            break;
        }
        listing.payload.push(kind);
        listing.payload.extend_from_slice(path.as_bytes());
        listing.payload.push(0);
        listing.count += 1;
    }
    Ok(listing)
}

pub fn run(json: &Value, aborted: &dyn Fn() -> bool) -> Result<(Value, Vec<u8>), Failure> {
    let root = json["path"]
        .as_str()
        .ok_or_else(|| Failure::new("EINVAL", "missing field path"))?;
    let listing = list(root, &Options::from_json(json)?, aborted)?;
    Ok((
        json!({ "count": listing.count, "truncated": listing.truncated }),
        listing.payload,
    ))
}
