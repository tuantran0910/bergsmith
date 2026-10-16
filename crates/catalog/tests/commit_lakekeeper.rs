//! Conditional commits against the Compose Lakekeeper, with manifests
//! written to RustFS through vended credentials (BS-006).

mod support;

use bergsmith_catalog::{
    CommitOutcome, CommitRequest, IdempotencyKey, RejectionKind, SnapshotEvidence,
};
use support::commit::{append, baseline, commit, data_file, rewrite, table_with_files};

#[tokio::test]
async fn a_rewrite_commits_and_leaves_evidence() {
    let (_warehouse, connection, name, files) = table_with_files(&["a", "b"]).await;
    let baseline = baseline(&connection, &name).await;
    let output = data_file(&baseline, "c", 2);
    let prepared = rewrite(&baseline, files, vec![output])
        .prepare(&baseline)
        .await
        .unwrap();

    let outcome = connection.commit(&prepared.request, None).await.unwrap();

    let CommitOutcome::Committed(committed) = outcome else {
        panic!("the rewrite commits: {outcome:?}");
    };
    let published = SnapshotEvidence::Published {
        sequence_number: baseline.main_snapshot().unwrap().sequence_number() + 1,
        main_head: true,
    };
    assert_eq!(
        prepared
            .expected
            .find_in(committed.metadata.as_ref().unwrap()),
        published
    );
    assert_eq!(
        connection
            .snapshot_evidence(&name, &prepared.expected)
            .await
            .unwrap(),
        published
    );
}

#[tokio::test]
async fn a_proposal_on_a_stale_baseline_is_rejected() {
    let (_warehouse, connection, name, files) = table_with_files(&["a"]).await;
    let stale = baseline(&connection, &name).await;
    let prepared = rewrite(&stale, files, vec![data_file(&stale, "c", 1)])
        .prepare(&stale)
        .await
        .unwrap();
    append(&connection, &name, &["late"]).await;

    let outcome = connection.commit(&prepared.request, None).await.unwrap();

    let CommitOutcome::Rejected(rejection) = outcome else {
        panic!("the stale commit is rejected: {outcome:?}");
    };
    assert_eq!(rejection.kind, RejectionKind::RequirementFailed);
    assert_eq!(
        connection
            .snapshot_evidence(&name, &prepared.expected)
            .await
            .unwrap(),
        SnapshotEvidence::Absent
    );
}

#[tokio::test]
async fn evidence_is_found_after_the_head_advances() {
    let (_warehouse, connection, name, files) = table_with_files(&["a"]).await;
    let baseline = baseline(&connection, &name).await;
    let prepared = rewrite(&baseline, files, vec![data_file(&baseline, "c", 1)])
        .prepare(&baseline)
        .await
        .unwrap();
    commit(&connection, &prepared.request).await;
    append(&connection, &name, &["late"]).await;

    let evidence = connection
        .snapshot_evidence(&name, &prepared.expected)
        .await
        .unwrap();

    assert!(
        matches!(
            evidence,
            SnapshotEvidence::Published {
                main_head: false,
                ..
            }
        ),
        "{evidence:?}"
    );
}

#[tokio::test]
async fn a_replay_with_the_same_key_does_not_publish_twice() {
    let (_warehouse, connection, name, files) = table_with_files(&["a"]).await;
    let baseline = baseline(&connection, &name).await;
    let prepared = rewrite(&baseline, files, vec![data_file(&baseline, "c", 1)])
        .prepare(&baseline)
        .await
        .unwrap();
    let key = IdempotencyKey::generate();
    let persisted = CommitRequest::from_persisted(name.clone(), prepared.request.body().to_vec());

    let first = connection
        .commit(&prepared.request, Some(key))
        .await
        .unwrap();
    let replay = connection.commit(&persisted, Some(key)).await.unwrap();

    assert!(matches!(first, CommitOutcome::Committed(_)), "{first:?}");
    assert!(matches!(replay, CommitOutcome::Committed(_)), "{replay:?}");
    assert_eq!(persisted.sha256(), prepared.request.sha256());
    let after = connection.load_table(&name, false).await.unwrap().metadata;
    assert_eq!(after.snapshots().len(), 2);
    assert_eq!(
        prepared.expected.find_in(&after),
        SnapshotEvidence::Published {
            sequence_number: 2,
            main_head: true
        }
    );
}
