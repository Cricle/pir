//! Remote path rules: the remote system's own `path.posix` / `path.win32`
//! semantics, which the client applies before sending a path over the wire
//! (docs/protocol.md: "Paths are absolute JSON strings; the client resolves
//! relative paths, `~` and `file://` URLs before sending").
//!
//! This is a lexical implementation of `node:path`'s `normalize` / `resolve` /
//! `join` / `basename` for the two flavors a daemon can run: it never touches
//! the local file system (the paths belong to another machine), and `..` above
//! a root is dropped exactly as Node does. `resolve` takes an explicit base, so
//! the caller owns the "current directory" rule (`RemoteExecutionEnv::resolve`
//! carries Node's drive-relative fallbacks from `remote-env.ts`).

use std::fmt;

use serde_json::Value;

/// The remote system's path rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemotePathFlavor {
    Posix,
    Win32,
}

impl RemotePathFlavor {
    /// The flavor for Node's platform name (`win32` is Windows).
    pub fn for_platform(platform: &str) -> RemotePathFlavor {
        if platform == "win32" {
            RemotePathFlavor::Win32
        } else {
            RemotePathFlavor::Posix
        }
    }

    /// Whether `character` separates path components in this flavor.
    pub fn is_separator(self, character: char) -> bool {
        match self {
            RemotePathFlavor::Posix => character == '/',
            RemotePathFlavor::Win32 => character == '/' || character == '\\',
        }
    }

    /// The separator this flavor writes (`path.sep`).
    pub fn separator(self) -> char {
        match self {
            RemotePathFlavor::Posix => '/',
            RemotePathFlavor::Win32 => '\\',
        }
    }

    /// Whether the path is absolute in this flavor's rules: a leading `/`
    /// (POSIX), or a rooted path (Windows: `C:\x`, `\\server\share\x`, `\x` —
    /// root-relative, i.e. on the base's drive).
    ///
    /// A drive-qualified but **not** rooted path (`C:foo`) is *not* absolute,
    /// exactly as `path.win32.isAbsolute` reports it: Node resolves such a path
    /// against that drive's own working directory, which
    /// [`RemoteExecutionEnv::resolve`](super::remote_env::RemoteExecutionEnv::resolve)
    /// mirrors through `hello.driveCwds`.
    pub fn is_absolute(self, path: &str) -> bool {
        match self {
            RemotePathFlavor::Posix => path.starts_with('/'),
            RemotePathFlavor::Win32 => win32_parts(path).rooted,
        }
    }

    /// Whether the path is rooted without naming a device (`\x`, `/x` on
    /// Windows): absolute, but resolved on the base's drive.
    pub fn is_root_relative(self, path: &str) -> bool {
        match self {
            RemotePathFlavor::Posix => false,
            RemotePathFlavor::Win32 => {
                let parts = win32_parts(path);
                parts.rooted && parts.device.is_none()
            }
        }
    }

    /// `path.normalize`: collapse `.` and `..`, fix separators, keep the root.
    pub fn normalize(self, path: &str) -> String {
        match self {
            RemotePathFlavor::Posix => {
                let rooted = path.starts_with('/');
                let segments =
                    normalize_segments(path.split('/').filter(|part| !part.is_empty()), rooted);
                let joined = segments.join("/");
                if rooted {
                    format!("/{joined}")
                } else {
                    joined
                }
            }
            RemotePathFlavor::Win32 => {
                let parts = win32_parts(path);
                let segments =
                    normalize_segments(parts.segments.iter().map(String::as_str), parts.rooted);
                let separator = self.separator();
                let mut result = String::new();
                if let Some(device) = &parts.device {
                    result.push_str(device);
                }
                if parts.rooted {
                    result.push(separator);
                }
                result.push_str(&segments.join(&separator.to_string()));
                if result.is_empty() {
                    ".".to_string()
                } else {
                    result
                }
            }
        }
    }

