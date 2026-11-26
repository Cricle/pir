//! Incremental zlib decoders used by the streaming `PirZlibTransform`.
//!
//! The one-shot codec entry (`zlib_op`) keeps whole-input semantics for every
//! family. A decompression stream, however, has to observe Node's streaming
//! contract:
//!
//! * `write()` can complete the compressed stream and end the readable side
//!   without `end()` being called;
//! * `_readableState.buffer` fills to the high-water mark and decompression
//!   stops there (test-zlib-brotli-16GB);
//! * `bytesWritten` is the input the decoder consumed, so trailing bytes after
//!   the stream's end are not counted.
//!
//! A one-shot call cannot stop mid-frame and resume later without re-decoding
//! (which is O(n^2) and would allocate the whole 16 GB output). This module
//! therefore keeps a decoder state per stream on the host side:
//!
//! * `zlibStreamOpen`   — build an `Inflate`/`Brotli` session from the codec
//!   options; returns `{ id }`.
//! * `zlibStreamPush`   — append input bytes and decode at most `maxOut` output
//!   bytes; returns `{ data, consumed, ended }`.
//! * `zlibStreamDrain`  — continue decoding already-buffered input/output.
//! * `zlibStreamFinish` — the `end()` flush (`finishFlush` semantics).
//! * `zlibStreamClose`  — drop the session.
//!
//! Only the decoders whose streaming path is exercised by the Node suite are
//! hosted here (`inflate`, `inflateRaw`, `brotliDecompress`). `gunzip`,
//! `unzip` and the zstd pair stay on the whole-input codec.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Value as JsonValue};

use super::{
    brotli_decoder_error, payload_bool, payload_i32, payload_str, zlib_dictionary,
    zlib_limit_error, zlib_max_output, zlib_output_limit, zlib_window_bits, NodeError, NodeResult,
};

/// Default per-call output budget. Node's zlib chunk size is 16 KiB; the JS
/// stream asks for at most the readable high-water mark, so this is only a
/// safety bound for direct host use.
const DEFAULT_MAX_OUT: usize = 16 * 1024;

/// Input retained per session before the consumed prefix is dropped.
const COMPACT_INPUT_BYTES: usize = 64 * 1024;

struct PushResult {
    out: Vec<u8>,
    consumed: u64,
    ended: bool,
}

#[allow(clippy::large_enum_variant)]
enum Session {
    Inflate(InflateSession),
    Brotli(BrotliSession),
}

struct InflateSession {
    decoder: flate2::Decompress,
    dictionary: Option<Vec<u8>>,
    /// Buffered input not yet consumed, starting at `in_pos`.
    input: Vec<u8>,
    in_pos: usize,
    limit: u64,
    max_output: Option<f64>,
    out_total: u64,
    consumed_total: u64,
    finish_flush: Option<i32>,
    reject_garbage: bool,
    ended: bool,
    ended_consumed: u64,
    ended_trailing: bool,
}

impl InflateSession {
    fn new(payload: &JsonValue, header: bool) -> NodeResult<Self> {
        let window_bits = zlib_window_bits(payload_i32(payload, "windowBits"));
        let dictionary = zlib_dictionary(payload)?;
        let max_output = zlib_max_output(payload);
        let mut decoder = flate2::Decompress::new_with_window_bits(header, window_bits);
        if !header {
            // Raw DEFLATE has no header to prime, so the dictionary installs
            // up front (same rule as the one-shot decoder).
            if let Some(dictionary) = dictionary.as_deref() {
                decoder
                    .set_dictionary(dictionary)
                    .map_err(|error| super::zlib_decompress_error(&error))?;
            }
        }
        Ok(Self {
            decoder,
            dictionary,
            input: Vec::new(),
            in_pos: 0,
            limit: zlib_output_limit(max_output),
            max_output,
            out_total: 0,
            consumed_total: 0,
            finish_flush: payload_i32(payload, "finishFlush"),
            reject_garbage: payload_bool(payload, "rejectGarbageAfterEnd"),
            ended: false,
            ended_consumed: 0,
            ended_trailing: false,
        })
    }

    fn compact(&mut self) {
        if self.in_pos >= COMPACT_INPUT_BYTES && self.in_pos * 2 >= self.input.len() {
            self.input.drain(..self.in_pos);
            self.in_pos = 0;
        }
    }

