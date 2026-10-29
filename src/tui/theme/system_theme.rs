//! The `system` theme: pi's colors derived from the terminal's own theme —
//! the Rust port of upstream
//! `packages/coding-agent/src/modes/interactive/theme/system-theme.ts`.
//!
//! Every token belongs to a color family (its hue) and has contrast rules:
//! it must reach a contrast level on the background and on the panels it is
//! drawn on. Hue and saturation come from the terminal's palette color for
//! the family's ANSI slot, or from the family's own hue when the terminal
//! reports no palette. Lightness comes from the rules alone.
//!
//! Depending on what the terminal reports, the theme is generated in one of
//! three tiers (`system-theme.ts:20-25`):
//! - background and palette: hues from the palette, lightness from the
//!   background;
//! - background only: the families' own hues, lightness from the background;
//! - nothing: ANSI palette indices and the default colors, which the
//!   terminal renders itself ([`indexed_colors`], upstream :642-660).
//!
//! A contrast level is a target-lightness curve: a polynomial in the
//! surface's OKLab lightness giving the lightness a token needs on it
//! (`LEVELS`, upstream :147-252). Mid-gray backgrounds cannot fit every
//! level, so the generator relaxes as little as possible (`solve(0)` failing
//! starts a bisection on the relaxation parameter, upstream :560-576).

use std::collections::BTreeMap;
use std::sync::OnceLock;

use super::color::{
    okhsl_to_rgb, oklab_to_okhsl_lightness, oklch_to_rgb, relative_luminance, rgb_to_okhsl,
    rgb_to_oklch_channels,
};
use super::palette::{Appearance, ThemeColor};
use super::terminal_colors::{Rgb, TerminalColors};
use super::theme_json::ColorValue;

/// The name reserved for the generated theme (upstream `SYSTEM_THEME_NAME`,
/// system-theme.ts:35). It takes precedence over custom themes of the same
/// name (`loadTheme`, theme.ts:557-559).
pub const SYSTEM_THEME_NAME: &str = "system";

/// WCAG 2 contrast ratio that body text must reach on the surfaces it is
/// drawn on (upstream `TEXT_MINIMUM_WCAG_CONTRAST`, system-theme.ts:350).
const TEXT_MINIMUM_WCAG_CONTRAST: f64 = 4.5;

/// A family's OKHSL hue and saturation: `max` at mid lightness, falling
/// toward `min` at black and white, plus the ANSI palette slot the family
/// takes its hue and saturation from (upstream `Family`, system-theme.ts:38-47).
#[derive(Debug, Clone, Copy)]
struct Family {
    hue: f64,
    sat_min: f64,
    sat_max: f64,
    slot: usize,
}

// The families (upstream `FAMILIES`, system-theme.ts:49-66).
const F_NEUTRAL: Family = Family {
    hue: 231.49,
    sat_min: 0.02,
    sat_max: 0.08,
    slot: 8,
};
const F_BLUE: Family = Family {
    hue: 231.49,
    sat_min: 0.1,
    sat_max: 0.68,
    slot: 4,
};
const F_GREEN: Family = Family {
    hue: 158.68,
    sat_min: 0.1,
    sat_max: 0.76,
    slot: 2,
};
const F_RED: Family = Family {
    hue: 20.0,
    sat_min: 0.1,
    sat_max: 0.92,
    slot: 1,
};
const F_YELLOW: Family = Family {
    hue: 82.36,
    sat_min: 0.5,
    sat_max: 1.0,
    slot: 3,
};
const F_ORANGE: Family = Family {
    hue: 52.0,
    sat_min: 0.12,
    sat_max: 0.85,
    slot: 3,
};
const F_VIOLET: Family = Family {
    hue: 295.0,
    sat_min: 0.2,
    sat_max: 0.6,
    slot: 5,
};
const F_CALAMINE: Family = Family {
    hue: 202.43,
    sat_min: 0.1,
    sat_max: 0.74,
    slot: 6,
};
const F_THINKING_SLATE: Family = Family {
    hue: 231.49,
    sat_min: 0.08,
    sat_max: 0.2,
    slot: 4,
};
const F_THINKING_BLUE: Family = Family {
    hue: 231.49,
    sat_min: 0.2,
    sat_max: 0.45,
    slot: 4,
};
const F_THINKING_PERIWINKLE: Family = Family {
    hue: 263.25,
    sat_min: 0.3,
    sat_max: 0.6,
    slot: 6,
};
const F_THINKING_VIOLET: Family = Family {
    hue: 295.0,
    sat_min: 0.4,
    sat_max: 0.75,
    slot: 5,
};
const F_THINKING_MAGENTA: Family = Family {
    hue: 337.5,
    sat_min: 0.5,
    sat_max: 0.85,
    slot: 13,
};
const F_THINKING_RED: Family = Family {
    hue: 20.0,
    sat_min: 0.95,
    sat_max: 1.0,
    slot: 1,
};

