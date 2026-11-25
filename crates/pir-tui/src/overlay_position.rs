//! Floating custom-overlay positioning — port of upstream
//! `packages/tui/src/tui.ts` `OverlayOptions` / `resolveOverlayLayout` /
//! `compositeOverlays` (tui.ts:254-290,1241-1448).
//!
//! Upstream shows `ctx.ui.custom(factory, { overlay: true })` components on a
//! floating overlay stack, positioned by `overlayOptions` (width / maxHeight /
//! anchor / offsets / row / col / margin). pir's custom overlays otherwise
//! occupy the full-width editor slot, which stays the DEFAULT (options absent
//! or `overlay` not true). When `overlay: true` arrives, the slot no longer
//! hosts the component: [`OverlayOptions::layout`] computes its terminal
//! rectangle, the renderer draws it at that width, and the frame decorator
//! composites the cached rows over the painted frame.
//!
//! Documented deviations:
//! - `visible(termWidth, termHeight)` is a JavaScript callback and cannot
//!   cross the host boundary, so the static `nonCapturing` and geometry fields
//!   are honored and the visibility callback is not (the overlay stays
//!   visible).
//! - `overlayOptions` may be a function upstream (re-evaluated per render);
//!   the prelude resolves it once when the request is opened.

use serde_json::Value;

use super::ansi;

/// Anchor point for floating overlays (upstream `OverlayAnchor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayAnchor {
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    LeftCenter,
    RightCenter,
}

impl OverlayAnchor {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "center" => Some(Self::Center),
            "top-left" => Some(Self::TopLeft),
            "top-right" => Some(Self::TopRight),
            "bottom-left" => Some(Self::BottomLeft),
            "bottom-right" => Some(Self::BottomRight),
            "top-center" => Some(Self::TopCenter),
            "bottom-center" => Some(Self::BottomCenter),
            "left-center" => Some(Self::LeftCenter),
            "right-center" => Some(Self::RightCenter),
            _ => None,
        }
    }
}

/// Margin from the terminal edges (upstream `OverlayMargin`); a numeric
/// `margin` expands to all four sides.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverlayMargin {
    pub top: usize,
    pub right: usize,
    pub bottom: usize,
    pub left: usize,
}

/// Absolute cells or a percentage of the reference dimension (upstream
/// `SizeValue`: `number | "<n>%"`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeValue {
    Absolute(usize),
    Percent(f64),
}

impl SizeValue {
    fn resolve(self, reference: usize) -> usize {
        match self {
            Self::Absolute(value) => value,
            Self::Percent(percent) => {
                let scaled = reference as f64 * percent / 100.0;
                if scaled.is_finite() && scaled > 0.0 {
                    scaled.floor() as usize
                } else {
                    0
                }
            }
        }
    }
}

/// Terminal-relative rectangle of one rendered overlay (upstream
/// `OverlayBounds`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverlayBounds {
    pub row: usize,
    pub col: usize,
    pub width: usize,
    pub height: usize,
}

impl OverlayBounds {
    /// The JSON shape the JS handle's `getBounds()` reports (upstream
    /// `OverlayBounds` field names).
    pub fn to_json(self) -> Value {
        serde_json::json!({
            "row": self.row,
            "col": self.col,
            "width": self.width,
            "height": self.height,
        })
    }
}

/// The parsed `OverlayOptions` of one `ctx.ui.custom` request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverlayOptions {
    pub width: Option<SizeValue>,
    pub min_width: Option<usize>,
    pub max_height: Option<SizeValue>,
    pub anchor: Option<OverlayAnchor>,
    pub offset_x: isize,
    pub offset_y: isize,
    pub row: Option<SizeValue>,
    pub col: Option<SizeValue>,
    pub margin: OverlayMargin,
    /// Don't capture keyboard focus when shown (upstream `nonCapturing`).
    pub non_capturing: bool,
}