    /// Decode with `flush`, appending at most `max_out` bytes to `out`.
    fn decode(
        &mut self,
        max_out: usize,
        flush: flate2::FlushDecompress,
        out: &mut Vec<u8>,
    ) -> NodeResult<()> {
        loop {
            if self.ended || out.len() >= max_out {
                break;
            }
            let room = (max_out - out.len()).min(64 * 1024);
            let mut scratch = vec![0u8; room];
            let before_in = self.decoder.total_in();
            let before_out = self.decoder.total_out();
            let status =
                match self
                    .decoder
                    .decompress(&self.input[self.in_pos..], &mut scratch, flush)
                {
                    Ok(status) => status,
                    Err(error) => {
                        let used = (self.decoder.total_in() - before_in) as usize;
                        self.in_pos += used;
                        self.consumed_total = self.decoder.total_in();
                        if error.needs_dictionary().is_some() {
                            if let Some(dictionary) = self.dictionary.as_deref() {
                                // RFC 1950: the header's DICTID must be read before
                                // the dictionary is installed (zlib's contract). A
                                // dictionary whose Adler-32 does not match the
                                // header's DICTID is Node's "Bad dictionary".
                                self.decoder
                                    .set_dictionary(dictionary)
                                    .map_err(|_| NodeError::new("Z_NEED_DICT", "Bad dictionary"))?;
                                continue;
                            }
                        }
                        return Err(super::zlib_decompress_error(&error));
                    }
                };
            let used = (self.decoder.total_in() - before_in) as usize;
            let produced = (self.decoder.total_out() - before_out) as usize;
            self.in_pos += used;
            self.consumed_total = self.decoder.total_in();
            self.out_total += produced as u64;
            if produced > 0 {
                out.extend_from_slice(&scratch[..produced]);
            }
            if self.out_total > self.limit {
                return Err(zlib_limit_error(self.max_output));
            }
            if status == flate2::Status::StreamEnd {
                self.ended = true;
                self.ended_consumed = self.decoder.total_in();
                self.ended_trailing = self.ended_consumed < self.input.len() as u64;
                if self.reject_garbage && self.ended_trailing {
                    return Err(NodeError::new(
                        "ERR_TRAILING_JUNK_AFTER_STREAM_END",
                        "Trailing garbage after stream end",
                    ));
                }
                // Bytes after the stream's end are trailing garbage: Node's
                // decoder ignores them and they are not consumed.
                self.input.clear();
                self.in_pos = 0;
                break;
            }
            if used == 0 && produced == 0 {
                // No progress: needs more input (or the output slice filled).
                if out.len() >= max_out {
                    break;
                }
                if self.in_pos >= self.input.len() {
                    break;
                }
                // Spare output room and spare input: the stream is invalid.
                return Err(NodeError::new("Z_DATA_ERROR", "invalid input data"));
            }
            if self.in_pos >= self.input.len() && flush != flate2::FlushDecompress::Finish {
                break;
            }
        }
        self.compact();
        Ok(())
    }

    fn push(&mut self, data: &[u8], max_out: usize) -> NodeResult<PushResult> {
        if self.ended {
            return Ok(PushResult {
                out: Vec::new(),
                consumed: 0,
                ended: true,
            });
        }
        if !data.is_empty() {
            self.input.extend_from_slice(data);
        }
        let before = self.consumed_total;
        let mut out = Vec::new();
        self.decode(max_out, flate2::FlushDecompress::None, &mut out)?;
        Ok(PushResult {
            out,
            consumed: self.consumed_total - before,
            ended: self.ended,
        })
    }

    fn finish(&mut self, max_out: usize) -> NodeResult<PushResult> {
        if self.ended {
            return Ok(PushResult {
                out: Vec::new(),
                consumed: 0,
                ended: true,
            });
        }
        let before = self.consumed_total;
        let mut out = Vec::new();
        self.decode(max_out, flate2::FlushDecompress::Finish, &mut out)?;
        if !self.ended && out.len() < max_out {
            // Every byte consumed and the frame has not ended: the stream is
            // truncated. Node's default finishFlush (Z_FINISH=4) reports
            // Z_BUF_ERROR; a non-finalizing flush returns the partial output.
            if self.finish_flush.unwrap_or(4) != 4 {
                self.ended = true;
            } else {
                return Err(NodeError::new("Z_BUF_ERROR", "unexpected end of file"));
            }
        }
        Ok(PushResult {
            out,
            consumed: self.consumed_total - before,
            ended: self.ended,
        })
    }
}

type BrotliAlloc = brotli::enc::StandardAlloc;
type BrotliState = brotli::BrotliState<BrotliAlloc, BrotliAlloc, BrotliAlloc>;

