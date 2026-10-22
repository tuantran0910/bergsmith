# 0046. Web UI refresh and BYOC delivery

- Status: Accepted
- Date: 2026-10-22
- Plan reference: BS-021, public API and UI; open decisions (SaaS deployment
  model)

## Context

Bergsmith is to become a SaaS product. The web UI from
[0037](0037-web-ui-and-generated-client.md) is correct but plain: about 200
lines of hand-written CSS, a small path router that cannot keep list state in
the URL, and pages that show raw identifiers first. A product needs a modern,
consistent appearance and better everyday UX, starting simple.

How the SaaS is delivered constrains the UI. Compaction reads and rewrites
whole tables, so a vendor-hosted data plane pays for every byte and takes
custody of customer data. A vendor-hosted coordinator would hold a catalog
principal that can write every maintained table, and would see manifest
metadata, including column bounds and partition values in file paths.

## Decision

### Delivery

- The SaaS is delivered first as bring your own cloud (BYOC): the whole stack
  (coordinator, workers, PostgreSQL, UI) runs in the operator's environment,
  on any cloud or on premises, from the existing binaries, Compose files, and
  the M8 Helm chart. Table data, catalog credentials, and manifest metadata
  never leave that environment. No architecture invariant changes.
- A fully hosted data plane is ruled out. A hosted control plane and an agent
  hybrid are deferred; either needs its own decision.
- If a deployment later serves several organizations behind one host, tenants
  are addressed by path (`/<org>/...`), not by subdomain, so the session and
  CSRF cookies keep one origin.

### UI stack

These additions supersede 0037's choice of an in-application router; the rest
of 0037 stands.

- Tailwind CSS v4 for styling and shadcn/ui on Base UI for accessible
  primitives. The component source is copied into `web/src/ui/` and owned by
  the repository, so upgrades are reviewed changes, not transitive updates.
- TanStack Router with code-based route definitions, so routes, path params,
  and search params are typed without a generated route file. List state the
  API supports lives in validated search params.
- Playwright with `@axe-core/playwright` as a browser gate against a
  deterministic fake coordinator whose fixtures are typed by the generated
  schema.
- Every dependency is pinned exactly and justified in its pull request, as
  `AGENTS.md` requires.

### Appearance and UX

- Design tokens are CSS variables in OKLCH for light and dark themes:
  neutral graphite surfaces and one accent. Fonts are the system stack and a
  monospace stack for identifiers; tables use tabular numerals.
- Status is always text and an icon, never color alone. Motion honors
  `prefers-reduced-motion`. The browser gate fails on WCAG 2.2 AA violations.
- Pages lead with readable names; identifiers are shortened with a copy
  button. Every page has a loading skeleton and an empty state.
- The behaviors 0037 requires are kept unchanged: execution and publication
  shown separately, distinct `ReplanRequired` and `CommitOutcomeUnknown`
  notices with operator actions, typed error reasons, idempotent submission
  and retry, the CSRF echo, the identity cache purge, and the SSE reconnect
  rules. Cancellation moves into a confirmation dialog that shows the
  reported outcome.

### Security

- The UI loads nothing from outside its own origin at run time: no CDN, font
  service, or analytics. This keeps BYOC and air-gapped deployments working
  and keeps the CSP closed.
- The coordinator's CSP drops `'unsafe-inline'` from `style-src` and adds
  `require-trusted-types-for 'script'; trusted-types 'none'`. The browser gate
  fails on any CSP violation. Hashed assets are immutable; `index.html` is
  `no-cache`.
- Lint bans `dangerouslySetInnerHTML` and enforces the strict accessibility
  rules of `eslint-plugin-jsx-a11y-x`, the maintained fork that supports
  ESLint 10 (the original plugin supports ESLint 9 at most).
- Sessions stay server-side with `HttpOnly` cookies; no token is visible to
  JavaScript (0032).

## Consequences

- Operators can run Bergsmith anywhere without trusting a vendor with data or
  catalog credentials; the vendor must support many independent deployments,
  which raises the importance of the M8 Helm chart, upgrade paths, and
  diagnostics.
- The web toolchain grows by Tailwind, Base UI, TanStack Router, Playwright,
  and axe-core. Owned component source means the team maintains it.
- `web/src/router.tsx` is removed; journey tests move to the new router with
  the same behavioral assertions.
- A stricter style policy rules out libraries that inject `<style>` elements
  at run time.
- Deferred: an overview dashboard (needs aggregate APIs), a command palette,
  schedule and policy pages, connection setup, effective permissions in
  `/auth/session`, and organization-prefixed routes.