/// The family a token takes its hue and saturation from, by family name
/// (upstream `TOKEN_FAMILIES`, system-theme.ts:68-129). `None` for names the
/// generator does not know.
fn family_by_name(name: &str) -> Option<Family> {
    Some(match name {
        "neutral" => F_NEUTRAL,
        "blue" => F_BLUE,
        "green" => F_GREEN,
        "red" => F_RED,
        "yellow" => F_YELLOW,
        "orange" => F_ORANGE,
        "violet" => F_VIOLET,
        "calamine" => F_CALAMINE,
        "thinkingSlate" => F_THINKING_SLATE,
        "thinkingBlue" => F_THINKING_BLUE,
        "thinkingPeriwinkle" => F_THINKING_PERIWINKLE,
        "thinkingViolet" => F_THINKING_VIOLET,
        "thinkingMagenta" => F_THINKING_MAGENTA,
        "thinkingRed" => F_THINKING_RED,
        _ => return None,
    })
}

/// The family of every generated token, by family name (upstream
/// `TOKEN_FAMILIES`, system-theme.ts:68-129). `None` for names the generator
/// does not know.
fn token_family_name(token: &str) -> Option<&'static str> {
    let name = match token {
        "selectedBg" => "blue",
        "searchMatchBg" => "orange",
        "userMessageBg" => "blue",
        "customMessageBg" => "violet",
        "toolPendingBg" => "neutral",
        "toolSuccessBg" => "green",
        "toolErrorBg" => "red",

        "text" | "userMessageText" | "customMessageText" | "toolTitle" | "syntaxOperator"
        | "syntaxPunctuation" | "muted" | "dim" | "thinkingText" | "toolOutput" | "mdLinkUrl"
        | "mdQuote" | "mdQuoteBorder" | "mdHr" | "mdCodeBlockBorder" | "toolDiffContext"
        | "syntaxComment" | "scrollbarTrack" | "scrollbarThumb" | "searchMatchText"
        | "borderMuted" => "neutral",

        "accent" | "borderAccent" | "customMessageLabel" | "mdCode" | "mdListBullet"
        | "syntaxType" => "violet",
        "border" | "mdLink" | "syntaxKeyword" => "blue",
        "syntaxVariable" => "calamine",
        "success" | "mdCodeBlock" | "toolDiffAdded" | "bashMode" | "syntaxNumber" => "green",
        "error" | "toolDiffRemoved" => "red",
        "warning" | "mdHeading" | "syntaxFunction" => "yellow",
        "syntaxString" => "orange",

        "thinkingOff" => "neutral",
        "thinkingMinimal" => "thinkingSlate",
        "thinkingLow" => "thinkingBlue",
        "thinkingMedium" => "thinkingPeriwinkle",
        "thinkingHigh" => "thinkingViolet",
        "thinkingXhigh" => "thinkingMagenta",
        "thinkingMax" => "thinkingRed",
        _ => return None,
    };
    Some(name)
}

/// Palette slots for tokens that would otherwise share a hue with a similar
/// token (upstream `TOKEN_SLOTS`, system-theme.ts:131).
fn token_slot(token: &str) -> Option<usize> {
    match token {
        "syntaxString" => Some(2),
        "syntaxNumber" => Some(5),
        "searchMatchBg" => Some(3),
        _ => None,
    }
}

/// Text-level tokens that take the terminal's foreground (upstream
/// `FOREGROUND_TOKENS`, system-theme.ts:347).
const FOREGROUND_TOKENS: [&str; 3] = ["text", "userMessageText", "toolTitle"];

/// A target-lightness curve: a polynomial in the surface's OKLab lightness
/// giving the OKLab lightness a token needs on it; `reachable` is the range
/// of surface lightness where the level can be reached (upstream `Curve`,
/// system-theme.ts:139-145).
#[derive(Debug, Clone, Copy)]
struct Curve {
    coefficients: [f64; 6],
    reachable: [f64; 2],
}

const fn curve(coefficients: [f64; 6], reachable: [f64; 2]) -> Curve {
    Curve {
        coefficients,
        reachable,
    }
}

/// A contrast level (upstream `LEVELS`, system-theme.ts:147-252).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Panel,
    Track,
    Thinking0,
    Thinking1,
    Thinking2,
    Thinking3,
    Thinking4,
    Thinking5,
    Thinking6,
    Subtle,
    Thumb,
    Readable,
    Emphasis,
    TextOnPanel,
    Text,
}

