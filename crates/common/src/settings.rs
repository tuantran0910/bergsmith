//! Operating bounds one catalog connection declares (BS-022.T3).
//!
//! `docs/operations/catalog-connections.md` asks every connection to define two
//! bounds: the maximum execution duration (E) of one job, and the maximum
//! unresolved-publication age (U). They exist because Bergsmith cannot make an
//! external cleaner respect its own reservations. The operator states how long
//! inputs and outputs must stay readable, and Bergsmith keeps its own work
//! inside that budget.
//!
//! The bounds belong to a connection rather than to the deployment: the writer
//! cadence, the catalog, and the cleaners are properties of one catalog, and
//! two connections may serve tables with very different cadences.

/// The operating bounds one catalog connection declares.
///
/// The fields are private: a value that was not validated by
/// [`ConnectionSettings::new`] must not reach the ledger, so the only way to
/// build one is through the constructor or [`Default`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionSettings {
    max_execution_seconds: u64,
    max_unresolved_publication_seconds: u64,
    maintenance_enabled: bool,
}

impl ConnectionSettings {
    /// The shortest accepted execution budget: one second.
    pub const MIN_EXECUTION_SECONDS: u64 = 1;
    /// The longest accepted execution budget: one day.
    pub const MAX_EXECUTION_SECONDS: u64 = 24 * 60 * 60;
    /// The shortest accepted unresolved-publication age: one second.
    pub const MIN_UNRESOLVED_SECONDS: u64 = 1;
    /// The longest accepted unresolved-publication age: thirty days.
    pub const MAX_UNRESOLVED_SECONDS: u64 = 30 * 24 * 60 * 60;
    /// The default execution budget: three minutes, which fits a five-minute
    /// writer cadence.
    pub const DEFAULT_EXECUTION_SECONDS: u64 = 3 * 60;
    /// The default unresolved-publication age: one hour.
    pub const DEFAULT_UNRESOLVED_SECONDS: u64 = 60 * 60;

    /// How long one job may run on this connection, in seconds.
    ///
    /// A job is given up when it outruns the budget. The bound is also what
    /// the operator keeps the table's source files and manifests readable for,
    /// so a job never fails because its input was cleaned away.
    #[must_use]
    pub const fn max_execution_seconds(&self) -> u64 {
        self.max_execution_seconds
    }

    /// How long a publication on this connection may stay unresolved before an
    /// operator investigates, in seconds.
    ///
    /// A publication is unresolved while its commit outcome is unknown. The
    /// bound is the age at which the coordinator reports it as overdue; it
    /// never resolves a publication by itself.
    #[must_use]
    pub const fn max_unresolved_publication_seconds(&self) -> u64 {
        self.max_unresolved_publication_seconds
    }

    /// Whether maintenance may run on this connection's tables.
    ///
    /// A paused connection stays in service: previews still work and the
    /// connection is still readable. Only new maintenance work stops, and
    /// recovery of an unresolved publication continues.
    #[must_use]
    pub const fn maintenance_enabled(&self) -> bool {
        self.maintenance_enabled
    }

    /// Validates and builds the settings of one connection.
    ///
    /// # Errors
    ///
    /// [`SettingsError::ExecutionBudget`] when the execution budget is outside
    /// its supported range, and [`SettingsError::UnresolvedAge`] when the
    /// unresolved-publication age is outside its supported range.
    pub const fn new(
        max_execution_seconds: u64,
        max_unresolved_publication_seconds: u64,
        maintenance_enabled: bool,
    ) -> Result<Self, SettingsError> {
        if max_execution_seconds < Self::MIN_EXECUTION_SECONDS
            || max_execution_seconds > Self::MAX_EXECUTION_SECONDS
        {
            return Err(SettingsError::ExecutionBudget {
                seconds: max_execution_seconds,
                min: Self::MIN_EXECUTION_SECONDS,
                max: Self::MAX_EXECUTION_SECONDS,
            });
        }
        if max_unresolved_publication_seconds < Self::MIN_UNRESOLVED_SECONDS
            || max_unresolved_publication_seconds > Self::MAX_UNRESOLVED_SECONDS
        {
            return Err(SettingsError::UnresolvedAge {
                seconds: max_unresolved_publication_seconds,
                min: Self::MIN_UNRESOLVED_SECONDS,
                max: Self::MAX_UNRESOLVED_SECONDS,
            });
        }
        Ok(Self {
            max_execution_seconds,
            max_unresolved_publication_seconds,
            maintenance_enabled,
        })
    }
}

