//! Node `path` kernels for the O(n) char-scanning operations.
//!
//! `join`, `resolve`, `normalize` and `relative` are called constantly by
//! extension code (and measured 15–49 µs each in QuickJS on this host, versus
//! a few µs of Rust work). This is a faithful port of Node v24's
//! `lib/path.js` algorithms for both namespaces; the JS shim keeps argument
//! validation, the `parse`/`format`/`basename`/`dirname`/`extname` surface and
//! every fallback, so behavior never depends on this module being present.
//!
//! The port scans UTF-8 bytes. Every structural character (both separators,
//! `.`, `:`) is ASCII, so a byte scan makes exactly the same decisions as
//! Node's UTF-16 scan and every produced slice boundary is a UTF-8 char
//! boundary. The old `Vec<char>` copy of the whole input (one allocation per
//! op, three or four for `join`) is gone; each kernel now builds only its
//! output string.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Win32,
    Posix,
}

impl Flavor {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "win32" => Some(Self::Win32),
            "posix" => Some(Self::Posix),
            _ => None,
        }
    }
}

const WINDOWS_RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "COM\u{b9}",
    "COM\u{b2}", "COM\u{b3}", "LPT\u{b9}", "LPT\u{b2}", "LPT\u{b3}",
];

#[inline]
fn is_sep_byte(byte: u8) -> bool {
    byte == b'\\' || byte == b'/'
}

#[inline]
fn is_separator(code: char, flavor: Flavor) -> bool {
    match flavor {
        Flavor::Win32 => code == '\\' || code == '/',
        Flavor::Posix => code == '/',
    }
}

#[inline]
fn is_windows_device_root(byte: u8) -> bool {
    byte.is_ascii_alphabetic()
}

/// `isWindowsReservedName(path, colonIndex)` with only the device prefix:
/// the bytes before the colon, compared case-insensitively (ASCII case
/// folding; the `COM¹`-style entries carry no case).
fn is_reserved_prefix(prefix: &[u8]) -> bool {
    WINDOWS_RESERVED_NAMES.iter().any(|name| {
        let name = name.as_bytes();
        name.len() == prefix.len()
            && name
                .iter()
                .zip(prefix)
                .all(|(&expected, &actual)| expected == actual.to_ascii_uppercase())
    })
}

/// `isWindowsReservedName(path, colonIndex)` when the only thing that matters
/// is the device prefix. Node always calls it with `colonIndex` =
/// `indexOf(':')`, so a missing colon passes `-1`: `slice(0, -1)` drops the
/// final code unit and `"COM9."` is then compared as `"COM9"` (the reason
/// `path.win32.normalize('COM9.')` prefixes `.\`). An astral final character
/// would leave a lone high surrogate in Node's slice, which can never equal a
/// reserved name, so the whole path is compared instead.
fn is_reserved_name(path: &str, colon: Option<usize>) -> bool {
    let end = match colon {
        Some(index) => index,
        None => match path.char_indices().next_back() {
            Some((index, ch)) if (ch as u32) <= 0xFFFF => index,
            Some(_) => path.len(),
            None => 0,
        },
    };
    is_reserved_prefix(&path.as_bytes()[..end.min(path.len())])
}

/// Node's `normalizeString`: resolves `.`/`..` segments (the shared core of
/// `normalize`, `join` and `resolve`). A stack of borrowed segments plus one
/// output buffer: no intermediate per-segment `String`.
fn normalize_string(path: &str, allow_above_root: bool, separator: char, flavor: Flavor) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split(|code| is_separator(code, flavor)) {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            match segments.last() {
                Some(&last) if last != ".." => {
                    segments.pop();
                }
                _ => {
                    if allow_above_root {
                        segments.push("..");
                    }
                }
            }
        } else {
            segments.push(segment);
        }
    }
    if segments.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(path.len());
    for (index, segment) in segments.iter().enumerate() {
        if index > 0 {
            out.push(separator);
        }
        out.push_str(segment);
    }
    out
}

// ---------------------------------------------------------------------------
// win32
// ---------------------------------------------------------------------------

