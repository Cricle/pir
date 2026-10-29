//! Unicode LaTeX renderer (upstream `renderLatex`, latex.ts:1488-1506):
//! converts a basic LaTeX math expression into terminal-friendly text.
//! Display mode stacks fractions and operator limits vertically; inline mode
//! stays linear. Returns `None` for unsupported or malformed syntax.

// ---------------------------------------------------------------------------
// LaTeX engine — port of `pi-tui`'s `src/latex.ts`
// ---------------------------------------------------------------------------

use crate::tui::ansi::visible_width;

/// Private-use markers shared with upstream latex.ts (U+F0000..U+F0005).
const LAYOUT_MARKER_START: char = '\u{f0000}';
const LAYOUT_MARKER_END: char = '\u{f0001}';
const PROTECTED_SPACE: char = '\u{f0002}';
const NAMED_OPERATOR_START: char = '\u{f0004}';
const NAMED_OPERATOR_END: char = '\u{f0005}';
/// Upstream `NEGATIVE_SPACE` (latex.ts:525).
const NEGATIVE_SPACE: &str = "\u{0}";

/// Upstream `SYMBOLS` (latex.ts:3-228).
fn symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ϵ",
        "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "varkappa" => "ϰ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "varrho" => "ϱ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "ϕ",
        "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        "pm" => "±",
        "mp" => "∓",
        "times" => "×",
        "div" => "÷",
        "cdot" => "·",
        "ast" => "∗",
        "star" => "⋆",
        "circ" => "∘",
        "bullet" => "•",
        "oplus" => "⊕",
        "ominus" => "⊖",
        "otimes" => "⊗",
        "oslash" => "⊘",
        "odot" => "⊙",
        "bigcirc" => "○",
        "dagger" => "†",
        "ddagger" => "‡",
        "amalg" => "⨿",
        "uplus" => "⊎",
        "sqcap" => "⊓",
        "sqcup" => "⊔",
        "bowtie" => "⋈",
        "Join" => "⋈",
        "ltimes" => "⋉",
        "rtimes" => "⋊",
        "leftouterjoin" => "⟕",
        "rightouterjoin" => "⟖",
        "fullouterjoin" => "⟗",
        "triangleleft" => "◁",
        "triangleright" => "▷",
        "wr" => "≀",
        "cap" => "∩",
        "cup" => "∪",
        "bigcap" => "⋂",
        "bigcup" => "⋃",
        "bigwedge" => "⋀",
        "bigvee" => "⋁",
        "bigsqcup" => "⨆",
        "biguplus" => "⨄",
        "bigoplus" => "⨁",
        "bigotimes" => "⨂",
        "bigodot" => "⨀",
        "setminus" => "∖",
        "in" => "∈",
        "notin" => "∉",
        "ni" => "∋",
        "subset" => "⊂",
        "supset" => "⊃",
        "subseteq" => "⊆",
        "supseteq" => "⊇",
        "sqsubset" => "⊏",
        "sqsupset" => "⊐",
        "sqsubseteq" => "⊑",
        "sqsupseteq" => "⊒",
        "prec" => "≺",
        "preceq" => "≼",
        "succ" => "≻",
        "succeq" => "≽",
        "ll" => "≪",
        "gg" => "≫",
        "le" => "≤",
        "leq" => "≤",
        "leqslant" => "≤",
        "ge" => "≥",
        "geq" => "≥",
        "geqslant" => "≥",
        "ne" => "≠",
        "neq" => "≠",
        "equiv" => "≡",
        "approx" => "≈",
        "sim" => "∼",
        "simeq" => "≃",
        "cong" => "≅",
        "asymp" => "≍",
        "doteq" => "≐",
        "propto" => "∝",
        "parallel" => "∥",
        "perp" => "⊥",
        "mid" => "∣",
        "vdash" => "⊢",
        "dashv" => "⊣",
        "models" => "⊨",
        "Vdash" => "⊩",
        "Vvdash" => "⊪",
        "nvdash" => "⊬",
        "nvDash" => "⊭",
        "forall" => "∀",
        "exists" => "∃",
        "nexists" => "∄",
        "neg" => "¬",
        "land" => "∧",
        "wedge" => "∧",
        "lor" => "∨",
        "vee" => "∨",
        "to" => "→",
        "rightarrow" => "→",
        "longrightarrow" => "→",
        "leftarrow" => "←",
        "longleftarrow" => "←",
        "gets" => "←",
        "leftrightarrow" => "↔",
        "longleftrightarrow" => "↔",
        "hookleftarrow" => "↩",
        "hookrightarrow" => "↪",
        "twoheadleftarrow" => "↞",
        "twoheadrightarrow" => "↠",
        "leftharpoonup" => "↼",
        "leftharpoondown" => "↽",
        "rightharpoonup" => "⇀",
        "rightharpoondown" => "⇁",
        "rightleftharpoons" => "⇌",
        "leftrightharpoons" => "⇋",
        "nearrow" => "↗",
        "searrow" => "↘",
        "swarrow" => "↙",
        "nwarrow" => "↖",
        "rightsquigarrow" => "⇝",
        "leadsto" => "⇝",
        "Rightarrow" => "⇒",
        "Longrightarrow" => "⇒",
        "Leftarrow" => "⇐",
        "Longleftarrow" => "⇐",
        "Leftrightarrow" => "⇔",
        "Longleftrightarrow" => "⇔",
        "implies" => "⇒",
        "iff" => "⇔",
        "mapsto" => "↦",
        "longmapsto" => "↦",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "partial" => "∂",
        "nabla" => "∇",
        "int" => "∫",
        "iint" => "∬",
        "iiint" => "∭",
        "oint" => "∮",
        "sum" => "∑",
        "prod" => "∏",
        "coprod" => "∐",
        "infty" => "∞",
        "emptyset" => "∅",
        "varnothing" => "∅",
        "angle" => "∠",
        "therefore" => "∴",
        "because" => "∵",
        "aleph" => "ℵ",
        "beth" => "ℶ",
        "gimel" => "ℷ",
        "daleth" => "ℸ",
        "top" => "⊤",
        "bot" => "⊥",
        "triangle" => "△",
        "square" => "□",
        "lozenge" => "◊",
        "checkmark" => "✓",
        "complement" => "∁",
        "wp" => "℘",
        "prime" => "′",
        "ldots" => "…",
        "dots" => "…",
        "cdots" => "⋯",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "ell" => "ℓ",
        "hbar" => "ℏ",
        "Im" => "ℑ",
        "Re" => "ℜ",
        "langle" => "⟨",
        "rangle" => "⟩",
        "vert" => "|",
        "lvert" => "|",
        "rvert" => "|",
        "Vert" => "‖",
        "lVert" => "‖",
        "rVert" => "‖",
        "lbrace" => "{",
        "rbrace" => "}",
        "backslash" => "\\",
        "lfloor" => "⌊",
        "rfloor" => "⌋",
        "lceil" => "⌈",
        "rceil" => "⌉",
        "colon" => ":",
        _ => return None,
    })
}

/// Upstream `NAMED_OPERATORS` (latex.ts:230-263).
fn named_operator(name: &str) -> bool {
    matches!(
        name,
        "arccos"
            | "arcsin"
            | "arctan"
            | "arg"
            | "cos"
            | "cosh"
            | "cot"
            | "coth"
            | "csc"
            | "deg"
            | "det"
            | "dim"
            | "exp"
            | "gcd"
            | "hom"
            | "inf"
            | "ker"
            | "lg"
            | "lim"
            | "liminf"
            | "limsup"
            | "ln"
            | "log"
            | "max"
            | "min"
            | "Pr"
            | "sec"
            | "sin"
            | "sinh"
            | "sup"
            | "tan"
            | "tanh"
    )
}

