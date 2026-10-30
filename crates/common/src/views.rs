//! Saved list views (BS-033.T6, ADR 0067).
//!
//! A person can name the filters, search, order, and hidden columns of a list
//! and reopen them later. A view stores those as the list's own URL search
//! parameters, so it holds nothing a link could not. Each list accepts only
//! its known parameter names, every value is bounded text, and a view is at
//! most [`MAX_VIEW_PARAMS_BYTES`] of canonical JSON. The page re-validates
//! the values when it opens a view, exactly as it does for a link.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// The most bytes a view's parameters take as canonical JSON.
pub const MAX_VIEW_PARAMS_BYTES: usize = 4096;

/// The most views one person keeps in one organization.
pub const MAX_VIEWS_PER_PRINCIPAL: i64 = 50;

/// The most characters in a view's name, after trimming.
pub const MAX_VIEW_NAME_CHARS: usize = 100;

/// The most characters in one parameter value.
pub const MAX_VIEW_VALUE_CHARS: usize = 512;

/// The list a view belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ViewPage {
    /// The jobs list.
    Jobs,
    /// The tables list.
    Tables,
    /// The catalogs list.
    Catalogs,
    /// The audit log.
    Audit,
    /// The principals list.
    Principals,
}

impl ViewPage {
    /// Every list that can keep views.
    pub const ALL: &'static [Self] = &[
        Self::Jobs,
        Self::Tables,
        Self::Catalogs,
        Self::Audit,
        Self::Principals,
    ];

    /// The stable name used in the API and in PostgreSQL.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Jobs => "jobs",
            Self::Tables => "tables",
            Self::Catalogs => "catalogs",
            Self::Audit => "audit",
            Self::Principals => "principals",
        }
    }

    /// The search parameters this list understands. `hidden` names the
    /// columns the view hides.
    #[must_use]
    pub const fn parameters(self) -> &'static [&'static str] {
        match self {
            Self::Jobs => &["q", "state", "operation", "order", "hidden"],
            Self::Tables => &["q", "catalog", "health", "sort", "order", "hidden"],
            Self::Catalogs => &["q", "enabled", "hidden"],
            Self::Audit => &[
                "q",
                "outcome",
                "action",
                "resource_type",
                "actor",
                "order",
                "hidden",
            ],
            Self::Principals => &["q", "kind", "active", "hidden"],
        }
    }
}

impl fmt::Display for ViewPage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The name is not a list that keeps views.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown view page")]
pub struct UnknownViewPage;

impl FromStr for ViewPage {
    type Err = UnknownViewPage;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|page| page.as_str() == value)
            .ok_or(UnknownViewPage)
    }
}

/// Why a view was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ViewError {
    /// The name is empty after trimming, too long, or has control characters.
    #[error("view name must be 1 to 100 characters without control characters")]
    InvalidName,
    /// The list does not understand this parameter.
    #[error("parameter {0} is not one this list understands")]
    UnknownParameter(String),
    /// A value is empty, too long, or has control characters.
    #[error("parameter {0} has an invalid value")]
    InvalidValue(String),
    /// The parameters exceed [`MAX_VIEW_PARAMS_BYTES`].
    #[error("view parameters exceed 4096 bytes")]
    TooLarge,
}

fn clean(value: &str) -> bool {
    !value.chars().any(char::is_control)
}

/// The length of `text` as a JSON string, quotes included. Control
/// characters are refused before this runs, so only `"` and `\` escape.
fn quoted_len(text: &str) -> usize {
    2 + text
        .chars()
        .map(|c| {
            if matches!(c, '"' | '\\') {
                2
            } else {
                c.len_utf8()
            }
        })
        .sum::<usize>()
}

/// The length of `params` as compact JSON, as `serde_json` writes it.
fn json_len(params: &BTreeMap<String, String>) -> usize {
    let entries: usize = params
        .iter()
        .map(|(name, value)| quoted_len(name) + 1 + quoted_len(value))
        .sum();
    2 + entries + params.len().saturating_sub(1)
}

/// A view's name: trimmed, 1 to [`MAX_VIEW_NAME_CHARS`] characters, and free
/// of control characters.
///
/// # Errors
///
/// [`ViewError::InvalidName`] for anything else.
pub fn view_name(value: &str) -> Result<String, ViewError> {
    let trimmed = value.trim();
    let length = trimmed.chars().count();
    if length == 0 || length > MAX_VIEW_NAME_CHARS || !clean(trimmed) {
        return Err(ViewError::InvalidName);
    }
    Ok(trimmed.to_owned())
}