    /// `path.resolve(base, path)`: an absolute `path` normalizes on its own, a
    /// relative one resolves against `base` (which the caller guarantees is
    /// absolute).
    pub fn resolve(self, base: &str, path: &str) -> String {
        match self {
            RemotePathFlavor::Posix => {
                if self.is_absolute(path) {
                    return self.normalize(path);
                }
                let joined = if base.is_empty() {
                    path.to_string()
                } else {
                    format!("{base}/{path}")
                };
                self.normalize(&joined)
            }
            RemotePathFlavor::Win32 => {
                let parts = win32_parts(path);
                let separator = self.separator();
                let base = self.normalize(base);
                let base_device = win32_parts(&base).device;
                if path.is_empty() {
                    return base;
                }
                if parts.rooted {
                    if parts.device.is_some() {
                        // `C:\x`, `\\server\share\x`, `\\?\C:\x`: absolute on
                        // their own.
                        return self.normalize(path);
                    }
                    // `\x`: absolute, but on the base's drive (`path.win32.resolve`
                    // resolves it against the drive of its first argument).
                    let mut result = base_device.unwrap_or_default();
                    result.push(separator);
                    result.push_str(
                        &normalize_segments(parts.segments.iter().map(String::as_str), true)
                            .join(&separator.to_string()),
                    );
                    return result;
                }
                if let Some(device) = parts.device {
                    // `C:foo`: that drive's working directory plus the rest.
                    // On the base's drive the base *is* that working directory;
                    // another drive's is the caller's to supply
                    // (`RemoteExecutionEnv::resolve` reads `hello.driveCwds`),
                    // so a plain resolve uses the drive's root.
                    let same_drive = base_device
                        .as_deref()
                        .is_some_and(|base| base.eq_ignore_ascii_case(&device));
                    let mut result = if same_drive { base } else { device };
                    result.push(separator);
                    result.push_str(&parts.segments.join(&separator.to_string()));
                    return self.normalize(&result);
                }
                self.normalize(&format!("{base}{separator}{path}"))
            }
        }
    }

    /// `path.join(parts…)`: join without resolving against a base (a relative
    /// first part stays relative).
    pub fn join(self, parts: &[&str]) -> String {
        let separator = self.separator();
        let joined = parts
            .iter()
            .filter(|part| !part.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(&separator.to_string());
        self.normalize(&joined)
    }

    /// `path.basename`: the last segment (the root itself has none).
    pub fn basename(self, path: &str) -> String {
        let normalized = self.normalize(path);
        let separator = self.separator();
        match normalized.rfind(separator) {
            Some(index) => normalized[index + 1..].to_string(),
            None => normalized,
        }
    }

    /// `path.dirname`.
    pub fn dirname(self, path: &str) -> String {
        let normalized = self.normalize(path);
        let separator = self.separator();
        match normalized.rfind(separator) {
            None => ".".to_string(),
            Some(0) => {
                if self == RemotePathFlavor::Win32 {
                    normalized
                } else {
                    "/".to_string()
                }
            }
            Some(index) => normalized[..index].to_string(),
        }
    }

    /// Whether `candidate` is `root` or below it, component-wise. Windows
    /// compares case-insensitively (`path.relative`-style root checks there are
    /// case-insensitive too).
    pub fn contains(self, root: &str, candidate: &str) -> bool {
        let root = self.normalize(root);
        let candidate = self.normalize(candidate);
        let fold = |text: &str| {
            if self == RemotePathFlavor::Win32 {
                text.to_lowercase()
            } else {
                text.to_string()
            }
        };
        let (root, candidate) = (fold(&root), fold(&candidate));
        if root == candidate {
            return true;
        }
        let separator = self.separator();
        let prefix = if root.ends_with(separator) {
            root
        } else {
            format!("{root}{separator}")
        };
        candidate.starts_with(&prefix)
    }
}

impl fmt::Display for RemotePathFlavor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            RemotePathFlavor::Posix => "posix",
            RemotePathFlavor::Win32 => "win32",
        })
    }
}

/// The drive/device prefix, rootedness and segments of a Windows path.
struct Win32Parts {
    /// `C:` or `\\server\share` (the UNC device), in the spelling Node uses.
    device: Option<String>,
    rooted: bool,
    segments: Vec<String>,
}