impl Level {
    /// The level's curve for an appearance.
    fn curve(self, appearance: Appearance) -> Curve {
        let dark = appearance == Appearance::Dark;
        match self {
            Level::Panel => {
                if dark {
                    curve(
                        [0.29131, -0.39746, 2.33185, -0.85524, -1.2076, 0.86276],
                        [0.0, 0.979],
                    )
                } else {
                    curve(
                        [-3.74073, 27.94549, -78.44258, 112.6798, -79.60015, 22.11277],
                        [0.348, 1.0],
                    )
                }
            }
            Level::Track => {
                if dark {
                    curve(
                        [0.39028, -0.23015, 0.83573, 2.43829, -4.38292, 2.01582],
                        [0.0, 0.946],
                    )
                } else {
                    curve(
                        [
                            -5.24921, 38.37322, -107.28833, 152.10005, -106.17127, 29.18061,
                        ],
                        [0.368, 1.0],
                    )
                }
            }
            Level::Thinking0 => {
                if dark {
                    curve(
                        [0.52988, -0.05809, -0.30924, 4.63567, -6.52933, 2.89108],
                        [0.0, 0.873],
                    )
                } else {
                    curve(
                        [
                            -28.27749, 182.85284, -469.62416, 603.15916, -384.59976, 97.35147,
                        ],
                        [0.51, 1.0],
                    )
                }
            }
            Level::Thinking1 => {
                if dark {
                    curve(
                        [0.55278, -0.03667, -0.45659, 4.95347, -6.90265, 3.0706],
                        [0.0, 0.858],
                    )
                } else {
                    curve(
                        [
                            -37.10484, 235.86282, -596.62344, 754.3633, -474.00763, 118.3551,
                        ],
                        [0.535, 1.0],
                    )
                }
            }
            Level::Thinking2 => {
                if dark {
                    curve(
                        [0.57486, -0.01765, -0.58987, 5.25227, -7.27175, 3.25532],
                        [0.0, 0.842],
                    )
                } else {
                    curve(
                        [
                            -59.89653, 377.05024, -945.07843, 1182.03145, -734.96375, 181.68658,
                        ],
                        [0.556, 1.0],
                    )
                }
            }
            Level::Thinking3 => {
                if dark {
                    curve(
                        [0.59621, -0.00062, -0.71148, 5.53588, -7.6392, 3.44606],
                        [0.0, 0.827],
                    )
                } else {
                    curve(
                        [
                            -72.07122,
                            445.84082,
                            -1099.57352,
                            1353.88793,
                            -829.53392,
                            202.26164,
                        ],
                        [0.58, 1.0],
                    )
                }
            }
            Level::Thinking4 => {
                if dark {
                    curve(
                        [0.61691, 0.01462, -0.82288, 5.80651, -8.00641, 3.64333],
                        [0.0, 0.811],
                    )
                } else {
                    curve(
                        [
                            -110.14338,
                            674.21488,
                            -1645.75941,
                            2004.32367,
                            -1215.15899,
                            293.3183,
                        ],
                        [0.6, 1.0],
                    )
                }
            }
            Level::Thinking5 => {
                if dark {
                    curve(
                        [0.63702, 0.02826, -0.92498, 6.06465, -8.37246, 3.84651],
                        [0.0, 0.795],
                    )
                } else {
                    curve(
                        [
                            -175.47701,
                            1063.54495,
                            -2570.70594,
                            3098.80776,
                            -1860.15527,
                            444.76392,
                        ],
                        [0.62, 1.0],
                    )
                }
            }
            Level::Thinking6 => {
                if dark {
                    curve(
                        [0.65658, 0.04044, -1.01835, 6.30989, -8.73529, 4.05439],
                        [0.0, 0.779],
                    )
                } else {
                    curve(
                        [
                            -183.81712,
                            1094.70055,
                            -2602.68539,
                            3088.71276,
                            -1826.91131,
                            430.75931,
                        ],
                        [0.643, 1.0],
                    )
                }
            }
            Level::Subtle => {
                if dark {
                    curve(
                        [0.56762, -0.02475, -0.5383, 5.12628, -7.10931, 3.17324],
                        [0.0, 0.848],
                    )
                } else {
                    curve(
                        [
                            -232.85459,
                            1376.54473,
                            -3249.11801,
                            3827.91186,
                            -2248.29472,
                            526.55751,
                        ],
                        [0.657, 1.0],
                    )
                }
            }
            Level::Thumb => {
                if dark {
                    curve(
                        [0.60323, 0.00278, -0.73328, 5.57157, -7.68067, 3.46933],
                        [0.0, 0.823],
                    )
                } else {
                    curve(
                        [
                            -82.89897,
                            511.01355,
                            -1255.98095,
                            1540.76821,
                            -940.68087,
                            228.58523,
                        ],
                        [0.586, 1.0],
                    )
                }
            }
            Level::Readable => {
                if dark {
                    curve(
                        [0.66937, 0.04704, -1.06871, 6.43941, -8.9332, 4.17229],
                        [0.0, 0.77],
                    )
                } else {
                    curve(
                        [
                            -1554.52576,
                            8733.56817,
                            -19604.93507,
                            21977.72696,
                            -12300.99599,
                            2749.81288,
                        ],
                        [0.751, 1.0],
                    )
                }
            }
            Level::Emphasis => {
                if dark {
                    curve(
                        [0.7303, 0.07695, -1.31626, 7.1681, -10.14436, 4.92846],
                        [0.0, 0.712],
                    )
                } else {
                    curve(
                        [
                            -4948.31942,
                            26870.91986,
                            -58334.48399,
                            63280.17197,
                            -34298.01053,
                            7430.30146,
                        ],
                        [0.811, 1.0],
                    )
                }
            }
            Level::TextOnPanel => {
                if dark {
                    curve(
                        [0.86713, 0.05232, -0.89428, 4.79014, -5.5432, 1.75023],
                        [0.0, 0.542],
                    )
                } else {
                    curve(
                        [
                            -8570.89457,
                            43954.60805,
                            -90084.00702,
                            92220.6791,
                            -47152.15802,
                            9632.27113,
                        ],
                        [0.867, 1.0],
                    )
                }
            }
            Level::Text => {
                if dark {
                    curve(
                        [0.89242, 0.02311, -0.44862, 2.34417, -0.06084, -2.63844],
                        [0.0, 0.5],
                    )
                } else {
                    curve(
                        [
                            -2004.67048,
                            6664.47299,
                            -6060.70202,
                            -1792.61209,
                            5133.82359,
                            -1939.85583,
                        ],
                        [0.894, 1.0],
                    )
                }
            }
        }
    }
}

