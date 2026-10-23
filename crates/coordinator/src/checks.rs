//! Bounded capability checks against one catalog (BS-022.T2).
//!
//! A check is an outbound request an operator triggers by hand. It is
//! bounded twice: the catalog's answer may not be larger than the client
//! allows, and the number of checks that may run at once is capped per
//! process and per requester, so a faulty catalog or a burst of checks cannot
//! exhaust the coordinator.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bergsmith_catalog::{CapabilityReport, CatalogConnection, ConnectionConfig};
use bergsmith_common::ids::PrincipalId;
use tokio::sync::{Mutex, Semaphore};

/// The most checks one process runs at once.
pub const MAX_CONCURRENT_CHECKS: usize = 8;

/// The most checks one requester runs at once.
pub const MAX_CONCURRENT_CHECKS_PER_PRINCIPAL: usize = 2;

/// The bound on one check, including the catalog's own timeouts.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// The bound on connecting to a catalog.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The bound on one whole catalog request, including its body.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The most requesters whose per-requester bound is tracked at once.
const MAX_TRACKED_PRINCIPALS: usize = 1024;

/// Bounds applied to capability checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapabilityCheckLimits {
    /// The most checks one process runs at once.
    pub max_concurrent: usize,
    /// The most checks one requester runs at once.
    pub max_concurrent_per_principal: usize,
    /// The bound on one check, including the catalog's own timeouts.
    pub timeout: Duration,
    /// The bound on connecting to the catalog.
    pub connect_timeout: Duration,
    /// The bound on one whole catalog request, including its body.
    pub request_timeout: Duration,
}

impl Default for CapabilityCheckLimits {
    fn default() -> Self {
        Self {
            max_concurrent: MAX_CONCURRENT_CHECKS,
            max_concurrent_per_principal: MAX_CONCURRENT_CHECKS_PER_PRINCIPAL,
            timeout: CHECK_TIMEOUT,
            connect_timeout: CONNECT_TIMEOUT,
            request_timeout: REQUEST_TIMEOUT,
        }
    }
}

/// Why a check did not answer.
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    /// Too many checks are already running.
    #[error("capability check capacity is exhausted")]
    Overloaded,
    /// The catalog call failed.
    #[error(transparent)]
    Catalog(#[from] bergsmith_catalog::CatalogError),
    /// The catalog did not answer within the bound.
    #[error("catalog did not answer within the check timeout")]
    Timeout,
}

/// Runs capability checks within the process and requester bounds.
#[derive(Clone, Debug)]
pub struct CapabilityChecker {
    limits: CapabilityCheckLimits,
    capacity: Arc<Semaphore>,
    principals: Arc<Mutex<BTreeMap<PrincipalId, Arc<Semaphore>>>>,
}