pub fn win32_normalize(path: &str) -> String {
    let bytes = path.as_bytes();
    let len = bytes.len();
    if len == 0 {
        return ".".to_owned();
    }

    let mut root_end = 0usize;
    let mut device: Option<String> = None;
    let mut is_absolute = false;
    let code = bytes[0];

    if len == 1 {
        return if code == b'/' {
            "\\".to_owned()
        } else {
            path.to_owned()
        };
    }

    if is_sep_byte(code) {
        is_absolute = true;
        if is_sep_byte(bytes[1]) {
            let mut j = 2usize;
            let mut last = j;
            while j < len && !is_sep_byte(bytes[j]) {
                j += 1;
            }
            if j < len && j != last {
                let first_part = &path[last..j];
                last = j;
                while j < len && is_sep_byte(bytes[j]) {
                    j += 1;
                }
                if j < len && j != last {
                    last = j;
                    while j < len && !is_sep_byte(bytes[j]) {
                        j += 1;
                    }
                    if j == len || j != last {
                        if first_part == "." || first_part == "?" {
                            device = Some(format!("\\\\{first_part}"));
                            root_end = 4;
                            let colon_end = path.find(':').map(|c| c + 1).unwrap_or(4).min(len);
                            let possible_device = if colon_end >= 4 {
                                &path[4..colon_end]
                            } else {
                                ""
                            };
                            let device_prefix = possible_device.as_bytes();
                            if !device_prefix.is_empty()
                                && is_reserved_prefix(&device_prefix[..device_prefix.len() - 1])
                            {
                                device = Some(format!("\\\\?\\{possible_device}"));
                                root_end = 4 + possible_device.len();
                            }
                        } else if j == len {
                            return format!("\\\\{first_part}\\{}\\" , &path[last..]);
                        } else {
                            device = Some(format!("\\\\{first_part}\\{}", &path[last..j]));
                            root_end = j;
                        }
                    }
                }
            }
        } else {
            root_end = 1;
        }
    } else {
        let colon_index = path.find(':');
        if let Some(colon) = colon_index {
            if colon > 0 {
                if is_windows_device_root(code) && colon == 1 {
                    device = Some(path[0..2].to_owned());
                    root_end = 2;
                    if len > 2 && is_sep_byte(bytes[2]) {
                        is_absolute = true;
                        root_end = 3;
                    }
                } else if is_reserved_name(path, Some(colon)) {
                    device = Some(path[0..colon + 1].to_owned());
                    root_end = colon + 1;
                }
            }
        }
    }

    let mut tail = if root_end < len {
        normalize_string(&path[root_end..], !is_absolute, '\\', Flavor::Win32)
    } else {
        String::new()
    };
    if tail.is_empty() && !is_absolute {
        tail = ".".to_owned();
    }
    if !tail.is_empty() && is_sep_byte(bytes[len - 1]) {
        tail.push('\\');
    }

    if !is_absolute && device.is_none() && path.contains(':') {
        let tail_bytes = tail.as_bytes();
        if tail_bytes.len() >= 2
            && is_windows_device_root(tail_bytes[0])
            && tail_bytes[1] == b':'
        {
            return format!(".\\{tail}");
        }
        let mut search = 0usize;
        while let Some(found) = path[search..].find(':') {
            let index = search + found;
            if index == len - 1 || is_sep_byte(bytes[index + 1]) {
                return format!(".\\{tail}");
            }
            search = index + 1;
        }
    }

    if is_reserved_name(path, path.find(':')) {
        return format!(".\\{}{}", device.unwrap_or_default(), tail);
    }

    let device = device.unwrap_or_default();
    if is_absolute {
        format!("{device}\\{tail}")
    } else {
        format!("{device}{tail}")
    }
}

