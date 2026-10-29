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

### Engineering limits

These are enforced by gates, not by review:

- Bundle: first-load JavaScript stays within the 175 kB gzipped budget (0056),
  and BS-033 adds at most 15 kB to it. Stylesheets have their own gzipped
  budget in the same build plugin, set at about 10% over the 9.9 kB measured
  when this decision was made, and each lazy chunk is limited to 50 kB
  gzipped.
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
- Later phases add the collapsible shell, the motion primitives, the toolbar
  data table, and the API additions on top of these tokens.