// ============================================================================
// Surfaces, rules and solve order
// ============================================================================

/// A surface a token is drawn on (upstream `Surface`, system-theme.ts:257):
/// the background, a theme background token, or the scrollbar track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    Background,
    ScrollbarTrack,
    Panel(&'static str),
}

impl Surface {
    /// The key a surface shares with the solved-color map.
    fn key(self) -> &'static str {
        match self {
            Surface::Background => "background",
            Surface::ScrollbarTrack => "scrollbarTrack",
            Surface::Panel(name) => name,
        }
    }
}

/// The theme background tokens, in upstream `PANELS` order
/// (system-theme.ts:261-269). These never take a `""`/index value: they are
/// generated colors, or the background itself when set to `""`.
const PANEL_NAMES: [&str; 7] = [
    "userMessageBg",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
    "selectedBg",
    "searchMatchBg",
    "customMessageBg",
];

const BACKGROUND: &[Surface] = &[Surface::Background];
const TOOL_PANELS: &[Surface] = &[
    Surface::Panel("toolPendingBg"),
    Surface::Panel("toolSuccessBg"),
    Surface::Panel("toolErrorBg"),
];
const ON_BG_SELECTED_TOOLS: &[Surface] = &[
    Surface::Background,
    Surface::Panel("selectedBg"),
    Surface::Panel("toolPendingBg"),
    Surface::Panel("toolSuccessBg"),
    Surface::Panel("toolErrorBg"),
];
const ON_BG_CUSTOM_SELECTED_TOOLS: &[Surface] = &[
    Surface::Background,
    Surface::Panel("customMessageBg"),
    Surface::Panel("selectedBg"),
    Surface::Panel("toolPendingBg"),
    Surface::Panel("toolSuccessBg"),
    Surface::Panel("toolErrorBg"),
];
const ON_CUSTOM_MESSAGE_TOOLS: &[Surface] = &[
    Surface::Panel("customMessageBg"),
    Surface::Panel("toolPendingBg"),
    Surface::Panel("toolSuccessBg"),
    Surface::Panel("toolErrorBg"),
];
const ON_BG_MESSAGES: &[Surface] = &[
    Surface::Background,
    Surface::Panel("userMessageBg"),
    Surface::Panel("customMessageBg"),
];
const ON_BG_MESSAGES_TOOLS: &[Surface] = &[
    Surface::Background,
    Surface::Panel("userMessageBg"),
    Surface::Panel("customMessageBg"),
    Surface::Panel("toolPendingBg"),
    Surface::Panel("toolSuccessBg"),
    Surface::Panel("toolErrorBg"),
];
const ON_BG_TOOLS: &[Surface] = &[
    Surface::Background,
    Surface::Panel("toolPendingBg"),
    Surface::Panel("toolSuccessBg"),
    Surface::Panel("toolErrorBg"),
];
const ON_SEARCH_MATCH_BG: &[Surface] = &[Surface::Panel("searchMatchBg")];
const ON_SCROLLBAR_TRACK: &[Surface] = &[Surface::ScrollbarTrack];

/// One token's contrast requirement on a list of surfaces (upstream `Rule`,
/// system-theme.ts:254-258).
#[derive(Debug, Clone, Copy)]
struct Rule {
    token: &'static str,
    on: &'static [Surface],
    level: Level,
}

