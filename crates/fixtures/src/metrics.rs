//! Column metrics computed from declared rows, as engines record them.

use std::cmp::Ordering;
use std::collections::HashMap;

use iceberg::spec::{Datum, Schema};

use crate::value::{Row, Value};

/// Per-column value, null, and NaN counts and lower and upper bounds.
///
/// Bounds are exact (never truncated) and exclude nulls and NaN. A column
/// with only null or NaN values has counts but no bounds.
#[derive(Debug, Default)]
pub(crate) struct ColumnMetrics {
    pub(crate) value_counts: HashMap<i32, u64>,
    pub(crate) null_value_counts: HashMap<i32, u64>,
    pub(crate) nan_value_counts: HashMap<i32, u64>,
    pub(crate) lower_bounds: HashMap<i32, Datum>,
    pub(crate) upper_bounds: HashMap<i32, Datum>,
}

impl ColumnMetrics {
    /// The metrics of `rows` for every top-level field of `schema`. A field
    /// a row omits counts as null.
    pub(crate) fn of(schema: &Schema, rows: &[Row]) -> Self {
        let mut metrics = Self::default();
        for field in schema.as_struct().fields() {
            let id = field.id;
            let (mut nulls, mut nans) = (0_u64, 0_u64);
            let (mut lower, mut upper): (Option<Datum>, Option<Datum>) = (None, None);
            for row in rows {
                let value = row.get(id).unwrap_or(&Value::Null);
                if is_nan(value) {
                    nans += 1;
                    continue;
                }
                let Some(datum) = datum(value) else {
                    nulls += 1;
                    continue;
                };
                if lower
                    .as_ref()
                    .is_none_or(|bound| datum.partial_cmp(bound) == Some(Ordering::Less))
                {
                    lower = Some(datum.clone());
                }
                if upper
                    .as_ref()
                    .is_none_or(|bound| datum.partial_cmp(bound) == Some(Ordering::Greater))
                {
                    upper = Some(datum);
                }
            }
            metrics.value_counts.insert(id, rows.len() as u64);
            metrics.null_value_counts.insert(id, nulls);
            if matches!(
                field.field_type.as_primitive_type(),
                Some(iceberg::spec::PrimitiveType::Float | iceberg::spec::PrimitiveType::Double)
            ) {
                metrics.nan_value_counts.insert(id, nans);
            }
            if let (Some(lower), Some(upper)) = (lower, upper) {
                metrics.lower_bounds.insert(id, lower);
                metrics.upper_bounds.insert(id, upper);
            }
        }
        metrics
    }
}

fn is_nan(value: &Value) -> bool {
    match value {
        Value::Float(bits) => f32::from_bits(*bits).is_nan(),
        Value::Double(bits) => f64::from_bits(*bits).is_nan(),
        _ => false,
    }
}

/// The datum of a value, typed by the value's own variant; `None` for null.
fn datum(value: &Value) -> Option<Datum> {
    Some(match value {
        Value::Null => return None,
        Value::Boolean(v) => Datum::bool(*v),
        Value::Int(v) => Datum::int(*v),
        Value::Long(v) => Datum::long(*v),
        Value::Float(bits) => Datum::float(f32::from_bits(*bits)),
        Value::Double(bits) => Datum::double(f64::from_bits(*bits)),
        Value::Date(days) => Datum::date(*days),
        Value::String(v) => Datum::string(v),
        Value::Binary(v) => Datum::binary(v.clone()),
    })
}

#[cfg(test)]
mod tests {
    use iceberg::spec::{NestedField, PrimitiveType, Type};

    use super::*;
    use crate::row;

    #[test]
    fn bounds_skip_nulls_and_nan_and_counts_include_them() {
        let schema = Schema::builder()
            .with_fields([
                NestedField::optional(1, "id", Type::Primitive(PrimitiveType::Long)).into(),
                NestedField::optional(2, "x", Type::Primitive(PrimitiveType::Double)).into(),
                NestedField::optional(3, "empty", Type::Primitive(PrimitiveType::String)).into(),
            ])
            .build()
            .unwrap();
        let rows = [
            row![1 => 7_i64, 2 => Value::double(f64::NAN)],
            row![1 => Value::Null, 2 => Value::double(-1.5)],
            row![1 => -3_i64, 2 => Value::double(2.0)],
        ];
        let metrics = ColumnMetrics::of(&schema, &rows);
        assert_eq!(metrics.value_counts[&1], 3);
        assert_eq!(metrics.null_value_counts[&1], 1);
        assert_eq!(metrics.lower_bounds[&1], Datum::long(-3));
        assert_eq!(metrics.upper_bounds[&1], Datum::long(7));
        assert_eq!(metrics.nan_value_counts[&2], 1);
        assert!(!metrics.nan_value_counts.contains_key(&1));
        assert_eq!(metrics.lower_bounds[&2], Datum::double(-1.5));
        assert_eq!(metrics.upper_bounds[&2], Datum::double(2.0));
        assert_eq!(metrics.null_value_counts[&3], 3);
        assert!(!metrics.lower_bounds.contains_key(&3));
    }
}
