//! Names as Windows sees them.
//!
//! Characters Windows forbids in names (`\ : * ? " < > |` and 0x01-0x1F) are
//! shown as the private-use character U+F000 + code, the convention used by
//! Cygwin, and mapped back when Windows hands a name to dada. Reserved device
//! names (CON, NUL, COM1...) cannot be created from Windows; existing ones
//! are shown with their first character escaped the same way.

/// Start of the private-use range used for escaping.
const ESCAPE_BASE: u32 = 0xF000;

const FORBIDDEN: &[char] = &['\\', ':', '*', '?', '"', '<', '>', '|'];

const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn is_forbidden(c: char) -> bool {
    FORBIDDEN.contains(&c) || ('\u{1}'..='\u{1f}').contains(&c)
}

fn escape_char(c: char) -> char {
    char::from_u32(ESCAPE_BASE + c as u32).unwrap_or(c)
}

/// Whether `name` is a reserved device name, with or without extension
/// (`CON`, `con.txt`, `NUL ` ...).
pub fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem))
}

/// A dada name as shown to Windows.
pub fn to_windows(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if is_forbidden(c) { escape_char(c) } else { c })
        .collect();
    if is_reserved(name) {
        if let Some(first) = out.chars().next() {
            out.replace_range(..first.len_utf8(), &escape_char(first).to_string());
        }
    }
    out
}

/// A name received from Windows, as stored on dada.
pub fn from_windows(name: &str) -> String {
    name.chars()
        .enumerate()
        .map(|(i, c)| {
            let original = (c as u32)
                .checked_sub(ESCAPE_BASE)
                .filter(|&code| code < 0x80)
                .and_then(char::from_u32);
            match original {
                Some(o) if is_forbidden(o) => o,
                // The first letter of an escaped reserved name.
                Some(o) if i == 0 && o.is_ascii_alphabetic() => o,
                _ => c,
            }
        })
        .collect()
}

/// Components of a Windows path (`\dir\file`), mapped to dada names.
pub fn components(path: &str) -> Vec<String> {
    path.split('\\')
        .filter(|c| !c.is_empty())
        .map(from_windows)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_characters_round_trip() {
        let name = "a:b*c?\"d<e>f|g\u{1}h";
        let shown = to_windows(name);
        assert!(!shown.chars().any(is_forbidden), "{shown:?}");
        assert_eq!(shown.chars().nth(1), Some('\u{f03a}'));
        assert_eq!(from_windows(&shown), name);
        assert_eq!(to_windows("plain-name.txt"), "plain-name.txt");
        assert_eq!(to_windows("été"), "été");
    }

    #[test]
    fn reserved_names() {
        for name in [
            "CON",
            "con",
            "Nul.txt",
            "COM1",
            "lpt9.tar.gz",
            "AUX ",
            "prn",
        ] {
            assert!(is_reserved(name), "{name}");
        }
        for name in ["CONSOLE", "COM10", "LPT0", "nul_", "x.con", ""] {
            assert!(!is_reserved(name), "{name}");
        }
        // Shown with the first character escaped, mapped back unchanged.
        let shown = to_windows("con.txt");
        assert_eq!(shown, "\u{f063}on.txt");
        assert!(!is_reserved(&shown));
        assert_eq!(from_windows(&shown), "con.txt");
    }

    #[test]
    fn paths() {
        assert_eq!(components("\\"), Vec::<String>::new());
        assert_eq!(components("\\dir\\a\u{f03a}b"), ["dir", "a:b"]);
    }
}