fn push_each(rules: &mut Vec<Rule>, tokens: &[&'static str], on: &'static [Surface], level: Level) {
    for token in tokens {
        rules.push(Rule { token, on, level });
    }
}

/// The contrast rules (upstream `RULES`, system-theme.ts:292-339). Order
/// matters: the solve order is derived from it.
fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let mut rules = Vec::new();
        push_each(&mut rules, &PANEL_NAMES, BACKGROUND, Level::Panel);
        rules.push(Rule {
            token: "text",
            on: BACKGROUND,
            level: Level::Text,
        });
        rules.push(Rule {
            token: "text",
            on: &[Surface::Panel("selectedBg")],
            level: Level::TextOnPanel,
        });
        rules.push(Rule {
            token: "userMessageText",
            on: &[Surface::Panel("userMessageBg")],
            level: Level::TextOnPanel,
        });
        rules.push(Rule {
            token: "toolTitle",
            on: TOOL_PANELS,
            level: Level::TextOnPanel,
        });
        push_each(
            &mut rules,
            &["accent", "success", "error", "warning"],
            ON_BG_SELECTED_TOOLS,
            Level::Readable,
        );
        rules.push(Rule {
            token: "muted",
            on: ON_BG_CUSTOM_SELECTED_TOOLS,
            level: Level::Readable,
        });
        rules.push(Rule {
            token: "dim",
            on: ON_BG_CUSTOM_SELECTED_TOOLS,
            level: Level::Subtle,
        });
        rules.push(Rule {
            token: "thinkingText",
            on: BACKGROUND,
            level: Level::Readable,
        });
        rules.push(Rule {
            token: "customMessageText",
            on: ON_CUSTOM_MESSAGE_TOOLS,
            level: Level::Readable,
        });
        rules.push(Rule {
            token: "customMessageLabel",
            on: ON_BG_CUSTOM_SELECTED_TOOLS,
            level: Level::Readable,
        });
        rules.push(Rule {
            token: "toolOutput",
            on: ON_BG_TOOLS,
            level: Level::Readable,
        });
        push_each(
            &mut rules,
            &[
                "mdHeading",
                "mdLink",
                "mdLinkUrl",
                "mdCode",
                "mdQuote",
                "mdCodeBlockBorder",
                "mdListBullet",
            ],
            ON_BG_MESSAGES,
            Level::Readable,
        );
        rules.push(Rule {
            token: "mdCodeBlock",
            on: ON_BG_MESSAGES_TOOLS,
            level: Level::Readable,
        });
        push_each(
            &mut rules,
            &["toolDiffAdded", "toolDiffRemoved", "toolDiffContext"],
            ON_BG_TOOLS,
            Level::Readable,
        );
        push_each(
            &mut rules,
            &[
                "syntaxComment",
                "syntaxKeyword",
                "syntaxFunction",
                "syntaxVariable",
                "syntaxString",
                "syntaxNumber",
                "syntaxType",
                "syntaxOperator",
                "syntaxPunctuation",
            ],
            ON_BG_MESSAGES_TOOLS,
            Level::Readable,
        );
        rules.push(Rule {
            token: "searchMatchText",
            on: ON_SEARCH_MATCH_BG,
            level: Level::Readable,
        });
        push_each(
            &mut rules,
            &["bashMode", "border", "borderAccent"],
            BACKGROUND,
            Level::Readable,
        );
        rules.push(Rule {
            token: "borderMuted",
            on: BACKGROUND,
            level: Level::Subtle,
        });
        push_each(
            &mut rules,
            &["mdQuoteBorder", "mdHr"],
            ON_BG_MESSAGES_TOOLS,
            Level::Readable,
        );
        rules.push(Rule {
            token: "scrollbarTrack",
            on: BACKGROUND,
            level: Level::Track,
        });
        rules.push(Rule {
            token: "scrollbarThumb",
            on: ON_SCROLLBAR_TRACK,
            level: Level::Thumb,
        });
        let thinking = [
            "thinkingOff",
            "thinkingMinimal",
            "thinkingLow",
            "thinkingMedium",
            "thinkingHigh",
            "thinkingXhigh",
            "thinkingMax",
        ];
        let thinking_levels = [
            Level::Thinking0,
            Level::Thinking1,
            Level::Thinking2,
            Level::Thinking3,
            Level::Thinking4,
            Level::Thinking5,
            Level::Thinking6,
        ];
        for (token, level) in thinking.iter().zip(thinking_levels) {
            rules.push(Rule {
                token,
                on: BACKGROUND,
                level,
            });
        }
        rules
    })
}

/// Tokens in dependency order: every surface before the tokens drawn on it
/// (upstream `SOLVE_ORDER`, system-theme.ts:353-363).
fn solve_order() -> &'static [&'static str] {
    static ORDER: OnceLock<Vec<&'static str>> = OnceLock::new();
    ORDER.get_or_init(|| {
        fn visit(token: &'static str, order: &mut Vec<&'static str>) {
            if order.contains(&token) {
                return;
            }
            for rule in rules() {
                if rule.token != token {
                    continue;
                }
                for surface in rule.on {
                    let key = surface.key();
                    if key != "background" {
                        visit(key, order);
                    }
                }
            }
            order.push(token);
        }
        let mut order = Vec::new();
        for rule in rules() {
            visit(rule.token, &mut order);
        }
        order
    })
}

// ============================================================================
// Public API
// ============================================================================

/// What the terminal reported, plus the generation knobs (upstream
/// `SystemThemeInput`, system-theme.ts:371-381).
#[derive(Debug, Clone, Default)]
pub struct SystemThemeInput {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    /// ANSI colors 0-15.
    pub palette: Option<Vec<Rgb>>,
    /// Saturation multiplier from 0 (grayscale) to 1. The first frame renders
    /// in grayscale until colors arrive; `None` is 1.
    pub saturation: Option<f64>,
    /// Appearance when the terminal did not report its background, e.g. from
    /// its light/dark report (mode 2031) or `COLORFGBG`.
    pub appearance_hint: Option<Appearance>,
}

impl SystemThemeInput {
    /// The input for the terminal's reported colors (upstream
    /// `createSystemTheme`, theme.ts:537-548).
    pub fn from_terminal_colors(colors: &TerminalColors) -> Self {
        SystemThemeInput {
            foreground: colors.foreground,
            background: colors.background,
            palette: colors.palette.clone(),
            saturation: None,
            appearance_hint: None,
        }
    }
}

