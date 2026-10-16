//! Encoding declared rows as Parquet with Iceberg field IDs.

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::{
    ArrayRef, BinaryArray, BooleanArray, Date32Array, Float32Array, Float64Array, Int32Array,
    Int64Array, LargeBinaryArray, LargeStringArray, RecordBatch, StringArray,
};
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use iceberg::arrow::schema_to_arrow_schema;
use iceberg::spec::{Literal, Schema, Struct};
use parquet::arrow::{ArrowWriter, PARQUET_FIELD_ID_META_KEY};
use parquet::file::properties::WriterProperties;

use crate::error::{FixtureError, Result};
use crate::value::{Row, Value};

/// The reserved field ID of a position delete's `file_path` column.
pub(crate) const FILE_PATH_FIELD_ID: i32 = 2_147_483_546;
/// The reserved field ID of a position delete's `pos` column.
pub(crate) const POS_FIELD_ID: i32 = 2_147_483_545;

/// A batch of `rows` in `schema`'s top-level fields. A field a row omits is
/// null.
pub(crate) fn rows_batch(schema: &Schema, rows: &[Row]) -> Result<RecordBatch> {
    let arrow = Arc::new(schema_to_arrow_schema(schema)?);
    let mut columns = Vec::with_capacity(arrow.fields().len());
    for (field, arrow_field) in schema.as_struct().fields().iter().zip(arrow.fields()) {
        let values: Vec<&Value> = rows
            .iter()
            .map(|row| row.get(field.id).unwrap_or(&Value::Null))
            .collect();
        if field.required && values.iter().any(|value| value.is_null()) {
            return Err(FixtureError::Declaration(format!(
                "required field {} has a null value",
                field.id
            )));
        }
        columns.push(column(field.id, arrow_field.data_type(), &values)?);
    }
    Ok(RecordBatch::try_new(arrow, columns)?)
}

/// A position-delete batch, sorted by path and position as the spec
/// requires.
pub(crate) fn position_deletes_batch(mut deletes: Vec<(String, i64)>) -> Result<RecordBatch> {
    deletes.sort();
    let field = |name: &str, data_type, id: i32| {
        Field::new(name, data_type, false).with_metadata(HashMap::from([(
            PARQUET_FIELD_ID_META_KEY.to_owned(),
            id.to_string(),
        )]))
    };
    let schema = ArrowSchema::new(vec![
        field("file_path", DataType::Utf8, FILE_PATH_FIELD_ID),
        field("pos", DataType::Int64, POS_FIELD_ID),
    ]);
    let (paths, positions): (Vec<String>, Vec<i64>) = deletes.into_iter().unzip();
    Ok(RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(StringArray::from(paths)),
            Arc::new(Int64Array::from(positions)),
        ],
    )?)
}

/// `batch` as an uncompressed Parquet file, with row groups of at most
/// `row_group_rows` rows.
pub(crate) fn parquet_bytes(batch: &RecordBatch, row_group_rows: Option<usize>) -> Result<Vec<u8>> {
    if row_group_rows == Some(0) {
        return Err(FixtureError::Declaration(
            "row groups must hold at least one row".into(),
        ));
    }
    let mut properties = WriterProperties::builder();
    if row_group_rows.is_some() {
        properties = properties.set_max_row_group_row_count(row_group_rows);
    }
    let mut bytes = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut bytes, batch.schema(), Some(properties.build()))?;
    writer.write(batch)?;
    writer.close()?;
    Ok(bytes)
}

/// The partition tuple for `values`.
pub(crate) fn partition(values: &[Value]) -> Struct {
    values.iter().map(literal).collect()
}

fn literal(value: &Value) -> Option<Literal> {
    Some(match value {
        Value::Null => return None,
        Value::Boolean(v) => Literal::bool(*v),
        Value::Int(v) => Literal::int(*v),
        Value::Long(v) => Literal::long(*v),
        Value::Float(bits) => Literal::float(f32::from_bits(*bits)),
        Value::Double(bits) => Literal::double(f64::from_bits(*bits)),
        Value::Date(days) => Literal::date(*days),
        Value::String(v) => Literal::string(v.clone()),
        Value::Binary(v) => Literal::binary(v.clone()),
    })
}

fn column(field_id: i32, data_type: &DataType, values: &[&Value]) -> Result<ArrayRef> {
    let mismatch = |value: &Value| {
        FixtureError::Declaration(format!(
            "field {field_id} is {data_type}, but a row holds {value:?}"
        ))
    };
    macro_rules! collect {
        ($array:ty, $pattern:pat => $out:expr) => {{
            let mut out = Vec::with_capacity(values.len());
            for value in values {
                out.push(match value {
                    Value::Null => None,
                    $pattern => Some($out),
                    other => return Err(mismatch(other)),
                });
            }
            Arc::new(<$array>::from(out)) as ArrayRef
        }};
    }
    Ok(match data_type {
        DataType::Boolean => collect!(BooleanArray, Value::Boolean(v) => *v),
        DataType::Int32 => collect!(Int32Array, Value::Int(v) => *v),
        DataType::Int64 => collect!(Int64Array, Value::Long(v) => *v),
        DataType::Float32 => collect!(Float32Array, Value::Float(bits) => f32::from_bits(*bits)),
        DataType::Float64 => collect!(Float64Array, Value::Double(bits) => f64::from_bits(*bits)),
        DataType::Date32 => collect!(Date32Array, Value::Date(days) => *days),
        DataType::Utf8 => collect!(StringArray, Value::String(v) => v.as_str()),
        DataType::LargeUtf8 => collect!(LargeStringArray, Value::String(v) => v.as_str()),
        DataType::Binary => collect!(BinaryArray, Value::Binary(v) => v.as_slice()),
        DataType::LargeBinary => collect!(LargeBinaryArray, Value::Binary(v) => v.as_slice()),
        other => {
            return Err(FixtureError::Declaration(format!(
                "field {field_id} has unsupported type {other}"
            )));
        }
    })
}