pub fn win32_resolve<S: AsRef<str>>(parts: &[S], cwd: &str) -> String {
    let host_is_windows = cfg!(windows);

    // Fast path for the common `resolve("a", "b", "..", "c")` shape: no part
    // is absolute or drive-relative and the cwd is drive-rooted, so the
    // device/tail dance collapses to normalizing `cwd \ parts...`. This skips
    // the reverse loop's per-part root parsing entirely.
    let cwd_bytes = cwd.as_bytes();
    let cwd_is_drive = cwd_bytes.len() >= 2
        && cwd_bytes[0].is_ascii_alphabetic()
        && cwd_bytes[1] == b':'
        && (cwd_bytes.len() == 2 || is_sep_byte(cwd_bytes[2]));
    if cwd_is_drive {
        let mut any_non_empty = false;
        let mut all_relative = true;
        for part in parts {
            let part = part.as_ref();
            if part.is_empty() {
                continue;
            }
            any_non_empty = true;
            let bytes = part.as_bytes();
            if is_sep_byte(bytes[0])
                || (bytes.len() > 1 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
            {
                all_relative = false;
                break;
            }
        }
        if any_non_empty && all_relative {
            let total = cwd.len()
                + parts
                    .iter()
                    .map(|part| part.as_ref().len() + 1)
                    .sum::<usize>();
            let mut combined = String::with_capacity(total);
            combined.push_str(cwd);
            for part in parts {
                let part = part.as_ref();
                if part.is_empty() {
                    continue;
                }
                combined.push('\\');
                combined.push_str(part);
            }
            return win32_normalize(&combined);
        }
    }

    let mut resolved_device = String::new();
    // One buffer grows at the front per part: components arrive last-to-first,
    // and `insert_str` only shifts bytes (the capacity covers every part plus
    // the cwd anchor, so it never reallocates).
    let mut resolved_tail = String::with_capacity(
        cwd.len() + parts.iter().map(|part| part.as_ref().len() + 1).sum::<usize>(),
    );
    let mut resolved_absolute = false;

    let mut i = parts.len() as isize - 1;
    while i >= -1 {
        let mut owned;
        let path: &str = if i >= 0 {
            let part = parts[i as usize].as_ref();
            if part.is_empty() {
                i -= 1;
                continue;
            }
            part
        } else if resolved_device.is_empty() {
            let path = cwd;
            let fast = parts.is_empty()
                || (parts.len() == 1
                    && (parts[0].as_ref().is_empty() || parts[0].as_ref() == ".")
                    && path.as_bytes().first().is_some_and(|&b| is_sep_byte(b)));
            if fast {
                if !host_is_windows {
                    return path.replace('/', "\\");
                }
                return path.to_owned();
            }
            path
        } else {
            let env_key = format!("={resolved_device}");
            owned = std::env::var(&env_key).unwrap_or_else(|_| cwd.to_owned());
            let replace = {
                let prefix: String = owned.chars().take(2).collect();
                owned.is_empty()
                    || (prefix.to_lowercase() != resolved_device.to_lowercase()
                        && owned.as_bytes().get(2) == Some(&b'\\'))
            };
            if replace {
                owned = format!("{resolved_device}\\");
            }
            &owned
        };

        let bytes = path.as_bytes();
        let len = bytes.len();
        if len == 0 {
            i -= 1;
            continue;
        }
        let mut root_end = 0usize;
        let mut device = String::new();
        let mut is_absolute = false;
        let code = bytes[0];

        if len == 1 {
            if is_sep_byte(code) {
                root_end = 1;
                is_absolute = true;
            }
        } else if is_sep_byte(code) {
            is_absolute = true;
            if is_sep_byte(bytes[1]) {
                let mut j = 2usize;
                let mut last = j;
                while j < len && !is_sep_byte(bytes[j]) {
                    j += 1;
                }
                if j < len && j != last {
                    let first_part = &path[last..j];
                    last = j;
                    while j < len && is_sep_byte(bytes[j]) {
                        j += 1;
                    }
                    if j < len && j != last {
                        last = j;
                        while j < len && !is_sep_byte(bytes[j]) {
                            j += 1;
                        }
                        if j == len || j != last {
                            if first_part != "." && first_part != "?" {
                                device = format!("\\\\{first_part}\\{}", &path[last..j]);
                                root_end = j;
                            } else {
                                device = format!("\\\\{first_part}");
                                root_end = 4;
                            }
                        }
                    }
                }
            } else {
                root_end = 1;
            }
        } else if is_windows_device_root(code) && len > 1 && bytes[1] == b':' {
            device = path[0..2].to_owned();
            root_end = 2;
            if len > 2 && is_sep_byte(bytes[2]) {
                is_absolute = true;
                root_end = 3;
            }
        }

        if !device.is_empty() {
            if !resolved_device.is_empty() {
                if device.to_lowercase() != resolved_device.to_lowercase() {
                    i -= 1;
                    continue;
                }
            } else {
                resolved_device = device;
            }
        }

        if resolved_absolute {
            if !resolved_device.is_empty() {
                break;
            }
        } else {
            let tail_rest = if root_end < len {
                &path[root_end..]
            } else {
                ""
            };
            if !tail_rest.is_empty() {
                if resolved_tail.is_empty() {
                    resolved_tail.push_str(tail_rest);
                } else {
                    resolved_tail.insert_str(0, "\\");
                    resolved_tail.insert_str(0, tail_rest);
                }
            }
            resolved_absolute = is_absolute;
            if is_absolute && !resolved_device.is_empty() {
                break;
            }
        }
        i -= 1;
    }

    let normalized = normalize_string(&resolved_tail, !resolved_absolute, '\\', Flavor::Win32);

    if resolved_absolute {
        let mut out = String::with_capacity(resolved_device.len() + 1 + normalized.len());
        out.push_str(&resolved_device);
        out.push('\\');
        out.push_str(&normalized);
        out
    } else if resolved_device.is_empty() {
        if normalized.is_empty() {
            ".".to_owned()
        } else {
            normalized
        }
    } else {
        let mut out = resolved_device;
        out.push_str(&normalized);
        out
    }
}

pub fn win32_join<S: AsRef<str>>(parts: &[S]) -> String {
    let mut count = 0usize;
    let mut total = 0usize;
    for part in parts {
        let part = part.as_ref();
        if !part.is_empty() {
            count += 1;
            total += part.len();
        }
    }
    if count == 0 {
        return ".".to_owned();
    }

    let mut joined = String::with_capacity(total + count - 1);
    let mut first_part = "";
    let mut wrote = false;
    for part in parts {
        let part = part.as_ref();
        if part.is_empty() {
            continue;
        }
        if wrote {
            joined.push('\\');
        } else {
            first_part = part;
            wrote = true;
        }
        joined.push_str(part);
    }

    let first = first_part.as_bytes();
    let mut needs_replace = true;
    let mut slash_count = 0usize;
    if !first.is_empty() && is_sep_byte(first[0]) {
        slash_count += 1;
        if first.len() > 1 && is_sep_byte(first[1]) {
            slash_count += 1;
            if first.len() > 2 {
                if is_sep_byte(first[2]) {
                    slash_count += 1;
                } else {
                    needs_replace = false;
                }
            }
        }
    }
    if needs_replace {
        while slash_count < joined.len() && is_sep_byte(joined.as_bytes()[slash_count]) {
            slash_count += 1;
        }
        if slash_count >= 2 {
            joined.replace_range(..slash_count, "\\");
        }
    }

    let mut has_reserved = false;
    for segment in joined.split('\\') {
        if segment.is_empty() {
            continue;
        }
        if let Some(colon) = segment.find(':') {
            if is_reserved_prefix(&segment.as_bytes()[..colon]) {
                has_reserved = true;
                break;
            }
        }
    }
    if has_reserved {
        return joined.replace('/', "\\");
    }

    win32_normalize(&joined)
}

pub fn win32_relative(from: &str, to: &str, cwd: &str) -> String {
    if from == to {
        return String::new();
    }
    let from_orig = win32_resolve(&[from], cwd);
    let to_orig = win32_resolve(&[to], cwd);
    if from_orig == to_orig {
        return String::new();
    }
    let from_lower = from_orig.to_lowercase();
    let to_lower = to_orig.to_lowercase();
    if from_lower == to_lower {
        return String::new();
    }

    if from_orig.chars().count() != from_lower.chars().count()
        || to_orig.chars().count() != to_lower.chars().count()
    {
        let mut from_split: Vec<&str> = from_orig.split('\\').collect();
        let mut to_split: Vec<&str> = to_orig.split('\\').collect();
        if from_split.last().is_some_and(|s| s.is_empty()) {
            from_split.pop();
        }
        if to_split.last().is_some_and(|s| s.is_empty()) {
            to_split.pop();
        }
        let from_len = from_split.len();
        let to_len = to_split.len();
        let length = from_len.min(to_len);
        let mut i = 0usize;
        while i < length {
            if from_split[i].to_lowercase() != to_split[i].to_lowercase() {
                break;
            }
            i += 1;
        }
        if i == 0 {
            return to_orig;
        }
        if i == length {
            if to_len > length {
                return to_split[i..].join("\\");
            }
            if from_len > length {
                return format!("{}{}", "..\\".repeat(from_len - 1 - i), "..");
            }
            return String::new();
        }
        return format!(
            "{}{}",
            "..\\".repeat(from_len - i),
            to_split[i..].join("\\")
        );
    }

    let from_bytes = from_lower.as_bytes();
    let to_bytes = to_lower.as_bytes();
    let orig_to = to_orig.as_bytes();

    let mut from_start = 0usize;
    while from_start < from_bytes.len() && from_bytes[from_start] == b'\\' {
        from_start += 1;
    }
    let mut from_end = from_bytes.len();
    while from_end > from_start + 1 && from_bytes[from_end - 1] == b'\\' {
        from_end -= 1;
    }
    let from_len = from_end - from_start;

    let mut to_start = 0usize;
    while to_start < to_bytes.len() && to_bytes[to_start] == b'\\' {
        to_start += 1;
    }
    let mut to_end = to_bytes.len();
    while to_end > to_start + 1 && to_bytes[to_end - 1] == b'\\' {
        to_end -= 1;
    }
    let to_len = to_end - to_start;

    let length = from_len.min(to_len);
    let mut last_common_sep: isize = -1;
    let mut i = 0usize;
    while i < length {
        let from_code = from_bytes[from_start + i];
        if from_code != to_bytes[to_start + i] {
            break;
        } else if from_code == b'\\' {
            last_common_sep = i as isize;
        }
        i += 1;
    }

    if i != length {
        if last_common_sep == -1 {
            return to_orig;
        }
    } else {
        if to_len > length {
            if to_bytes.get(to_start + i) == Some(&b'\\') {
                return to_orig[to_start + i + 1..].to_owned();
            }
            if i == 2 {
                return to_orig[to_start + i..].to_owned();
            }
        }
        if from_len > length {
            if from_bytes.get(from_start + i) == Some(&b'\\') {
                last_common_sep = i as isize;
            } else if i == 2 {
                last_common_sep = 3;
            }
        }
        if last_common_sep == -1 {
            last_common_sep = 0;
        }
    }

    let mut out = String::new();
    let mut k = from_start + last_common_sep.max(0) as usize + 1;
    while k <= from_end {
        if k == from_end || from_bytes.get(k) == Some(&b'\\') {
            if out.is_empty() {
                out.push_str("..");
            } else {
                out.push_str("\\..");
            }
        }
        k += 1;
    }

    let to_start = to_start + last_common_sep.max(0) as usize;
    if !out.is_empty() {
        return format!("{out}{}", &to_orig[to_start..to_end]);
    }
    let mut to_start = to_start;
    if orig_to.get(to_start) == Some(&b'\\') {
        to_start += 1;
    }
    to_orig[to_start..to_end].to_owned()
}

// ---------------------------------------------------------------------------
// posix
// ---------------------------------------------------------------------------

fn posix_cwd(cwd: &str) -> String {
    if cfg!(windows) {
        let slash = cwd.replace('\\', "/");
        match slash.find('/') {
            Some(index) => slash[index..].to_owned(),
            None => slash,
        }
    } else {
        cwd.to_owned()
    }
}

pub fn posix_normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let bytes = path.as_bytes();
    let is_absolute = bytes[0] == b'/';
    let trailing_separator = bytes[bytes.len() - 1] == b'/';
    let mut normalized = normalize_string(path, !is_absolute, '/', Flavor::Posix);
    if normalized.is_empty() {
        if is_absolute {
            return "/".to_owned();
        }
        return if trailing_separator {
            "./".to_owned()
        } else {
            ".".to_owned()
        };
    }
    if trailing_separator {
        normalized.push('/');
    }
    if is_absolute {
        let mut out = String::with_capacity(normalized.len() + 1);
        out.push('/');
        out.push_str(&normalized);
        out
    } else {
        normalized
    }
}