struct BrotliSession {
    state: BrotliState,
    /// Input handed to the decoder, starting at `in_pos`.
    input: Vec<u8>,
    in_pos: usize,
    available_in: usize,
    limit: u64,
    max_output: Option<f64>,
    out_total: u64,
    consumed_total: u64,
    /// The decoder's own running output counter (brotli's API parameter).
    stream_total_out: usize,
    reject_garbage: bool,
    ended: bool,
}

impl BrotliSession {
    fn new(payload: &JsonValue) -> NodeResult<Self> {
        use brotli::SliceWrapperMut as _;

        let dictionary = zlib_dictionary(payload)?;
        let max_output = zlib_max_output(payload);
        let limit = zlib_output_limit(max_output);
        let mut alloc_u8 = brotli::enc::StandardAlloc::default();
        let state = match dictionary {
            Some(dictionary) if !dictionary.is_empty() => {
                let mut cell = <brotli::enc::StandardAlloc as brotli::Allocator<u8>>::alloc_cell(
                    &mut alloc_u8,
                    dictionary.len(),
                );
                cell.slice_mut().copy_from_slice(&dictionary);
                brotli::BrotliState::new_with_custom_dictionary(
                    alloc_u8,
                    brotli::enc::StandardAlloc::default(),
                    brotli::enc::StandardAlloc::default(),
                    cell,
                )
            }
            _ => brotli::BrotliState::new(
                alloc_u8,
                brotli::enc::StandardAlloc::default(),
                brotli::enc::StandardAlloc::default(),
            ),
        };
        Ok(Self {
            state,
            input: Vec::new(),
            in_pos: 0,
            available_in: 0,
            limit,
            max_output,
            out_total: 0,
            consumed_total: 0,
            stream_total_out: 0,
            reject_garbage: payload_bool(payload, "rejectGarbageAfterEnd"),
            ended: false,
        })
    }

    fn compact(&mut self) {
        if self.in_pos >= COMPACT_INPUT_BYTES && self.in_pos * 2 >= self.input.len() {
            self.input.drain(..self.in_pos);
            self.in_pos = 0;
        }
    }

    fn decode(&mut self, max_out: usize, out: &mut Vec<u8>) -> NodeResult<()> {
        loop {
            if self.ended || out.len() >= max_out {
                break;
            }
            let room = (max_out - out.len()).min(64 * 1024);
            let mut scratch = vec![0u8; room];
            let mut available_out = room;
            let mut output_offset = 0usize;
            let before_in = self.available_in;
            let result = brotli::BrotliDecompressStream(
                &mut self.available_in,
                &mut self.in_pos,
                &self.input,
                &mut available_out,
                &mut output_offset,
                &mut scratch,
                &mut self.stream_total_out,
                &mut self.state,
            );
            let used = before_in.saturating_sub(self.available_in);
            self.consumed_total += used as u64;
            out.extend_from_slice(&scratch[..output_offset]);
            self.out_total += output_offset as u64;
            if self.out_total > self.limit {
                return Err(zlib_limit_error(self.max_output));
            }
            match result {
                brotli::BrotliResult::ResultSuccess => {
                    if self.reject_garbage && self.available_in != 0 {
                        return Err(NodeError::new(
                            "ERR_TRAILING_JUNK_AFTER_STREAM_END",
                            "Trailing garbage after stream end",
                        ));
                    }
                    self.ended = true;
                    // Trailing bytes after the stream's end are ignored and
                    // not counted as consumed.
                    self.input.clear();
                    self.in_pos = 0;
                    self.available_in = 0;
                    break;
                }
                brotli::BrotliResult::NeedsMoreOutput => {
                    if output_offset == 0 {
                        return Err(NodeError::new("Z_BUF_ERROR", "unexpected end of file"));
                    }
                }
                brotli::BrotliResult::NeedsMoreInput => {
                    if self.available_in != 0
                        && (output_offset != 0 || self.available_in != before_in)
                    {
                        continue;
                    }
                    break;
                }
                brotli::BrotliResult::ResultFailure => {
                    return Err(brotli_decoder_error(self.state.error_code as i32));
                }
            }
        }
        self.compact();
        Ok(())
    }

    fn push(&mut self, data: &[u8], max_out: usize) -> NodeResult<PushResult> {
        if self.ended {
            return Ok(PushResult {
                out: Vec::new(),
                consumed: 0,
                ended: true,
            });
        }
        if !data.is_empty() {
            self.input.extend_from_slice(data);
            self.available_in += data.len();
        }
        let before = self.consumed_total;
        let mut out = Vec::new();
        self.decode(max_out, &mut out)?;
        Ok(PushResult {
            out,
            consumed: self.consumed_total - before,
            ended: self.ended,
        })
    }

