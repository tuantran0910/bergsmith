//! Lost-response fixtures against the Compose Lakekeeper (BS-007.T2).
//!
//! A proxy forwards each commit, lets Lakekeeper apply or reject it, and
//! then closes the connection without answering. The outcome is unknown, and
//! only retained snapshot evidence can resolve it.

// clippy's `allow-*-in-tests` settings cover only `#[test]` functions; the
// helpers below fail the test on setup errors.
#![expect(
    clippy::unwrap_used,
    reason = "test helpers fail the test on setup errors"
)]

mod support;

use bergsmith_catalog::{
    CatalogConnection, CommitOutcome, IdempotencyKey, PreparedCommit, RejectionKind,
    SnapshotEvidence, TableName, UnknownOutcome,
};
use support::Warehouse;
use support::commit::{append, baseline, data_file, rewrite, table_with_files};
use support::proxy::LossyProxy;

struct Fixture {
    _warehouse: Warehouse,
    direct: CatalogConnection,
    lossy: CatalogConnection,
    proxy: LossyProxy,
    name: TableName,
    prepared: PreparedCommit,
}

/// A rewrite of `a` ready to send, and connections with and without the
/// proxy.
async fn fixture() -> Fixture {
    let (warehouse, direct, name, files) = table_with_files(&["a"]).await;
    let proxy = LossyProxy::start().await;
    let lossy = warehouse.connect_via(proxy.url()).await;
    let baseline = baseline(&direct, &name).await;
    let prepared = rewrite(&baseline, files, vec![data_file(&baseline, "c", 1)])
        .prepare(&baseline)
        .await
        .unwrap();
    Fixture {
        _warehouse: warehouse,
        direct,
        lossy,
        proxy,
        name,
        prepared,
    }
}

impl Fixture {
    /// Sends the rewrite through the proxy and loses the response.
    async fn send_and_lose(&self, key: Option<IdempotencyKey>) {
        self.proxy.drop_commit_responses(1);
        let outcome = self
            .lossy
            .commit(&self.prepared.request, key)
            .await
            .unwrap();
        assert!(
            matches!(
                outcome,
                CommitOutcome::Unknown(UnknownOutcome::Transport(_))
            ),
            "{outcome:?}"
        );
    }

    async fn evidence(&self) -> SnapshotEvidence {
        self.direct
            .snapshot_evidence(&self.name, &self.prepared.expected)
            .await
            .unwrap()
    }

    async fn snapshot_count(&self) -> usize {
        self.direct
            .load_table(&self.name, false)
            .await
            .unwrap()
            .metadata
            .snapshots()
            .len()
    }
}

#[tokio::test]
async fn a_lost_success_is_recovered_from_snapshot_evidence() {
    let fixture = fixture().await;

    fixture.send_and_lose(None).await;

    assert!(
        matches!(
            fixture.evidence().await,
            SnapshotEvidence::Published {
                main_head: true,
                ..
            }
        ),
        "{:?}",
        fixture.evidence().await
    );
}

#[tokio::test]
async fn evidence_survives_the_head_advancing_after_a_lost_response() {
    let fixture = fixture().await;
    fixture.send_and_lose(None).await;

    append(&fixture.direct, &fixture.name, &["late"]).await;

    assert!(
        matches!(
            fixture.evidence().await,
            SnapshotEvidence::Published {
                main_head: false,
                ..
            }
        ),
        "{:?}",
        fixture.evidence().await
    );
}

#[tokio::test]
async fn a_replay_with_the_same_key_reports_the_lost_success() {
    let fixture = fixture().await;
    let key = IdempotencyKey::generate();
    fixture.send_and_lose(Some(key)).await;

    let replay = fixture
        .direct
        .commit(&fixture.prepared.request, Some(key))
        .await
        .unwrap();

    assert!(matches!(replay, CommitOutcome::Committed(_)), "{replay:?}");
    assert_eq!(fixture.snapshot_count().await, 2);
}

/// Without a key, the replay fails its original `main` requirement because
/// the first send published. The conflict proves nothing about the first
/// send; the evidence does.
#[tokio::test]
async fn a_conflicting_replay_does_not_prove_that_the_first_send_failed() {
    let fixture = fixture().await;
    fixture.send_and_lose(None).await;

    let replay = fixture
        .direct
        .commit(&fixture.prepared.request, None)
        .await
        .unwrap();

    assert!(
        matches!(&replay, CommitOutcome::Rejected(r) if r.kind == RejectionKind::RequirementFailed),
        "{replay:?}"
    );
    assert!(matches!(
        fixture.evidence().await,
        SnapshotEvidence::Published { .. }
    ));
    assert_eq!(fixture.snapshot_count().await, 2);
}

/// A lost rejection looks exactly like a lost success to the sender. Absent
/// evidence leaves it unresolved rather than failed.
#[tokio::test]
async fn a_lost_rejection_leaves_the_outcome_unresolved() {
    let fixture = fixture().await;
    append(&fixture.direct, &fixture.name, &["late"]).await;

    fixture.send_and_lose(None).await;

    assert_eq!(fixture.evidence().await, SnapshotEvidence::Absent);
}

#[tokio::test]
async fn the_proxy_passes_answers_through_unless_told_to_lose_them() {
    let fixture = fixture().await;

    let outcome = fixture
        .lossy
        .commit(&fixture.prepared.request, None)
        .await
        .unwrap();

    assert!(
        matches!(outcome, CommitOutcome::Committed(_)),
        "{outcome:?}"
    );
}