/// Upstream `LIMIT_OPERATORS` (latex.ts:265-277).
fn limit_operator(name: &str) -> bool {
    matches!(
        name,
        "argmax"
            | "argmin"
            | "inf"
            | "injlim"
            | "lim"
            | "liminf"
            | "limsup"
            | "max"
            | "min"
            | "projlim"
            | "sup"
    )
}

/// Upstream `DISPLAY_LIMIT_SYMBOLS` (latex.ts:279-296).
fn display_limit_symbol(name: &str) -> bool {
    matches!(
        name,
        "bigcap"
            | "bigcup"
            | "bigodot"
            | "bigoplus"
            | "bigotimes"
            | "bigsqcup"
            | "biguplus"
            | "bigvee"
            | "bigwedge"
            | "coprod"
            | "int"
            | "iint"
            | "iiint"
            | "oint"
            | "prod"
            | "sum"
    )
}

/// Upstream `RELATION_COMMANDS` (latex.ts:298-387).
fn relation_command(name: &str) -> bool {
    matches!(
        name,
        "Leftarrow"
            | "Leftrightarrow"
            | "Longleftarrow"
            | "Longleftrightarrow"
            | "Longrightarrow"
            | "Rightarrow"
            | "Join"
            | "Vdash"
            | "Vvdash"
            | "approx"
            | "asymp"
            | "bowtie"
            | "cong"
            | "dashv"
            | "fullouterjoin"
            | "doteq"
            | "downarrow"
            | "equiv"
            | "ge"
            | "geq"
            | "geqslant"
            | "gets"
            | "gg"
            | "hookleftarrow"
            | "hookrightarrow"
            | "iff"
            | "implies"
            | "in"
            | "leadsto"
            | "le"
            | "leftarrow"
            | "leftharpoondown"
            | "leftharpoonup"
            | "leftrightarrow"
            | "leftrightharpoons"
            | "leftouterjoin"
            | "leq"
            | "leqslant"
            | "ll"
            | "longleftarrow"
            | "longleftrightarrow"
            | "longmapsto"
            | "longrightarrow"
            | "ltimes"
            | "mapsto"
            | "mid"
            | "models"
            | "ne"
            | "nearrow"
            | "neq"
            | "ni"
            | "notin"
            | "nvdash"
            | "nvDash"
            | "nwarrow"
            | "parallel"
            | "perp"
            | "prec"
            | "preceq"
            | "propto"
            | "rightharpoondown"
            | "rightharpoonup"
            | "rightleftharpoons"
            | "rightouterjoin"
            | "rightarrow"
            | "rightsquigarrow"
            | "rtimes"
            | "searrow"
            | "sim"
            | "simeq"
            | "sqsubset"
            | "sqsubseteq"
            | "sqsupset"
            | "sqsupseteq"
            | "subset"
            | "subseteq"
            | "succ"
            | "succeq"
            | "supset"
            | "supseteq"
            | "swarrow"
            | "to"
            | "triangleleft"
            | "triangleright"
            | "twoheadleftarrow"
            | "twoheadrightarrow"
            | "uparrow"
            | "vdash"
    )
}

/// Upstream `NEGATED_SYMBOLS` (latex.ts:389-420).
fn negated_symbol(value: &str) -> Option<&'static str> {
    Some(match value {
        "<" => "≮",
        ">" => "≯",
        "=" => "≠",
        "∈" => "∉",
        "∋" => "∌",
        "∣" => "∤",
        "∥" => "∦",
        "∼" => "≁",
        "≃" => "≄",
        "≅" => "≇",
        "≈" => "≉",
        "≡" => "≢",
        "≤" => "≰",
        "≥" => "≱",
        "≺" => "⊀",
        "≻" => "⊁",
        "⊂" => "⊄",
        "⊃" => "⊅",
        "⊆" => "⊈",
        "⊇" => "⊉",
        "⊢" => "⊬",
        "⊨" => "⊭",
        "↔" => "↮",
        "←" => "↚",
        "→" => "↛",
        "⇒" => "⇏",
        "⇐" => "⇍",
        "⇔" => "⇎",
        "≼" => "⋠",
        "≽" => "⋡",
        _ => return None,
    })
}

/// Upstream `BLACKBOARD` (latex.ts:422-430).
fn blackboard(c: char) -> Option<char> {
    Some(match c {
        'C' => 'ℂ',
        'H' => 'ℍ',
        'N' => 'ℕ',
        'P' => 'ℙ',
        'Q' => 'ℚ',
        'R' => 'ℝ',
        'Z' => 'ℤ',
        _ => return None,
    })
}

/// Upstream `SUPERSCRIPTS` (latex.ts:432-473).
fn superscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'a' => 'ᵃ',
        'b' => 'ᵇ',
        'c' => 'ᶜ',
        'd' => 'ᵈ',
        'e' => 'ᵉ',
        'f' => 'ᶠ',
        'g' => 'ᵍ',
        'h' => 'ʰ',
        'i' => 'ⁱ',
        'j' => 'ʲ',
        'k' => 'ᵏ',
        'l' => 'ˡ',
        'm' => 'ᵐ',
        'n' => 'ⁿ',
        'o' => 'ᵒ',
        'p' => 'ᵖ',
        'r' => 'ʳ',
        's' => 'ˢ',
        't' => 'ᵗ',
        'u' => 'ᵘ',
        'v' => 'ᵛ',
        'w' => 'ʷ',
        'x' => 'ˣ',
        'y' => 'ʸ',
        'z' => 'ᶻ',
        _ => return None,
    })
}

/// Upstream `SUBSCRIPTS` (latex.ts:475-508).
fn subscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'h' => 'ₕ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'k' => 'ₖ',
        'l' => 'ₗ',
        'm' => 'ₘ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'p' => 'ₚ',
        'r' => 'ᵣ',
        's' => 'ₛ',
        't' => 'ₜ',
        'u' => 'ᵤ',
        'v' => 'ᵥ',
        'x' => 'ₓ',
        _ => return None,
    })
}

/// Upstream `SPACING_COMMANDS` / `NEGATIVE_SPACING_COMMANDS`
/// (latex.ts:510-524).
fn spacing_command(name: &str) -> Option<&'static str> {
    if matches!(
        name,
        "," | ":"
            | ";"
            | " "
            | ">"
            | "enspace"
            | "enskip"
            | "medspace"
            | "quad"
            | "qquad"
            | "thickspace"
            | "thinspace"
    ) {
        return Some(" ");
    }
    if matches!(name, "!" | "negmedspace" | "negthickspace" | "negthinspace") {
        return Some(NEGATIVE_SPACE);
    }
    None
}

/// Upstream `FONT_SWITCH_COMMANDS` (latex.ts:526).
fn font_switch_command(name: &str) -> bool {
    matches!(name, "bf" | "cal" | "it" | "rm" | "sf" | "sl" | "tt")
}

/// Upstream `IGNORED_COMMANDS` (latex.ts:527-534).
fn ignored_command(name: &str) -> bool {
    matches!(
        name,
        "displaystyle" | "limits" | "nolimits" | "scriptstyle" | "scriptscriptstyle" | "textstyle"
    )
}

/// Upstream `SIZE_COMMANDS` (latex.ts:535-548).
fn size_command(name: &str) -> bool {
    matches!(
        name,
        "big"
            | "Big"
            | "bigg"
            | "Bigg"
            | "bigl"
            | "Bigl"
            | "biggl"
            | "Biggl"
            | "bigr"
            | "Bigr"
            | "biggr"
            | "Biggr"
    )
}