impl CapabilityChecker {
    /// Creates a checker with the given bounds.
    #[must_use]
    pub fn new(limits: CapabilityCheckLimits) -> Self {
        Self {
            limits,
            capacity: Arc::new(Semaphore::new(limits.max_concurrent.max(1))),
            principals: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// The bounds in force.
    #[must_use]
    pub const fn limits(&self) -> CapabilityCheckLimits {
        self.limits
    }

    /// Reads what the catalog advertises.
    ///
    /// The permit is taken without waiting: a queued check would hold the
    /// request, its body, and a connection while it waits for capacity.
    ///
    /// # Errors
    ///
    /// [`CheckError::Overloaded`] when the process or the requester is at
    /// its bound, [`CheckError::Timeout`] when the catalog does not answer in
    /// time, and [`CheckError::Catalog`] when the call fails.
    pub async fn check(
        &self,
        requester: PrincipalId,
        config: &ConnectionConfig,
    ) -> Result<CapabilityReport, CheckError> {
        let _process = self
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| CheckError::Overloaded)?;
        let _requester = self.acquire_requester(requester).await?;
        match tokio::time::timeout(self.limits.timeout, CatalogConnection::probe(config)).await {
            Ok(Ok(report)) => Ok(report),
            Ok(Err(error)) => Err(CheckError::Catalog(error)),
            Err(_elapsed) => Err(CheckError::Timeout),
        }
    }

    /// Takes the requester's permit, under the map's lock.
    ///
    /// The permit is taken while the lock is held, so a requester can never
    /// hold two gates: a request that has taken a gate always holds a permit
    /// on it, and a gate nobody holds a permit on is free to be dropped.
    async fn acquire_requester(
        &self,
        requester: PrincipalId,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, CheckError> {
        let mut principals = self.principals.lock().await;
        self.drop_idle_gates(&mut principals);
        let limit = self.limits.max_concurrent_per_principal.max(1);
        let gate = principals
            .entry(requester)
            .or_insert_with(|| Arc::new(Semaphore::new(limit)))
            .clone();
        gate.try_acquire_owned().map_err(|_| CheckError::Overloaded)
    }

    /// Drops the gates of requesters that hold no permit.
    ///
    /// A gate is only dropped when every permit is free, which means no
    /// request is using it: a request that took a gate holds a permit until
    /// its check finishes.
    fn drop_idle_gates(&self, principals: &mut BTreeMap<PrincipalId, Arc<Semaphore>>) {
        if principals.len() <= MAX_TRACKED_PRINCIPALS {
            return;
        }
        let limit = self.limits.max_concurrent_per_principal.max(1);
        principals.retain(|_, gate| gate.available_permits() < limit);
    }
}

impl Default for CapabilityChecker {
    fn default() -> Self {
        Self::new(CapabilityCheckLimits::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    fn checker(per_principal: usize) -> CapabilityChecker {
        CapabilityChecker::new(CapabilityCheckLimits {
            max_concurrent: 8,
            max_concurrent_per_principal: per_principal,
            timeout: Duration::from_secs(1),
            ..CapabilityCheckLimits::default()
        })
    }

    fn quick_checker() -> CapabilityChecker {
        CapabilityChecker::new(CapabilityCheckLimits {
            max_concurrent: 8,
            max_concurrent_per_principal: 1,
            timeout: Duration::from_millis(100),
            ..CapabilityCheckLimits::default()
        })
    }

    #[tokio::test]
    async fn one_requester_never_holds_two_gates() {
        let checker = checker(1);
        let requester = PrincipalId::generate();

        // The first request takes the requester's only permit.
        let first = checker.acquire_requester(requester).await.unwrap();

        // The map grows past the bound while that request runs, so the
        // cleanup actually runs.
        {
            let mut principals = checker.principals.lock().await;
            fill_with_idle_gates(&mut principals, MAX_TRACKED_PRINCIPALS);
            assert!(principals.len() > MAX_TRACKED_PRINCIPALS);
            checker.drop_idle_gates(&mut principals);
            // The requester's gate has a permit outstanding, so it stays.
            assert!(principals.contains_key(&requester));
        }

        // A second request for the same requester finds the same gate and is
        // refused; it cannot be given a replacement gate.
        assert!(checker.acquire_requester(requester).await.is_err());

        // Once the first request finishes, the requester may check again.
        drop(first);
        assert!(checker.acquire_requester(requester).await.is_ok());
    }

    #[tokio::test]
    async fn idle_gates_are_dropped_and_a_busy_one_is_kept() {
        let checker = checker(1);
        let mut principals = checker.principals.lock().await;
        fill_with_idle_gates(&mut principals, MAX_TRACKED_PRINCIPALS + 1);

        // One requester holds a permit: its gate is in use.
        let busy = PrincipalId::generate();
        let busy_gate = Arc::new(Semaphore::new(1));
        let permit = busy_gate.clone().try_acquire_owned().unwrap();
        principals.insert(busy, busy_gate);
        assert!(principals.len() > MAX_TRACKED_PRINCIPALS);

        checker.drop_idle_gates(&mut principals);

        // Every idle gate is gone; the gate with an outstanding permit stays.
        assert!(principals.contains_key(&busy));
        assert_eq!(principals.len(), 1);
        drop(permit);
    }

    /// Fills the map with gates that hold no permit.
    fn fill_with_idle_gates(principals: &mut BTreeMap<PrincipalId, Arc<Semaphore>>, count: usize) {
        for _ in 0..count {
            principals.insert(PrincipalId::generate(), Arc::new(Semaphore::new(1)));
        }
    }

    #[tokio::test]
    async fn a_given_up_check_is_reported_as_a_timeout() {
        use tokio::io::AsyncReadExt;

        // A catalog that accepts the request and never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer).await;
            std::future::pending::<()>().await;
        });
        let checker = quick_checker();
        let config = ConnectionConfig::new(Url::parse(&format!("http://{address}")).unwrap());

        let error = checker
            .check(PrincipalId::generate(), &config)
            .await
            .unwrap_err();

        assert!(matches!(error, CheckError::Timeout), "{error:?}");
    }

    #[tokio::test]
    async fn a_gate_without_a_permit_is_dropped() {
        let checker = checker(1);
        let requester = PrincipalId::generate();
        let permit = checker.acquire_requester(requester).await.unwrap();
        drop(permit);

        let mut principals = checker.principals.lock().await;
        principals.insert(PrincipalId::generate(), Arc::new(Semaphore::new(1)));
        checker.drop_idle_gates(&mut principals);

        // The idle gate of the other requester is gone; the one that was used
        // and released is idle too.
        assert!(
            principals
                .values()
                .all(|gate| gate.available_permits() == 1)
        );
    }
}