/// The generated theme's colors (upstream `SystemThemeColors`,
/// system-theme.ts:382-389): hex colors, ANSI palette indices, or `""` for
/// the terminal default; `dim` lists the foreground tokens rendered faint
/// (SGR 2) for terminals that reported nothing.
#[derive(Debug, Clone, Default)]
pub struct SystemThemeColors {
    pub colors: BTreeMap<&'static str, ColorValue>,
    pub dim: Vec<ThemeColor>,
    pub appearance: Option<Appearance>,
}

/// WCAG 2 contrast ratio, 1-21 (upstream `wcagContrast`,
/// system-theme.ts:405-410).
pub fn wcag_contrast(first: Rgb, second: Rgb) -> f64 {
    let a = relative_luminance(first.0, first.1, first.2);
    let b = relative_luminance(second.0, second.1, second.2);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// OKLab lightness of an sRGB color, 0-1 (upstream `oklabLightness`,
/// system-theme.ts:391-393).
fn oklab_lightness(color: Rgb) -> f64 {
    rgb_to_oklch_channels(color).0
}

fn hex_of(rgb: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb.0, rgb.1, rgb.2)
}

/// Saturation weight at a lightness: a Gaussian (center 0.5, sigma 0.25), 0
/// at black and white, 1 in the middle (upstream `bellWeight`,
/// system-theme.ts:444-448).
fn bell_weight(lightness: f64) -> f64 {
    let gaussian = |x: f64| -> f64 { (-(x - 0.5).powi(2) / (2.0 * 0.25f64.powi(2))).exp() };
    let at_zero = gaussian(0.0);
    (gaussian(lightness) - at_zero) / (1.0 - at_zero)
}

/// A family's saturation curve relative to its maximum: 1 at mid lightness,
/// `min / max` at black and white (upstream `saturationCurve`,
/// system-theme.ts:450-454).
fn saturation_curve(family: Family, lightness: f64) -> f64 {
    let floor = if family.sat_max > 0.0 {
        family.sat_min / family.sat_max
    } else {
        1.0
    };
    floor + (1.0 - floor) * bell_weight(lightness)
}

/// A terminal color's OKHSL channels and its OKLCH chroma (upstream
/// `SourceColor`, system-theme.ts:591-600).
#[derive(Debug, Clone, Copy)]
struct SourceColor {
    h: f64,
    s: f64,
    l: f64,
    chroma: f64,
}

fn source_of(color: Rgb) -> SourceColor {
    let (h, s, l) = rgb_to_okhsl(color.0, color.1, color.2);
    SourceColor {
        h,
        s,
        l,
        chroma: rgb_to_oklch_channels(color).1,
    }
}

/// A source color's hue at another OKHSL lightness, with its saturation
/// applying at its own lightness and falling off toward black and white
/// along the family's saturation curve, never rising above it. Chroma is also
/// capped at the source's, so a pastel palette color stays pastel
/// (upstream `anchored`, system-theme.ts:610-618, #10255).
fn anchored(source: SourceColor, family: Family, lightness: f64, saturation: f64) -> Rgb {
    let anchor = saturation_curve(family, source.l);
    let falloff = if anchor > 0.0 {
        (saturation_curve(family, lightness) / anchor).min(1.0)
    } else {
        1.0
    };
    let color = okhsl_to_rgb(source.h, source.s * falloff * saturation, lightness);
    let cap = source.chroma * falloff * saturation;
    let (l, c, _h) = rgb_to_oklch_channels(color);
    if c <= cap {
        color
    } else {
        oklch_to_rgb(l, cap, source.h)
    }
}

/// Move a text color toward white or black until it reaches the WCAG minimum
/// on every surface (upstream `withTextContrast`, system-theme.ts:620-638).
fn with_text_contrast(color: Rgb, surfaces: &[Rgb], lighter: bool) -> Rgb {
    let meets = |candidate: Rgb| {
        surfaces
            .iter()
            .all(|surface| wcag_contrast(candidate, *surface) >= TEXT_MINIMUM_WCAG_CONTRAST)
    };
    if meets(color) {
        return color;
    }
    let (h, s, l) = rgb_to_okhsl(color.0, color.1, color.2);
    let at = |lightness: f64| okhsl_to_rgb(h, s, lightness);
    let extreme = if lighter { 1.0 } else { 0.0 };
    if !meets(at(extreme)) {
        return at(extreme);
    }
    let (mut low, mut high) = (l, extreme);
    for _ in 0..20 {
        let middle = (low + high) / 2.0;
        if meets(at(middle)) {
            high = middle;
        } else {
            low = middle;
        }
    }
    at(high)
}

/// Everything the generation needs from the input, with the derived
/// appearance and the background's lightness (upstream
/// `generateSystemThemeColors`'s closure environment, system-theme.ts:465-527).
struct Generator {
    background: Rgb,
    background_l: f64,
    appearance: Appearance,
    lighter: bool,
    /// White on a dark background, black on a light one (upstream `extreme`,
    /// system-theme.ts:470).
    extreme_lightness: f64,
    extreme_text: Rgb,
    saturation: f64,
    palette: Option<Vec<SourceColor>>,
}

impl Generator {
    /// The target lightness for a level on a surface, or `None` where the
    /// level cannot be reached (upstream `levelTarget`,
    /// system-theme.ts:456-462).
    fn level_target(&self, level: Level, surface_l: f64) -> Option<f64> {
        let curve = level.curve(self.appearance);
        if surface_l < curve.reachable[0] || surface_l > curve.reachable[1] {
            return None;
        }
        Some(
            curve
                .coefficients
                .iter()
                .enumerate()
                .map(|(power, coefficient)| coefficient * surface_l.powi(power as i32))
                .sum(),
        )
    }