pub fn posix_join<S: AsRef<str>>(parts: &[S]) -> String {
    let mut count = 0usize;
    let mut total = 0usize;
    for part in parts {
        let part = part.as_ref();
        if !part.is_empty() {
            count += 1;
            total += part.len();
        }
    }
    if count == 0 {
        return ".".to_owned();
    }
    let mut joined = String::with_capacity(total + count - 1);
    let mut wrote = false;
    for part in parts {
        let part = part.as_ref();
        if part.is_empty() {
            continue;
        }
        if wrote {
            joined.push('/');
        }
        joined.push_str(part);
        wrote = true;
    }
    posix_normalize(&joined)
}

pub fn posix_resolve<S: AsRef<str>>(parts: &[S], cwd: &str) -> String {
    if parts.is_empty()
        || (parts.len() == 1 && {
            let only = parts[0].as_ref();
            only.is_empty() || only == "."
        })
    {
        let cwd = posix_cwd(cwd);
        if cwd.starts_with('/') {
            return cwd;
        }
    }

    let mut start = parts.len();
    let mut resolved_absolute = false;
    for index in (0..parts.len()).rev() {
        let part = parts[index].as_ref();
        if part.is_empty() {
            continue;
        }
        start = index;
        if part.starts_with('/') {
            resolved_absolute = true;
            break;
        }
    }

    let mut resolved_path = String::new();
    for part in parts.iter().skip(start) {
        let part = part.as_ref();
        if part.is_empty() {
            continue;
        }
        if !resolved_path.is_empty() {
            resolved_path.push('/');
        }
        resolved_path.push_str(part);
    }

    if !resolved_absolute {
        let cwd = posix_cwd(cwd);
        resolved_absolute = cwd.starts_with('/');
        let mut prefixed = String::with_capacity(cwd.len() + 1 + resolved_path.len());
        prefixed.push_str(&cwd);
        prefixed.push('/');
        prefixed.push_str(&resolved_path);
        resolved_path = prefixed;
    }

    let normalized = normalize_string(&resolved_path, !resolved_absolute, '/', Flavor::Posix);
    if resolved_absolute {
        let mut out = String::with_capacity(normalized.len() + 1);
        out.push('/');
        out.push_str(&normalized);
        out
    } else if normalized.is_empty() {
        ".".to_owned()
    } else {
        normalized
    }
}