/// Upstream `PLAIN_WRAPPERS` (latex.ts:549-580).
fn plain_wrapper(name: &str) -> bool {
    matches!(
        name,
        "emph"
            | "mathcal"
            | "mathbf"
            | "mathfrak"
            | "mathit"
            | "mathrm"
            | "mathnormal"
            | "mathscr"
            | "mathsf"
            | "mathtt"
            | "mathup"
            | "mbox"
            | "overbrace"
            | "pmb"
            | "smash"
            | "substack"
            | "text"
            | "textbf"
            | "textit"
            | "textmd"
            | "textnormal"
            | "textrm"
            | "textsc"
            | "textsf"
            | "textsl"
            | "texttt"
            | "textup"
            | "underbrace"
            | "bm"
            | "boldsymbol"
    )
}

/// Upstream `ACCENTS` (latex.ts:581-600).
fn accent(name: &str) -> Option<char> {
    Some(match name {
        "acute" => '\u{0301}',
        "bar" => '\u{0305}',
        "breve" => '\u{0306}',
        "check" => '\u{030c}',
        "ddot" => '\u{0308}',
        "dot" => '\u{0307}',
        "grave" => '\u{0300}',
        "hat" => '\u{0302}',
        "mathring" => '\u{030a}',
        "overleftarrow" => '\u{20d6}',
        "overleftrightarrow" => '\u{20e1}',
        "overline" => '\u{0305}',
        "overrightarrow" => '\u{20d7}',
        "tilde" => '\u{0303}',
        "underline" => '\u{0332}',
        "vec" => '\u{20d7}',
        "widehat" => '\u{0302}',
        "widetilde" => '\u{0303}',
        _ => return None,
    })
}

/// Upstream `normalizeScriptValue` (latex.ts:614-616).
fn normalize_script_value(value: &str) -> String {
    let chars: Vec<char> = value.trim().chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut index = 0usize;
    while index < chars.len() {
        let c = chars[index];
        if c == '=' || c == '+' || c == '-' {
            while out.ends_with(|ch: char| ch.is_whitespace()) {
                out.pop();
            }
            out.push(c);
            index += 1;
            while index < chars.len() && chars[index].is_whitespace() {
                index += 1;
            }
            continue;
        }
        out.push(c);
        index += 1;
    }
    out
}

/// Upstream `formatUnicodeScript` (latex.ts:618-620).
fn format_unicode_script(value: &str, sub: bool) -> Option<String> {
    let normalized = normalize_script_value(value);
    let mut out = String::with_capacity(normalized.len());
    for c in normalized.chars() {
        let replaced = if sub { subscript(c) } else { superscript(c) }?;
        out.push(replaced);
    }
    Some(out)
}

/// Upstream `formatScript` (latex.ts:622-634).
fn format_script(value: &str, sub: bool) -> String {
    let value = normalize_script_value(value);
    if let Some(unicode) = format_unicode_script(&value, sub) {
        return unicode;
    }
    let prefix = if sub { "_" } else { "^" };
    let single = value.chars().count() == 1;
    let letters = sub && !value.is_empty() && value.chars().all(|c| c.is_ascii_alphabetic());
    if single || letters {
        format!("{prefix}{value}")
    } else {
        format!("{prefix}({value})")
    }
}

/// `[\p{L}\p{N}.]+` (upstream inline regexes; `+` needs a non-empty match).
fn letters_digits_dots(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_alphabetic() || c.is_numeric() || c == '.')
}

/// `[\p{N}.]+`.
fn digits_dots(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_numeric() || c == '.')
}

/// Upstream `formatFraction` (latex.ts:636-642).
fn format_fraction(numerator: &str, denominator: &str) -> String {
    let numerator = numerator.trim();
    let denominator = denominator.trim();
    let simple_numerator = letters_digits_dots(numerator);
    let simple_denominator = digits_dots(denominator) || denominator.chars().count() == 1;
    let num = if simple_numerator {
        numerator.to_string()
    } else {
        format!("({numerator})")
    };
    let den = if simple_denominator {
        denominator.to_string()
    } else {
        format!("({denominator})")
    };
    format!("{num}/{den}")
}

/// Upstream `formatRoot` (latex.ts:644-647).
fn format_root(value: &str, symbol: &str) -> String {
    let value = value.trim();
    if letters_digits_dots(value) {
        format!("{symbol}{value}")
    } else {
        format!("{symbol}({value})")
    }
}

