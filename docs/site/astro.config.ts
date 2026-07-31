import { defineConfig } from "astro/config";
import icon from "astro-icon";
import tailwindcss from "@tailwindcss/vite";
import nimbus, { defineConfig as defineNimbusConfig } from "@cloudflare/nimbus-docs";
import { tableScroll } from "@cloudflare/nimbus-docs/markdown";

const nimbusConfig = defineNimbusConfig({
  // Canonical origin (no trailing slash). Drives canonical URLs, absolute
  // OG image URLs, robots.txt, sitemap, and the links in /llms.txt.
  site: "https://docs.ochub.org",
  title: "OcHub",
  description:
    "A native desktop control center for AI coding tools. Switch model providers, manage shared capabilities, and run a local gateway from one place.",
  // Site-wide default only. Pages carry their own locale — see
  // src/lib/i18n.ts, which drives <html lang> and hreflang per page.
  locale: "en",
  github: "https://github.com/OcHub-team/OcHub",
  socialImageAlt: "OcHub documentation",

  // Locales are declared through the `versions` manifest, which is the
  // only mechanism Nimbus exposes for scoping navigation to a non-primary
  // collection: `buildStructuralTree()` ignores its `collection` argument
  // unless the slug appears in `versions.others`. Without this, every
  // locale renders the English sidebar, breadcrumbs and prev/next.
  //
  // What we take from it: per-collection nav trees and `/ja` + `/zh` URL
  // prefixes (`docs-<slug>` → `/<slug>`).
  //
  // What we override: versioning implies "these are the same page, the
  // current one is canonical." Translations are not duplicates, so
  // BaseLayout suppresses the cross-version canonical and emits
  // self-canonical + hreflang instead. See src/lib/i18n.ts.
  versions: {
    current: "en",
    others: ["ja", "zh"],
  },
});

export default defineConfig({
  output: "static",
  // Per-tool guides moved out of /guides/* into top-level groups. Keep the
  // old URLs alive for every locale (static meta-refresh pages).
  redirects: {
    "/guides/claude": "/claude",
    "/guides/claude-advanced": "/claude/advanced",
    "/guides/codex": "/codex",
    "/guides/codex-advanced": "/codex/advanced",
    "/guides/grok-build-advanced": "/grok-build/advanced",
    "/guides/opencode-advanced": "/opencode/advanced",
    "/guides/open-tools": "/guides",
    "/ja/guides/claude": "/ja/claude",
    "/ja/guides/claude-advanced": "/ja/claude/advanced",
    "/ja/guides/codex": "/ja/codex",
    "/ja/guides/codex-advanced": "/ja/codex/advanced",
    "/ja/guides/grok-build-advanced": "/ja/grok-build/advanced",
    "/ja/guides/opencode-advanced": "/ja/opencode/advanced",
    "/ja/guides/open-tools": "/ja/guides",
    "/zh/guides/claude": "/zh/claude",
    "/zh/guides/claude-advanced": "/zh/claude/advanced",
    "/zh/guides/codex": "/zh/codex",
    "/zh/guides/codex-advanced": "/zh/codex/advanced",
    "/zh/guides/grok-build-advanced": "/zh/grok-build/advanced",
    "/zh/guides/opencode-advanced": "/zh/opencode/advanced",
    "/zh/guides/open-tools": "/zh/guides",
  },
  // Tailwind v4 via its Vite plugin (the integration Astro recommends for
  // Tailwind v4 — replaces the PostCSS plugin, which doesn't build under
  // Astro 7's Vite 8 bundler).
  vite: {
    plugins: [tailwindcss()],
  },
  // Hover-prefetch link targets so full-page navigations feel instant without
  // a client-side router.
  prefetch: {
    prefetchAll: true,
    defaultStrategy: "hover",
  },
  integrations: [
    icon(),
    nimbus(nimbusConfig, {
      // Authoring rules are opt-in by design — your repo, your taste. The
      // two below are the load-bearing pair: frontmatter has to validate
      // against the content schema for the page to render properly, and
      // broken internal links are 404s for your readers. Add the others
      // (heading hierarchy, code-block language, style, etc.) when you're
      // ready to enforce them — see `nimbus-docs lint --help`.
      rules: {
        "nimbus/frontmatter-shape": "error",
        "nimbus/internal-link": "error",
      },
      // Wrap wide tables so they scroll instead of overflowing the page
      // (styled by `.nb-table-scroll` in src/styles/prose.css).
      markdown: {
        hastPlugins: [tableScroll()],
      },
    }),
  ],
});
