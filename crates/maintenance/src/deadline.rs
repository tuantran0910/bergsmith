//! Reject late completions even when work overruns a deadline inside one poll.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

pub(crate) async fn within<F: Future>(budget: Duration, future: F) -> Option<F::Output> {
    let started = Instant::now();
    let result = tokio::time::timeout(budget, future).await.ok()?;
    // Tokio polls the inner future before checking its timer. A blocking or
    // CPU-heavy poll can finish late and still return Ok from timeout. Reject
    // that completion, without claiming that cancellation stopped the work.
    (started.elapsed() < budget).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    #[tokio::test(start_paused = true)]
    async fn work_that_completes_after_its_deadline_cannot_report_success() {
        let late = || async {
            // Poll advance once to move the paused clock, then finish this
            // poll without yielding to timeout's timer branch.
            let _ = tokio::time::advance(Duration::from_secs(2)).now_or_never();
            "completed after the deadline"
        };
        assert!(
            tokio::time::timeout(Duration::from_secs(1), late())
                .await
                .is_ok()
        );
        let result = within(Duration::from_secs(1), late()).await;
        assert_eq!(result, None);
    }
}