/// Upstream `normalizeOutput` (latex.ts:654-665): named-operator spacing,
/// marker removal, per-line whitespace collapse and interior blank lines.
fn normalize_output(value: &str) -> String {
    let original: Vec<char> = value.chars().collect();
    // 1. `(?<=[\p{L}\p{N}\]}\u{f0001}])\u{f0004}` → " "; 2. drop the rest.
    let mut step: Vec<char> = Vec::with_capacity(original.len());
    for (index, &c) in original.iter().enumerate() {
        if c == NAMED_OPERATOR_START {
            let previous = if index > 0 { original[index - 1] } else { '\0' };
            if previous.is_alphanumeric()
                || previous == ']'
                || previous == '}'
                || previous == LAYOUT_MARKER_END
            {
                step.push(' ');
            }
        } else {
            step.push(c);
        }
    }
    // 3. `\u{f0005}(?=[\p{L}\p{N}√\u{f0000}])` → " "; 4. drop the rest.
    let mut spaced: Vec<char> = Vec::with_capacity(step.len());
    for (index, &c) in step.iter().enumerate() {
        if c == NAMED_OPERATOR_END {
            if let Some(next) = step.get(index + 1).copied() {
                if next.is_alphanumeric() || next == '√' || next == LAYOUT_MARKER_START {
                    spaced.push(' ');
                }
            }
        } else {
            spaced.push(c);
        }
    }
    // Per line: `[ \t]+` → " ", trim, keep interior empty lines.
    let lines: Vec<String> = spaced
        .into_iter()
        .collect::<String>()
        .split('\n')
        .map(|line| {
            let mut collapsed = String::with_capacity(line.len());
            let mut in_run = false;
            for c in line.chars() {
                if c == ' ' || c == '\t' {
                    if !in_run {
                        collapsed.push(' ');
                        in_run = true;
                    }
                } else {
                    collapsed.push(c);
                    in_run = false;
                }
            }
            collapsed.trim().to_string()
        })
        .collect();
    let last = lines.len().saturating_sub(1);
    lines
        .iter()
        .enumerate()
        .filter(|(index, line)| !line.is_empty() || (*index > 0 && *index < last))
        .map(|(_, line)| line.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Upstream `LayoutNode` union (latex.ts:667-692).
enum LayoutNode {
    Fraction {
        numerator: String,
        denominator: String,
    },
    Operator {
        operator: String,
        lower: Option<String>,
        upper: Option<String>,
    },
    Script {
        lower: Option<String>,
        upper: Option<String>,
    },
    Matrix {
        lines: Vec<String>,
        baseline: usize,
    },
}

/// Upstream `Layout` (latex.ts:694-698).
struct Layout {
    lines: Vec<String>,
    width: usize,
    baseline: usize,
}

/// Upstream `padLayoutLine` (latex.ts:706-710).
fn pad_layout_line(line: &str, width: usize, centered: bool) -> String {
    let padding = width.saturating_sub(visible_width(line));
    let left = if centered { padding / 2 } else { 0 };
    format!("{}{}{}", " ".repeat(left), line, " ".repeat(padding - left))
}

/// Upstream `joinLayouts` (latex.ts:712-735): baseline-aligned horizontal
/// concatenation of sub-layouts.
fn join_layouts(layouts: &[Layout]) -> Layout {
    if layouts.is_empty() {
        return Layout {
            lines: vec![String::new()],
            width: 0,
            baseline: 0,
        };
    }
    let baseline = layouts
        .iter()
        .map(|layout| layout.baseline)
        .max()
        .unwrap_or(0);
    let below = layouts
        .iter()
        .map(|layout| layout.lines.len().saturating_sub(layout.baseline + 1))
        .max()
        .unwrap_or(0);
    let mut lines: Vec<String> = Vec::with_capacity(baseline + below + 1);
    for row in 0..=baseline + below {
        let mut line = String::new();
        for layout in layouts {
            let source_row = row as i64 - baseline as i64 + layout.baseline as i64;
            if source_row >= 0 && (source_row as usize) < layout.lines.len() {
                line.push_str(&pad_layout_line(
                    &layout.lines[source_row as usize],
                    layout.width,
                    false,
                ));
            } else {
                line.push_str(&" ".repeat(layout.width));
            }
        }
        lines.push(line.trim_end().to_string());
    }
    Layout {
        lines,
        width: layouts.iter().map(|layout| layout.width).sum(),
        baseline,
    }
}

/// Text piece between layout markers (upstream `renderLayout` slice
/// handling, latex.ts:750-761).
fn layout_text(
    sliced: &str,
    previous_is_matrix: bool,
    has_previous: bool,
    next_is_matrix: bool,
) -> String {
    let preserve_leading =
        previous_is_matrix && sliced.chars().next().is_some_and(|c| c.is_whitespace());
    let preserve_trailing =
        next_is_matrix && sliced.chars().last().is_some_and(|c| c.is_whitespace());
    let trimmed = if has_previous {
        sliced.trim_start()
    } else {
        sliced
    };
    let trimmed = trimmed.trim_end();
    if trimmed.is_empty() {
        if preserve_leading || preserve_trailing {
            " ".to_string()
        } else {
            String::new()
        }
    } else {
        format!(
            "{}{trimmed}{}",
            if preserve_leading { " " } else { "" },
            if preserve_trailing { " " } else { "" }
        )
    }
}

/// Upstream `renderLayout` (latex.ts:737-836): expands layout markers in
/// `source` into stacked lines.
fn render_layout(source: &str, nodes: &[LayoutNode]) -> Layout {
    let mut rendered_lines: Vec<String> = Vec::new();
    let mut first_baseline = 0usize;
    for source_line in source.split('\n') {
        let chars: Vec<char> = source_line.chars().collect();
        let mut layouts: Vec<Layout> = Vec::new();
        let mut position = 0usize;
        let mut previous_node: Option<usize> = None;
        let mut index = 0usize;
        while index < chars.len() {
            if chars[index] == LAYOUT_MARKER_START {
                let mut cursor = index + 1;
                let mut value = 0usize;
                let mut has_digits = false;
                while cursor < chars.len() && chars[cursor].is_ascii_digit() {
                    value = value
                        .saturating_mul(10)
                        .saturating_add((chars[cursor] as usize) - ('0' as usize));
                    cursor += 1;
                    has_digits = true;
                }
                if has_digits && chars.get(cursor) == Some(&LAYOUT_MARKER_END) {
                    if let Some(node) = nodes.get(value) {
                        if index > position {
                            let sliced: String = chars[position..index].iter().collect();
                            let previous_is_matrix = previous_node
                                .map(|n| matches!(nodes[n], LayoutNode::Matrix { .. }))
                                .unwrap_or(false);
                            let next_is_matrix = matches!(node, LayoutNode::Matrix { .. });
                            let text = layout_text(
                                &sliced,
                                previous_is_matrix,
                                previous_node.is_some(),
                                next_is_matrix,
                            );
                            let width = visible_width(&text);
                            layouts.push(Layout {
                                lines: vec![text],
                                width,
                                baseline: 0,
                            });
                        }
                        layouts.push(render_layout_node(node, nodes));
                        position = cursor + 1;
                        previous_node = Some(value);
                        index = position;
                        continue;
                    }
                }
            }
            index += 1;
        }
        if position < chars.len() {
            let sliced: String = chars[position..].iter().collect();
            let previous_is_matrix = previous_node
                .map(|n| matches!(nodes[n], LayoutNode::Matrix { .. }))
                .unwrap_or(false);
            let trimmed = if previous_node.is_some() {
                sliced.trim_start().to_string()
            } else {
                sliced.clone()
            };
            let text =
                if previous_is_matrix && sliced.chars().next().is_some_and(|c| c.is_whitespace()) {
                    format!(" {trimmed}")
                } else {
                    trimmed
                };
            let width = visible_width(&text);
            layouts.push(Layout {
                lines: vec![text],
                width,
                baseline: 0,
            });
        }
        let line_layout = join_layouts(&layouts);
        if rendered_lines.is_empty() {
            first_baseline = line_layout.baseline;
        }
        rendered_lines.extend(line_layout.lines);
    }
    let width = rendered_lines
        .iter()
        .map(|line| visible_width(line))
        .max()
        .unwrap_or(0);
    Layout {
        lines: rendered_lines,
        width,
        baseline: first_baseline,
    }
}

/// One layout node as a sub-layout (upstream `renderLayout` node switch,
/// latex.ts:762-815).
fn render_layout_node(node: &LayoutNode, nodes: &[LayoutNode]) -> Layout {
    match node {
        LayoutNode::Fraction {
            numerator,
            denominator,
        } => {
            let numerator_layout = render_layout(numerator, nodes);
            let denominator_layout = render_layout(denominator, nodes);
            let content_width = numerator_layout.width.max(denominator_layout.width).max(1);
            let width = content_width + 2;
            let mut lines: Vec<String> = Vec::new();
            for line in &numerator_layout.lines {
                lines.push(pad_layout_line(line, width, true));
            }
            lines.push(format!(" {} ", "─".repeat(content_width)));
            for line in &denominator_layout.lines {
                lines.push(pad_layout_line(line, width, true));
            }
            Layout {
                lines,
                width,
                baseline: numerator_layout.lines.len(),
            }
        }
        LayoutNode::Operator {
            operator,
            lower,
            upper,
        } => {
            let content_width = visible_width(operator)
                .max(lower.as_deref().map(visible_width).unwrap_or(0))
                .max(upper.as_deref().map(visible_width).unwrap_or(0));
            let mut lines: Vec<String> = Vec::new();
            if let Some(upper) = upper {
                lines.push(format!("{} ", pad_layout_line(upper, content_width, true)));
            }
            lines.push(format!(
                "{} ",
                pad_layout_line(operator, content_width, true)
            ));
            if let Some(lower) = lower {
                lines.push(format!("{} ", pad_layout_line(lower, content_width, true)));
            }
            Layout {
                lines,
                width: content_width + 1,
                baseline: if upper.is_none() { 0 } else { 1 },
            }
        }
        LayoutNode::Script { lower, upper } => {
            let upper_layout = upper.as_ref().map(|value| render_layout(value, nodes));
            let lower_layout = lower.as_ref().map(|value| render_layout(value, nodes));
            let width = upper_layout
                .as_ref()
                .map(|layout| layout.width)
                .unwrap_or(0)
                .max(
                    lower_layout
                        .as_ref()
                        .map(|layout| layout.width)
                        .unwrap_or(0),
                );
            let mut lines: Vec<String> = Vec::new();
            if let Some(layout) = &upper_layout {
                for line in &layout.lines {
                    lines.push(pad_layout_line(line, width, false));
                }
            }
            lines.push(" ".repeat(width));
            if let Some(layout) = &lower_layout {
                for line in &layout.lines {
                    lines.push(pad_layout_line(line, width, false));
                }
            }
            Layout {
                lines,
                width,
                baseline: upper_layout
                    .as_ref()
                    .map(|layout| layout.lines.len())
                    .unwrap_or(0),
            }
        }
        LayoutNode::Matrix { lines, baseline } => {
            let width = lines
                .iter()
                .map(|line| visible_width(line))
                .max()
                .unwrap_or(0);
            Layout {
                lines: lines
                    .iter()
                    .map(|line| pad_layout_line(line, width, false))
                    .collect(),
                width,
                baseline: *baseline,
            }
        }
    }
}

/// Lower-script style for `parseOperator` (upstream `inlineLowerStyle`).
#[derive(Clone, Copy)]
enum LowerStyle {
    Bracket,
    Script,
}

/// Upstream `LatexParser` (latex.ts:838-1477).
struct LatexParser<'a> {
    source: Vec<char>,
    layout_nodes: &'a mut Vec<LayoutNode>,
    display: bool,
    position: usize,
    supported: bool,
    stack_fractions: bool,
    script_depth: usize,
}

impl<'a> LatexParser<'a> {
    fn new(source: &str, layout_nodes: &'a mut Vec<LayoutNode>, display: bool) -> Self {
        Self {
            source: source.chars().collect(),
            layout_nodes,
            display,
            position: 0,
            supported: true,
            stack_fractions: true,
            script_depth: 0,
        }
    }

    /// Upstream `render` (latex.ts:853-859).
    fn render(&mut self) -> Option<String> {
        let rendered = self.parse_sequence(None);
        if !self.supported || self.position != self.source.len() {
            return None;
        }
        Some(normalize_output(&rendered))
    }

    /// Upstream `parseSequence` (latex.ts:861-947).
    fn parse_sequence(&mut self, end: Option<char>) -> String {
        let mut result = String::new();
        while self.position < self.source.len() {
            let character = self.source[self.position];
            if end == Some(character) {
                self.position += 1;
                return result;
            }
            if character == '}' {
                self.supported = false;
                return result;
            }
            if character == '{' {
                self.position += 1;
                let inner = self.parse_sequence(Some('}'));
                result.push_str(&inner);
                continue;
            }
            if character == '\\' {
                let command = self.parse_command();
                if command == NEGATIVE_SPACE {
                    let trimmed = result.trim_end().len();
                    result.truncate(trimmed);
                    if result.ends_with(NAMED_OPERATOR_END) {
                        result.pop();
                    }
                } else {
                    result.push_str(&command);
                }
                continue;
            }
            if character == '^' || character == '_' {
                self.position += 1;
                let trimmed = result.trim_end().len();
                result.truncate(trimmed);
                let script = self.parse_scripts(character);
                if result.ends_with(NAMED_OPERATOR_END) {
                    result.pop();
                    result.push_str(&script);
                    result.push(NAMED_OPERATOR_END);
                } else {
                    result.push_str(&script);
                }
                continue;
            }
            if character.is_whitespace() {
                result.push_str(&self.parse_whitespace());
                continue;
            }
            if character == '=' || character == '<' || character == '>' {
                let trimmed = result.trim_end().to_string();
                result = format!("{trimmed} {character} ");
                self.position += 1;
                continue;
            }
            if character == '&' {
                self.position += 1;
                continue;
            }
            if character == '~' {
                self.position += 1;
                result.push(' ');
                continue;
            }
            if character == '.' {
                if let Some(value) = trailing_layout_marker(&result) {
                    if let Some(LayoutNode::Matrix { lines, .. }) = self.layout_nodes.get_mut(value)
                    {
                        if let Some(last) = lines.last_mut() {
                            last.push('.');
                        }
                        self.position += 1;
                        continue;
                    }
                }
            }
            result.push(character);
            self.position += 1;
        }
        if end.is_some() {
            self.supported = false;
        }
        result
    }

    /// Upstream `parseScripts` (latex.ts:949-1005).
    fn parse_scripts(&mut self, initial: char) -> String {
        let mut sub: Option<String> = None;
        let mut sup: Option<String> = None;
        let mut order: Vec<char> = Vec::new();

        let parse_one = |parser: &mut LatexParser<'a>,
                         marker: char,
                         sub: &mut Option<String>,
                         sup: &mut Option<String>,
                         order: &mut Vec<char>| {
            parser.script_depth += 1;
            let value = parser.parse_required_argument(false);
            parser.script_depth -= 1;
            if marker == '_' {
                *sub = Some(value);
            } else {
                *sup = Some(value);
            }
            order.push(marker);
        };
        parse_one(self, initial, &mut sub, &mut sup, &mut order);

        let mut next_position = self.position;
        while next_position < self.source.len() && self.source[next_position].is_whitespace() {
            next_position += 1;
        }
        let next_marker = self.source.get(next_position).copied();
        if let Some(marker) = next_marker {
            if (marker == '^' || marker == '_') && marker != initial {
                self.position = next_position + 1;
                parse_one(self, marker, &mut sub, &mut sup, &mut order);
            }
        }

        let sub_unicode = sub.as_ref().map(|value| format_unicode_script(value, true));
        let sup_unicode = sup
            .as_ref()
            .map(|value| format_unicode_script(value, false));
        let can_use_layout = [sub.as_ref(), sup.as_ref()]
            .into_iter()
            .flatten()
            .all(|value| {
                !(value.contains('/')
                    || (!value.contains(LAYOUT_MARKER_START)
                        && value.chars().count() > 1
                        && !value
                            .chars()
                            .any(|c| c.is_ascii_uppercase() || c == '*' || c == '∗')))
            });
        let needs_layout = self.display
            && can_use_layout
            && (self.script_depth > 0
                || (sub.is_some() && sub_unicode.as_ref().is_some_and(|u| u.is_none()))
                || (sup.is_some() && sup_unicode.as_ref().is_some_and(|u| u.is_none())));
        if !needs_layout {
            let mut out = String::new();
            for kind in &order {
                if *kind == '_' {
                    let value = match &sub_unicode {
                        Some(Some(unicode)) => unicode.clone(),
                        _ => format_script(sub.as_deref().unwrap_or(""), true),
                    };
                    out.push_str(&value);
                } else {
                    let value = match &sup_unicode {
                        Some(Some(unicode)) => unicode.clone(),
                        _ => format_script(sup.as_deref().unwrap_or(""), false),
                    };
                    out.push_str(&value);
                }
            }
            return out;
        }

        let index = self.layout_nodes.len();
        self.layout_nodes.push(LayoutNode::Script {
            lower: sub.map(|value| normalize_output(&value)),
            upper: sup.map(|value| normalize_output(&value)),
        });
        format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}")
    }

    /// Upstream `parseWhitespace` (latex.ts:1007-1012).
    fn parse_whitespace(&mut self) -> String {
        while self.position < self.source.len() && self.source[self.position].is_whitespace() {
            self.position += 1;
        }
        " ".to_string()
    }

    /// Upstream `parseCommand` (latex.ts:1014-1192).
    fn parse_command(&mut self) -> String {
        self.position += 1;
        if self.position >= self.source.len() {
            self.supported = false;
            return String::new();
        }

        let first = self.source[self.position];
        if first == '\n' || first == '\r' {
            self.position += 1;
            if first == '\r' && self.source.get(self.position) == Some(&'\n') {
                self.position += 1;
            }
            return " ".to_string();
        }
        let command: String;
        if first.is_ascii_alphabetic() {
            let start = self.position;
            while self.position < self.source.len()
                && self.source[self.position].is_ascii_alphabetic()
            {
                self.position += 1;
            }
            command = self.source[start..self.position].iter().collect();
        } else {
            command = first.to_string();
            self.position += 1;
        }

        if command == "\\" {
            return "\n".to_string();
        }
        if let Some(spacing) = spacing_command(&command) {
            return spacing.to_string();
        }
        if font_switch_command(&command) {
            while self.position < self.source.len() && self.source[self.position].is_whitespace() {
                self.position += 1;
            }
            return String::new();
        }
        if ignored_command(&command) {
            return String::new();
        }
        if matches!(command.as_str(), "{" | "}" | "$" | "%" | "#" | "_" | "&") {
            return command;
        }
        if command == "|" {
            return "‖".to_string();
        }
        if command == "not" {
            let value = self.parse_required_argument(false).trim().to_string();
            if let Some(negated) = negated_symbol(&value) {
                return format!(" {negated} ");
            }
            let mut characters = value.chars();
            let Some(first) = characters.next() else {
                self.supported = false;
                return String::new();
            };
            let rest: String = characters.collect();
            return format!(" {first}\u{338}{rest} ");
        }
        if limit_operator(&command) {
            return self.parse_operator(&command, LowerStyle::Bracket, true, true);
        }
        if let Some(symbol) = symbol(&command) {
            if display_limit_symbol(&command) {
                return self.parse_operator(symbol, LowerStyle::Script, true, false);
            }
            return if command == "cdot" || command == "times" || relation_command(&command) {
                format!(" {symbol} ")
            } else {
                symbol.to_string()
            };
        }
        if named_operator(&command) {
            return format!("{NAMED_OPERATOR_START}{command}{NAMED_OPERATOR_END}");
        }
        if size_command(&command) {
            return String::new();
        }
        if matches!(command.as_str(), "left" | "middle" | "right") {
            if self.source.get(self.position) == Some(&'.') {
                self.position += 1;
            }
            return String::new();
        }
        if matches!(command.as_str(), "frac" | "dfrac" | "tfrac") {
            let should_stack = self.display && self.stack_fractions && command != "tfrac";
            let numerator = self.parse_required_argument(!should_stack);
            let denominator = self.parse_required_argument(!should_stack);
            if should_stack {
                let index = self.layout_nodes.len();
                self.layout_nodes.push(LayoutNode::Fraction {
                    numerator: normalize_output(&numerator),
                    denominator: normalize_output(&denominator),
                });
                return format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}");
            }
            return format_fraction(&numerator, &denominator);
        }
        if command == "sqrt" {
            let degree = self.parse_optional_argument().map(|d| d.trim().to_string());
            let value = self.parse_required_argument(true);
            return match degree.as_deref() {
                None | Some("2") => format_root(&value, "√"),
                Some("3") => format_root(&value, "∛"),
                Some("4") => format_root(&value, "∜"),
                Some(other) => format!(
                    "{}{}",
                    format_script(other, false),
                    format_root(&value, "√")
                ),
            };
        }
        if matches!(command.as_str(), "boxed" | "fbox") {
            return format!("[{}]", self.parse_required_argument(true).trim());
        }
        if matches!(command.as_str(), "binom" | "dbinom" | "tbinom") {
            return format!(
                "({} choose {})",
                self.parse_required_argument(true),
                self.parse_required_argument(true)
            );
        }
        if let Some(accent_mark) = accent(&command) {
            let value = self.parse_required_argument(true);
            return if value.chars().count() == 1 {
                format!("{value}{accent_mark}")
            } else {
                format!("{command}({value})")
            };
        }
        if command == "mathbb" {
            let value = self.parse_required_argument(true);
            return value
                .chars()
                .map(|c| blackboard(c).unwrap_or(c).to_string())
                .collect();
        }
        if command == "operatorname" {
            let starred = self.source.get(self.position) == Some(&'*');
            if starred {
                self.position += 1;
            }
            let operator = normalize_output(&self.parse_required_argument(true))
                .trim()
                .to_string();
            return self.parse_operator(&operator, LowerStyle::Bracket, starred, true);
        }
        if matches!(command.as_str(), "mod" | "bmod") {
            return " mod ".to_string();
        }
        if matches!(command.as_str(), "pmod" | "pod") {
            let value = self.parse_required_argument(true).trim().to_string();
            return if command == "pmod" {
                format!(" (mod {value})")
            } else {
                format!(" ({value})")
            };
        }
        if matches!(command.as_str(), "overset" | "stackrel") {
            let upper = self.parse_required_argument(true);
            let value = self.parse_required_argument(true).trim().to_string();
            return format!("{value}{}", format_script(&upper, false));
        }
        if command == "underset" {
            let lower = self.parse_required_argument(true);
            let value = self.parse_required_argument(true).trim().to_string();
            return format!("{value}{}", format_script(&lower, true));
        }
        if plain_wrapper(&command) {
            let value = self.parse_required_argument(true);
            return if command.starts_with("text") || command == "mbox" {
                value
            } else {
                value.trim().to_string()
            };
        }
        if command == "begin" {
            return self.parse_environment();
        }
        if command == "end" {
            self.supported = false;
            return String::new();
        }

        self.supported = false;
        format!("\\{command}")
    }

    /// Upstream `parseOperator` (latex.ts:1194-1250).
    fn parse_operator(
        &mut self,
        operator: &str,
        inline_lower_style: LowerStyle,
        display_limits: bool,
        spaced: bool,
    ) -> String {
        let mut use_display_limits = display_limits;
        let mut modifier_position = self.position;
        while modifier_position < self.source.len()
            && (self.source[modifier_position] == ' ' || self.source[modifier_position] == '\t')
        {
            modifier_position += 1;
        }
        // `/^\\(limits|nolimits)(?![A-Za-z])/`.
        for (name, value) in [("\\limits", true), ("\\nolimits", false)] {
            let name_chars: Vec<char> = name.chars().collect();
            if self.source.len() >= modifier_position + name_chars.len()
                && self.source[modifier_position..modifier_position + name_chars.len()]
                    == name_chars[..]
                && !self
                    .source
                    .get(modifier_position + name_chars.len())
                    .is_some_and(|c| c.is_ascii_alphabetic())
            {
                use_display_limits = value;
                self.position = modifier_position + name_chars.len();
                break;
            }
        }

        let mut lower: Option<String> = None;
        let mut upper: Option<String> = None;
        loop {
            let mut script_position = self.position;
            while script_position < self.source.len()
                && (self.source[script_position] == ' ' || self.source[script_position] == '\t')
            {
                script_position += 1;
            }
            let kind = match self.source.get(script_position) {
                Some(&kind) if kind == '_' || kind == '^' => kind,
                _ => break,
            };
            self.position = script_position + 1;
            let value = normalize_output(&self.parse_required_argument(false)).replace(' ', "");
            if kind == '_' {
                if lower.is_some() {
                    self.supported = false;
                }
                lower = Some(value);
            } else {
                if upper.is_some() {
                    self.supported = false;
                }
                upper = Some(value);
            }
        }

        if self.display && use_display_limits && (lower.is_some() || upper.is_some()) {
            let index = self.layout_nodes.len();
            self.layout_nodes.push(LayoutNode::Operator {
                operator: operator.to_string(),
                lower,
                upper,
            });
            return format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}");
        }

        let mut rendered = operator.to_string();
        if let Some(value) = &lower {
            rendered.push_str(&match inline_lower_style {
                LowerStyle::Bracket => format!("[{value}]"),
                LowerStyle::Script => format_script(value, true),
            });
        }
        if let Some(value) = &upper {
            rendered.push_str(&format_script(value, false));
        }
        if spaced {
            format!(" {rendered} ")
        } else {
            rendered
        }
    }

    /// Upstream `parseRequiredArgument` (latex.ts:1252-1258).
    fn parse_required_argument(&mut self, stack_fractions: bool) -> String {
        let previous = self.stack_fractions;
        self.stack_fractions = previous && stack_fractions;
        let value = self.parse_required_argument_value();
        self.stack_fractions = previous;
        value
    }

    /// Upstream `parseRequiredArgumentValue` (latex.ts:1260-1278).
    fn parse_required_argument_value(&mut self) -> String {
        while self.position < self.source.len() && self.source[self.position].is_whitespace() {
            self.position += 1;
        }
        if self.position >= self.source.len() {
            self.supported = false;
            return String::new();
        }
        let character = self.source[self.position];
        if character == '{' {
            self.position += 1;
            return self.parse_sequence(Some('}'));
        }
        if character == '\\' {
            return self.parse_command();
        }
        self.position += 1;
        character.to_string()
    }

    /// Upstream `parseOptionalArgument` (latex.ts:1280-1295).
    fn parse_optional_argument(&mut self) -> Option<String> {
        while self.position < self.source.len()
            && (self.source[self.position] == ' ' || self.source[self.position] == '\t')
        {
            self.position += 1;
        }
        if self.source.get(self.position) != Some(&'[') {
            return None;
        }
        let mut end: Option<usize> = None;
        let mut cursor = self.position + 1;
        while cursor < self.source.len() {
            if self.source[cursor] == ']' {
                end = Some(cursor);
                break;
            }
            cursor += 1;
        }
        let Some(end) = end else {
            self.supported = false;
            return None;
        };
        let value: String = self.source[self.position + 1..end].iter().collect();
        self.position = end + 1;
        Some(self.render_nested(&value, true))
    }

    /// Upstream `readRawGroup` (latex.ts:1297-1325).
    fn read_raw_group(&mut self) -> Option<String> {
        while self.position < self.source.len()
            && (self.source[self.position] == ' ' || self.source[self.position] == '\t')
        {
            self.position += 1;
        }
        if self.source.get(self.position) != Some(&'{') {
            self.supported = false;
            return None;
        }
        self.position += 1;
        let start = self.position;
        let mut depth = 1usize;
        while self.position < self.source.len() {
            let character = self.source[self.position];
            if character == '\\' {
                self.position += 2;
                continue;
            }
            if character == '{' {
                depth += 1;
            }
            if character == '}' {
                depth -= 1;
            }
            if depth == 0 {
                let value: String = self.source[start..self.position].iter().collect();
                self.position += 1;
                return Some(value);
            }
            self.position += 1;
        }
        self.supported = false;
        None
    }

    /// Upstream `parseEnvironment` (latex.ts:1331-1391).
    fn parse_environment(&mut self) -> String {
        let environment = match self.read_raw_group() {
            Some(value) => value,
            None => return String::new(),
        };
        if environment.is_empty() {
            // `\begin{}` — upstream `if (!environment) return "";` bails out
            // before touching `supported` (latex.ts:1332-1335).
            return String::new();
        }
        let end_marker: Vec<char> = format!("\\end{{{environment}}}").chars().collect();
        let mut end: Option<usize> = None;
        if end_marker.len() <= self.source.len() {
            let mut cursor = self.position;
            while cursor + end_marker.len() <= self.source.len() {
                if self.source[cursor..cursor + end_marker.len()] == end_marker[..] {
                    end = Some(cursor);
                    break;
                }
                cursor += 1;
            }
        }
        let Some(end) = end else {
            self.supported = false;
            return String::new();
        };
        let body: String = self.source[self.position..end].iter().collect();
        self.position = end + end_marker.len();

        if matches!(
            environment.as_str(),
            "equation" | "equation*" | "displaymath"
        ) {
            return self.render_nested(&body, true).trim().to_string();
        }

        if matches!(
            environment.as_str(),
            "aligned"
                | "align"
                | "align*"
                | "alignedat"
                | "alignat"
                | "alignat*"
                | "gather"
                | "gathered"
                | "multline"
                | "multline*"
                | "split"
        ) {
            let aligned_at = matches!(environment.as_str(), "alignedat" | "alignat" | "alignat*");
            let aligned_body = if aligned_at {
                strip_leading_group(&body)
            } else {
                body.clone()
            };
            let mut out: Vec<String> = Vec::new();
            for row in split_environment_rows(&aligned_body) {
                let cells: Vec<&str> = row.split('&').collect();
                let source = if aligned_at {
                    let mut pairs: Vec<String> = Vec::new();
                    let mut k = 0usize;
                    while k < cells.len() {
                        pairs.push(format!(
                            "{}{}",
                            cells[k],
                            cells.get(k + 1).copied().unwrap_or("")
                        ));
                        k += 2;
                    }
                    pairs.join(" ")
                } else {
                    cells.concat()
                };
                let rendered = self.render_nested(&source, true).trim().to_string();
                if !rendered.is_empty() {
                    out.push(rendered);
                }
            }
            return out.join("\n");
        }

        if matches!(environment.as_str(), "cases" | "cases*") {
            return self.render_cases(&body);
        }

        if matches!(
            environment.as_str(),
            "array"
                | "matrix"
                | "smallmatrix"
                | "pmatrix"
                | "bmatrix"
                | "Bmatrix"
                | "vmatrix"
                | "Vmatrix"
        ) {
            let matrix_body = if environment == "array" {
                strip_leading_group(&body)
            } else {
                body.clone()
            };
            return self.render_matrix(&environment, &matrix_body);
        }

        self.supported = false;
        body
    }

    /// Upstream `renderCases` (latex.ts:1393-1422).
    fn render_cases(&mut self, body: &str) -> String {
        let mut rows: Vec<Vec<String>> = Vec::new();
        for row in split_environment_rows(body) {
            let cells: Vec<String> = row
                .split('&')
                .map(|cell| self.render_nested(cell, false).trim().to_string())
                .collect();
            if cells.iter().any(|cell| !cell.is_empty()) {
                rows.push(cells);
            }
        }
        let value_width = rows
            .iter()
            .map(|row| {
                visible_width(&strip_trailing_comma_ws(
                    row.first().map(String::as_str).unwrap_or(""),
                ))
            })
            .max()
            .unwrap_or(0);
        let mut contents: Vec<String> = Vec::new();
        for row in &rows {
            let value = strip_trailing_comma_ws(row.first().map(String::as_str).unwrap_or(""));
            let condition = row.get(1).map(String::as_str).unwrap_or("");
            if condition.is_empty() {
                contents.push(value);
            } else {
                let condition_prefix = if starts_with_case_keyword(condition) {
                    " "
                } else {
                    " if "
                };
                contents.push(format!(
                    "{}{}{}{}",
                    value,
                    PROTECTED_SPACE
                        .to_string()
                        .repeat(value_width.saturating_sub(visible_width(&value))),
                    condition_prefix,
                    condition
                ));
            }
        }
        if contents.len() <= 1 {
            return contents
                .first()
                .map(|content| format!("⎧ {content}"))
                .unwrap_or_default();
        }
        let middle = contents.len() / 2;
        let mut visual_rows: Vec<Option<String>> = contents.iter().cloned().map(Some).collect();
        if contents.len().is_multiple_of(2) {
            visual_rows.insert(middle, None);
        }
        let lines: Vec<String> = visual_rows
            .iter()
            .enumerate()
            .map(|(index, content)| {
                let delimiter = if index == 0 {
                    '⎧'
                } else if index == visual_rows.len() - 1 {
                    '⎩'
                } else {
                    '⎨'
                };
                match content {
                    Some(value) => format!("{delimiter} {value}"),
                    None => delimiter.to_string(),
                }
            })
            .collect();
        let index = self.layout_nodes.len();
        self.layout_nodes.push(LayoutNode::Matrix {
            lines,
            baseline: middle,
        });
        format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}")
    }

    /// Upstream `renderMatrix` (latex.ts:1424-1467).
    fn render_matrix(&mut self, environment: &str, body: &str) -> String {
        let matrix: Vec<Vec<String>> = split_environment_rows(body)
            .iter()
            .map(|row| {
                row.split('&')
                    .map(|cell| self.render_nested(cell, false).trim().to_string())
                    .collect()
            })
            .filter(|row: &Vec<String>| row.iter().any(|cell| !cell.is_empty()))
            .collect();
        let column_count = matrix.iter().map(|row| row.len()).max().unwrap_or(0);
        let column_widths: Vec<usize> = (0..column_count)
            .map(|column| {
                matrix
                    .iter()
                    .map(|row| visible_width(row.get(column).map(String::as_str).unwrap_or("")))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let rows: Vec<String> = matrix
            .iter()
            .map(|row| {
                (0..column_count)
                    .map(|column| {
                        let cell = row.get(column).map(String::as_str).unwrap_or("");
                        format!(
                            "{cell}{}",
                            PROTECTED_SPACE
                                .to_string()
                                .repeat(column_widths[column].saturating_sub(visible_width(cell)))
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" │ ")
            })
            .collect();

        let lines: Vec<String> = match environment {
            "array" | "matrix" | "smallmatrix" => rows,
            _ => {
                let delimiters: Option<[&str; 6]> = match environment {
                    "pmatrix" => Some(["⎛", "⎞", "⎜", "⎟", "⎝", "⎠"]),
                    "bmatrix" => Some(["⎡", "⎤", "⎢", "⎥", "⎣", "⎦"]),
                    "Bmatrix" => Some(["⎧", "⎫", "⎨", "⎬", "⎩", "⎭"]),
                    "vmatrix" => Some(["│", "│", "│", "│", "│", "│"]),
                    "Vmatrix" => Some(["║", "║", "║", "║", "║", "║"]),
                    _ => None,
                };
                let Some(delimiters) = delimiters else {
                    self.supported = false;
                    return rows.join("\n");
                };
                rows.iter()
                    .enumerate()
                    .map(|(index, row)| {
                        let left = if index == 0 {
                            delimiters[0]
                        } else if index == rows.len() - 1 {
                            delimiters[4]
                        } else {
                            delimiters[2]
                        };
                        let right = if index == 0 {
                            delimiters[1]
                        } else if index == rows.len() - 1 {
                            delimiters[5]
                        } else {
                            delimiters[3]
                        };
                        format!("{left} {row} {right}")
                    })
                    .collect()
            }
        };

        if lines.len() <= 1 {
            return lines.first().cloned().unwrap_or_default();
        }
        let index = self.layout_nodes.len();
        self.layout_nodes
            .push(LayoutNode::Matrix { lines, baseline: 0 });
        format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}")
    }

    /// Upstream `renderNested` (latex.ts:1469-1476).
    fn render_nested(&mut self, source: &str, stack_fractions: bool) -> String {
        let display = self.display && stack_fractions;
        let mut nested = LatexParser::new(source, &mut *self.layout_nodes, display);
        match nested.render() {
            Some(rendered) => rendered,
            None => {
                self.supported = false;
                source.to_string()
            }
        }
    }
}

/// Upstream `splitEnvironmentRows` (latex.ts:1327-1329):
/// `body.split(/\\\\(?:\[[^\]\n]*\])?/)`.
fn split_environment_rows(body: &str) -> Vec<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] == '\\' && chars.get(index + 1) == Some(&'\\') {
            let mut next = index + 2;
            if chars.get(next) == Some(&'[') {
                let mut cursor = next + 1;
                while cursor < chars.len() && chars[cursor] != '\n' && chars[cursor] != ']' {
                    cursor += 1;
                }
                if chars.get(cursor) == Some(&']') {
                    next = cursor + 1;
                }
            }
            rows.push(std::mem::take(&mut current));
            index = next;
            continue;
        }
        current.push(chars[index]);
        index += 1;
    }
    rows.push(current);
    rows
}

