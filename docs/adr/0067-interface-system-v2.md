# 0067. Interface system v2

- Status: Proposed
- Date: 2026-10-29
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` BS-033

## Context

[0046](0046-web-ui-refresh-and-byoc-delivery.md) set the UI stack and tokens,
and [0056](0056-ui-platform-navigation-and-names-first-operation.md) set the shell,
templates, and performance rules. The pages that followed are consistent but
dense in the wrong places: a 14 px body with three weights, a blue accent that
competes with the status colors, flat surfaces that separate regions only with
borders, a fixed 15rem rail, a page column capped at `max-w-6xl`, and almost no
motion. Operators scan long lists of jobs and tables; the interface should
recede so that state stands out.

BS-033 redesigns the interface in phases (T1–T9). This decision fixes the
visual system, the motion rules, and the engineering limits every phase is
held to. Routes, permissions, API contracts, and the commit path are
unchanged.

## Decision

### Color

- The palette is neutral graphite with no brand accent. Color is reserved for
  status, and status is still always text and an icon (0046).
- Text has four tiers: `foreground`, `foreground-secondary`,
  `muted-foreground`, and `foreground-faint`. `faint` is decorative only
  (separators, disabled controls) and never carries information, because it
  is deliberately below 4.5:1.
- Surfaces form a lightness ladder: `surface-sunken`, `background`,
  `surface-raised` (cards), `popover`, `surface-overlay`. In the dark theme the
  steps are L 0.15, 0.17, 0.195, 0.22, and 0.24 with chroma at most 0.006, so
  elevation reads without shadows. Lines are a hairline `border` and a
  `border-strong` for controls.
- Filled actions (`primary`) are graphite: near-black in light, near-white in
  dark. Links take the text color; inside running text an underline marks
  them, elsewhere their position does and the underline appears on hover. The
  focus ring is the secondary text tier, at least 7:1 on every surface.
- Status tokens come in pairs: a strong value for text and icons, and a soft
  translucent value for fills: `success`, `warning`, `destructive`, `info`,
  and `running`. Strong text meets 4.5:1 on its own fill.

This supersedes 0046's "neutral graphite surfaces and one accent".

### Type

- Inter Variable, self-hosted from `@fontsource-variable/inter` (OFL-1.1). The
  build emits one file per script subset, each loaded only when the page uses
  it through `unicode-range`. The Latin file (about 48 kB) is preloaded by a
  build plugin, which fails the build if the file is missing. A system
  fallback metric-matched with `size-adjust` and ascent, descent, and line-gap
  overrides keeps the swap from shifting text. Monospace stays the system
  stack.
- One scale: 11, 12, 13 (body), 14, 16, and 20 px (`text-2xs` to `text-xl`),
  each with a fixed line height. Two weights: 400 and 600. Lint rejects the
  other weight utilities.
- Text fields render at 16 px on coarse pointers, so phones do not zoom into a
  focused field.

### Shape, layout, and motion

- Radius 0.5rem (8 px for cards and dialogs, 6 px for controls, 4 px for
  small items).
- Layout tokens: `sidebar-width` 14rem, `sidebar-width-collapsed` 3.25rem,
  `header-height` 3rem, `peek-width` `clamp(24rem, 40vw, 40rem)`.
- Motion reports state changes and never decorates. Durations are by role
  (`instant` 80 ms, `fast` 140 ms, `base` 200 ms, `emphasis` 360 ms) and
  easings by direction (`ease-out` entering, `ease-in` leaving,
  `ease-standard` moving). Only `opacity` and `transform` animate. Under
  `prefers-reduced-motion`, every duration token is zero, transitions are
  removed, and finite animations end at once.

### Primitives

- No animation library. Base UI exposes `data-starting-style` and
  `data-ending-style` on every popup, so CSS transitions on the motion tokens
  cover enter and exit without the bundle cost or a library that could
  inject `<style>` elements.
- The app renders inside Base UI's `CSPProvider` with
  `disableStyleElements`; the one scrollbar rule Base UI would inject lives
  in `styles.css`. The custom Select that 0056 kept for this reason can now
  be revisited; it stays until a task needs more than it offers.
- Floating primitives (menu, popover, tooltip) cost about 26 kB gzipped and
  stay out of the first-load bundle. Page-level uses load with their page
  chunk; shell-level menus use `MenuButton`, a plain eager button whose menu
  loads on hover, focus, or the first press and anchors to it.
- Menus and the peek panel are modal. Base UI's non-modal popups add
  focusable focus guards marked `aria-hidden`, which fail WCAG checks.
- Shell controls avoid primitives with a measurable cost: the segmented
  control is native `aria-pressed` buttons, and scroll panes use native thin
  scrollbars instead of a scroll-area component.
- Lint rejects `innerHTML` and `outerHTML` assignment,
  `insertAdjacentHTML`, `document.write`, `eval`, `new Function`, and the
  JSX `style` attribute, in addition to `dangerouslySetInnerHTML` (0046).

### Shell

- The rail collapses to its icons with a top-bar button or Ctrl+\ (⌘+\ on
  macOS). Like theme and density, the choice is a per-browser preference
  applied as `data-sidebar` on the root before the first render, so a
  collapsed rail never flashes open. A `rail-collapsed` variant hides the
  labels visually only: each link keeps its name and gains a tooltip.
  The width changes at once: animating the grid column would lay out the
  whole page on every frame, and only `opacity` and `transform` animate.
- The account (name, theme, density, sign-out) is one button at the foot of
  the rail; its panel is a modal popover that loads on first use, like
  `MenuButton`. The phone drawer keeps those controls inline.
- From `md` up, a sticky top bar holds the rail toggle, the breadcrumb,
  search, and New. Page content is no longer capped at `max-w-6xl`; it fills
  the column up to 96rem, so wide tables use wide screens.
- A new page fades in through the Web Animations API, keyed by the path
  only, so tabs and filters (search parameters) do not replay it and nothing
  remounts. It reads the duration token, so reduced motion turns it off.

### List API

- The jobs, tables, catalogs, audit, and principals lists take filters beside
  their page bounds. Filters combine with AND; one filter's comma-separated
  values (at most 16) combine with OR. An empty or unknown value is a 400, so
  a mistyped filter never widens a list.
- `q` searches display names only: a table alias's `namespace.name`, a
  catalog's name, a principal's display name, an audit actor's display name.
  It is trimmed, 1 to 128 characters, free of control characters, and
  matched with `ILIKE` after escaping `%`, `_`, and `\`. A table matches only
  through aliases the caller may see, so a name in a hidden catalog finds
  nothing.
- Jobs and audit take `order=newest`. Identifiers are UUIDv7 or a sequence,
  so the cursor stays one index range condition in either direction.
- Each list has a `/facets` read: per dimension, the values with their counts
  under every filter except that dimension's own, so a person sees what
  widening it would add. A value is exactly the parameter value that selects
  it. Counts run through the list's grant predicate in the same query, and
  the audit and principal facets keep their lists' organization-wide
  permission. Table health is evaluated, not stored, so its facet scans the
  visible inventory in the same bounded chunks as the health filter.
- Queries are built from compile-time fragments and bind every value. An
  absent filter adds no predicate, so each combination gets its own plan with
  a direct index condition rather than a nullable one. A filtered cursor
  carries a digest of its filter and order and is refused under any other.
- Migration 0040 indexes each job and audit filter as
  `(organization_id, value, id)`. A test seeds 5,000 rows with rare values and
  fails if a filtered page discards the organization's other rows. Name
  search has no index: the names live in inventory-sized tables.

### Engineering limits

These are enforced by gates, not by review:

- Bundle: first-load JavaScript stays within the 175 kB gzipped budget (0056),
  and BS-033 adds at most 15 kB to it. Stylesheets have their own gzipped
  budget in the same build plugin, set at about 10% over the 9.9 kB measured
  when this decision was made, and each lazy chunk is limited to 50 kB
  gzipped.
- Static files are compressed once, at build time: the build writes Brotli
  and gzip copies of every text file of at least 1 KiB, and the coordinator
  serves the best copy the request's `Accept-Encoding` admits, with
  `Vary: Accept-Encoding`, falling back to the file as built. Brotli cuts the
  entry script by about 13% over gzip. API responses stay uncompressed, so
  no response that carries a session's secrets is exposed to
  compression-oracle attacks.
- The coordinator's CSP and response headers do not change. Every new popup
  primitive and animation library passes the browser gate's policy check
  before a page uses it; positioning writes CSSOM properties, never `style`
  attributes (0056 kept the custom Select for that reason).
- Lists, facets, search, live streams, and saved views are scoped by grant
  inside the database query, so counts and suggestions cannot reveal
  resources outside the caller's grant.

## Consequences

- Status is easier to find: it is the only color on the page.
- Every existing size utility shrinks by one step, and every
  `font-medium` became `font-semibold`; T1 reviews each page once and
  re-records the visual baselines.
- The production build carries about 200 kB of font files, but a page
  downloads only the subsets it uses; most pages need only the Latin file.
- The browser gate now serves built files as bytes and fails on a font
  Chromium cannot decode, which it previously reported only as a warning.
- The stylesheet is close to its budget (10.8 of 11 kB after the shell);
  later phases must reuse utilities rather than add bespoke rules, or
  revisit the budget in this record.
- Later phases add the toolbar data table and the API additions on top of
  these tokens.
