# Shared UI review — 2026-10-13

Reviewed the staged `../ochub-ui` implementation of tokens, navigation, tables,
notifications, layout, controls, text input, and gallery. The sibling repository
was read only. These patches are unapplied review artifacts for validation in a
permitted working copy. No Cargo process or GUI was used for this review.

## Required corrections

1. **High — toast hit testing (`src/notifications.rs:998`)**. The transient
   stack has a normal GPUI hitbox, so a click on its title or blank card area can
   also reach a control behind it. The pinned GPUI implementation dispatches
   clicks to every matching, unoccluded hitbox; painting an opaque background
   does not block input. `01-toast-hit-testing.patch` blocks pointer interactions
   at the actual stack boundary while retaining page scrolling. The boundary is
   the stack rather than individual cards so its own hover listener still pauses
   the expiry timer. The added test clicks a toast over another button, checks
   that the button did not activate and the timer is held, then confirms the
   button remains clickable once the toast is dismissed. Run this test before
   and after the implementation hunk to establish the regression.

2. **Medium — secondary text contrast on tinted surfaces
   (`src/navigation.rs:31`, `src/notifications.rs:695,730,779,797,805,847`)**.
   The normal `muted` token is validated against `surface`, but these labels use
   `panel`, `surface_hover`, or `overlay`. Direct sRGB contrast calculations give
   4.35:1 for OcHub light muted/panel, 3.97:1 for muted/hover, 4.47:1 for Ember
   dark muted/overlay and 4.07:1 for muted/hover. These are 12px captions, not
   large text. `02-console-secondary-text-contrast.patch` uses the existing
   `subtext` token in those components. It is above 4.5:1 on all three surfaces
   for all four builtin palettes; user theme files and global palette tokens
   remain unchanged.

## Findings that do not require another feature

- The public notification helpers and request builder methods remain available;
  `.message()` maps to details. History remains in memory and the previous
  `clear()` behavior continues to clear transient previews only.
- Deduplicated requests retain their ID and obtain a globally advancing timer
  epoch; stale timers cannot dismiss a refreshed notification. Hover and
  keyboard focus pause countdowns. Transient eviction retains history.
- The center bounds its content to the viewport and separates scrollable text
  from fixed close/copy/read actions. Long unbroken details already have a
  dedicated layout test, including a 420×400 window.
- Modal focus scope, Tab containment, Escape, and detail/back refocus are wired
  through the shared modal implementation and covered by existing tests. The
  native host should still verify focus after dismissing a center opened from a
  toast, because the original toast disappears during that transition.
- Table header and data rows share one column definition and minimum width;
  callers remain responsible for the documented horizontal scroll viewport.
- New 28/32px controls are sensible desktop targets and leave a 20px input line
  box. Disabled controls remain out of the tab order. The staged changes preserve
  the prior limitation that disabled status is not explicitly reported in AX;
  the pinned GPUI fluent API has no `aria_disabled` helper.
- New components consume semantic theme colors; no arbitrary brand color or
  platform-specific font has been embedded in the shared control builders.

## Validation still owned by integration

Integration update: both patches were applied in `/tmp/ochub-console-ui` and
all 104 library tests passed, including the pointer regression. The combined
snapshot also passed workspace compilation and Clippy. The source sibling
remains unchanged by these review patches; they are included in
`ochub-ui-complete.patch` for a clean baseline. Native GUI validation is still
pending because the current session has no required computer-use tool and
`just qa-app` stopped at the Xcode icon compiler. See
`../../../CONSOLE-IMPLEMENTATION-2026-10-13.md` for the exact integration state.

Apply both patches to the permitted library working copy, run the added pointer
regression and the existing UI tests, then build the fixed QA app. Native QA
should include toast-over-control clicks, hover/keyboard expiry pause, center
open/close focus, and light/dark long-details screenshots. This review does not
claim Windows or Linux native rendering was exercised.