impl OverlayOptions {
    /// Parse the request payload `{overlay, overlayOptions, ...}` the prelude
    /// sends for `ctx.ui.custom`. `None` means "not a floating overlay":
    /// the component keeps the full-width editor slot.
    pub fn from_request(request: &Value) -> Option<Self> {
        if request.get("overlay").and_then(Value::as_bool) != Some(true) {
            return None;
        }
        let mut options = Self::default();
        let Some(raw) = request.get("overlayOptions").filter(|raw| raw.is_object()) else {
            // `overlay: true` without options still floats (upstream falls
            // back to the default center placement / component width).
            return Some(options);
        };
        if let Some(width) = raw.get("width").and_then(parse_size_value) {
            options.width = Some(width);
        }
        if let Some(min_width) = raw.get("minWidth").and_then(Value::as_u64) {
            options.min_width = Some(min_width as usize);
        }
        if let Some(max_height) = raw.get("maxHeight").and_then(parse_size_value) {
            options.max_height = Some(max_height);
        }
        if let Some(anchor) = raw
            .get("anchor")
            .and_then(Value::as_str)
            .and_then(OverlayAnchor::parse)
        {
            options.anchor = Some(anchor);
        }
        if let Some(offset_x) = raw.get("offsetX").and_then(Value::as_i64) {
            options.offset_x = offset_x.clamp(isize::MIN as i64, isize::MAX as i64) as isize;
        }
        if let Some(offset_y) = raw.get("offsetY").and_then(Value::as_i64) {
            options.offset_y = offset_y.clamp(isize::MIN as i64, isize::MAX as i64) as isize;
        }
        if let Some(row) = raw.get("row").and_then(parse_size_value) {
            options.row = Some(row);
        }
        if let Some(col) = raw.get("col").and_then(parse_size_value) {
            options.col = Some(col);
        }
        options.margin = parse_margin(raw.get("margin"));
        options.non_capturing = raw
            .get("nonCapturing")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Some(options)
    }

    /// Width the component renders at (upstream
    /// `resolveOverlayLayout(..., 0, …).width`): the configured width or
    /// `min(80, available)`, raised to `minWidth`, clamped to the margins.
    pub fn render_width(&self, term_width: usize) -> usize {
        let available = self.available_width(term_width);
        let mut width = self
            .width
            .map(|value| value.resolve(term_width))
            .unwrap_or_else(|| available.min(80));
        if let Some(min_width) = self.min_width {
            width = width.max(min_width);
        }
        width.clamp(1, available.max(1))
    }

    /// Maximum rendered rows (upstream `maxHeight`, clamped to the margins).
    pub fn max_height(&self, term_height: usize) -> Option<usize> {
        let available = self.available_height(term_height);
        self.max_height
            .map(|value| value.resolve(term_height).clamp(1, available.max(1)))
    }

    /// The overlay rectangle for a `content_height`-row component
    /// (upstream `resolveOverlayLayout` with the rendered height).
    pub fn layout(
        &self,
        term_width: usize,
        term_height: usize,
        content_height: usize,
    ) -> OverlayBounds {
        let available_width = self.available_width(term_width);
        let available_height = self.available_height(term_height);
        let width = self.render_width(term_width);
        let max_height = self.max_height(term_height);
        let height = max_height
            .map(|max_height| content_height.min(max_height))
            .unwrap_or(content_height);
        let margin = self.margin;

        let mut row = match self.row {
            Some(SizeValue::Percent(percent)) => {
                let max_row = available_height.saturating_sub(height);
                margin.top + ((max_row as f64 * percent / 100.0).floor() as usize)
            }
            Some(SizeValue::Absolute(row)) => row,
            None => anchor_row(
                self.anchor.unwrap_or(OverlayAnchor::Center),
                height,
                available_height,
                margin.top,
            ),
        };
        let mut col = match self.col {
            Some(SizeValue::Percent(percent)) => {
                let max_col = available_width.saturating_sub(width);
                margin.left + ((max_col as f64 * percent / 100.0).floor() as usize)
            }
            Some(SizeValue::Absolute(col)) => col,
            None => anchor_col(
                self.anchor.unwrap_or(OverlayAnchor::Center),
                width,
                available_width,
                margin.left,
            ),
        };

        row = apply_offset(row, self.offset_y);
        col = apply_offset(col, self.offset_x);
        // Upstream clamps the anchored value to the terminal bounds first and
        // then to the margins (`Math.max(marginTop, Math.min(row, upper))`).
        let upper_row = term_height.saturating_sub(margin.bottom + height);
        row = row.min(upper_row).max(margin.top);
        let upper_col = term_width.saturating_sub(margin.right + width);
        col = col.min(upper_col).max(margin.left);

        OverlayBounds {
            row,
            col,
            width,
            height,
        }
    }

