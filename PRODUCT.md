# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

Data platform engineers who operate Apache Iceberg tables and are accountable
for their maintenance. They run a self-hosted Bergsmith deployment, connect
standard Iceberg REST catalogs, and supervise the coordinator and pull-based
workers. They plan and launch maintenance manually or on schedules, watch
jobs to a terminal outcome, triage failures and `ReplanRequired` states, and
manage catalog connections, people, and worker capacity. Administrators hold
the organization-level grants for access and worker management; operators run
and observe maintenance.

## Product Purpose

Bergsmith keeps Iceberg tables healthy by planning and executing maintenance
against existing catalogs. It exists so a small platform team can run
compaction, delete application, and later broader maintenance on its own
infrastructure, with every job explainable from an immutable plan to a
published outcome. Success means operators trust that maintenance preserved
the live rows, publishes only through the catalog, and leaves a durable,
auditable execution record.

## Positioning

Standard Iceberg REST catalogs keep metadata authority. Bergsmith is a
catalog-authoritative maintenance plane: a coordinator plans and publishes,
pull-based workers rewrite data, and PostgreSQL holds the durable execution
ledger. It does not own table metadata, does not read table rows in the
coordinator, and never asks operators to move tables into a proprietary
format.

## Operating Context

Self-hosted, single-tenant deployments on the operator's own infrastructure.
Operators connect catalogs over the standard Iceberg REST API (Lakekeeper
v0.14.0 is the first certification target; further catalogs arrive through
compatibility tests) and store data on S3 or S3-compatible object storage
through catalog-vended credentials. PostgreSQL is required for the execution ledger and migrations
run as a separate job before replicas start. Identity is OIDC with scoped
organization, catalog, and table grants. Work is driven from the browser UI,
the HTTP API, and scheduled policies; jobs run on workers the operator
supervises, and recovery after coordinator restarts is part of normal
operation.

## Capabilities and Constraints

The first usable release covers whole-file binpack compaction of v2 Parquet
tables on `main`, position and equality delete application, conservative
standard REST commits with unknown-outcome recovery, distributed workers,
retries, cancellation, crash recovery, scoped authorization, the web UI, and
manual and scheduled runs. Later work adds health analysis, notifications,
broader maintenance, Helm and standby operation, and more storage backends.

Constraints that hold across work:

- Only the coordinator publishes, through the standard REST commit with the
  original planning-baseline requirements. Workers never touch PostgreSQL,
  hold catalog credentials, or delete source files.
- Terminal jobs never transition; retries create a new job with a fresh plan.
  An ambiguous commit is never resolved without conclusive evidence.
- Storage paths come from typed locations, never string concatenation.
- No external CDN assets, external fonts, or analytics at runtime. The UI
  follows the coordinator's Content Security Policy and its JavaScript
  first-load budget.
- Operations, deployment assets, and credentials stay in their documented
  places; development credentials are obviously fake.

Open product decisions are tracked in `.private/BERGSMITH_PROJECT_PLAN.md`
and the ADRs; this file does not substitute for them.

## Brand Commitments

The product is named Bergsmith. It is self-hosted, engineering-led tooling
for operators rather than a hosted analytics product. No public logo,
testimonial, customer, benchmark, or pricing claim exists; future work must
not fabricate one.

## Evidence on Hand

- `docs/adr/` records each architecture decision; `docs/support-matrix.md`
  lists what is supported and certified.
- `crates/fixtures` holds native Iceberg fixtures and an independent live-row
  oracle; correctness tests run against real metadata and Parquet files.
- The web browser gate runs the production build in a pinned Chromium under
  the coordinator's CSP with axe (WCAG 2.2 AA) in light and dark themes.
- Development Compose services (PostgreSQL, RustFS, Lakekeeper) and
  `make` targets document the supported local workflow.
- No production telemetry, customer data, or benchmark results are available
  to cite.

## Product Principles

1. The catalog is the source of truth; Bergsmith earns trust by leaving that
   authority intact.
2. Every outcome is explainable: an immutable plan, accepted work, and a
   publication record that can be inspected later.
3. Ambiguity is surfaced, never guessed away; unknown publication stays
   unknown until evidence resolves it.
4. Operators see the state of the system at a glance and can trace a problem
   to the job, task, and attempt that caused it.
5. Self-hosted means no hidden dependencies: no CDN, no telemetry, and the
   only external runtime service is the operator's own identity provider.

## Accessibility & Inclusion

The browser UI targets WCAG 2.2 AA: semantic structure, labeled controls,
visible focus, keyboard operation, text alternatives for status, reduced
motion, usable narrow layouts, and light and dark themes. Accessibility is
part of the browser acceptance gate, not a follow-up.
