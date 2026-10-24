# 0056. UI platform, navigation, and names-first operation

- Status: Proposed
- Date: 2026-10-25
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4, proposed BS-032;
  BS-024–BS-029 adopt these rules as they land

## Context

[0046](0046-web-ui-refresh-and-byoc-delivery.md) chose the UI stack: Tailwind
v4, shadcn/ui on Base UI, TanStack Router, accessible tokens, a browser gate,
and BYOC-safe delivery. BS-021 shipped that foundation. BS-022 and BS-023 then
added catalog, table, job, worker, audit, principal, grant, and token pages on
top of it.

Those pages work, but they were built one feature at a time. The result is a
competent administration console rather than a maintenance operations console:

- The shell is a flat seven-item sidebar with no grouping, no global search,
  and no landing view. `/` redirects to the table list, so the first screen
  answers "what tables exist?" rather than "is maintenance healthy?".
- Lists expose only cursor pagination. Tables, jobs, workers, and audit have no
  search, filters, sorting, saved views, or density control, and several lead
  with identifiers instead of names.
- Detail pages are long card stacks. Tables, catalogs, jobs, and principals do
  not have task-oriented tabs, so access, settings, discovery, statistics, and
  history compete for the same scroll.
- The mobile layout is not usable for operations: the navigation is a clipped
  horizontal strip that hides destinations, and the grants table crushes four
  columns into unreadable text.
- A job cannot be diagnosed from the UI. Statistics count tasks and attempts,
  but there is no task or attempt view and no failure reason per attempt.
- The JavaScript budget has little headroom. The production build is about
  166 kB gzipped of a 175 kB budget, in one roughly 540 kB minified chunk.

BS-024–BS-029 will add policies, schedules, statistics, a table explorer, an
overview, health, and notifications. Adding those pages with the current
patterns would multiply the inconsistency. A cross-cutting decision is needed
before more screens are built. The existing page-level work remains valid; this
ADR extends 0046 rather than replacing it.

## Decision

### 1. Names are the interface

Every list row and detail header leads with a human-readable name. Stable IDs
remain the keys for links, authorization, and API calls, but they are copyable
metadata rather than primary content.

- A shared `ResourceLabel` renders a primary name, optional context (for
  example `lake.sales.orders`), a copy action, and a details slot.
- A table list does not have a UUID column. A job list leads with its table and
  operation; the short job ID is secondary. A grant row shows principal name,
  role, and scope name; the principal and scope IDs live in the details or copy
  affordance. Catalog and table detail pages do not lead with connection or
  table UUIDs.
- A full identifier appears only when no name is available, when the user opens
  technical details, or when an operator explicitly asks to copy it.
- Names are display-only. They never participate in authorization. The server
  remains responsible for filtering names the caller may not see, following the
  existing grant and alias rules.

### 2. Grouped shell and a real landing page

Destinations are grouped by the operator's job:

- **Overview**
- **Data**: Catalogs, Tables
- **Operations**: Jobs, Schedules, Workers
- **Administration**: Access (Users, Service identities), Audit, Settings

The desktop shell uses a 248–264 px rail that can collapse to icons. The top
bar holds breadcrumbs, global search or the command palette, a `+ New` menu,
help, and the account menu. Theme and density move into the account menu.

On narrow screens the rail becomes a drawer opened by a menu button. The
current destination is always visible. A skip link targets a labeled `main`
landmark.

Overview is the signed-in landing page. It is attention-first: unresolved
publications, failed and replan-required jobs, critical tables, worker
capacity, and missed or late schedules, each with a link to the filtered view.
It depends on the aggregates in BS-025 and BS-027.T3; until those exist, the
route may ship with only the sections whose APIs are available.

### 3. Four page templates

Every page is built from one of four templates, and pages that do not fit are a
design review item:

- **List**: title, purpose, primary action, search/filter bar, saved views,
  data table, pagination, empty/loading/error/partial states.
- **Detail**: breadcrumbs, name-led header with status, actions, tabs, summary
  strip, and a details drawer for identifiers and revisions.
- **Settings**: sectioned form, human units, dirty state, revision-aware save
  bar, conflict explanation, and a separate dangerous zone.
- **Dashboard**: attention cards, trends, and recent activity over a time
  range.

Tables, catalogs, jobs, and principals use tabs rather than stacked cards:

- Table: Overview, Jobs, Health, Partitions/Files, Snapshots, Access, Settings.
- Catalog: Overview, Tables, Jobs, Access, Settings.
- Job: Overview, Tasks, Events, Artifacts.
- Principal: Access, Activity, Tokens (services only).

While a job is running, Events is the default job tab; once terminal, Overview
with impact statistics is the default.

### 4. Lists are searchable and URL-addressable

Search, filters, sorting, and column visibility are list-template
responsibilities, not page-specific additions. Supported state lives in
validated search parameters so links, back/forward, and saved views work.

- Filters and sorting are pushed to the API when the API supports them. A
  client-side filter is acceptable only for a bounded first page, never by
  fetching every row.
- Cursor pagination stays the contract. Saved views store filters and sort
  order, not result sets.