    /// The relaxation floor: levels stronger than it compress toward it
    /// (upstream `READABLE_FLOOR`, system-theme.ts:341).
    fn readable_floor(&self) -> Level {
        if self.appearance == Appearance::Dark {
            Level::Readable
        } else {
            Level::Subtle
        }
    }

    /// The lightness a rule needs on a surface, relaxed by `t`: from 0 to 1,
    /// levels stronger than the readable floor move toward it; from 1 to 2,
    /// all levels move toward the surface itself (upstream `target`,
    /// system-theme.ts:497-508). `None` where the level cannot be reached and
    /// no relaxation is allowed.
    fn target(&self, level: Level, surface_l: f64, t: f64) -> Option<f64> {
        let reached = self.level_target(level, surface_l);
        if reached.is_none() && t == 0.0 {
            return None;
        }
        let distance = reached.unwrap_or(self.extreme_lightness) - surface_l;
        let floor = self
            .level_target(self.readable_floor(), surface_l)
            .unwrap_or(self.extreme_lightness)
            - surface_l;
        let compressed = if distance.abs() > floor.abs() {
            distance - (distance - floor) * t.min(1.0)
        } else {
            distance
        };
        Some(surface_l + compressed * (1.0 - (t - 1.0).max(0.0)))
    }

    /// A token's color at an OKLab lightness (upstream `paint`,
    /// system-theme.ts:489-495): with a palette the palette color's
    /// saturation applies at its own lightness and falls off along the
    /// family's curve; without one the family's own hue and saturation are
    /// used, scaled by the input saturation.
    fn paint(&self, token: &str, oklab_l: f64) -> Rgb {
        let lightness = oklab_to_okhsl_lightness(oklab_l);
        let family = token_family_name(token)
            .and_then(family_by_name)
            .unwrap_or(F_NEUTRAL);
        match &self.palette {
            None => okhsl_to_rgb(
                family.hue,
                (family.sat_min + (family.sat_max - family.sat_min) * bell_weight(lightness))
                    * self.saturation,
                lightness,
            ),
            Some(palette) => {
                let slot = token_slot(token).unwrap_or(family.slot);
                anchored(palette[slot], family, lightness, self.saturation)
            }
        }
    }

    /// Whether a color reaches the WCAG minimum for text on it (upstream
    /// `readable`, system-theme.ts:513-515).
    fn readable(&self, color: Rgb) -> bool {
        wcag_contrast(self.extreme_text, color) >= TEXT_MINIMUM_WCAG_CONTRAST
    }

    /// Keep a panel light enough (or dark enough) for extreme text on it
    /// (upstream `limitPanel`, system-theme.ts:516-525).
    fn limit_panel(&self, token: &str, l: f64) -> Rgb {
        let color = self.paint(token, l);
        if self.readable(color) {
            return color;
        }
        let (mut low, mut high) = (self.background_l, l);
        for _ in 0..20 {
            let middle = (low + high) / 2.0;
            if self.readable(self.paint(token, middle)) {
                low = middle;
            } else {
                high = middle;
            }
        }
        self.paint(token, low)
    }

    /// Solve every token's color with the relaxation parameter `t`, or
    /// `None` when some level cannot be reached from its surface (upstream
    /// `solve`, system-theme.ts:527-545).
    fn solve(&self, t: f64) -> Option<BTreeMap<&'static str, Rgb>> {
        let mut colors: BTreeMap<&'static str, Rgb> = BTreeMap::new();
        colors.insert("background", self.background);
        for &token in solve_order() {
            let mut targets: Vec<f64> = Vec::new();
            for rule in rules() {
                if rule.token != token {
                    continue;
                }
                for surface in rule.on {
                    let surface_color = colors
                        .get(surface.key())
                        .copied()
                        .unwrap_or(self.background);
                    let value = self.target(rule.level, oklab_lightness(surface_color), t)?;
                    if !(0.0..=1.0).contains(&value) {
                        return None;
                    }
                    targets.push(value);
                }
            }
            let l = if self.lighter {
                targets.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            } else {
                targets.iter().copied().fold(f64::INFINITY, f64::min)
            };
            let color = if PANEL_NAMES.contains(&token) {
                self.limit_panel(token, l)
            } else {
                self.paint(token, l)
            };
            colors.insert(token, color);
        }
        Some(colors)
    }
}