    fn finish(&mut self, max_out: usize) -> NodeResult<PushResult> {
        if self.ended {
            return Ok(PushResult {
                out: Vec::new(),
                consumed: 0,
                ended: true,
            });
        }
        let before = self.consumed_total;
        let mut out = Vec::new();
        self.decode(max_out, &mut out)?;
        if !self.ended && out.len() < max_out {
            // Brotli has no partial-flush mode: a stream that stops before the
            // frame's end is truncated (Node reports Z_BUF_ERROR).
            return Err(NodeError::new("Z_BUF_ERROR", "unexpected end of file"));
        }
        Ok(PushResult {
            out,
            consumed: self.consumed_total - before,
            ended: self.ended,
        })
    }
}

static SESSIONS: OnceLock<Mutex<std::collections::HashMap<u64, Session>>> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn sessions() -> &'static Mutex<std::collections::HashMap<u64, Session>> {
    SESSIONS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn lock_sessions() -> std::sync::MutexGuard<'static, std::collections::HashMap<u64, Session>> {
    sessions().lock().unwrap_or_else(|error| error.into_inner())
}

fn payload_id(payload: &JsonValue) -> NodeResult<u64> {
    payload
        .get("id")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| NodeError::new("EINVAL", "zlib stream call requires a numeric `id`"))
}

fn payload_max_out(payload: &JsonValue) -> usize {
    payload
        .get("maxOut")
        .and_then(JsonValue::as_u64)
        .map(|value| value.clamp(1, 1024 * 1024) as usize)
        .unwrap_or(DEFAULT_MAX_OUT)
}

fn payload_data(payload: &JsonValue) -> NodeResult<Vec<u8>> {
    let text = payload
        .get("data")
        .and_then(JsonValue::as_str)
        .unwrap_or("");
    STANDARD
        .decode(text.as_bytes())
        .map_err(|error| NodeError::new("EINVAL", format!("invalid base64 payload: {error}")))
}

fn reply(result: PushResult) -> JsonValue {
    let encoded = STANDARD.encode(&result.out);
    JsonValue::String(
        json!({
            "data": encoded,
            "consumed": result.consumed,
            "ended": result.ended,
        })
        .to_string(),
    )
}

pub(super) fn open(payload: &JsonValue) -> NodeResult<JsonValue> {
    let op = payload_str(payload, "op")?;
    let session = match op {
        "inflate" => Session::Inflate(InflateSession::new(payload, true)?),
        "inflateRaw" => Session::Inflate(InflateSession::new(payload, false)?),
        "brotliDecompress" => Session::Brotli(BrotliSession::new(payload)?),
        other => {
            return Err(NodeError::new(
                "ENOSYS",
                format!("zlib stream op `{other}` is not streamable"),
            ))
        }
    };
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    lock_sessions().insert(id, session);
    Ok(JsonValue::String(json!({ "id": id }).to_string()))
}

pub(super) fn push(payload: &JsonValue) -> NodeResult<JsonValue> {
    let id = payload_id(payload)?;
    let data = payload_data(payload)?;
    let max_out = payload_max_out(payload);
    let mut sessions = lock_sessions();
    let session = sessions
        .get_mut(&id)
        .ok_or_else(|| NodeError::new("EINVAL", "zlib stream session is closed"))?;
    let result = match session {
        Session::Inflate(session) => session.push(&data, max_out)?,
        Session::Brotli(session) => session.push(&data, max_out)?,
    };
    Ok(reply(result))
}

/// Continue decoding without new input (output was capped by backpressure).
pub(super) fn drain(payload: &JsonValue) -> NodeResult<JsonValue> {
    push(&json!({
        "id": payload.get("id").cloned().unwrap_or(JsonValue::Null),
        "data": "",
        "maxOut": payload.get("maxOut").cloned().unwrap_or(JsonValue::Null),
    }))
}

pub(super) fn finish(payload: &JsonValue) -> NodeResult<JsonValue> {
    let id = payload_id(payload)?;
    let max_out = payload_max_out(payload);
    let mut sessions = lock_sessions();
    let session = sessions
        .get_mut(&id)
        .ok_or_else(|| NodeError::new("EINVAL", "zlib stream session is closed"))?;
    let result = match session {
        Session::Inflate(session) => session.finish(max_out)?,
        Session::Brotli(session) => session.finish(max_out)?,
    };
    Ok(reply(result))
}

pub(super) fn close(payload: &JsonValue) -> NodeResult<JsonValue> {
    if let Some(id) = payload.get("id").and_then(JsonValue::as_u64) {
        lock_sessions().remove(&id);
    }
    Ok(JsonValue::Null)
}