impl Default for ConnectionSettings {
    /// The bounds a connection starts with: a three-minute job budget, a
    /// one-hour unresolved-publication age, and maintenance allowed.
    fn default() -> Self {
        Self {
            max_execution_seconds: Self::DEFAULT_EXECUTION_SECONDS,
            max_unresolved_publication_seconds: Self::DEFAULT_UNRESOLVED_SECONDS,
            maintenance_enabled: true,
        }
    }
}

/// Why a connection's operating bounds were refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    /// The execution budget was outside the supported range.
    #[error("the execution budget must be between {min} and {max} seconds, not {seconds}")]
    ExecutionBudget {
        /// The refused value.
        seconds: u64,
        /// The smallest accepted value.
        min: u64,
        /// The largest accepted value.
        max: u64,
    },
    /// The unresolved-publication age was outside the supported range.
    #[error(
        "the unresolved-publication age must be between {min} and {max} seconds, not {seconds}"
    )]
    UnresolvedAge {
        /// The refused value.
        seconds: u64,
        /// The smallest accepted value.
        min: u64,
        /// The largest accepted value.
        max: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_valid() {
        let settings = ConnectionSettings::default();
        assert_eq!(
            ConnectionSettings::new(
                settings.max_execution_seconds(),
                settings.max_unresolved_publication_seconds(),
                settings.maintenance_enabled(),
            ),
            Ok(settings)
        );
        assert_eq!(settings.max_execution_seconds(), 180);
        assert_eq!(settings.max_unresolved_publication_seconds(), 3600);
        assert!(settings.maintenance_enabled());
    }

    #[test]
    fn the_execution_budget_is_bounded() {
        for seconds in [
            0,
            ConnectionSettings::MIN_EXECUTION_SECONDS - 1,
            ConnectionSettings::MAX_EXECUTION_SECONDS + 1,
        ] {
            assert!(matches!(
                ConnectionSettings::new(seconds, 3600, true),
                Err(SettingsError::ExecutionBudget { .. })
            ));
        }
        for seconds in [
            ConnectionSettings::MIN_EXECUTION_SECONDS,
            ConnectionSettings::MAX_EXECUTION_SECONDS,
        ] {
            assert!(
                ConnectionSettings::new(seconds, 3600, true).is_ok(),
                "{seconds} seconds should be accepted"
            );
        }
    }

    #[test]
    fn the_unresolved_age_is_bounded() {
        for seconds in [
            0,
            ConnectionSettings::MIN_UNRESOLVED_SECONDS - 1,
            ConnectionSettings::MAX_UNRESOLVED_SECONDS + 1,
        ] {
            assert!(matches!(
                ConnectionSettings::new(180, seconds, true),
                Err(SettingsError::UnresolvedAge { .. })
            ));
        }
        for seconds in [
            ConnectionSettings::MIN_UNRESOLVED_SECONDS,
            ConnectionSettings::MAX_UNRESOLVED_SECONDS,
        ] {
            assert!(
                ConnectionSettings::new(180, seconds, true).is_ok(),
                "{seconds} seconds should be accepted"
            );
        }
    }

    #[test]
    fn maintenance_may_be_paused() {
        let settings = ConnectionSettings::new(180, 3600, false).unwrap();
        assert!(!settings.maintenance_enabled());
    }
}
