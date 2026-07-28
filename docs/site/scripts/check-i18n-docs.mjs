import { readdir, readFile } from "node:fs/promises";
import path from "node:path";

const contentRoot = path.resolve("src/content");
const locales = {
  en: path.join(contentRoot, "docs"),
  zh: path.join(contentRoot, "docs-zh"),
  ja: path.join(contentRoot, "docs-ja"),
};

async function listMdx(directory, prefix = "") {
  const entries = await readdir(directory, { withFileTypes: true });
  const files = [];

  for (const entry of entries) {
    const relative = path.posix.join(prefix, entry.name);
    if (entry.isDirectory()) {
      files.push(...(await listMdx(path.join(directory, entry.name), relative)));
    } else if (entry.isFile() && entry.name.endsWith(".mdx")) {
      files.push(relative);
    }
  }

  return files.sort();
}

function countMatches(source, pattern) {
  return [...source.matchAll(pattern)].length;
}

const filesByLocale = Object.fromEntries(
  await Promise.all(
    Object.entries(locales).map(async ([locale, directory]) => [
      locale,
      await listMdx(directory),
    ]),
  ),
);

const sourceFiles = filesByLocale.en;
const failures = [];

for (const locale of ["zh", "ja"]) {
  const localized = new Set(filesByLocale[locale]);
  for (const file of sourceFiles) {
    if (!localized.has(file)) {
      failures.push(`${locale}: missing ${file}`);
    }
  }
  for (const file of localized) {
    if (!sourceFiles.includes(file)) {
      failures.push(`${locale}: extra ${file}`);
    }
  }
}

for (const file of sourceFiles) {
  const entries = await Promise.all(
    Object.entries(locales).map(async ([locale, directory]) => {
      const source = await readFile(path.join(directory, file), "utf8");
      return {
        locale,
        headings: countMatches(source, /^## /gm),
        figures: countMatches(
          source,
          /<OcHub(?:AppFigure|ConnectionMap|StepsVisual)\b/g,
        ),
        hasTitle: /^title:\s*\S+/m.test(source),
      };
    }),
  );

  const expected = entries[0];
  for (const entry of entries) {
    if (!entry.hasTitle) {
      failures.push(`${entry.locale}: ${file} has no title`);
    }
    if (entry.headings !== expected.headings) {
      failures.push(
        `${file}: heading count en=${expected.headings}, ${entry.locale}=${entry.headings}`,
      );
    }
    if (entry.figures !== expected.figures) {
      failures.push(
        `${file}: figure count en=${expected.figures}, ${entry.locale}=${entry.figures}`,
      );
    }
  }
}

if (failures.length > 0) {
  console.error("Documentation i18n parity check failed:");
  for (const failure of failures) console.error(`- ${failure}`);
  process.exitCode = 1;
} else {
  console.log(
    `Documentation i18n parity: ${sourceFiles.length} pages aligned across en, zh, and ja.`,
  );
}
