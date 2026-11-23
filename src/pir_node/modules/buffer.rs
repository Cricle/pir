//! Pure byte-array kernels behind the native `Buffer` codec
//! (`__pirBufferCodec`, see `src/extensions/runtime/host_functions/buffer_ops.rs`).
//!
//! The JS shim keeps argument validation, encoding lookup and every fallback;
//! these functions are the O(n) byte work that the engine would otherwise run
//! one JS array element at a time. They live here so the algorithms are
//! testable without an rquickjs runtime.

// ---------------------------------------------------------------------------
// Codec kernels
//
// These live here (not in the extension host crate) because the workspace dev
// profile compiles this crate at opt-level 3 while the host stays at O0. The
// encode/decode state machines are pure functions of their input, so the host
// (`__pirBufferCodec`) is a thin marshalling wrapper around them.
// ---------------------------------------------------------------------------

/// latin1 → string: one byte is one code point (U+0000..=U+00FF), which is a
/// single UTF-8 byte below 0x80 and two above. ASCII input is a straight copy.
#[allow(unsafe_code)]
pub fn latin1_decode(bytes: &[u8]) -> String {
    if bytes.is_ascii() {
        // SAFETY: ASCII is valid UTF-8.
        return unsafe { String::from_utf8_unchecked(bytes.to_vec()) };
    }
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        if byte < 0x80 {
            out.push(byte);
        } else {
            out.push(0xc0 | (byte >> 6));
            out.push(0x80 | (byte & 0x3f));
        }
    }
    // SAFETY: the loop above emits only valid UTF-8 scalar encodings.
    unsafe { String::from_utf8_unchecked(out) }
}

/// string → latin1 bytes: one byte per UTF-16 unit (the JS `latin1Bytes`
/// `charCodeAt(i) & 0xff` semantics).
pub fn latin1_encode(text: &str) -> Vec<u8> {
    // One byte per UTF-16 unit, and a unit never outgrows the UTF-8 bytes of
    // its char, so `len()` is an exact capacity bound.
    let mut bytes = Vec::with_capacity(text.len());
    bytes.extend(text.encode_utf16().map(|unit| (unit & 0xff) as u8));
    bytes
}

/// string → utf16le bytes (two bytes per UTF-16 unit, LE).
pub fn utf16le_encode(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() * 2);
    bytes.extend(
        text.encode_utf16()
            .flat_map(|unit| [unit as u8, (unit >> 8) as u8]),
    );
    bytes
}

#[allow(unsafe_code)]
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() * 2);
    // Two direct stores per byte into the reserved capacity (the `push` pair
    // this replaces re-checked the length twice per input byte).
    unsafe {
        let ptr: *mut u8 = out.as_mut_ptr();
        for (i, &byte) in bytes.iter().enumerate() {
            ptr.add(2 * i).write(HEX[(byte >> 4) as usize]);
            ptr.add(2 * i + 1).write(HEX[(byte & 15) as usize]);
        }
        out.set_len(bytes.len() * 2);
        // SAFETY: every byte written above comes from the ASCII `HEX` table.
        String::from_utf8_unchecked(out)
    }
}

/// Hex nibble lookup: 0..=15 for a hex digit, `0xff` for anything else. A
/// table keeps `hex_decode` branch-free per byte (the `match` it replaces
/// mispredicted on every digit class change).
const HEX_NIBBLE: [u8; 256] = {
    let mut table = [0xffu8; 256];
    let mut i = 0;
    while i < 10 {
        table[b'0' as usize + i] = i as u8;
        i += 1;
    }
    let mut i = 0;
    while i < 6 {
        table[b'a' as usize + i] = (i + 10) as u8;
        table[b'A' as usize + i] = (i + 10) as u8;
        i += 1;
    }
    table
};

/// Hex decode mirroring the JS `hexBytes`: stops at the first invalid pair
/// and ignores a trailing single character.
#[allow(unsafe_code)]
pub fn hex_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() / 2);
    let ptr = out.as_mut_ptr();
    let mut count = 0usize;
    let mut i = 0;
    while i + 1 < bytes.len() {
        let hi = HEX_NIBBLE[bytes[i] as usize];
        let lo = HEX_NIBBLE[bytes[i + 1] as usize];
        if (hi | lo) == 0xff {
            break;
        }
        // SAFETY: `count` stays below `bytes.len() / 2`, the reserved
        // capacity, and the bytes written are arbitrary `u8` values.
        unsafe { ptr.add(count).write((hi << 4) | lo) };
        count += 1;
        i += 2;
    }
    // SAFETY: the first `count` bytes of the capacity were initialized above.
    unsafe { out.set_len(count) };
    out
}