/// A view's parameters, in canonical (sorted) order, checked against its
/// list.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct ViewParams(BTreeMap<String, String>);

impl ViewParams {
    /// Checks `params` against `page`'s parameter names and the size limits.
    ///
    /// # Errors
    ///
    /// [`ViewError::UnknownParameter`] for a name the list does not take,
    /// [`ViewError::InvalidValue`] for an empty, over-long, or control-bearing
    /// value, and [`ViewError::TooLarge`] past [`MAX_VIEW_PARAMS_BYTES`].
    pub fn parse(page: ViewPage, params: BTreeMap<String, String>) -> Result<Self, ViewError> {
        for (name, value) in &params {
            if !page.parameters().contains(&name.as_str()) {
                return Err(ViewError::UnknownParameter(name.clone()));
            }
            let length = value.chars().count();
            if length == 0 || length > MAX_VIEW_VALUE_CHARS || !clean(value) {
                return Err(ViewError::InvalidValue(name.clone()));
            }
        }
        let bytes = json_len(&params);
        if bytes > MAX_VIEW_PARAMS_BYTES {
            return Err(ViewError::TooLarge);
        }
        Ok(Self(params))
    }

    /// The parameters by name.
    #[must_use]
    pub const fn as_map(&self) -> &BTreeMap<String, String> {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn page_names_round_trip() {
        for page in ViewPage::ALL {
            assert_eq!(page.as_str().parse::<ViewPage>(), Ok(*page));
        }
        assert_eq!("Jobs".parse::<ViewPage>(), Err(UnknownViewPage));
    }

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(view_name("  Failed today ").unwrap(), "Failed today");
        assert_eq!(view_name(" "), Err(ViewError::InvalidName));
        assert_eq!(view_name("a\u{7}b"), Err(ViewError::InvalidName));
        assert!(view_name(&"é".repeat(100)).is_ok());
        assert_eq!(view_name(&"é".repeat(101)), Err(ViewError::InvalidName));
    }

    #[test]
    fn each_list_accepts_only_its_own_parameters() {
        let jobs = ViewParams::parse(
            ViewPage::Jobs,
            params(&[("state", "Failed,Running"), ("order", "oldest")]),
        )
        .unwrap();
        assert_eq!(jobs.as_map().len(), 2);
        assert_eq!(
            ViewPage::parse_err(ViewPage::Jobs, &[("health", "critical")]),
            ViewError::UnknownParameter("health".to_owned())
        );
        assert!(ViewParams::parse(ViewPage::Tables, params(&[("health", "critical")])).is_ok());
    }

    #[test]
    fn values_and_size_are_bounded() {
        assert_eq!(
            ViewPage::parse_err(ViewPage::Jobs, &[("q", "")]),
            ViewError::InvalidValue("q".to_owned())
        );
        assert_eq!(
            ViewPage::parse_err(ViewPage::Jobs, &[("q", "a\nb")]),
            ViewError::InvalidValue("q".to_owned())
        );
        let long = "x".repeat(MAX_VIEW_VALUE_CHARS + 1);
        assert_eq!(
            ViewPage::parse_err(ViewPage::Jobs, &[("q", &long)]),
            ViewError::InvalidValue("q".to_owned())
        );
        // Quotes escape to two bytes, so these values exceed the byte limit.
        let full = "\"".repeat(MAX_VIEW_VALUE_CHARS);
        let entries: Vec<(&str, &str)> = ViewPage::Audit
            .parameters()
            .iter()
            .map(|name| (*name, full.as_str()))
            .collect();
        assert_eq!(
            ViewPage::parse_err(ViewPage::Audit, &entries),
            ViewError::TooLarge
        );
    }

    #[test]
    fn measures_json_as_serde_writes_it() {
        let entries = params(&[("q", "say \"hi\" \\ é"), ("order", "oldest")]);
        assert_eq!(
            json_len(&entries),
            serde_json::to_vec(&entries).unwrap().len()
        );
        assert_eq!(json_len(&BTreeMap::new()), 2);
    }

    impl ViewPage {
        fn parse_err(page: Self, entries: &[(&str, &str)]) -> ViewError {
            ViewParams::parse(page, params(entries)).unwrap_err()
        }
    }
}
