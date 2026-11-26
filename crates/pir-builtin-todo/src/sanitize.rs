//! Terminal-text sanitizer for model-controlled task text.

/// Remove terminal control characters from model-controlled task text before
/// it reaches a renderer: complete CSI/OSC sequences are dropped whole,
/// newlines/tabs become spaces, and bidi controls are removed. Single pass,
/// O(chars), no allocation beyond the output.
pub fn sanitize_terminal_text(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        match character {
            // CSI: ESC '[' or the C1 single-byte introducer.
            '\u{001b}' | '\u{009b}'
                if chars.get(index + 1) == Some(&'[') || character == '\u{009b}' =>
            {
                index += if character == '\u{001b}' { 2 } else { 1 };
                // Parameters: 0x30-0x3F; intermediates: 0x20-0x2F; final: 0x40-0x7E.
                while let Some(&current) = chars.get(index) {
                    index += 1;
                    if ('\u{40}'..='\u{7e}').contains(&current) {
                        break;
                    }
                }
            }
            // OSC: ESC ']' or the C1 introducer; an unterminated OSC swallows
            // the rest of the string, matching a real terminal.
            '\u{009d}' | '\u{001b}'
                if character == '\u{009d}' || chars.get(index + 1) == Some(&']') =>
            {
                index += if character == '\u{001b}' { 2 } else { 1 };
                while let Some(&current) = chars.get(index) {
                    index += 1;
                    if current == '\u{0007}' || current == '\u{009c}' {
                        break;
                    }
                    if current == '\u{001b}' {
                        // Optional String Terminator 'ESC \'.
                        if chars.get(index) == Some(&'\\') {
                            index += 1;
                        }
                        break;
                    }
                }
            }
            // Any remaining two-character ESC sequence.
            '\u{001b}' => {
                index += if chars.get(index + 1).is_some() { 2 } else { 1 };
            }
            // Unicode line/paragraph separators join lines like \n.
            '\u{2028}' | '\u{2029}' => {
                out.push(' ');
                index += 1;
            }
            // C0/C1 controls: newline, carriage return and tab become spaces.
            _ if character.is_control() && !matches!(character, '\n' | '\r' | '\t') => {
                index += 1;
            }
            // Bidi embedding/override/isolate controls and LRM/RLM marks.
            '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
                index += 1;
            }
            '\n' | '\r' | '\t' => {
                out.push(' ');
                index += 1;
            }
            _ => {
                out.push(character);
                index += 1;
            }
        }
    }
    out
}
