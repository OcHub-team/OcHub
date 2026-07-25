/**
 * Factories for the per-locale agent surface (`.md` / `.mdx` twins).
 *
 * The starter ships these routes hardcoded to the primary `docs`
 * collection, with a note that non-primary collections mount sibling
 * routes at `pages/<prefix>/[...slug]/index.md.ts`. Each locale is a
 * non-primary collection, so rather than copy the body three times we
 * build the route pair from the collection + locale here.
 */

import {
  getCollectionLlmsUrl,
  getIndexedEntries,
  renderEntryAsMarkdown,
  type IndexedEntry,
} from "@cloudflare/nimbus-docs";
import { config } from "virtual:nimbus/config";
import { localePath, type Locale } from "./i18n";

interface SlugProps {
  item: IndexedEntry;
}

/** Resolve the social image for an entry, falling back to the site default. */
function resolveSocialImage(entry: IndexedEntry["entry"]): string | undefined {
  const data = (entry.data ?? {}) as Record<string, unknown>;
  const raw = data.socialImage;
  return typeof raw === "string" && raw.length > 0 ? raw : config.socialImage;
}

/** Shared frontmatter block for both twins. */
function frontmatter(item: IndexedEntry): string[] {
  const { entry, title, description, version } = item;
  const socialImage = resolveSocialImage(entry);
  return [
    "---",
    `title: ${JSON.stringify(title)}`,
    ...(description ? [`description: ${JSON.stringify(description)}`] : []),
    ...(socialImage
      ? [`image: ${JSON.stringify(new URL(socialImage, config.site).href)}`]
      : []),
    ...(version ? [`version: ${JSON.stringify(version)}`] : []),
    "---",
  ];
}

/** `/<prefix>/<slug>/index.md` — downleveled render for reading. */
export function markdownTwinRoute(collection: string, locale: Locale) {
  async function getStaticPaths() {
    const indexed = await getIndexedEntries();
    return indexed
      .filter((item) => item.collection === collection)
      .map((item) => ({
        params: { slug: item.entry.id === "index" ? undefined : item.entry.id },
        props: { item } as SlugProps,
      }));
  }

  async function GET({ props }: { props: SlugProps }) {
    const { item } = props;
    const { entry, title, sourceUrl, markdownUrl } = item;

    // Point agents at this locale's index, not the English root — a
    // Japanese page should lead to the Japanese corpus.
    const llmsUrl = new URL(await getCollectionLlmsUrl(collection), config.site)
      .href;

    const body = [
      ...frontmatter(item),
      "",
      "> Documentation Index",
      `> Fetch the complete documentation index at: ${llmsUrl}`,
      "> Use this file to discover all available pages before exploring further.",
      "",
      `# ${title}`,
      "",
      renderEntryAsMarkdown(entry),
      "",
      `Source: ${new URL(sourceUrl ?? markdownUrl ?? localePath(locale, entry.id), config.site).href}`,
      "",
    ].join("\n");

    return new Response(body, {
      headers: { "Content-Type": "text/markdown; charset=utf-8" },
    });
  }

  return { getStaticPaths, GET };
}

/** `/<prefix>/<slug>/index.mdx` — raw authored source. */
export function mdxTwinRoute(collection: string) {
  async function getStaticPaths() {
    const indexed = await getIndexedEntries();
    return indexed
      .filter(
        (item) => item.collection === collection && item.sourceUrl !== undefined,
      )
      .map((item) => ({
        params: { slug: item.entry.id === "index" ? undefined : item.entry.id },
        props: { item } as SlugProps,
      }));
  }

  async function GET({ props }: { props: SlugProps }) {
    const { item } = props;
    const body = [...frontmatter(item), "", item.entry.body ?? ""].join("\n");

    return new Response(body, {
      headers: { "Content-Type": "text/markdown; charset=utf-8" },
    });
  }

  return { getStaticPaths, GET };
}