fn win32_parts(path: &str) -> Win32Parts {
    let mut rooted = false;
    let mut rest = path;

    if let Some(stripped) = rest
        .strip_prefix("\\\\?\\")
        .or_else(|| rest.strip_prefix("\\\\.\\"))
    {
        // `\\?\C:\x` and `\\?\UNC\server\share\x`: the verbatim prefix stays in
        // the device, as Node's `win32.parse` reports it in `root`.
        rooted = true;
        let normalized = stripped.replace('/', "\\");
        let mut parts = normalized.splitn(2, '\\');
        let head = parts.next().unwrap_or_default();
        let tail = parts.next().unwrap_or_default();
        if head.starts_with("UNC") {
            let mut unc_parts = tail.splitn(2, '\\');
            let server = unc_parts.next().unwrap_or_default();
            let share_and_rest = unc_parts.next().unwrap_or_default();
            let mut share_parts = share_and_rest.splitn(2, '\\');
            let share = share_parts.next().unwrap_or_default();
            return Win32Parts {
                device: Some(format!("\\\\?\\UNC\\{server}\\{share}")),
                rooted,
                segments: share_parts
                    .next()
                    .map(split_win32_segments)
                    .unwrap_or_default(),
            };
        }
        return Win32Parts {
            device: Some(format!("\\\\?\\{head}")),
            rooted,
            segments: split_win32_segments(tail),
        };
    }
    if let Some(stripped) = rest
        .strip_prefix("\\\\")
        .or_else(|| rest.strip_prefix("//"))
    {
        // `\\server\share\x`: the device is `\\server\share`, the rest is below it.
        let mut parts = stripped.splitn(3, ['\\', '/']);
        let server = parts.next().unwrap_or_default();
        let share = parts.next().unwrap_or_default();
        if !server.is_empty() && !share.is_empty() {
            return Win32Parts {
                device: Some(format!("\\\\{server}\\{share}")),
                rooted: true,
                segments: parts.next().map(split_win32_segments).unwrap_or_default(),
            };
        }
    }
    let mut device = None;
    if rest.len() >= 2 {
        let (first, second) = (rest.as_bytes()[0], rest.as_bytes()[1]);
        if second == b':' && first.is_ascii_alphabetic() {
            device = Some(rest[..2].to_string());
            rest = &rest[2..];
        }
    }
    rooted = rooted || rest.starts_with(['\\', '/']);
    Win32Parts {
        device,
        rooted,
        segments: split_win32_segments(rest),
    }
}

/// The `.`/`..`-free segment list of the text after a Windows root.
fn split_win32_segments(text: &str) -> Vec<String> {
    text.split(['\\', '/'])
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// Node's segment normalization: `.` dropped, `..` pops the previous segment,
/// and a `..` with nothing to pop is dropped at a root (`/../a` is `/a`) but
/// kept in a relative path (`../a` stays `../a`).
fn normalize_segments<'a>(parts: impl Iterator<Item = &'a str>, rooted: bool) -> Vec<String> {
    let mut segments: Vec<String> = Vec::new();
    for part in parts {
        match part {
            "." | "" => {}
            ".." => match segments.last() {
                Some(last) if last != ".." => {
                    segments.pop();
                }
                _ if rooted => {}
                _ => segments.push("..".to_string()),
            },
            other => segments.push(other.to_string()),
        }
    }
    segments
}

/// A `file://` URL as a path, or `None` when it is not a file URL. Mirrors
/// Node's `fileURLToPath` for the shapes a client sends: the platform's own
/// spelling has already been chosen by the caller (`{ windows: true }` on a
/// Windows daemon), so a leading `/C:/…` becomes `C:\…`.
pub fn file_url_to_path(url: &str, flavor: RemotePathFlavor) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    // `file://host/path` is a UNC path on Windows; a non-empty authority on
    // POSIX is not a local path at all.
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let decoded = percent_decode(path);
    match flavor {
        RemotePathFlavor::Posix => {
            if authority.is_empty() || authority == "localhost" {
                Some(decoded)
            } else {
                None
            }
        }
        RemotePathFlavor::Win32 => {
            if !authority.is_empty() {
                return Some(
                    RemotePathFlavor::Win32.normalize(&format!("\\\\{authority}{decoded}")),
                );
            }
            let trimmed = decoded.trim_start_matches('/');
            Some(RemotePathFlavor::Win32.normalize(trimmed))
        }
    }
}

/// Percent-decode a URL path, as `fileURLToPath` does.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(byte) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `driveCwds` entry for a drive letter: `hello.driveCwds` is keyed by the
/// uppercase drive (`=C:` becomes `C:`), as Node's `path.resolve` reads it.
pub fn drive_cwd(drive_cwds: &serde_json::Map<String, Value>, drive: &str) -> Option<String> {
    let key = drive.to_uppercase();
    drive_cwds
        .get(&key)
        .or_else(|| drive_cwds.get(key.trim_end_matches(':')))
        .and_then(Value::as_str)
        .map(str::to_string)
}