- At narrow widths, primary tables switch to a stacked card mode instead of
  forcing horizontal scrolling. Horizontal scroll remains acceptable for
  secondary technical tables, with an explicit scroll affordance.
- Density is a user preference with comfortable and compact modes, stored
  locally; it is not server state.

### 5. Owned primitives, not bespoke one-offs

Base UI already supplies Select, Dialog, Drawer, Menu, Popover, Tabs, Tooltip,
and Toast. The repository keeps thin styled wrappers around those primitives
instead of reimplementing behavior.

- The custom `web/src/ui/Select.tsx` is replaced by a Base UI `Select` wrapper.
- Shared additions: `ResourceLabel`, `DataTable`, `FilterBar`, `Breadcrumbs`,
  `CommandPalette`, `StatusBadge` variants, `Metric`, `DefinitionList`, and
  small SVG chart primitives.
- Dates, relative times, and numbers use `Intl`. A date library is not added.
- Charts are in-repository SVG with an accessible table fallback. A chart
  library is adopted only if a measured need justifies its bundle cost.
- Status vocabulary is centralized: execution, publication, health, lifecycle,
  and worker states each have one label, tone, and icon. Raw state names appear
  only in technical details.

### 6. Performance is a gate, not a later optimization

The UI must remain fast on a mid-tier laptop and must not grow by accident.

- Routes and heavy features are code-split. Charts, the command palette, and
  low-frequency administration pages are lazy.
- The bundle plugin distinguishes first-load JavaScript from lazy chunks.
  First-load JavaScript keeps the existing 175 kB gzipped budget. Lazy chunks
  are reported separately and require a measured justification; the gate is
  never weakened to accommodate a dependency.
- No runtime request leaves the deployment origin. ADR 0046's CSP, Trusted
  Types, no-inline-style, and no-external-asset rules still apply.
- The existing Playwright + axe gate remains the acceptance gate. The new
  Playwright MCP server is exploratory developer tooling; it supplements the
  committed browser journeys and never replaces them.

### 7. Accessibility and responsive behavior are acceptance criteria

- WCAG 2.2 AA in light and dark themes, with zero axe violations.
- Skip link, labeled `main`, landmarks, visible focus, keyboard-operable
  controls, target sizes, reduced motion, and text-plus-icon status.
- Primary journeys are keyboard-completable and tested at 320, 390, 768, 1024,
  and 1440 px.
- No horizontal document overflow. Narrow layouts use the card mode and drawer;
  they do not hide navigation or clip actions.

### 8. Delivery

The work is a platform workstream, proposed as **BS-032**, delivered in phases
rather than one branch:

1. Stabilize existing pages and split mixed-scope PRs.
2. Build the shell, templates, primitives, names-first pass, and route split.
3. Move administration and operations pages onto the templates, including
   BS-024 policies and schedules.
4. Adopt the templates for BS-025–BS-027 insight pages and add task/attempt
   triage.
5. Add BS-028 health and BS-029 notification pages.
6. Harden accessibility, performance, visual regression, documentation, and
   release notes.

Each task is still one branch and one pull request. Feature work that depends
on a new API may land its API first; the UI does not wait for the whole platform
phase. Existing pages migrate incrementally, and a page is not considered done
until its list, detail, empty, loading, error, mobile, and accessibility states
follow the template.

## Consequences

- Operators get a consistent, name-led interface that answers "what needs
  attention?" and supports job-to-task triage, instead of a collection of
  feature-specific screens.
- The UI gains a small owned component layer and a design review process. That
  is deliberate maintenance in exchange for consistency; it is less work than
  reimplementing behavior in each page.
- Several backend gaps become explicit: list search and filter parameters, a
  task/attempt view, aggregate endpoints for Overview, and resource display
  names in audit where safe. Those are scheduled with their feature tasks; the
  UI must not fake them with unbounded reads.
- Route splitting and the budget change add build complexity, but keep the
  product usable as pages and charts grow.
- Mobile becomes a supported surface for access and job triage. The old
  horizontally clipped navigation and four-column grant table are removed.
- Pages migrate over several releases. During migration, old and new patterns
  may coexist; each migrated page must fully adopt its template rather than
  mixing both.
- ADR 0046 remains in force for stack, tokens, CSP, BYOC, and the browser gate.
  This ADR adds the product-level interaction and performance rules.

## Alternatives considered

- **Keep the current shell and patch individual pages.** Rejected: each new
  page would invent its own hierarchy, and BS-024–BS-029 would deepen the
  inconsistency.
- **Copy the Databricks/GCP mega-menu.** Rejected: Bergsmith has a small,
  focused surface. A grouped rail of fewer than a dozen destinations is enough.
- **Adopt a full external design system or Storybook immediately.** Deferred:
  owned wrappers and the existing component tests cover current needs. A
  component gallery can be added later if design review requires it.
- **Add a charting library.** Deferred until a measured requirement and bundle
  headroom exist. Small SVG primitives cover the planned overview, impact, and
  health views.
- **Ship two UIs behind a flag.** Rejected for the first release: maintaining
  two shells doubles testing and documentation. Release candidates and design
  partners provide staging without a permanent dual path.
