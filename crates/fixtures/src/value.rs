//! Rows and row multisets, keyed by Iceberg field ID.

use std::collections::BTreeMap;
use std::fmt;

/// One column value.
///
/// Floats are held as their bit patterns, so every value, NaN included, is
/// equal to itself and rows can be ordered and counted.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Value {
    /// SQL null.
    Null,
    /// `boolean`.
    Boolean(bool),
    /// `int`.
    Int(i32),
    /// `long`.
    Long(i64),
    /// `float`, as `f32::to_bits`.
    Float(u32),
    /// `double`, as `f64::to_bits`.
    Double(u64),
    /// `date`, in days since the Unix epoch.
    Date(i32),
    /// `string`.
    String(String),
    /// `binary`.
    Binary(Vec<u8>),
}

impl Value {
    /// A `float` value.
    #[must_use]
    pub fn float(value: f32) -> Self {
        Self::Float(value.to_bits())
    }

    /// A `double` value.
    #[must_use]
    pub fn double(value: f64) -> Self {
        Self::Double(value.to_bits())
    }

    /// A `string` value.
    #[must_use]
    pub fn string(value: impl Into<String>) -> Self {
        Self::String(value.into())
    }

    /// Whether this is [`Value::Null`].
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl From<i32> for Value {
    fn from(value: i32) -> Self {
        Self::Int(value)
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Long(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::string(value)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Into::into)
    }
}

/// A row: one value per field ID.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Row(BTreeMap<i32, Value>);

impl Row {
    /// A row from `(field ID, value)` pairs. A later pair for the same field
    /// replaces an earlier one.
    #[must_use]
    pub fn new(values: impl IntoIterator<Item = (i32, Value)>) -> Self {
        Self(values.into_iter().collect())
    }

    /// The value of `field_id`, or `None` if the row has no such field.
    #[must_use]
    pub fn get(&self, field_id: i32) -> Option<&Value> {
        self.0.get(&field_id)
    }

    /// The row's `(field ID, value)` pairs, in field ID order.
    pub fn values(&self) -> impl Iterator<Item = (i32, &Value)> {
        self.0.iter().map(|(id, value)| (*id, value))
    }
}

/// `Row::new` from a literal list: `row![1 => 7_i64, 2 => "a"]`.
#[macro_export]
macro_rules! row {
    ($($id:expr => $value:expr),* $(,)?) => {
        $crate::Row::new([$(($id, $crate::Value::from($value))),*])
    };
}

/// A multiset of rows: each distinct row with its number of occurrences.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RowMultiset(BTreeMap<Row, u64>);

impl RowMultiset {
    /// An empty multiset.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one occurrence of `row`.
    pub fn insert(&mut self, row: Row) {
        *self.0.entry(row).or_default() += 1;
    }

    /// The total number of rows, counting duplicates.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.0.values().sum()
    }

    /// Whether the multiset holds no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many times `row` occurs.
    #[must_use]
    pub fn count(&self, row: &Row) -> u64 {
        self.0.get(row).copied().unwrap_or(0)
    }

    /// The rows `self` has more often than `other`, with the excess count.
    #[must_use]
    pub fn excess_over(&self, other: &Self) -> Vec<(Row, u64)> {
        self.0
            .iter()
            .filter_map(|(row, &count)| {
                let theirs = other.count(row);
                (count > theirs).then(|| (row.clone(), count - theirs))
            })
            .collect()
    }
}

impl FromIterator<Row> for RowMultiset {
    fn from_iter<I: IntoIterator<Item = Row>>(rows: I) -> Self {
        let mut multiset = Self::new();
        for row in rows {
            multiset.insert(row);
        }
        multiset
    }
}

impl Extend<Row> for RowMultiset {
    fn extend<I: IntoIterator<Item = Row>>(&mut self, rows: I) {
        for row in rows {
            self.insert(row);
        }
    }
}

/// Shows the size and at most the first ten distinct rows, so that a failed
/// comparison of large multisets stays readable.
impl fmt::Debug for RowMultiset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const SHOWN: usize = 10;
        write!(f, "RowMultiset({} rows", self.len())?;
        for (row, count) in self.0.iter().take(SHOWN) {
            write!(f, ", {count}x {row:?}")?;
        }
        if self.0.len() > SHOWN {
            write!(f, ", ...")?;
        }
        write!(f, ")")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nan_values_are_equal_to_themselves() {
        let mut rows = RowMultiset::new();
        rows.insert(row![1 => Value::double(f64::NAN)]);
        rows.insert(row![1 => Value::double(f64::NAN)]);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows.count(&row![1 => Value::double(f64::NAN)]), 2);
    }

    #[test]
    fn duplicates_are_counted() {
        let rows: RowMultiset = [row![1 => 1_i64], row![1 => 1_i64], row![1 => 2_i64]]
            .into_iter()
            .collect();

        assert_eq!(rows.len(), 3);
        assert_eq!(rows.count(&row![1 => 1_i64]), 2);
        assert!(!rows.is_empty());
    }

    #[test]
    fn excess_reports_missing_duplicates() {
        let left: RowMultiset = [row![1 => 1_i64], row![1 => 1_i64]].into_iter().collect();
        let right: RowMultiset = [row![1 => 1_i64]].into_iter().collect();

        assert_eq!(left.excess_over(&right), [(row![1 => 1_i64], 1)]);
        assert_eq!(right.excess_over(&left), []);
    }

    #[test]
    fn options_become_nulls() {
        assert_eq!(Value::from(None::<i64>), Value::Null);
        assert_eq!(Value::from(Some("a")), Value::string("a"));
        assert!(Value::Null.is_null());
    }

    #[test]
    fn debug_output_is_truncated() {
        let rows: RowMultiset = (0..20_i64).map(|id| row![1 => id]).collect();

        let shown = format!("{rows:?}");

        assert!(shown.starts_with("RowMultiset(20 rows"), "{shown}");
        assert!(shown.ends_with(", ...)"), "{shown}");
    }
}
