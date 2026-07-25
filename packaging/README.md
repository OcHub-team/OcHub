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
