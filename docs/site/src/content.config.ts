import { defineCollection } from "astro:content";
// `z` re-exported from `astro:content` is deprecated; import it from
// `astro/zod` (the pattern nimbus-docs' own schema helpers document).
import { z } from "astro/zod";
import { docsCollection, partialsCollection } from "@cloudflare/nimbus-docs/content";

// Shared across every locale so a translated page validates identically
// to its English source.
const docsSchemaFields = {
  // Nimbus docs are agent-friendly by default. Set `audience: human`
  // to flag a page that's written primarily for human readers.
  audience: z.literal("human").optional(),
};

export const collections = {
  // Default locale (en) — the primary collection, mounted at the site root.
  docs: defineCollection(docsCollection({ schemaFields: docsSchemaFields })),

  // Translations. Registered as ordinary non-primary collections and
  // mounted under /ja and /zh by the route files in src/pages/.
  // See src/lib/i18n.ts for why this isn't Nimbus's `versions` feature.
  "docs-ja": defineCollection(
    docsCollection({ base: "docs-ja", schemaFields: docsSchemaFields }),
  ),
  "docs-zh": defineCollection(
    docsCollection({ base: "docs-zh", schemaFields: docsSchemaFields }),
  ),

  partials: defineCollection(partialsCollection()),
};