/// Generate the system theme's colors from the terminal's reported colors
/// (upstream `generateSystemThemeColors`, system-theme.ts:465-587).
pub fn generate_system_theme_colors(input: &SystemThemeInput) -> SystemThemeColors {
    let saturation = input.saturation.unwrap_or(1.0).clamp(0.0, 1.0);
    let Some(background) = input.background else {
        return indexed_colors(saturation, input.appearance_hint);
    };
    let palette: Option<Vec<SourceColor>> = input
        .palette
        .as_ref()
        .filter(|colors| colors.len() == 16)
        .map(|colors| colors.iter().copied().map(source_of).collect());

    let appearance = super::terminal_colors::terminal_appearance(background, input.foreground);
    let lighter = appearance == Appearance::Dark;
    let extreme_lightness = if lighter { 1.0 } else { 0.0 };
    let generator = Generator {
        background,
        background_l: oklab_lightness(background),
        appearance,
        lighter,
        extreme_lightness,
        extreme_text: if lighter { (255, 255, 255) } else { (0, 0, 0) },
        saturation,
        palette,
    };

    let mut relaxation = 0.0;
    let mut colors = generator.solve(0.0);
    if colors.is_none() {
        // Mid-gray backgrounds cannot fit every level: relax as little as
        // possible. Full relaxation always fits (system-theme.ts:560-576).
        let (mut low, mut high) = (0.0, 2.0);
        colors = generator.solve(high);
        for _ in 0..20 {
            let middle = (low + high) / 2.0;
            let attempt = generator.solve(middle);
            if let Some(solved) = attempt {
                high = middle;
                colors = Some(solved);
            } else {
                low = middle;
            }
        }
        relaxation = high;
    }
    let solved = colors.unwrap_or_default();

    let mut result: BTreeMap<&'static str, ColorValue> = BTreeMap::new();
    for token in super::theme_json::all_color_keys() {
        result.insert(
            token,
            match solved.get(token) {
                Some(color) => ColorValue::Hex(hex_of(*color)),
                None => ColorValue::Empty,
            },
        );
    }

    for token in FOREGROUND_TOKENS {
        let mut surfaces: Vec<Rgb> = Vec::new();
        for rule in rules() {
            if rule.token != token {
                continue;
            }
            for surface in rule.on {
                surfaces.push(solved.get(surface.key()).copied().unwrap_or(background));
            }
        }
        // Body text uses the terminal's own foreground where it is clearly
        // stronger than muted text; otherwise the foreground's hue at just
        // enough lightness (system-theme.ts:579-586).
        let mut text = solved.get(token).copied();
        if let Some(foreground) = input.foreground {
            let targets: Vec<Option<f64>> = surfaces
                .iter()
                .map(|surface| {
                    generator.target(Level::Emphasis, oklab_lightness(*surface), relaxation)
                })
                .collect();
            if targets
                .iter()
                .all(|target| target.is_some_and(|value| (0.0..=1.0).contains(&value)))
            {
                let values: Vec<f64> = targets.iter().filter_map(|target| *target).collect();
                let needed = if lighter {
                    values.iter().copied().fold(f64::NEG_INFINITY, f64::max)
                } else {
                    values.iter().copied().fold(f64::INFINITY, f64::min)
                };
                let foreground_l = oklab_lightness(foreground);
                if if lighter {
                    foreground_l >= needed
                } else {
                    foreground_l <= needed
                } {
                    result.insert(token, ColorValue::Empty);
                    continue;
                }
                text = Some(anchored(
                    source_of(foreground),
                    F_NEUTRAL,
                    oklab_to_okhsl_lightness(needed),
                    saturation,
                ));
            }
        }
        // Body text keeps at least 4.5:1 on the surfaces it is drawn on,
        // even on relaxed mid-gray backgrounds.
        if let Some(text) = text {
            result.insert(
                token,
                ColorValue::Hex(hex_of(with_text_contrast(text, &surfaces, lighter))),
            );
        }
    }

    SystemThemeColors {
        colors: result,
        dim: Vec::new(),
        appearance: Some(appearance),
    }
}

/// Colors for terminals that reported nothing (upstream `indexedColors`,
/// system-theme.ts:642-660): the terminal renders ANSI indices 0-15 and the
/// default colors with its own theme, so they fit any background. Neutral
/// tokens below body text are faint (SGR 2) instead of bright black, which
/// some themes make nearly invisible. Panels have no background.
fn indexed_colors(saturation: f64, appearance: Option<Appearance>) -> SystemThemeColors {
    let mut colors: BTreeMap<&'static str, ColorValue> = BTreeMap::new();
    let mut dim: Vec<ThemeColor> = Vec::new();
    for token in super::theme_json::all_color_keys() {
        if PANEL_NAMES.contains(&token) {
            colors.insert(token, ColorValue::Empty);
            continue;
        }
        let Some(family_name) = token_family_name(token) else {
            continue;
        };
        let neutral = family_name == "neutral";
        let slot = token_slot(token).or_else(|| family_by_name(family_name).map(|f| f.slot));
        colors.insert(
            token,
            if !neutral && saturation > 0.0 {
                slot.map_or(ColorValue::Empty, |slot| ColorValue::Index(slot as u8))
            } else {
                ColorValue::Empty
            },
        );
        if neutral && !FOREGROUND_TOKENS.contains(&token) {
            if let Some(color) = theme_color_of(token) {
                dim.push(color);
            }
        }
    }
    SystemThemeColors {
        colors,
        dim,
        appearance,
    }
}

/// The [`ThemeColor`] for a token key (the inverse of `Theme::key`); `None`
/// for keys without a variant (none today).
fn theme_color_of(token: &str) -> Option<ThemeColor> {
    super::all_theme_colors()
        .into_iter()
        .find(|color| super::Theme::key(*color) == token)
}