pub fn posix_relative(from: &str, to: &str, cwd: &str) -> String {
    if from == to {
        return String::new();
    }
    let from = posix_resolve(&[from], cwd);
    let to = posix_resolve(&[to], cwd);
    if from == to {
        return String::new();
    }

    let from_bytes = from.as_bytes();
    let to_bytes = to.as_bytes();
    let from_start = 1usize;
    let from_end = from_bytes.len();
    let from_len = from_end - from_start;
    let to_start = 1usize;
    let to_len = to_bytes.len() - to_start;

    let length = from_len.min(to_len);
    let mut last_common_sep: isize = -1;
    let mut i = 0usize;
    while i < length {
        let from_code = from_bytes[from_start + i];
        if from_code != to_bytes[to_start + i] {
            break;
        } else if from_code == b'/' {
            last_common_sep = i as isize;
        }
        i += 1;
    }

    if i == length {
        if to_len > length {
            if to_bytes.get(to_start + i) == Some(&b'/') {
                return to[to_start + i + 1..].to_owned();
            }
            if i == 0 {
                return to[to_start + i..].to_owned();
            }
        } else if from_len > length {
            if from_bytes.get(from_start + i) == Some(&b'/') {
                last_common_sep = i as isize;
            } else if i == 0 {
                last_common_sep = 0;
            }
        }
    }

    let mut out = String::new();
    // Node starts at `fromStart + lastCommonSep + 1`, so a common separator
    // of -1 (the paths differ at their first character) still walks this
    // path's components from the root. Using max(0) here produced
    // "..\..bin" instead of "../../bin".
    let mut k = (from_start as isize + last_common_sep + 1) as usize;
    while k <= from_end {
        if k == from_end || from_bytes.get(k) == Some(&b'/') {
            if out.is_empty() {
                out.push_str("..");
            } else {
                out.push_str("/..");
            }
        }
        k += 1;
    }

    let tail_start = (to_start as isize + last_common_sep) as usize;
    let mut out = out;
    out.push_str(&to[tail_start..]);
    out
}

// ---------------------------------------------------------------------------
// Public dispatch
// ---------------------------------------------------------------------------

pub fn normalize(flavor: Flavor, path: &str) -> String {
    match flavor {
        Flavor::Win32 => win32_normalize(path),
        Flavor::Posix => posix_normalize(path),
    }
}

pub fn join<S: AsRef<str>>(flavor: Flavor, parts: &[S]) -> String {
    match flavor {
        Flavor::Win32 => win32_join(parts),
        Flavor::Posix => posix_join(parts),
    }
}

pub fn resolve<S: AsRef<str>>(flavor: Flavor, parts: &[S], cwd: &str) -> String {
    match flavor {
        Flavor::Win32 => win32_resolve(parts, cwd),
        Flavor::Posix => posix_resolve(parts, cwd),
    }
}

pub fn relative(flavor: Flavor, from: &str, to: &str, cwd: &str) -> String {
    match flavor {
        Flavor::Win32 => win32_relative(from, to, cwd),
        Flavor::Posix => posix_relative(from, to, cwd),
    }
}

