# Console documentation patch · 2026-10-13

`docs.patch` targets the independent `ochub-docs` repository at
`e791d241d7a7d7a8566be7cd6efab659deef8038`. It changes 18 MDX files with 356 added lines.
It contains user documentation only; no documentation-site source or dependency
copy belongs in the OcHub repository.

SHA-256: `b28baa3cc80507e36dfaab30ade9be9ed6913554ff323e3648046ec00b5cf43b`

## Covered behavior

- New English, simplified Chinese, and Japanese Overview guides, linked from the
  existing feature index and product overview.
- Configured route inspection, app selections and draft counts, time ranges,
  trend values, retained request previews, independent section failures, remote
  capability requirements, and the distinction between configuration and
  observed request paths.
- Actual sidebar groups, collapsible app connections, fixed footer entry points,
  compact toasts, and the current-launch notification center. Reading or closing
  a notice is not described as resolving an operation.
- `ochcli overview`, explicit UTC-second bounds, `data` / `error` sections,
  preview limits, and the remote capability names.

The patch describes the implemented controls. It does not claim a global density
preference, request trace explorer, persistent notification audit, universal
remote plugin editing, or native validation on Windows/Linux.

## Validation

The source checkout was copied to `/tmp/ochub-console-docs-review` without `.git`,
`node_modules`, build output, or compiler targets. Installed dependencies were
read through a symlink to the existing docs dependencies. The copy's Astro and
Vite caches were explicitly directed to `/tmp`; those validation-only config
changes are not in the patch.

| Check | Result |
| --- | --- |
| `nimbus-docs lint` | 127 files clean |
| `node scripts/check-i18n-docs.mjs` | 42 pages aligned across en, zh, ja |
| `astro check` | 117 files; 0 errors, 0 warnings, 0 hints |
| `astro build` (site only) | 130 pages built |
| Pagefind | 126 pages, 3 languages indexed |
| `node scripts/check-wasm-routes.mjs` | 324 explicit routes; existing WASM 8.32 MiB, below 10 MiB limit |
| New localized pages | HTML, Markdown alternate, discovery link, and CLI fragment checked in generated output |
| `git apply --check docs.patch` against the independent docs repo | Passed |

The site build reports existing static redirect pages without outer `<html>` and
Pagefind's lack of stemming for Japanese and simplified Chinese. The search
bundle is present. Existing WASM assets were reused; no WASM code changed, so no
new Rust/WASM build or native GUI validation was performed for this patch.

Logs are `/tmp/ochub-console-docs-{typecheck,build-final,lint-final,i18n-final,wasm-routes-final}.log`.

## Applying the reviewed change

The current environment permits writes to OcHub and temporary directories, but
not the sibling `ochub-docs` checkout or its Git metadata. The sibling working
tree remains clean. The patch has therefore not been applied or committed there;
no push or deployment occurred in that source checkout.

A writable clone at `/tmp/ochub-console-docs-commit` now contains signed commit
`d421498db16a9fc23ab15e8b13cb0467eec4fa1c` on `docs/overview-20261013`, with the
original docs baseline as its parent. All 18 changed files match the validated
snapshot byte for byte. `/tmp/ochub-console-docs.bundle` preserves the commit,
and bundle verification passed. The SSH signature was verified using the
already-configured public signing key through a temporary allowed-signers file;
global signing and trust configuration was not changed. The commit still needs
to be integrated into the sibling checkout and published once access is restored.

The following patch-based instructions remain an alternative if the prepared
commit cannot be imported; do not apply the patch again after importing it.

```sh
cd ../ochub-docs
git apply --check ../OcHub/docs/design/ochub-console-2026-10-13/patches/docs.patch
git apply ../OcHub/docs/design/ochub-console-2026-10-13/patches/docs.patch
pnpm build:site
pnpm lint:docs
pnpm check:i18n
pnpm typecheck
pnpm check:wasm-routes
git add src/content/docs src/content/docs-zh src/content/docs-ja
git commit -m "docs: explain overview dashboard and notification center"
```
