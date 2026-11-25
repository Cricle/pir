//! Color math: hex → 256-color mapping and the OKLab / OKHSL <-> sRGB
//! conversions (upstream `oklab.ts`, Björn Ottosson's reference
//! implementation port, MIT). Hand-rolled: no new dependencies.

// ---------------------------------------------------------------------------
// Hex → 256-color mapping (upstream `rgbTo256`: cube + grayscale ramp,
// weighted distance, saturation preference for the cube).
// ---------------------------------------------------------------------------

const CUBE_VALUES: [u16; 6] = [0, 95, 135, 175, 215, 255];
const GRAY_VALUES: [u16; 24] = {
    let mut grays = [0u16; 24];
    let mut i = 0;
    while i < 24 {
        grays[i] = 8 + i as u16 * 10;
        i += 1;
    }
    grays
};

pub(super) fn hex_to_rgb(hex: &str) -> Option<(u16, u16, u16)> {
    let cleaned = hex.trim_start_matches('#');
    if cleaned.len() != 6 {
        return None;
    }
    let r = u16::from_str_radix(&cleaned[0..2], 16).ok()?;
    let g = u16::from_str_radix(&cleaned[2..4], 16).ok()?;
    let b = u16::from_str_radix(&cleaned[4..6], 16).ok()?;
    Some((r, g, b))
}

fn color_distance(r1: u16, g1: u16, b1: u16, r2: u16, g2: u16, b2: u16) -> f64 {
    let dr = (r1 as f64 - r2 as f64).powi(2) * 0.299;
    let dg = (g1 as f64 - g2 as f64).powi(2) * 0.587;
    let db = (b1 as f64 - b2 as f64).powi(2) * 0.114;
    dr + dg + db
}

fn find_closest_index(value: u16, values: &[u16]) -> usize {
    let mut best = 0usize;
    let mut best_dist = f64::INFINITY;
    for (i, v) in values.iter().enumerate() {
        let dist = (value as f64 - *v as f64).abs();
        if dist < best_dist {
            best_dist = dist;
            best = i;
        }
    }
    best
}

pub(super) fn rgb_to_256(r: u16, g: u16, b: u16) -> u16 {
    let r_idx = find_closest_index(r, &CUBE_VALUES);
    let g_idx = find_closest_index(g, &CUBE_VALUES);
    let b_idx = find_closest_index(b, &CUBE_VALUES);
    let cube_r = CUBE_VALUES[r_idx];
    let cube_g = CUBE_VALUES[g_idx];
    let cube_b = CUBE_VALUES[b_idx];
    let cube_index = 16 + 36 * r_idx as u16 + 6 * g_idx as u16 + b_idx as u16;
    let cube_dist = color_distance(r, g, b, cube_r, cube_g, cube_b);

    let gray = (0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64).round() as u16;
    let gray_idx = find_closest_index(gray, &GRAY_VALUES);
    let gray_value = GRAY_VALUES[gray_idx];
    let gray_index = 232 + gray_idx as u16;
    let gray_dist = color_distance(r, g, b, gray_value, gray_value, gray_value);

    let max_c = r.max(g).max(b);
    let min_c = r.min(g).min(b);
    let spread = max_c - min_c;
    if spread < 10 && gray_dist < cube_dist {
        gray_index
    } else {
        cube_index
    }
}

// ---------------------------------------------------------------------------
// OKLab / OKHSL <-> sRGB (upstream `oklab.ts`, Björn Ottosson's reference
// implementation port, MIT). Hand-rolled: no new dependencies.
// ---------------------------------------------------------------------------

const K1: f64 = 0.206;
const K2: f64 = 0.03;
const K3: f64 = (1.0 + K1) / (1.0 + K2);

const LINEAR_SRGB_TO_LMS: [[f64; 3]; 3] = [
    [0.4122214694707629, 0.5363325372617349, 0.0514459932675022],
    [0.2119034958178251, 0.6806995506452344, 0.1073969535369405],
    [0.0883024591900564, 0.2817188391361215, 0.6299787016738222],
];
const LMS_TO_LAB: [[f64; 3]; 3] = [
    [0.210454268309314, 0.793617774702305, -0.0040720430116193],
    [1.9779985324311684, -2.42859224204858, 0.450593709617411],
    [0.0259040424655478, 0.7827717124575296, -0.8086757549230774],
];
const LAB_TO_LMS: [[f64; 3]; 3] = [
    [1.0, 0.3963377773761749, 0.2158037573099136],
    [1.0, -0.1055613458156586, -0.0638541728258133],
    [1.0, -0.0894841775298119, -1.2914855480194092],
];
const LMS_TO_LINEAR_SRGB: [[f64; 3]; 3] = [
    [4.076741636075958, -3.307711539258063, 0.2309699031821043],
    [-1.2684379732850315, 2.609757349287688, -0.341319376002657],
    [-0.0041960761386756, -0.7034186179359362, 1.7076146940746117],
];