/// `base64_value` as a 256-entry table (`0xff` = not in the alphabet, which
/// also covers `=`); a lookup keeps the bulk decoder branch-free.
const BASE64_LUT: [u8; 256] = {
    let mut table = [0xffu8; 256];
    let mut i = 0;
    while i < 26 {
        table[b'A' as usize + i] = i as u8;
        table[b'a' as usize + i] = (i + 26) as u8;
        i += 1;
    }
    let mut d = 0;
    while d < 10 {
        table[b'0' as usize + d] = (d + 52) as u8;
        d += 1;
    }
    // Node's decoder table accepts the URL-safe alphabet in both modes
    // (`Buffer.from('__--_--_--__', 'base64')` decodes like base64url).
    table[b'+' as usize] = 62;
    table[b'-' as usize] = 62;
    table[b'/' as usize] = 63;
    table[b'_' as usize] = 63;
    table
};

/// Base64 decode mirroring the JS `base64Bytes`: `=` ends the payload,
/// non-alphabet characters are skipped, bits flush on byte boundaries.
pub fn base64_decode(text: &str) -> Vec<u8> {
    // Fast path: strictly-valid standard base64 (the common shape — no
    // whitespace, no URL-safe characters, padding only at the end) decodes
    // with the `base64` crate's table decoder. Anything it rejects falls
    // through to the permissive JS-parity loop below.
    use base64::Engine as _;
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(text.as_bytes()) {
        return bytes;
    }
    // Every four input characters carry at most three output bytes (skipped
    // characters and `=` only lower that), so this is a hard upper bound and
    // the buffer never has to grow — same sizing idiom as `base64_encode`.
    let input = text.as_bytes();
    let mut out = Vec::with_capacity(input.len().div_ceil(4) * 3);
    let mut i = 0;
    // Bulk path: four alphabet characters per step. `0xff` marks either `=`
    // (payload end) or a character to skip; both hand the rest to the
    // byte-at-a-time tail, which keeps the reference semantics exactly.
    while i + 4 <= input.len() {
        let a = BASE64_LUT[input[i] as usize];
        let b = BASE64_LUT[input[i + 1] as usize];
        let c = BASE64_LUT[input[i + 2] as usize];
        let d = BASE64_LUT[input[i + 3] as usize];
        if (a | b | c | d) & 0x80 != 0 {
            break;
        }
        out.push((a << 2) | (b >> 4));
        out.push((b << 4) | (c >> 2));
        out.push((c << 6) | d);
        i += 4;
    }
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in &input[i..] {
        if byte == b'=' {
            break;
        }
        let value = BASE64_LUT[byte as usize];
        if value == 0xff {
            continue;
        }
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    out
}

/// Base64 encode with standard `=` padding (JS `base64Encode` parity).
pub fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Base64url encode: URL-safe alphabet, padding dropped (JS `base64urlEncode`
/// parity: `base64Encode(bytes).replace(/\+/g,'-').replace(/\//g,'_').replace(/=+$/,'')`).
pub fn base64url_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// WHATWG UTF-8 decode with replacement semantics (Node/V8 parity): each
/// maximal invalid subpart becomes exactly one U+FFFD.
pub fn utf8_decode(bytes: &[u8]) -> String {
    // Fast path: valid UTF-8 (the overwhelmingly common case) is a validate +
    // copy instead of a byte-at-a-time decode.
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_owned();
    }
    utf8_decode_whatwg(bytes, true).0
}

/// Strict UTF-8 validation with no replacement: `Some(text)` only when the
/// whole input is valid UTF-8. Callers that only need "valid or not" avoid the
/// replacement decoder's per-byte fallback.
pub fn utf8_decode_strict(bytes: &[u8]) -> Option<String> {
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

/// WHATWG UTF-8 decoder. Returns `(text, consumed, invalid)`:
///
/// - `consumed < bytes.len()` only when `!is_final` and the tail is a valid
///   but incomplete sequence; the caller holds those bytes for the next chunk.
/// - `invalid` is true when a U+FFFD was emitted for an ill-formed sequence
///   (or for a truncated tail at end of input).
///
/// Consumption follows the maximal-subpart rule: a bad *second* byte consumes
/// only the lead, a bad later continuation consumes the lead plus the valid
/// continuations before it, and the per-lead second-byte range rejects
/// overlongs (`E0 80`), surrogates (`ED A0`) and beyond-U+10FFFF (`F4 90`)
/// instead of decoding them.
pub fn utf8_decode_whatwg(bytes: &[u8], is_final: bool) -> (String, usize, bool) {
    let mut out = String::with_capacity(bytes.len());
    let mut invalid = false;
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        if b0 < 0x80 {
            out.push(b0 as char);
            i += 1;
            continue;
        }
        // (continuations needed, allowed range for the second byte)
        let (need, lo, hi) = match b0 {
            0xc2..=0xdf => (1, 0x80, 0xbf),
            0xe0 => (2, 0xa0, 0xbf),
            0xe1..=0xec => (2, 0x80, 0xbf),
            0xed => (2, 0x80, 0x9f),
            0xee..=0xef => (2, 0x80, 0xbf),
            0xf0 => (3, 0x90, 0xbf),
            0xf1..=0xf3 => (3, 0x80, 0xbf),
            0xf4 => (3, 0x80, 0x8f),
            _ => {
                invalid = true;
                out.push('\u{FFFD}');
                i += 1;
                continue;
            }
        };
        let avail = bytes.len() - i - 1;
        let checked = avail.min(need);
        // Lead plus every present byte that validated. On a bad byte this is
        // the maximal subpart to consume (and re-scan from).
        let mut valid_prefix = 1;
        let mut bad = false;
        if checked >= 1 {
            if !(lo..=hi).contains(&bytes[i + 1]) {
                bad = true;
            } else {
                valid_prefix = 2;
                let mut k = 2;
                while k <= checked {
                    if !(0x80..=0xbf).contains(&bytes[i + k]) {
                        bad = true;
                        break;
                    }
                    valid_prefix = k + 1;
                    k += 1;
                }
            }
        }
        if bad {
            invalid = true;
            out.push('\u{FFFD}');
            i += valid_prefix;
            continue;
        }
        if avail < need {
            // Valid prefix of a longer sequence.
            if !is_final {
                return (out, i, invalid);
            }
            invalid = true;
            out.push('\u{FFFD}');
            i += valid_prefix;
            continue;
        }
        let cp: u32 = match need {
            1 => (u32::from(b0 & 0x1f) << 6) | u32::from(bytes[i + 1] & 0x3f),
            2 => {
                (u32::from(b0 & 0x0f) << 12)
                    | (u32::from(bytes[i + 1] & 0x3f) << 6)
                    | u32::from(bytes[i + 2] & 0x3f)
            }
            _ => {
                (u32::from(b0 & 0x07) << 18)
                    | (u32::from(bytes[i + 1] & 0x3f) << 12)
                    | (u32::from(bytes[i + 2] & 0x3f) << 6)
                    | u32::from(bytes[i + 3] & 0x3f)
            }
        };
        out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
        i += need + 1;
    }
    (out, i, invalid)
}

/// latin1 bytes as UTF-16 code units (each byte is its own code point). Used
/// only when a latin1 decode cannot stay on the ASCII fast path: handing
/// `JS_NewStringUTF16` these units replaces the engine's UTF-8 scan+decode of
/// the 2-bytes-per-char image with a straight `memcpy`.
pub fn latin1_units(bytes: &[u8]) -> Vec<u16> {
    bytes.iter().map(|&byte| u16::from(byte)).collect()
}

/// UTF-16 code units for `text` when it contains a code point above U+00FF
/// (i.e. it cannot use QuickJS's narrow 8-bit string form); `None` otherwise.
///
/// Kept here so the decision scan and the `encode_utf16` pass run at O3: the
/// same iterator in the O0 host crate cost ~10µs per 1.2KB string.
pub fn utf8_units_if_wide(text: &str) -> Option<Vec<u16>> {
    if text.chars().any(|ch| ch as u32 > 0xff) {
        Some(text.encode_utf16().collect())
    } else {
        None
    }
}

/// UTF-16LE decode over whole bytes; the odd trailing byte is not consumed
/// (streaming carries it, the final flush drops it — JS parity). Returns the
/// code units *verbatim*, including unpaired surrogates: a JS string can hold
/// them, so the host builds the value with `JS_NewStringUTF16` instead of
/// falling back to the JS per-byte machine.
pub fn utf16le_decode_units(bytes: &[u8]) -> (Vec<u16>, usize) {
    let paired_len = bytes.len() - (bytes.len() % 2);
    let mut units = Vec::with_capacity(paired_len / 2);
    let mut at = 0;
    while at < paired_len {
        units.push(u16::from(bytes[at]) | (u16::from(bytes[at + 1]) << 8));
        at += 2;
    }
    (units, paired_len)
}

/// Lexicographic comparison of two byte ranges, matching Node's
/// `Buffer#compare`/`compareOffset` verdict (`-1` / `0` / `1`): the first
/// differing byte decides, and an equal common prefix leaves the verdict to
/// the range lengths (the shorter range sorts first).
pub fn compare_ranges(source: &[u8], target: &[u8]) -> i32 {    let common = source.len().min(target.len());
    for i in 0..common {
        if source[i] != target[i] {
            return if source[i] < target[i] { -1 } else { 1 };
        }
    }
    match source.len().cmp(&target.len()) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// In-place endian swap of every `size`-byte group (`Buffer#swap16/32/64`).
/// `size` must be 2, 4 or 8 and the buffer length must be a multiple of it;
/// the caller validates both (Node throws `ERR_INVALID_BUFFER_SIZE`).
pub fn swap_in_place(bytes: &mut [u8], size: usize) {
    debug_assert!(matches!(size, 2 | 4 | 8));
    for group in bytes.chunks_exact_mut(size) {
        group.reverse();
    }
}


