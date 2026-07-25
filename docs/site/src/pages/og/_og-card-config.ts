/**
 * Shared visual config for build-time OG cards.
 *
 * Edit this file to retune generated card colors, spacing, and fonts. Both
 * the per-page endpoint (`og/[...slug].ts`) and the homepage fallback
 * (`og.png.ts`) spread this object into `astro-og-canvas`.
 *
 * Leading underscore tells Astro to skip routing for this file — it sits
 * inside `src/pages/` to be next to its consumers, but it's not a route.
 */

import type { OGImageOptions } from "astro-og-canvas";

// Colors are the OCHUB_DARK palette from `crates/app/src/theme.rs`,
// converted to the RGB triples astro-og-canvas expects:
//   bg #151613 → surface #22231f   (gradient)
//   accentFill #3568c8             (border)
//   text #f1f0e8 / muted #96958d   (title / description)
export const ogCardConfig = {
  bgGradient: [
    [21, 22, 19],
    [34, 35, 31],
  ],
  border: { color: [53, 104, 200], width: 2, side: "inline-start" },
  padding: 96,
  fonts: ["./public/fonts/Inter-Bold.ttf"],
  font: {
    title: {
      color: [241, 240, 232],
      size: 64,
      weight: "Bold",
      families: ["Inter"],
      lineHeight: 1.1,
    },
    description: {
      color: [150, 149, 141],
      size: 32,
      weight: "Bold",
      families: ["Inter"],
      lineHeight: 1.3,
    },
  },
  format: "PNG",
} satisfies Partial<OGImageOptions>;
