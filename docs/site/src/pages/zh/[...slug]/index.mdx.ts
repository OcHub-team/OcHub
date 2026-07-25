/**
 * `/zh/<slug>/index.mdx` — the Simplified Chinese authored source twin.
 */

import { mdxTwinRoute } from "@/lib/agent-surface";
import { localeByKey } from "@/lib/i18n";

export const prerender = true;

export const { getStaticPaths, GET } = mdxTwinRoute(
  localeByKey("zh").collection,
);
