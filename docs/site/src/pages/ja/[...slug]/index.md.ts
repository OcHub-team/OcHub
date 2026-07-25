/**
 * `/ja/<slug>/index.md` — the Japanese `.md` twin. Sibling route to the
 * primary collection's `pages/[...slug]/index.md.ts`, per the
 * non-primary-collection convention. Body lives in lib/agent-surface.ts.
 */

import { markdownTwinRoute } from "@/lib/agent-surface";
import { localeByKey } from "@/lib/i18n";

export const prerender = true;

const locale = localeByKey("ja");
export const { getStaticPaths, GET } = markdownTwinRoute(
  locale.collection,
  locale,
);
