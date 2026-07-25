/**
 * Shared data loading for the per-locale catch-all routes.
 *
 * `src/pages/[...slug].astro` (en), `src/pages/ja/[...slug].astro` and
 * `src/pages/zh/[...slug].astro` are byte-for-byte identical apart from
 * which locale they serve, so the work lives here and each route file
 * stays a thin shell. Keeping them as separate route files (rather than
 * one `[locale]/[...slug]`) is deliberate: the default locale is mounted
 * at the site root with no prefix, which a single dynamic segment can't
 * express.
 */

import type { AstroGlobal } from "astro";
import type { SidebarItem, TOCItem } from "@cloudflare/nimbus-docs";
import {
  getBreadcrumbs,
  getCollectionPageProps,
  getEditUrl,
  getLastUpdated,
  getPrevNext,
  getRouteFlags,
  getSidebar,
  getTOC,
} from "@cloudflare/nimbus-docs";
import { getLocaleAlternates, localePath, t, type Locale } from "./i18n";

/**
 * The collections that hold documentation pages, one per locale. Passing
 * the literal union (rather than `string`) to `getCollectionPageProps`
 * matters: the helper is generic over the collection name, and a bare
 * `string` collapses `CollectionEntry<C>` to `never`.
 */
type DocsCollection = "docs" | "docs-ja" | "docs-zh";

export async function getLocalePageData(astro: AstroGlobal, locale: Locale) {
  const { entry, Content, headings } =
    await getCollectionPageProps<DocsCollection>(astro);

  const currentSlug = astro.url.pathname.replace(/\/$/, "") || "/";

  const { sidebar: sidebarOn, tableOfContents: tocOn } = await getRouteFlags(entry);

  // Every navigation helper is passed the locale's collection so sidebar
  // links, breadcrumbs and prev/next stay inside the current language.
  // Annotated rather than inferred: the ternary widens to
  // `boolean | SidebarItem[]`, but the layout prop is `false | SidebarItem[]`.
  const sidebar: false | SidebarItem[] = sidebarOn
    ? await getSidebar(currentSlug, { collection: entry.collection })
    : false;
  const prevNext = await getPrevNext(currentSlug, {
    sidebarTree: sidebar === false ? [] : sidebar,
    overrides: { prev: entry.data.prev, next: entry.data.next },
  });
  // `getBreadcrumbs` always roots the trail at the site home — label
  // "Home", href "/" — regardless of collection, which lands a reader of
  // the Chinese docs back on the English landing page. Repoint the root
  // crumb at this locale's home instead.
  const rawBreadcrumbs = await getBreadcrumbs(currentSlug, {
    collection: entry.collection,
  });
  const breadcrumbs = rawBreadcrumbs.map((crumb, i) =>
    i === 0 && crumb.href === "/"
      ? { ...crumb, label: t(locale, "home"), href: `${locale.prefix}/` }
      : crumb,
  );
  const editUrl = await getEditUrl(entry);
  const lastUpdated = entry.data.lastUpdated ?? (await getLastUpdated(entry));

  const tocConfig = entry.data.tableOfContents;
  const toc: false | TOCItem[] =
    tocOn && tocConfig !== false ? getTOC(headings, tocConfig) : false;

  // `.md` twin and OG image live under the locale prefix so the agent
  // surface mirrors the human one per language.
  const markdownPath = `${localePath(locale, entry.id)}/index.md`.replace(
    /\/{2,}/g,
    "/",
  );
  const markdownUrl = astro.site
    ? new URL(markdownPath, astro.site).href
    : markdownPath;
  // Per-page OG cards only where the card font covers the script; ja/zh
  // fall back to the site-wide image rather than rendering boxes. See
  // `Locale.hasOgFont`.
  const socialImage =
    entry.data.socialImage ??
    (locale.hasOgFont ? `/og/${entry.id}.png` : undefined);

  const alternates = await getLocaleAlternates(entry.id);

  return {
    entry,
    Content,
    sidebar,
    toc,
    breadcrumbs,
    prevNext,
    editUrl,
    lastUpdated,
    markdownUrl,
    socialImage,
    alternates,
  };
}