/// Upstream `body.replace(/^\s*\{[^}]*\}/, "")` (latex.ts:1363, :1385).
fn strip_leading_group(body: &str) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mut index = 0usize;
    while index < chars.len() && chars[index].is_whitespace() {
        index += 1;
    }
    if chars.get(index) != Some(&'{') {
        return body.to_string();
    }
    let mut close = index + 1;
    while close < chars.len() && chars[close] != '}' {
        close += 1;
    }
    if close >= chars.len() {
        return body.to_string();
    }
    chars[close + 1..].iter().collect()
}

/// Upstream `value.replace(/,\s*$/, "")` (latex.ts:1397-1399).
fn strip_trailing_comma_ws(value: &str) -> String {
    let trimmed = value.trim_end();
    if let Some(stripped) = trimmed.strip_suffix(',') {
        return stripped.trim_end().to_string();
    }
    value.to_string()
}

/// Upstream `/^(?:if|when|for|otherwise)\b/i` (latex.ts:1404).
fn starts_with_case_keyword(condition: &str) -> bool {
    let lower = condition.to_ascii_lowercase();
    for keyword in ["if", "when", "for", "otherwise"] {
        if let Some(rest) = lower.strip_prefix(keyword) {
            let boundary = rest
                .chars()
                .next()
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
            if boundary {
                return true;
            }
        }
    }
    false
}

