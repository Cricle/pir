//! Durable's output window (`ShellExecOptions.window`, `ShellOutputInfo.skipped`): undelivered command output is held
//! and coalesced; text followed by more than the window (by a byte or a line) can never be in the caller's tail and is
//! replaced by counts. Delivery follows the caller's commit pace, and waits while an earlier frame is still unsent.

use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Largest tail a window may retain. `Pending` holds up to four windows before
/// trimming, and one delivered event must still fit the frame cap with room
/// for its JSON (`MAX_FRAME - 64 KiB` is the payload budget). 4 MiB also
/// matches the bulk lane's backpressure limit, so one window cannot pin a
/// small device's memory for a single command.
const MAX_WINDOW_BYTES: usize = 4 * 1024 * 1024;
/// Largest line count a window may retain (a synthetic CPU bound; the byte
/// cap already bounds the text).
const MAX_WINDOW_LINES: usize = 1_000_000;
/// Largest delivery interval; longer pauses are clamped so a wire value can
/// never overflow `Duration` or park output for days.
const MAX_WINDOW_INTERVAL_MS: f64 = 60.0 * 60.0 * 1000.0;

#[derive(Clone, Copy)]
pub struct Window {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub min_interval: Duration,
    pub bytes_per_second: f64,
}

impl Window {
    pub fn from_json(json: &Value) -> Option<Window> {
        let object = json.as_object()?;
        let interval_ms = object.get("minIntervalMs")?.as_f64()?;
        // `Duration::from_secs_f64` panics on NaN, negative, or overflowing
        // input, and `f64::max` propagates neither as we need: clamp first.
        // JSON numbers are finite here, but a hand-built `Value` may not be.
        let interval_ms = if interval_ms.is_nan() {
            0.0
        } else {
            interval_ms.clamp(0.0, MAX_WINDOW_INTERVAL_MS)
        };
        let bytes_per_second = object.get("bytesPerSecond")?.as_f64()?;
        let bytes_per_second = if bytes_per_second.is_nan() {
            1.0
        } else {
            bytes_per_second.clamp(1.0, 1e12)
        };
        Some(Window {
            max_bytes: (object.get("maxBytes")?.as_u64()?).min(MAX_WINDOW_BYTES as u64) as usize,
            max_lines: (object.get("maxLines")?.as_u64()?).min(MAX_WINDOW_LINES as u64) as usize,
            min_interval: Duration::from_secs_f64(interval_ms / 1000.0),
            bytes_per_second,
        })
    }
}

#[derive(Default)]
struct Skipped {
    bytes: u64,
    newlines: u64,
    ends_with_newline: bool,
}

/// One output event: its stream, text, and the output omitted right before it.
pub struct Event {
    pub stream: &'static str,
    pub text: String,
    pub skipped: Option<Value>,
}

