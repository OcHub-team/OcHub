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

## macOS signing paths

`package-macos.sh` picks one of three, and verifies the result before the
release proceeds. The verification matters: an unsigned bundle still *packages*
successfully, so without it a release ships green while every user is told the
app is damaged.

| Configured | User experience on first launch |
| --- | --- |
| Developer ID signing + notarization secrets | Opens with no warning |
| Developer ID signing only | Apple identity is present, but Gatekeeper can still require approval |
| `MACOS_SELFSIGN_CERTIFICATE` + identity | "Unverified developer" — approve once in System Settings › Privacy & Security |
| Neither | **"App is damaged"** — most users delete it |

### Why unsigned reads as "damaged"

Apple Silicon requires every binary to carry at least an ad-hoc signature, so
the linker adds one automatically. That signature declares the binary belongs to
a signed bundle, but nothing signs the bundle, so no `Contents/_CodeSignature`
is produced. `codesign --verify` then reports *"code has no resources but
signature indicates they must be present"*, and macOS treats a
present-but-broken signature as tampering rather than as merely unsigned.

### Self-signed certificate

A stopgap until a Developer ID is available. It does **not** let users install
by double-clicking — Gatekeeper's trust anchor is Apple's root CA and a
self-signed certificate has no chain to it. What it buys is a valid seal
(so "damaged" becomes the far less alarming "unverified developer") and a stable
Designated Requirement, so a user's per-app approvals survive updates instead of
being re-requested on every build.

```sh
openssl req -x509 -newkey rsa:2048 -keyout key.pem -out cert.pem -days 3650 \
    -nodes -subj "/CN=OcHub Self Signed/O=OcHub/C=CN" \
    -addext basicConstraints=critical,CA:false \
    -addext keyUsage=critical,digitalSignature \
    -addext extendedKeyUsage=critical,codeSigning
# macOS Security rejects OpenSSL 3's default PKCS#12 MAC, hence the legacy flags
openssl pkcs12 -export -out selfsigned.p12 -inkey key.pem -in cert.pem \
    -macalg sha1 -certpbe PBE-SHA1-3DES -keypbe PBE-SHA1-3DES
security find-identity -p codesigning   # copy the 40-character SHA-1
```

| Secret | Value |
| --- | --- |
| `MACOS_SELFSIGN_CERTIFICATE` | `base64 -i selfsigned.p12` |
| `MACOS_SELFSIGN_CERTIFICATE_PASSWORD` | the export password |
| `MACOS_SELFSIGN_IDENTITY` | the certificate's **SHA-1 fingerprint** |

The identity must be the fingerprint, not the common name. macOS never counts a
self-signed certificate as a *valid* identity (`security find-identity -v`
reports zero), so `codesign -s "OcHub Self Signed"` fails with "no identity
found"; referring to it by hash is what works.

## Platform signing

GitHub release builds require Developer ID signing and fail rather than falling
back to a self-signed or unsigned macOS package. Configure these GitHub Actions
secrets:

- `APPLE_SIGNING_IDENTITY`
- `APPLE_CERTIFICATE` (base64-encoded Developer ID Application `.p12`)
- `APPLE_CERTIFICATE_PASSWORD`
- `APPLE_TEAM_ID`

Signing and notarization are separate. To notarize the signed app and DMG so
Gatekeeper allows first launch without manual approval, additionally configure
both:

- `APPLE_ID`
- `APPLE_PASSWORD` (an app-specific password)

Local packaging still supports self-signed and unsigned fallbacks for forks and
development. Set `MACOS_REQUIRE_DEVELOPER_ID_SIGNATURE=true` to make a local run
fail unless Developer ID credentials are present.

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
