//! Names: validation, NFC normalization and case folding (SPEC 4.9).

use unicode_normalization::UnicodeNormalization;

use crate::format::MAX_NAME_LEN;
use crate::DadaError;

/// NFC form of `name`, checked: 1 to 255 bytes, no `/` and no NUL.
/// `.` and `..` are accepted (lookups use them).
pub fn normalize(name: &str) -> Result<String, DadaError> {
    let nfc: String = name.nfc().collect();
    if nfc.len() > MAX_NAME_LEN {
        return Err(DadaError::NameTooLong);
    }
    if nfc.is_empty() || nfc.contains(['/', '\0']) {
        return Err(DadaError::InvalidName);
    }
    Ok(nfc)
}

/// Like `normalize`, for a new entry: `.` and `..` are reserved.
pub fn normalize_new(name: &str) -> Result<String, DadaError> {
    let nfc = normalize(name)?;
    if nfc == "." || nfc == ".." {
        return Err(DadaError::InvalidName);
    }
    Ok(nfc)
}

/// Comparison key of a name on a CASEFOLD volume: `nfc(default_case_fold(name))`.
pub fn fold(name: &str) -> String {
    caseless::default_case_fold_str(name).nfc().collect()
}

/// Matches stored names against one looked-up name.
pub struct NameMatcher {
    casefold: bool,
    key: String,
}

impl NameMatcher {
    /// `name` must already be normalized.
    pub fn new(name: &str, casefold: bool) -> Self {
        let key = if casefold {
            fold(name)
        } else {
            name.to_owned()
        };
        NameMatcher { casefold, key }
    }

    pub fn matches(&self, stored: &str) -> bool {
        if self.casefold {
            fold(stored) == self.key
        } else {
            stored == self.key
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfc_normalization() {
        // "é" decomposed (e + combining acute) becomes the single code point.
        assert_eq!(normalize("caf\u{65}\u{301}").unwrap(), "caf\u{e9}");
        assert_eq!(normalize("déjà").unwrap(), "déjà");
        assert_eq!(normalize("..").unwrap(), "..");
    }

    #[test]
    fn validation() {
        assert!(matches!(normalize(""), Err(DadaError::InvalidName)));
        assert!(matches!(normalize("a/b"), Err(DadaError::InvalidName)));
        assert!(matches!(normalize("a\0"), Err(DadaError::InvalidName)));
        assert!(matches!(
            normalize(&"x".repeat(256)),
            Err(DadaError::NameTooLong)
        ));
        assert!(normalize(&"x".repeat(255)).is_ok());
        // 128 decomposed "é" are 384 bytes, but 256 once composed: still too long.
        assert!(matches!(
            normalize(&"e\u{301}".repeat(128)),
            Err(DadaError::NameTooLong)
        ));
        assert!(normalize(&"e\u{301}".repeat(127)).is_ok());
        assert!(matches!(normalize_new("."), Err(DadaError::InvalidName)));
        assert!(matches!(normalize_new(".."), Err(DadaError::InvalidName)));
        assert!(normalize_new("...").is_ok());
    }

    #[test]
    fn matching() {
        let exact = NameMatcher::new("Readme.TXT", false);
        assert!(exact.matches("Readme.TXT"));
        assert!(!exact.matches("README.txt"));

        let folded = NameMatcher::new("README.txt", true);
        assert!(folded.matches("Readme.TXT"));
        assert!(folded.matches("readme.txt"));
        assert!(!folded.matches("readme.txt2"));
        // Full case folding: "ß" folds to "ss".
        assert!(NameMatcher::new("STRASSE", true).matches("straße"));
        // Accents are not folded away.
        assert!(!NameMatcher::new("cafe", true).matches("café"));
        assert!(NameMatcher::new("CAFÉ", true).matches("café"));
    }
}
