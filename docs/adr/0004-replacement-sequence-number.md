# 0004. Replacement files take the planning snapshot's sequence number

- Status: Accepted
- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §3 Delete-aware compaction

## Context

Rewritten data files need a data sequence number that keeps retained delete files correct.

## Decision

- Replacement data files use the planning snapshot's sequence number S, set with `ReplaceFilesAction::set_new_data_file_sequence_number` (present at the pinned revision).

## Consequences

- Retained equality deletes at sequence ≤ S do not apply to the new files; position deletes reference old paths.
- Equality deletes committed after planning still apply to rewritten rows, which keeps compatible rebasing possible later.
- Integration tests verify the written sequence numbers.