pub struct Pending {
    window: Window,
    /// Undelivered text in arrival order, by stream.
    segments: Vec<(&'static str, String)>,
    bytes: usize,
    newlines: usize,
    skipped: Option<Skipped>,
    next_send: Instant,
}

fn newline_count(text: &str) -> usize {
    text.bytes().filter(|byte| *byte == b'\n').count()
}

impl Pending {
    pub fn new(window: Window) -> Pending {
        Pending {
            window,
            segments: Vec::new(),
            bytes: 0,
            newlines: 0,
            skipped: None,
            next_send: Instant::now(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// When the next delivery may happen.
    pub fn next_send(&self) -> Instant {
        self.next_send
    }

    pub fn push(&mut self, stream: &'static str, text: String) {
        if text.is_empty() {
            return;
        }
        self.bytes = self.bytes.saturating_add(text.len());
        self.newlines = self.newlines.saturating_add(newline_count(&text));
        match self.segments.last_mut() {
            Some((last, existing)) if *last == stream => existing.push_str(&text),
            _ => self.segments.push((stream, text)),
        }
        // Trimming costs a pass over the held text; do it once a few windows have accumulated.
        let byte_limit = self.window.max_bytes.saturating_add(1).saturating_mul(4);
        let line_limit = self.window.max_lines.saturating_add(1).saturating_mul(4);
        if self.bytes > byte_limit || self.newlines > line_limit {
            self.trim();
        }
    }

    /// Whether the text after a cut still proves the cut text is outside the window.
    fn proves(&self, bytes: usize, newlines: usize) -> bool {
        bytes > self.window.max_bytes || newlines > self.window.max_lines
    }

    /// Replace the longest prefix whose remainder still proves exclusion by counts.
    fn trim(&mut self) {
        let mut removed = String::new();
        // Whole segments first.
        while self.segments.len() > 1 {
            let (bytes, newlines) = {
                let first = &self.segments[0].1;
                (first.len(), newline_count(first))
            };
            if !self.proves(self.bytes - bytes, self.newlines - newlines) {
                break;
            }
            let (_, text) = self.segments.remove(0);
            self.bytes -= bytes;
            self.newlines -= newlines;
            removed.push_str(&text);
        }
        // Then a prefix of the first remaining segment, at a character boundary.
        let rest_bytes = self.bytes - self.segments[0].1.len();
        let rest_newlines = self.newlines - newline_count(&self.segments[0].1);
        let first = &self.segments[0].1;
        let mut cut = 0;
        // By bytes: keep more than `max_bytes` overall.
        let keep_bytes = (self.window.max_bytes + 1).saturating_sub(rest_bytes);
        if first.len() > keep_bytes {
            let mut index = first.len() - keep_bytes;
            while index > 0 && !first.is_char_boundary(index) {
                index -= 1;
            }
            cut = cut.max(index);
        }
        // By lines: keep more than `max_lines` newlines overall, starting at a newline of this segment.
        let keep_newlines = (self.window.max_lines + 1).saturating_sub(rest_newlines);
        if keep_newlines > 0 {
            let positions: Vec<usize> = first
                .bytes()
                .enumerate()
                .filter(|(_, byte)| *byte == b'\n')
                .map(|(index, _)| index)
                .collect();
            if positions.len() >= keep_newlines {
                cut = cut.max(positions[positions.len() - keep_newlines]);
            }
        } else {
            cut = first.len();
        }
        if cut > 0 {
            let first = &mut self.segments[0].1;
            let prefix: String = first.drain(..cut).collect();
            self.bytes -= prefix.len();
            self.newlines -= newline_count(&prefix);
            removed.push_str(&prefix);
            if first.is_empty() {
                self.segments.remove(0);
            }
        }
        if !removed.is_empty() {
            let skipped = self.skipped.get_or_insert_with(Skipped::default);
            skipped.bytes += removed.len() as u64;
            skipped.newlines += newline_count(&removed) as u64;
            skipped.ends_with_newline = removed.ends_with('\n');
        }
    }

    /// Deliver everything held: one event carrying the skip and all text after it, or one event per segment.
    pub fn take(&mut self) -> Vec<Event> {
        // The push-time trim only runs every few windows; trim once more so a
        // single event can never exceed the window (and so the frame cap) when
        // a command ends mid-buffer.
        if !self.segments.is_empty() {
            self.trim();
        }
        let segments = std::mem::take(&mut self.segments);
        let delivered = self.bytes;
        self.bytes = 0;
        self.newlines = 0;
        // The caller writes at most the window per sample, and paces itself by what it wrote.
        let pace = Duration::from_secs_f64(
            delivered.min(self.window.max_bytes) as f64 / self.window.bytes_per_second,
        );
        self.next_send = Instant::now() + self.window.min_interval.max(pace);
        match self.skipped.take() {
            Some(skipped) => {
                let stream = segments
                    .last()
                    .map(|(stream, _)| *stream)
                    .unwrap_or("stdout");
                let text: String = segments.into_iter().map(|(_, text)| text).collect();
                vec![Event {
                    stream,
                    text,
                    skipped: Some(json!({
                        "bytes": skipped.bytes,
                        "newlines": skipped.newlines,
                        "endsWithNewline": skipped.ends_with_newline,
                    })),
                }]
            }
            None => segments
                .into_iter()
                .map(|(stream, text)| Event {
                    stream,
                    text,
                    skipped: None,
                })
                .collect(),
        }
    }
}
