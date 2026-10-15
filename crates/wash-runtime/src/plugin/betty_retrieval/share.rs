//! Each workload's share of the one pool every workload on the host uses.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::bindings::betty_blocks::retrieval::types;
use super::errors;

/// How many connections each workload holds, against a limit for each.
///
/// A workload takes one of its permits before it checks a connection out of
/// the pool, and keeps it for as long as it keeps the connection: one
/// statement, the rows of a query, or a whole transaction. One workload that
/// holds or leaks connections then uses up its own share, never the others'.
pub(crate) struct WorkloadShares {
    per_workload: usize,
    /// How long a workload over its share waits before `pool-exhausted`, or
    /// `None` to wait until a permit comes back.
    wait: Option<Duration>,
    shares: Mutex<HashMap<Arc<str>, Arc<Semaphore>>>,
}

impl WorkloadShares {
    pub(crate) fn new(per_workload: usize, wait: Option<Duration>) -> Self {
        Self {
            per_workload,
            wait,
            shares: Mutex::new(HashMap::new()),
        }
    }

    /// One of `workload`'s permits, waiting up to the configured wait for one
    /// to come back.
    pub(crate) async fn acquire(
        &self,
        workload: &Arc<str>,
    ) -> Result<OwnedSemaphorePermit, types::Error> {
        let share = Arc::clone(
            self.lock()
                .entry(Arc::clone(workload))
                .or_insert_with(|| Arc::new(Semaphore::new(self.per_workload))),
        );
        let acquired = match self.wait {
            Some(wait) => tokio::time::timeout(wait, share.acquire_owned()).await,
            None => Ok(share.acquire_owned().await),
        };
        match acquired {
            Ok(Ok(permit)) => Ok(permit),
            // Nothing closes these semaphores; kept so a change that does
            // reads as a failure rather than a hang.
            Ok(Err(_closed)) => Err(errors::connection_failed(
                "this workload's share of the pool was closed".to_string(),
            )),
            Err(_elapsed) => Err(types::Error::PoolExhausted),
        }
    }

    /// Drop `workload`'s entry once it is unbound. Permits it still holds
    /// keep their semaphore alive until they are dropped.
    pub(crate) fn forget(&self, workload: &str) {
        self.lock().remove(workload);
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.lock().len()
    }

    /// A panic while the map was locked cannot leave it half-changed: every
    /// change is one insert or one remove.
    fn lock(&self) -> MutexGuard<'_, HashMap<Arc<str>, Arc<Semaphore>>> {
        self.shares
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::plugin::betty_retrieval::bindings::betty_blocks::retrieval::types::Error;

    const SHORT_WAIT: Duration = Duration::from_millis(50);

    fn workload(id: &str) -> Arc<str> {
        Arc::from(id)
    }

    #[tokio::test]
    async fn a_workload_over_its_share_is_pool_exhausted_while_another_still_gets_one() {
        let shares = WorkloadShares::new(1, Some(SHORT_WAIT));
        let _held = shares
            .acquire(&workload("a"))
            .await
            .expect("a's first connection is within its share");
        assert!(
            matches!(
                shares.acquire(&workload("a")).await,
                Err(Error::PoolExhausted)
            ),
            "a second connection is over a's share of one"
        );
        let _other = shares
            .acquire(&workload("b"))
            .await
            .expect("b has a share of its own, which a cannot use up");
    }

    #[tokio::test]
    async fn a_share_comes_back_when_its_connection_is_done() {
        let shares = WorkloadShares::new(1, Some(SHORT_WAIT));
        let held = shares.acquire(&workload("a")).await.expect("within share");
        drop(held);
        let _again = shares
            .acquire(&workload("a"))
            .await
            .expect("the share the dropped connection held is free again");
    }

    #[tokio::test]
    async fn a_forgotten_workload_starts_again_with_a_whole_share() {
        let shares = WorkloadShares::new(1, Some(SHORT_WAIT));
        let _held = shares.acquire(&workload("a")).await.expect("within share");
        shares.forget("a");
        assert_eq!(shares.tracked(), 0, "an unbound workload leaves no entry");
        let _again = shares
            .acquire(&workload("a"))
            .await
            .expect("a workload bound again under the same id is not charged for the old one");
    }
}
