//! Typed partition keys.
//!
//! A key is a spec ID and, per partition field, the value in Iceberg
//! single-value binary form. That form is what task artifacts carry, and
//! ordering keys by it gives every plan the same group order.

use std::collections::BTreeMap;

use iceberg::spec::{
    Literal, PartitionSpec, PrimitiveLiteral, PrimitiveType, SchemaRef, Struct, TableMetadata, Type,
};
use serde::{Deserialize, Serialize};

use crate::error::PlanError;

/// A partition tuple of one spec.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartitionKey {
    spec_id: i32,
    values: Vec<PartitionValue>,
}

/// One partition field's value.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartitionValue {
    field_id: i32,
    /// Iceberg single-value serialization, as lowercase hex. `None` is null.
    #[serde(with = "crate::hex")]
    value: Option<Vec<u8>>,
}

impl PartitionKey {
    /// The empty tuple of the unpartitioned spec 0, for tests.
    #[cfg(test)]
    pub(crate) const fn unpartitioned() -> Self {
        Self {
            spec_id: 0,
            values: Vec::new(),
        }
    }

    /// The partition spec ID.
    #[must_use]
    pub const fn spec_id(&self) -> i32 {
        self.spec_id
    }

    /// The values, in partition-field order.
    #[must_use]
    pub fn values(&self) -> &[PartitionValue] {
        &self.values
    }
}

impl PartitionValue {
    /// The partition field ID.
    #[must_use]
    pub const fn field_id(&self) -> i32 {
        self.field_id
    }

    /// The value in Iceberg single-value binary form; `None` is null.
    #[must_use]
    pub fn bytes(&self) -> Option<&[u8]> {
        self.value.as_deref()
    }
}

/// The partition specs of a table, with the type of every partition field.
pub(crate) struct Specs {
    specs: BTreeMap<i32, BoundSpec>,
}

pub(crate) struct BoundSpec {
    pub(crate) spec: PartitionSpec,
    /// Per partition field: its name and result type.
    pub(crate) fields: Vec<(String, PrimitiveType)>,
}

impl Specs {
    /// Binds every spec of `metadata`. A spec whose source columns were
    /// dropped from the current schema is bound against the newest schema
    /// that still has them.
    pub(crate) fn of(metadata: &TableMetadata) -> Result<Self, PlanError> {
        let mut schemas: Vec<&SchemaRef> = metadata.schemas_iter().collect();
        schemas.sort_by_key(|schema| std::cmp::Reverse(schema.schema_id()));
        let mut specs = BTreeMap::new();
        for spec in metadata.partition_specs_iter() {
            let partition_type = schemas
                .iter()
                .find_map(|schema| spec.partition_type(schema).ok())
                .ok_or(PlanError::UnboundSpec {
                    spec_id: spec.spec_id(),
                })?;
            let mut fields = Vec::with_capacity(spec.fields().len());
            for (field, nested) in spec.fields().iter().zip(partition_type.fields()) {
                let Type::Primitive(primitive) = nested.field_type.as_ref() else {
                    return Err(PlanError::UnboundSpec {
                        spec_id: spec.spec_id(),
                    });
                };
                fields.push((field.name.clone(), primitive.clone()));
            }
            specs.insert(
                spec.spec_id(),
                BoundSpec {
                    spec: spec.as_ref().clone(),
                    fields,
                },
            );
        }
        Ok(Self { specs })
    }

