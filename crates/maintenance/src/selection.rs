//! Partition predicates that select whole files.
//!
//! A selection never becomes a row filter: a file is either selected with
//! every row or not at all.

use std::collections::BTreeMap;

use iceberg::spec::{Literal, Type};
use serde::{Deserialize, Serialize};

use crate::error::PlanError;
use crate::partition::{PartitionKey, Specs, single_value};

/// Files whose partition matches every listed value.
///
/// Values are Iceberg JSON single values (`"eu"`, `19000`, `null`) of the
/// partition field's result type, so a `day` field takes a date string and
/// a `bucket` field an integer. A predicate scoped to a spec ID matches only
/// files of that spec.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartitionPredicate {
    spec_id: Option<i32>,
    values: BTreeMap<String, serde_json::Value>,
}

impl PartitionPredicate {
    /// A predicate that matches every file.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Restricts the predicate to files of `spec_id`.
    #[must_use]
    pub const fn in_spec(mut self, spec_id: i32) -> Self {
        self.spec_id = Some(spec_id);
        self
    }

    /// Requires the partition field `name` to equal `value`.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: serde_json::Value) -> Self {
        self.values.insert(name.into(), value);
        self
    }
}

/// The (partition field ID, value) pairs one bound predicate requires.
type Required = Vec<(i32, Option<Vec<u8>>)>;

/// A selection bound to a table's specs: per spec, the alternatives.
pub(crate) struct Selection {
    /// `None` selects every file.
    alternatives: Option<BTreeMap<i32, Vec<Required>>>,
}

impl Selection {
    /// Binds `predicates`. An empty list selects every file.
    pub(crate) fn bind(
        predicates: &[PartitionPredicate],
        specs: &Specs,
    ) -> Result<Self, PlanError> {
        if predicates.is_empty() {
            return Ok(Self { alternatives: None });
        }
        let mut alternatives: BTreeMap<i32, Vec<_>> = BTreeMap::new();
        for predicate in predicates {
            let mut bound_anywhere = false;
            for bound in specs.iter() {
                let spec_id = bound.spec.spec_id();
                if predicate.spec_id.is_some_and(|id| id != spec_id) {
                    continue;
                }
                let mut required = Vec::with_capacity(predicate.values.len());
                for (name, json) in &predicate.values {
                    let Some(position) = bound.fields.iter().position(|(field, _)| field == name)
                    else {
                        break;
                    };
                    let primitive = &bound.fields[position].1;
                    let invalid = || {
                        PlanError::InvalidSelection(format!(
                            "{json} is not a {primitive} value for partition field {name}"
                        ))
                    };
                    let literal =
                        Literal::try_from_json(json.clone(), &Type::Primitive(primitive.clone()))
                            .map_err(|_| invalid())?;
                    let value = literal
                        .map(|literal| single_value(&literal, primitive).map_err(|()| invalid()))
                        .transpose()?;
                    required.push((bound.spec.fields()[position].field_id, value));
                }
                if required.len() == predicate.values.len() {
                    bound_anywhere = true;
                    alternatives.entry(spec_id).or_default().push(required);
                }
            }
            if !bound_anywhere {
                return Err(PlanError::InvalidSelection(match predicate.spec_id {
                    Some(id) if specs.get(id).is_err() => format!("spec {id} does not exist"),
                    _ => format!(
                        "no partition spec has the fields {:?}",
                        predicate.values.keys().collect::<Vec<_>>()
                    ),
                }));
            }
        }
        Ok(Self {
            alternatives: Some(alternatives),
        })
    }

    pub(crate) fn matches(&self, key: &PartitionKey) -> bool {
        let Some(alternatives) = &self.alternatives else {
            return true;
        };
        alternatives
            .get(&key.spec_id())
            .is_some_and(|alternatives| {
                alternatives.iter().any(|required| {
                    required.iter().all(|(field_id, value)| {
                        key.values()
                            .iter()
                            .any(|v| v.field_id() == *field_id && v.bytes() == value.as_deref())
                    })
                })
            })
    }
}