/// Upstream `trailing_layout_marker` regex usage (latex.ts:929-936):
/// the index of a `\u{f0000}digits\u{f0001}` marker ending `result`.
fn trailing_layout_marker(result: &str) -> Option<usize> {
    let chars: Vec<char> = result.chars().collect();
    let mut end = chars.len();
    if end == 0 || chars[end - 1] != LAYOUT_MARKER_END {
        return None;
    }
    end -= 1;
    let mut start = end;
    while start > 0 && chars[start - 1].is_ascii_digit() {
        start -= 1;
    }
    if start == end {
        return None;
    }
    if start == 0 || chars[start - 1] != LAYOUT_MARKER_START {
        return None;
    }
    chars[start..end].iter().collect::<String>().parse().ok()
}

/// Upstream `renderLatex` (latex.ts:1488-1506). Returns `None` when the
/// expression contains unsupported or malformed syntax.
pub(super) fn render_latex(source: &str, display: bool) -> Option<String> {
    let mut layout_nodes: Vec<LayoutNode> = Vec::new();
    let rendered = {
        let mut parser = LatexParser::new(source, &mut layout_nodes, display);
        parser.render()?
    };
    if layout_nodes.is_empty() {
        return Some(rendered.replace(PROTECTED_SPACE, " "));
    }
    let lines = render_layout(&rendered, &layout_nodes).lines;
    let indentation = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.chars().count() - line.trim_start().chars().count())
        .min()
        .unwrap_or(usize::MAX);
    let joined = lines
        .iter()
        .map(|line| {
            line.chars()
                .skip(indentation)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(joined.trim_end().replace(PROTECTED_SPACE, " "))
}