    fn available_width(&self, term_width: usize) -> usize {
        term_width
            .saturating_sub(self.margin.left + self.margin.right)
            .max(1)
    }

    fn available_height(&self, term_height: usize) -> usize {
        term_height
            .saturating_sub(self.margin.top + self.margin.bottom)
            .max(1)
    }
}

fn apply_offset(value: usize, offset: isize) -> usize {
    let shifted = value as isize + offset;
    shifted.max(0) as usize
}

fn anchor_row(anchor: OverlayAnchor, height: usize, available: usize, margin_top: usize) -> usize {
    match anchor {
        OverlayAnchor::TopLeft | OverlayAnchor::TopCenter | OverlayAnchor::TopRight => margin_top,
        OverlayAnchor::BottomLeft | OverlayAnchor::BottomCenter | OverlayAnchor::BottomRight => {
            margin_top + available.saturating_sub(height)
        }
        OverlayAnchor::LeftCenter | OverlayAnchor::Center | OverlayAnchor::RightCenter => {
            margin_top + (available.saturating_sub(height)) / 2
        }
    }
}

fn anchor_col(anchor: OverlayAnchor, width: usize, available: usize, margin_left: usize) -> usize {
    match anchor {
        OverlayAnchor::TopLeft | OverlayAnchor::LeftCenter | OverlayAnchor::BottomLeft => {
            margin_left
        }
        OverlayAnchor::TopRight | OverlayAnchor::RightCenter | OverlayAnchor::BottomRight => {
            margin_left + available.saturating_sub(width)
        }
        OverlayAnchor::TopCenter | OverlayAnchor::Center | OverlayAnchor::BottomCenter => {
            margin_left + (available.saturating_sub(width)) / 2
        }
    }
}

fn parse_size_value(value: &Value) -> Option<SizeValue> {
    if let Some(number) = value.as_u64() {
        return Some(SizeValue::Absolute(number as usize));
    }
    if let Some(number) = value.as_f64() {
        if number.is_finite() && number >= 0.0 {
            return Some(SizeValue::Absolute(number.floor() as usize));
        }
    }
    let text = value.as_str()?;
    let percent = text.strip_suffix('%')?.trim().parse::<f64>().ok()?;
    if percent.is_finite() {
        Some(SizeValue::Percent(percent))
    } else {
        None
    }
}

fn parse_margin(value: Option<&Value>) -> OverlayMargin {
    let Some(value) = value else {
        return OverlayMargin::default();
    };
    if let Some(all) = value.as_u64() {
        let all = all as usize;
        return OverlayMargin {
            top: all,
            right: all,
            bottom: all,
            left: all,
        };
    }
    if let Some(all) = value.as_f64() {
        let all = all.max(0.0).floor() as usize;
        return OverlayMargin {
            top: all,
            right: all,
            bottom: all,
            left: all,
        };
    }
    let side = |name: &str| -> usize {
        value
            .get(name)
            .and_then(|side| {
                side.as_u64()
                    .map(|side| side as usize)
                    .or_else(|| side.as_f64().map(|side| side.max(0.0).floor() as usize))
            })
            .unwrap_or(0)
    };
    OverlayMargin {
        top: side("top"),
        right: side("right"),
        bottom: side("bottom"),
        left: side("left"),
    }
}

/// Composite `overlay` rows over `lines` at `bounds` (upstream
/// `compositeOverlays`). Rows outside the frame are skipped; each overlay row
/// is cut/padded to the overlay width and spliced in at its column.
pub fn composite_lines(lines: &mut [String], overlay: &[String], bounds: OverlayBounds) {
    if bounds.width == 0 || bounds.height == 0 {
        return;
    }
    for (offset, overlay_line) in overlay.iter().take(bounds.height).enumerate() {
        let Some(base) = lines.get_mut(bounds.row + offset) else {
            break;
        };
        let mut replacement = ansi::truncate_to_width(overlay_line, bounds.width, "");
        replacement = ansi::pad_to_width(&replacement, bounds.width);
        let required = bounds.col.saturating_add(bounds.width);
        if ansi::visible_width(base) < required {
            *base = ansi::pad_to_width(base, required);
        }
        *base = ansi::replace_columns(base, bounds.col, bounds.width, &replacement);
    }
}