    pub(crate) fn get(&self, spec_id: i32) -> Result<&BoundSpec, PlanError> {
        self.specs
            .get(&spec_id)
            .ok_or(PlanError::UnknownSpec { spec_id })
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &BoundSpec> {
        self.specs.values()
    }

    /// The key of a file's partition tuple in `spec_id`.
    pub(crate) fn key(&self, spec_id: i32, partition: &Struct) -> Result<PartitionKey, PlanError> {
        let bound = self.get(spec_id)?;
        if partition.fields().len() != bound.fields.len() {
            return Err(PlanError::PartitionMismatch { spec_id });
        }
        let values = bound
            .spec
            .fields()
            .iter()
            .zip(&bound.fields)
            .zip(partition.iter())
            .map(|((field, (_, primitive)), literal)| {
                Ok(PartitionValue {
                    field_id: field.field_id,
                    value: literal
                        .map(|literal| single_value(literal, primitive))
                        .transpose()
                        .map_err(|()| PlanError::PartitionMismatch { spec_id })?,
                })
            })
            .collect::<Result<_, PlanError>>()?;
        Ok(PartitionKey { spec_id, values })
    }
}

impl BoundSpec {
    pub(crate) fn is_unpartitioned(&self) -> bool {
        self.spec.is_unpartitioned()
    }
}

/// The Iceberg single-value serialization of `literal` as a `primitive`.
/// `Err` when the literal cannot hold a value of that type.
pub(crate) fn single_value(literal: &Literal, primitive: &PrimitiveType) -> Result<Vec<u8>, ()> {
    use PrimitiveType as T;
    let Literal::Primitive(literal) = literal else {
        return Err(());
    };
    Ok(match (literal, primitive) {
        (PrimitiveLiteral::Boolean(v), T::Boolean) => vec![u8::from(*v)],
        (PrimitiveLiteral::Int(v), T::Int | T::Date) => v.to_le_bytes().to_vec(),
        (
            PrimitiveLiteral::Long(v),
            T::Long | T::Time | T::Timestamp | T::Timestamptz | T::TimestampNs | T::TimestamptzNs,
        ) => v.to_le_bytes().to_vec(),
        (PrimitiveLiteral::Float(v), T::Float) => v.to_le_bytes().to_vec(),
        (PrimitiveLiteral::Double(v), T::Double) => v.to_le_bytes().to_vec(),
        (PrimitiveLiteral::Int128(v), T::Decimal { .. }) => minimal_twos_complement(*v),
        (PrimitiveLiteral::String(v), T::String) => v.as_bytes().to_vec(),
        (PrimitiveLiteral::UInt128(v), T::Uuid) => v.to_be_bytes().to_vec(),
        (PrimitiveLiteral::Binary(v), T::Binary | T::Fixed(_)) => v.clone(),
        _ => return Err(()),
    })
}

/// Big-endian two's complement with the fewest bytes that keep the sign.
fn minimal_twos_complement(value: i128) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let mut start = 0;
    while start + 1 < bytes.len() {
        let (byte, next) = (bytes[start], bytes[start + 1]);
        let redundant = (byte == 0x00 && next & 0x80 == 0) || (byte == 0xff && next & 0x80 != 0);
        if !redundant {
            break;
        }
        start += 1;
    }
    bytes[start..].to_vec()
}

#[cfg(test)]
mod tests {
    use iceberg::spec::Datum;

    use super::*;

    fn round_trip(literal: &PrimitiveLiteral, primitive: &PrimitiveType) {
        let bytes = single_value(&Literal::Primitive(literal.clone()), primitive).unwrap();
        let datum = Datum::try_from_bytes(&bytes, primitive.clone()).unwrap();
        assert_eq!(datum.literal(), literal, "{primitive}");
    }

    #[test]
    fn single_values_round_trip_through_iceberg() {
        use PrimitiveType as T;
        let cases = [
            (PrimitiveLiteral::Boolean(true), T::Boolean),
            (PrimitiveLiteral::Int(-5), T::Int),
            (PrimitiveLiteral::Int(19_000), T::Date),
            (PrimitiveLiteral::Long(1 << 40), T::Long),
            (PrimitiveLiteral::Long(-1), T::Timestamptz),
            (PrimitiveLiteral::Float(1.5.into()), T::Float),
            (PrimitiveLiteral::Double((-0.25).into()), T::Double),
            (PrimitiveLiteral::String("eu".into()), T::String),
            (PrimitiveLiteral::UInt128(7), T::Uuid),
            (PrimitiveLiteral::Binary(vec![0, 255]), T::Binary),
            (PrimitiveLiteral::Binary(vec![1, 2]), T::Fixed(2)),
        ];
        for (literal, primitive) in &cases {
            round_trip(literal, primitive);
        }
        let decimal = T::Decimal {
            precision: 38,
            scale: 2,
        };
        for unscaled in [0, 1, -1, 127, 128, -128, -129, 1 << 70, -(1 << 70)] {
            round_trip(&PrimitiveLiteral::Int128(unscaled), &decimal);
        }
    }

    #[test]
    fn decimals_use_the_fewest_bytes() {
        assert_eq!(minimal_twos_complement(0), [0x00]);
        assert_eq!(minimal_twos_complement(127), [0x7f]);
        assert_eq!(minimal_twos_complement(128), [0x00, 0x80]);
        assert_eq!(minimal_twos_complement(-1), [0xff]);
        assert_eq!(minimal_twos_complement(-128), [0x80]);
        assert_eq!(minimal_twos_complement(-129), [0xff, 0x7f]);
    }

    #[test]
    fn mismatched_literals_are_refused() {
        let literal = Literal::Primitive(PrimitiveLiteral::String("x".into()));

        assert_eq!(single_value(&literal, &PrimitiveType::Int), Err(()));
    }
}
