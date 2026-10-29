//! Free-text search over display names (BS-033.T4, ADR 0067).
//!
//! Lists accept a `q` parameter that matches the names people read: table
//! names, catalog names, and principal display names. It never matches table
//! rows. The text is bounded before it reaches a query and is turned into a
//! `LIKE` pattern whose wildcards are escaped, so a `%` or `_` a person types
//! matches itself.

/// The most characters a search accepts, after trimming.
pub const MAX_SEARCH_CHARS: usize = 128;

/// The escape character of every pattern [`SearchText::contains_pattern`]
/// returns. Queries pass it as `ESCAPE '\'`.
pub const LIKE_ESCAPE: char = '\\';

/// Search text that is trimmed, non-empty, at most [`MAX_SEARCH_CHARS`]
/// characters, and free of control characters.
///
/// It serializes as the text itself; it is never deserialized, so every value
/// passes through [`SearchText::parse`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct SearchText(String);

/// Why search text was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SearchTextError {
    /// Nothing is left after trimming.
    #[error("search text is empty")]
    Empty,
    /// More than [`MAX_SEARCH_CHARS`] characters remain after trimming.
    #[error("search text is longer than {MAX_SEARCH_CHARS} characters")]
    TooLong,
    /// The text contains a control character, such as a newline.
    #[error("search text contains a control character")]
    ControlCharacter,
}

impl SearchText {
    /// Validates `raw`, trimming surrounding whitespace.
    ///
    /// # Errors
    ///
    /// [`SearchTextError`] when the trimmed text is empty, too long, or
    /// contains a control character.
    pub fn parse(raw: &str) -> Result<Self, SearchTextError> {
        let text = raw.trim();
        if text.is_empty() {
            return Err(SearchTextError::Empty);
        }
        if text.chars().count() > MAX_SEARCH_CHARS {
            return Err(SearchTextError::TooLong);
        }
        if text.chars().any(char::is_control) {
            return Err(SearchTextError::ControlCharacter);
        }
        Ok(Self(text.to_owned()))
    }

    /// The validated text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A case-insensitive `ILIKE` pattern that matches names containing the
    /// text. `%`, `_`, and the escape character itself are escaped with
    /// [`LIKE_ESCAPE`].
    #[must_use]
    pub fn contains_pattern(&self) -> String {
        let mut pattern = String::with_capacity(self.0.len() + 2);
        pattern.push('%');
        for character in self.0.chars() {
            if matches!(character, '%' | '_' | LIKE_ESCAPE) {
                pattern.push(LIKE_ESCAPE);
            }
            pattern.push(character);
        }
        pattern.push('%');
        pattern
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_text_is_trimmed() {
        assert_eq!(SearchText::parse("  orders ").unwrap().as_str(), "orders");
    }

    #[test]
    fn empty_and_blank_text_is_refused() {
        assert_eq!(SearchText::parse(""), Err(SearchTextError::Empty));
        assert_eq!(SearchText::parse(" \t "), Err(SearchTextError::Empty));
    }

    #[test]
    fn the_limit_counts_characters_not_bytes() {
        let longest = "é".repeat(MAX_SEARCH_CHARS);
        assert!(SearchText::parse(&longest).is_ok());
        assert_eq!(
            SearchText::parse(&format!("{longest}é")),
            Err(SearchTextError::TooLong)
        );
    }

    #[test]
    fn control_characters_are_refused() {
        assert_eq!(
            SearchText::parse("a\nb"),
            Err(SearchTextError::ControlCharacter)
        );
        assert_eq!(
            SearchText::parse("a\u{0}b"),
            Err(SearchTextError::ControlCharacter)
        );
    }

    #[test]
    fn wildcards_match_themselves() {
        let text = SearchText::parse(r"50%_off\x").unwrap();
        assert_eq!(text.contains_pattern(), r"%50\%\_off\\x%");
    }

    #[test]
    fn plain_text_is_wrapped_for_containment() {
        let text = SearchText::parse("db.events").unwrap();
        assert_eq!(text.contains_pattern(), "%db.events%");
    }
}