/// Per sRGB channel: the (a, b) half-plane where that channel clips first, and
/// the polynomial approximating the maximum saturation there.
const SATURATION_FIT: [((f64, f64), [f64; 5]); 3] = [
    (
        (-1.8817031, -0.80936501),
        [1.19086277, 1.76576728, 0.59662641, 0.75515197, 0.56771245],
    ),
    (
        (1.8144408, -1.19445267),
        [0.73956515, -0.45954404, 0.08285427, 0.12541073, -0.14503204],
    ),
    (
        (0.13110758, 1.81333971),
        [1.35733652, -0.00915799, -1.1513021, -0.50559606, 0.00692167],
    ),
];

/// The standard xterm palette entries 0-15 (upstream `BASIC_COLORS`).
const BASIC_16: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (128, 0, 0),
    (0, 128, 0),
    (128, 128, 0),
    (0, 0, 128),
    (128, 0, 128),
    (0, 128, 128),
    (192, 192, 192),
    (128, 128, 128),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (0, 0, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

/// Concrete RGB for a 256-color index (upstream `indexedColor`): the fixed
/// 16-color table, then the 6x6x6 cube, then the grayscale ramp.
pub(super) fn index_to_rgb(index: u8) -> (u8, u8, u8) {
    match index {
        0..=15 => BASIC_16[index as usize],
        16..=231 => {
            let value = index as usize - 16;
            (
                CUBE_VALUES[value / 36] as u8,
                CUBE_VALUES[(value / 6) % 6] as u8,
                CUBE_VALUES[value % 6] as u8,
            )
        }
        _ => {
            let gray = GRAY_VALUES[index as usize - 232] as u8;
            (gray, gray, gray)
        }
    }
}

pub(super) fn hex_of_rgb(r: u8, g: u8, b: u8) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn multiply_matrix_vector(m: [[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// sRGB transfer function: encoded (0-1) to linear channel.
fn srgb_to_linear(value: f64) -> f64 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB transfer function: linear (0-1) to encoded channel.
fn linear_to_srgb(value: f64) -> f64 {
    if value > 0.0031308 {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    } else {
        12.92 * value
    }
}

/// Linear sRGB (0-1) to Oklab [L, a, b].
fn linear_srgb_to_oklab(linear: [f64; 3]) -> [f64; 3] {
    let lms = multiply_matrix_vector(LINEAR_SRGB_TO_LMS, linear);
    let cbrt = [lms[0].cbrt(), lms[1].cbrt(), lms[2].cbrt()];
    multiply_matrix_vector(LMS_TO_LAB, cbrt)
}

/// Oklab [L, a, b] to linear sRGB (0-1, may leave the gamut).
fn oklab_to_linear_srgb(lab: [f64; 3]) -> [f64; 3] {
    let lms = multiply_matrix_vector(LAB_TO_LMS, lab);
    let cubes = [
        lms[0] * lms[0] * lms[0],
        lms[1] * lms[1] * lms[1],
        lms[2] * lms[2] * lms[2],
    ];
    multiply_matrix_vector(LMS_TO_LINEAR_SRGB, cubes)
}

/// sRGB channels (0-255) to Oklab [L, a, b].
pub(super) fn rgb_to_oklab(r: u8, g: u8, b: u8) -> [f64; 3] {
    linear_srgb_to_oklab([
        srgb_to_linear(r as f64 / 255.0),
        srgb_to_linear(g as f64 / 255.0),
        srgb_to_linear(b as f64 / 255.0),
    ])
}

/// Linear sRGB (0-1) to encoded sRGB bytes (0-255, rounded), clipping
/// out-of-gamut channels (upstream `linearSrgbToRgb`).
fn linear_srgb_to_rgb_bytes(linear: [f64; 3]) -> (u8, u8, u8) {
    let channel = |value: f64| -> u8 {
        let encoded = if value.is_finite() {
            linear_to_srgb(value)
        } else {
            0.0
        };
        (encoded.clamp(0.0, 1.0) * 255.0).round() as u8
    };
    (channel(linear[0]), channel(linear[1]), channel(linear[2]))
}

/// OKHSL lightness (0-1) to Oklab lightness.
fn okhsl_to_oklab_lightness(x: f64) -> f64 {
    (x * x + K1 * x) / (K3 * (x + K2))
}

/// Rate of change of each cube-root LMS component along a chroma direction.
fn lms_slopes(a: f64, b: f64) -> [f64; 3] {
    [
        LAB_TO_LMS[0][1] * a + LAB_TO_LMS[0][2] * b,
        LAB_TO_LMS[1][1] * a + LAB_TO_LMS[1][2] * b,
        LAB_TO_LMS[2][1] * a + LAB_TO_LMS[2][2] * b,
    ]
}

/// Largest saturation (C/L) inside sRGB for hue (a, b): polynomial fit plus
/// one Halley step.
fn max_saturation(a: f64, b: f64) -> f64 {
    let mut channel = 2usize;
    for (index, ((x, y), _)) in SATURATION_FIT.iter().enumerate() {
        if index == 2 || x * a + y * b > 1.0 {
            channel = index;
            break;
        }
    }
    let [k0, k1, k2, k3, k4] = SATURATION_FIT[channel].1;
    let weights = LMS_TO_LINEAR_SRGB[channel];
    let saturation = k0 + k1 * a + k2 * b + k3 * a * a + k4 * a * b;

    let slopes = lms_slopes(a, b);
    let base = [
        1.0 + saturation * slopes[0],
        1.0 + saturation * slopes[1],
        1.0 + saturation * slopes[2],
    ];
    let dot =
        |values: [f64; 3]| weights[0] * values[0] + weights[1] * values[1] + weights[2] * values[2];
    let f = dot([
        base[0] * base[0] * base[0],
        base[1] * base[1] * base[1],
        base[2] * base[2] * base[2],
    ]);
    let f1 = dot([
        3.0 * slopes[0] * base[0] * base[0],
        3.0 * slopes[1] * base[1] * base[1],
        3.0 * slopes[2] * base[2] * base[2],
    ]);
    let f2 = dot([
        6.0 * slopes[0] * slopes[0] * base[0],
        6.0 * slopes[1] * slopes[1] * base[1],
        6.0 * slopes[2] * slopes[2] * base[2],
    ]);
    saturation - (f * f1) / (f1 * f1 - 0.5 * f * f2)
}

/// Oklab lightness and chroma of the most saturated sRGB color of hue (a, b).
fn cusp(a: f64, b: f64) -> (f64, f64) {
    let saturation = max_saturation(a, b);
    let linear = oklab_to_linear_srgb([1.0, saturation * a, saturation * b]);
    let max_channel = linear[0].max(linear[1]).max(linear[2]);
    if max_channel <= 0.0 {
        return (1.0, 0.0);
    }
    let lightness = (1.0 / max_channel).cbrt();
    (lightness, lightness * saturation)
}

/// Chroma where the constant-lightness line at `lightness` leaves the sRGB
/// gamut.
fn max_chroma(a: f64, b: f64, lightness: f64, cusp_l: f64, cusp_c: f64) -> f64 {
    if lightness <= cusp_l {
        return (cusp_c * lightness) / cusp_l;
    }
    // Upper half: triangle edge, then one Halley step against each channel
    // reaching 1.
    let t = (cusp_c * (lightness - 1.0)) / (cusp_l - 1.0);
    let slopes = lms_slopes(a, b);
    let lms = [
        lightness + t * slopes[0],
        lightness + t * slopes[1],
        lightness + t * slopes[2],
    ];
    let cubes = [
        lms[0] * lms[0] * lms[0],
        lms[1] * lms[1] * lms[1],
        lms[2] * lms[2] * lms[2],
    ];
    let first = [
        3.0 * slopes[0] * lms[0] * lms[0],
        3.0 * slopes[1] * lms[1] * lms[1],
        3.0 * slopes[2] * lms[2] * lms[2],
    ];
    let second = [
        6.0 * slopes[0] * slopes[0] * lms[0],
        6.0 * slopes[1] * slopes[1] * lms[1],
        6.0 * slopes[2] * slopes[2] * lms[2],
    ];
    let mut best = f64::MAX;
    for row in LMS_TO_LINEAR_SRGB {
        let dot = |values: [f64; 3]| row[0] * values[0] + row[1] * values[1] + row[2] * values[2];
        let f = dot(cubes) - 1.0;
        let f1 = dot(first);
        let f2 = dot(second);
        let u = f1 / (f1 * f1 - 0.5 * f * f2);
        let step = if u >= 0.0 { -f * u } else { f64::MAX };
        best = best.min(step);
    }
    t + best
}

/// OKHSL's chroma reference points at lightness `l` and hue (a, b):
/// `(c0, c_mid, c_max)`.
fn chroma_stops(l: f64, a: f64, b: f64) -> (f64, f64, f64) {
    let (cusp_l, cusp_c) = cusp(a, b);
    let c_max = max_chroma(a, b, l, cusp_l, cusp_c);
    let k = c_max / (l * (cusp_c / cusp_l)).min((1.0 - l) * (cusp_c / (1.0 - cusp_l)));
    let mid_s = 0.11516993
        + 1.0
            / (7.4477897
                + 4.1590124 * b
                + a * (-2.19557347
                    + 1.75198401 * b
                    + a * (-2.13704948 - 10.02301043 * b
                        + a * (-4.24894561 + 5.38770819 * b + 4.69891013 * a))));
    let mid_t = 0.11239642
        + 1.0
            / (1.6132032 - 0.68124379 * b
                + a * (0.40370612
                    + 0.90148123 * b
                    + a * (-0.27087943
                        + 0.6122399 * b
                        + a * (0.00299215 - 0.45399568 * b - 0.14661872 * a))));
    let c_mid = 0.9
        * k
        * ((1.0 / (1.0 / (l * mid_s).powi(4) + 1.0 / ((1.0 - l) * mid_t).powi(4)))
            .sqrt()
            .sqrt());
    let c0 = (1.0 / (1.0 / (l * 0.4).powi(2) + 1.0 / ((1.0 - l) * 0.8).powi(2))).sqrt();
    (c0, c_mid, c_max)
}

/// OKLCH (lightness 0-1, chroma, hue in degrees) to sRGB channels (0-255,
/// rounded). Out-of-gamut colors are gamut-mapped, not clipped: a 20-step
/// chroma bisection reduces chroma until the color fits while preserving the
/// hue (upstream `oklchToRgb`, colors.ts:190-216). The achromatic color is
/// the fallback, so `oklch(100% 0.3 150)` maps to white.
pub fn oklch_to_rgb(l: f64, c: f64, h_degrees: f64) -> (u8, u8, u8) {
    // The gamut mapping keeps the hue fixed, so its direction is computed
    // once and scaled by chroma (colors.ts:191-195).
    let angle = h_degrees.to_radians();
    let (cos, sin) = (angle.cos(), angle.sin());
    let at_chroma = |chroma: f64| oklab_to_linear_srgb([l, chroma * cos, chroma * sin]);

    let direct = at_chroma(c);
    if in_srgb_gamut(direct) {
        return linear_srgb_to_rgb_bytes(direct);
    }

    // Reduce chroma until the color fits (colors.ts:200-215).
    let mut linear = at_chroma(0.0);
    let mut low = 0.0;
    let mut high = c;
    for _ in 0..20 {
        let chroma = (low + high) / 2.0;
        let candidate = at_chroma(chroma);
        if in_srgb_gamut(candidate) {
            low = chroma;
            linear = candidate;
        } else {
            high = chroma;
        }
    }
    linear_srgb_to_rgb_bytes(linear)
}

/// Gamut test with the upstream epsilon (colors.ts:185-188).
fn in_srgb_gamut(linear: [f64; 3]) -> bool {
    const EPSILON: f64 = 1e-7;
    linear
        .iter()
        .all(|&channel| (-EPSILON..=1.0 + EPSILON).contains(&channel))
}

/// OKHSL (hue in degrees, saturation and lightness 0-1, saturation relative to
/// the sRGB gamut) to sRGB channels (upstream `okhslToRgb`).
pub fn okhsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> (u8, u8, u8) {
    let l = okhsl_to_oklab_lightness(lightness);
    let mut lab = [l, 0.0, 0.0];
    if l > 0.0 && l < 1.0 && saturation > 0.0 {
        let angle = 2.0 * std::f64::consts::PI * hue.rem_euclid(360.0) / 360.0;
        let a = angle.cos();
        let b = angle.sin();
        let (c0, c_mid, c_max) = chroma_stops(l, a, b);
        // Chroma rises from 0 through c_mid at s = 0.8 to c_max at s = 1.
        let chroma = if saturation < 0.8 {
            let t = 1.25 * saturation;
            let k1 = 0.8 * c0;
            (t * k1) / (1.0 - (1.0 - k1 / c_mid) * t)
        } else {
            let t = 5.0 * (saturation - 0.8);
            let k1 = (0.2 * c_mid * c_mid * 1.25 * 1.25) / c0;
            c_mid + (t * k1) / (1.0 - (1.0 - k1 / (c_max - c_mid)) * t)
        };
        lab = [l, chroma * a, chroma * b];
    }
    linear_srgb_to_rgb_bytes(oklab_to_linear_srgb(lab))
}

/// Oklab lightness to OKHSL lightness (upstream `oklabToOkhslLightness`).
pub(super) fn oklab_to_okhsl_lightness(x: f64) -> f64 {
    0.5 * (K3 * x - K1 + ((K3 * x - K1).powi(2) + 4.0 * K2 * K3 * x).sqrt())
}

/// sRGB channels (0-255) to OKHSL (upstream `rgbToOkhsl`, oklab.ts:215-233):
/// hue in degrees (0 for grays), saturation and lightness 0-1 with saturation
/// relative to the sRGB gamut at the hue and lightness.
pub fn rgb_to_okhsl(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let [l, lab_a, lab_b] = rgb_to_oklab(r, g, b);
    let chroma = (lab_a * lab_a + lab_b * lab_b).sqrt();
    let lightness = oklab_to_okhsl_lightness(l);
    if chroma < 1e-9 || lightness <= 0.0 || lightness >= 1.0 {
        return (0.0, 0.0, lightness);
    }

    let hue = lab_b.atan2(lab_a).to_degrees().rem_euclid(360.0);
    let (c0, c_mid, c_max) = chroma_stops(l, lab_a / chroma, lab_b / chroma);
    let saturation = if chroma < c_mid {
        let k1 = 0.8 * c0;
        0.8 * (chroma / (k1 + (1.0 - k1 / c_mid) * chroma))
    } else {
        let k1 = (0.2 * c_mid * c_mid * 1.25 * 1.25) / c0;
        let offset = chroma - c_mid;
        0.8 + 0.2 * (offset / (k1 + (1.0 - k1 / (c_max - c_mid)) * offset))
    };
    (hue, saturation.clamp(0.0, 1.0), lightness)
}

/// Interpolation space for [`mix_colors`] (upstream `ColorMixSpace`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixSpace {
    /// Interpolate in OKLCH (upstream default).
    Oklch,
    /// Interpolate the raw sRGB channels.
    Srgb,
}

/// Mix two sRGB colors (upstream `mixColors`, colors.ts:241-257): `amount` 0
/// returns `first`, 1 returns `second`; out-of-range amounts are clamped
/// (upstream throws — a caller error pir degrades to clamping). The OKLCH
/// space interpolates lightness/chroma and takes the shortest hue path;
/// achromatic colors adopt the other endpoint's hue.
pub fn mix_colors(
    first: (u8, u8, u8),
    second: (u8, u8, u8),
    amount: f64,
    space: MixSpace,
) -> (u8, u8, u8) {
    let amount = amount.clamp(0.0, 1.0);
    match space {
        MixSpace::Srgb => {
            let lerp = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * amount).round() as u8;
            (
                lerp(first.0, second.0),
                lerp(first.1, second.1),
                lerp(first.2, second.2),
            )
        }
        MixSpace::Oklch => {
            let a = rgb_to_oklch_channels(first);
            let b = rgb_to_oklch_channels(second);
            let first_hue = if a.1 < 1e-7 { b.2 } else { a.2 };
            let second_hue = if b.1 < 1e-7 { first_hue } else { b.2 };
            let hue_delta = ((second_hue - first_hue + 540.0).rem_euclid(360.0)) - 180.0;
            let l = a.0 + (b.0 - a.0) * amount;
            let c = (a.1 + (b.1 - a.1) * amount).max(0.0);
            let h = (first_hue + hue_delta * amount).rem_euclid(360.0);
            oklch_to_rgb(l, c, h)
        }
    }
}

/// sRGB channels to OKLCH `(l, c, h)` (upstream `colorToOklch`).
pub(super) fn rgb_to_oklch_channels(rgb: (u8, u8, u8)) -> (f64, f64, f64) {
    let [l, a, b] = rgb_to_oklab(rgb.0, rgb.1, rgb.2);
    (
        l,
        (a * a + b * b).sqrt(),
        b.atan2(a).to_degrees().rem_euclid(360.0),
    )
}

/// WCAG 2 relative luminance of sRGB channels (upstream `relativeLuminance`).
pub fn relative_luminance(r: u8, g: u8, b: u8) -> f64 {
    let linear = |channel: u8| -> f64 {
        let value = channel as f64 / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}
