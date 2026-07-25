# Desktop release packaging

OcHub uses `cargo-packager` 0.11.8 and native GitHub-hosted runners. A tag
matching the Cargo workspace version (for example `v0.1.0`) produces:

- macOS Apple Silicon and Intel DMGs
- a Windows x64 NSIS installer and portable ZIP
- a Linux x64 AppImage and Debian package
- `SHA256SUMS` plus a GitHub build-provenance attestation

The tag is rejected when it does not exactly match the workspace version.
Transient workflow artifacts are kept for one day; the durable files live in
the GitHub Release.

## Updater signing key

The in-app updater installs a downloaded package only after verifying it
against a minisign public key compiled into the binary. This is separate from
platform code signing and from `SHA256SUMS`: those checksums live in the same
release as the packages, so they detect a corrupted download but not a forged
release. Without this key pair, builds still check for updates and still report
new versions — they refuse to install, and point at the release page instead.

Generate a key pair once, using the packager already pinned above:

```sh
cargo packager signer generate --path ochub-updater.key
```

Then configure:

| Where | Name | Value |
| --- | --- | --- |
| Repository **variable** | `OCHUB_UPDATER_PUBKEY` | contents of `ochub-updater.key.pub` |
| Repository **secret** | `OCHUB_SIGNING_PRIVATE_KEY` | contents of `ochub-updater.key` |
| Repository **secret** | `OCHUB_SIGNING_PRIVATE_KEY_PASSWORD` | the password, if one was set |

The public key is a repository *variable* rather than a secret on purpose: it
is public by construction, and keeping it readable lets anyone check that a
release's signatures match the key shipped in the binary.

Keep the private key backed up outside the repository. Losing it means no
existing install can verify a future release, and every user has to reinstall
by hand; rotating it has the same effect, because the old public key is baked
into every binary already in the field.

Signed releases additionally publish `latest.json`, which the updater fetches
from `releases/latest/download/latest.json`. Only formats the updater can
apply appear in it — the macOS `.app.tar.gz` payloads, the Windows NSIS
installer, and the Linux AppImage. A `.deb` is owned by the package manager and
the Windows portable ZIP has no installer to re-run, so both stay check-only.

## Optional platform signing

Unsigned packages are built when no signing secrets are configured. To enable
macOS signing and notarization, configure all of these GitHub Actions secrets:

- `APPLE_SIGNING_IDENTITY`
- `APPLE_CERTIFICATE` (base64-encoded Developer ID Application `.p12`)
- `APPLE_CERTIFICATE_PASSWORD`
- `APPLE_ID`
- `APPLE_PASSWORD` (an app-specific password)
- `APPLE_TEAM_ID`

To enable Windows Authenticode signing, configure both:

- `WINDOWS_CERTIFICATE_BASE64` (base64-encoded code-signing `.pfx`)
- `WINDOWS_CERTIFICATE_PASSWORD`

Secrets are consumed only by the tag release workflow. Pull-request CI has
read-only repository permissions and never receives release secrets.

## Local packaging

Install the pinned packager first:

```sh
cargo install cargo-packager --version 0.11.8 --locked
```

Then run the platform-native script:

```sh
./scripts/release/package-macos.sh
./scripts/ci/install-linux-deps.sh
./scripts/release/package-linux.sh
```

On Windows PowerShell:

```powershell
./scripts/release/package-windows.ps1
```
