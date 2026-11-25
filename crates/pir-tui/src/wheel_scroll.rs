//! Mouse-wheel scroll acceleration — port of upstream `wheel-scroll.ts`
//! (`packages/tui/src/wheel-scroll.ts`).
//!
//! Pure logic only (no terminal I/O): converts wheel events into logical line
//! counts, honoring the `fullscreenWheelScrollLines` setting (`"auto"` or a
//! fixed count, clamped to 1..=100 by the settings accessor).

/// Lines moved per mouse-wheel event, or [`WheelScrollLines::Auto`] to
/// accelerate fast wheel spins (upstream `WheelScrollLines`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelScrollLines {
    /// Accelerate: an isolated notch moves one line, a fast spin up to six
    /// (upstream `"auto"`).
    Auto,
    /// Fixed line count per event (upstream a number; settings clamp 1..=100).
    Lines(u32),
}

// Several events closer than this belong to one physical notch (Ghostty emits
// them ~4 ms apart) or come from a high-resolution source. They move one line
// each and do not accelerate (upstream `BURST_GAP_MS`).

/// Events closer than this belong to one physical notch and never accelerate
/// (upstream `BURST_GAP_MS`).
pub const BURST_GAP_MS: u64 = 5;
/// A pause longer than this ends a scroll gesture (upstream `GESTURE_GAP_MS`).
pub const GESTURE_GAP_MS: u64 = 200;
/// Average event gap that maps to one line per event. Faster events scale up
/// proportionally (upstream `REFERENCE_GAP_MS`).
pub const REFERENCE_GAP_MS: f64 = 100.0;
/// Ceiling on the accelerated lines per event.
pub const MAX_AUTO_LINES: u32 = 6;

/// Local macOS terminals receive wheel and trackpad deltas that the OS has
/// already accelerated, and they emit one event per line. Other platforms, and
/// SSH sessions where the client platform is unknown, usually send one event
/// per wheel notch (upstream `terminalAcceleratesWheel`).
pub fn terminal_accelerates_wheel() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("SSH_CONNECTION").is_none()
            && std::env::var_os("SSH_CLIENT").is_none()
            && std::env::var_os("SSH_TTY").is_none()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Converts wheel events into line counts (upstream `WheelScrollAccelerator`).
///
/// In [`WheelScrollLines::Auto`] mode on terminals that do not accelerate
/// wheel input, the count follows event velocity: an isolated notch moves one
/// line, while a fast spin moves up to [`MAX_AUTO_LINES`] lines per event.
#[derive(Debug)]
pub struct WheelScrollAccelerator {
    lines: WheelScrollLines,
    accelerate: bool,
    last_time: Option<u64>,
    last_direction: i32,
    average_gap: Option<f64>,
    carry: f64,
}

impl WheelScrollAccelerator {
    /// Defaults mirror upstream: accelerate everywhere except a local macOS
    /// terminal.
    pub fn new(lines: WheelScrollLines) -> Self {
        let accelerate = !terminal_accelerates_wheel();
        Self::with_accelerate(lines, accelerate)
    }

    pub fn with_accelerate(lines: WheelScrollLines, accelerate: bool) -> Self {
        Self {
            lines,
            accelerate,
            last_time: None,
            last_direction: 0,
            average_gap: None,
            carry: 0.0,
        }
    }

    pub fn set_lines(&mut self, lines: WheelScrollLines) {
        self.lines = lines;
        self.reset();
    }

    /// The configured mode (test observable).
    pub fn lines(&self) -> WheelScrollLines {
        self.lines
    }

    /// Return the positive line count for a wheel event in `direction` at time
    /// `now` (milliseconds, upstream `WheelScrollAccelerator.next`).
    pub fn next(&mut self, direction: i32, now: u64) -> u32 {
        match self.lines {
            WheelScrollLines::Lines(lines) => lines.max(1),
            WheelScrollLines::Auto if !self.accelerate => 1,
            WheelScrollLines::Auto => self.next_accelerated(direction, now),
        }
    }

    fn next_accelerated(&mut self, direction: i32, now: u64) -> u32 {
        let gap = self
            .last_time
            .map_or(u64::MAX, |last| now.saturating_sub(last));
        let same_gesture = direction == self.last_direction && gap <= GESTURE_GAP_MS;
        self.last_time = Some(now);
        self.last_direction = direction;
        if !same_gesture {
            self.average_gap = None;
            self.carry = 0.0;
            return 1;
        }
        // `gap == u64::MAX` (first event) never reaches here: it cannot share a
        // gesture with `last_direction == 0`.
        let gap = gap as f64;
        if gap < BURST_GAP_MS as f64 {
            return 1;
        }
        self.average_gap = Some(match self.average_gap {
            None => gap,
            Some(average) => (average + gap) / 2.0,
        });
        let average_gap = self.average_gap.unwrap_or(REFERENCE_GAP_MS);
        let lines = REFERENCE_GAP_MS / average_gap.max(f64::MIN_POSITIVE);
        let lines = (MAX_AUTO_LINES as f64).min(lines.max(1.0)) + self.carry;
        let whole = lines.floor();
        self.carry = lines - whole;
        whole.max(1.0) as u32
    }

    fn reset(&mut self) {
        self.last_time = None;
        self.last_direction = 0;
        self.average_gap = None;
        self.carry = 0.0;
    }
}
