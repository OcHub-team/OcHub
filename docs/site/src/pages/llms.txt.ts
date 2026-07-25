// Root /llms.txt — sectioned index for AI agents.
import { getIndexedTopLevel } from "@cloudflare/nimbus-docs";
import { config } from "virtual:nimbus/config";
import { localeSectionLabel } from "@/lib/i18n";

export const prerender = true;

export async function GET() {
  const { leaves, groups } = await getIndexedTopLevel();

  const lines = [
    `# ${config.title}`,
    "",
    config.description ?? "Documentation index for AI agents.",
    "",
    `Full corpus (all pages, one document): ${new URL("/llms-full.txt", config.site).href}`,
    "",
    "## Pages",
    "",
  ];

  // Sort leaves + groups alphabetically into a single stable list.
  type Row = { key: string; line: string };
  const rows: Row[] = [];

  for (const leaf of leaves) {
    const description = leaf.description ? ` — ${leaf.description}` : "";
    rows.push({
      key: leaf.url,
      line: `- [${leaf.title}](${new URL(leaf.markdownUrl, config.site).href})${description}`,
    });
  }

  for (const group of groups) {
    // This site's "versions" are locales (see astro.config.ts), so the
    // default rule — skip version groups, they're older docs nobody
    // should be indexing — would hide every translation from the root
    // agent index. Locales are listed; a genuine older version still
    // isn't.
    const localeLabel = localeSectionLabel(group.slug);
    if (group.kind === "version" && !localeLabel) continue;
    rows.push({
      key: `/${group.slug}`,
      line: `- [${localeLabel ?? group.label}](${new URL(`/${group.slug}/llms.txt`, config.site).href})`,
    });
  }

  rows.sort((a, b) => a.key.localeCompare(b.key));
  for (const row of rows) lines.push(row.line);

  lines.push("");

  return new Response(lines.join("\n"), {
    headers: { "Content-Type": "text/plain; charset=utf-8" },
  });
}
