/**
 * `/zh/<slug>/index.md` — the Simplified Chinese `.md` twin. Sibling
 * route to the primary collection's `pages/[...slug]/index.md.ts`, per
 * the non-primary-collection convention.
 */

import { markdownTwinRoute } from "@/lib/agent-surface";
import { localeByKey } from "@/lib/i18n";

export const prerender = true;

const locale = localeByKey("zh");
export const { getStaticPaths, GET } = markdownTwinRoute(
  locale.collection,
  locale,
);
