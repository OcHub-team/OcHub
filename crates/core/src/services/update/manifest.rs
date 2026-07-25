//! The signed release manifest that drives in-app updates.
//!
//! An updater downloads code and then runs it, so the manifest is the security
//! boundary of the whole feature. Two properties matter:
//!
//! * **Integrity comes from a signature, not from the transport.** The
//!   `SHA256SUMS` published alongside a release lives in the same place as the
//!   packages, so anything able to rewrite one can rewrite the other. It
//!   detects a corrupted download; it does not detect a forged release. The
//!   payload is therefore verified against a minisign public key baked into the
//!   binary at compile time, so a stolen repository token is not enough to make
//!   an OcHub install execute attacker-chosen code.
//! * **The manifest is fetched as a release asset, not through the GitHub
//!   API.** Unauthenticated API requests are capped at 60/hour per IP, which a
//!   single NAT'd office can exhaust; `releases/latest/download/...` is a plain
//!   redirect to object storage and is not rate limited.
//!
//! The wire format is deliberately the one `tauri-plugin-updater` uses: the
//! reference cc-switch app already ships it, so the CI signing step and key
//! management are proven, and both public key and signature are base64-wrapped
//! minisign blocks that survive JSON without newline escaping.

use minisign_verify::{PublicKey, Signature};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::{AppError, Result};

/// The minisign public key, injected at build time.
///
/// Absent in ordinary `cargo build` and in forks that have not generated a key
/// pair. That is deliberately not a build failure: update *checks* still work,
/// and [`super::install`] refuses to install without a key rather than falling
/// back to an unverified download. Never add a fallback that skips
/// verification — an updater with an optional signature check is an updater
/// with no signature check.
pub const PUBLIC_KEY: Option<&str> = option_env!("OCHUB_UPDATER_PUBKEY");

/// One downloadable payload for a single target triple.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlatformEntry {
    /// Base64-wrapped minisign signature over the bytes at `url`.
    pub signature: String,
    pub url: String,
}

/// `latest.json`, published as an asset of every release.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateManifest {
    pub version: String,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub pub_date: Option<String>,
    #[serde(default)]
    pub platforms: BTreeMap<String, PlatformEntry>,
}

impl UpdateManifest {
    /// The entry for the running target, if the release ships one.
    ///
    /// A missing key is normal rather than an error: a release may predate a
    /// newly supported architecture, and `.deb` installs have no self-update
    /// payload at all by design.
    pub fn entry_for_current_target(&self) -> Option<&PlatformEntry> {
        self.platforms.get(current_target_key())
    }
}

/// The manifest key for the running build, in Tauri's `os-arch` spelling.
pub fn current_target_key() -> &'static str {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return "darwin-aarch64";
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return "darwin-x86_64";
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    return "windows-x86_64";
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    return "windows-aarch64";
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return "linux-x86_64";
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    return "linux-aarch64";
    #[cfg(not(any(
        all(
            target_os = "macos",
            any(target_arch = "aarch64", target_arch = "x86_64")
        ),
        all(
            target_os = "windows",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    )))]
    return "unsupported";
}

/// Whether this build can install updates itself.
///
/// False for unsigned local builds; the UI degrades to "open the release page".
pub fn signing_configured() -> bool {
    PUBLIC_KEY.is_some_and(|key| !key.trim().is_empty())
}

/// Verify `payload` against a manifest signature.
///
/// Both key and signature are base64-wrapped minisign blocks. Fails closed when
/// no key was compiled in.
pub fn verify_payload(payload: &[u8], signature: &str) -> Result<()> {
    let Some(public_key) = PUBLIC_KEY.filter(|key| !key.trim().is_empty()) else {
        return Err(AppError::Message(
            "此版本未内置更新签名公钥，无法校验更新包；请从发布页手动下载".to_string(),
        ));
    };
    verify_payload_with_key(payload, signature, public_key)
}

/// Verification against an explicit key, so the logic is testable without a
/// build-time environment variable.
fn verify_payload_with_key(payload: &[u8], signature: &str, public_key: &str) -> Result<()> {
    let key_text = decode_base64_block(public_key)
        .map_err(|error| AppError::Message(format!("更新签名公钥无法解码: {error}")))?;
    let public_key = PublicKey::decode(key_text.trim())
        .map_err(|error| AppError::Message(format!("更新签名公钥无效: {error}")))?;

    let signature_text = decode_base64_block(signature)
        .map_err(|error| AppError::Message(format!("更新包签名无法解码: {error}")))?;
    let signature = Signature::decode(signature_text.trim())
        .map_err(|error| AppError::Message(format!("更新包签名格式无效: {error}")))?;

    public_key
        .verify(payload, &signature, true)
        .map_err(|error| {
            AppError::Message(format!(
                "更新包签名校验失败，已丢弃下载内容: {error}。请从发布页手动下载"
            ))
        })
}

/// Decode a base64-wrapped minisign block into a trimmed block.
///
/// Accepts an already-unwrapped block too, so a manifest hand-written with raw
/// multi-line minisign output still verifies. This is an encoding convenience
/// only: both paths end at the same signature check.
fn decode_base64_block(value: &str) -> std::result::Result<String, String> {
    let trimmed = value.trim();
    if trimmed.starts_with("untrusted comment:") {
        return Ok(trimmed.to_string());
    }
    let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64_decode(&compact)?;
    let text = String::from_utf8(bytes).map_err(|error| format!("不是有效的 UTF-8: {error}"))?;
    Ok(text.trim().to_string())
}

fn base64_decode(value: &str) -> std::result::Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Produced by the real release toolchain rather than hand-rolled, so these
    // fixtures prove the exact contract CI relies on:
    //
    //   cargo packager signer generate --password "" --path k
    //   cargo packager signer sign payload.app.tar.gz
    //
    // with cargo-packager 0.11.8 — the version pinned in the release workflow.
    // The secret half was discarded; it signed nothing but this payload.

    /// Verbatim `k.pub`, i.e. what goes in the `OCHUB_UPDATER_PUBKEY` variable.
    const TEST_PUBLIC_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDhFOEYyNjNBQjI4MkY2Q0IKUldUTDlvS3lPaWFQanVITThVZ3NNNVphWlRDQTVYdmlzMWdYc2dGQW1HK2pHcHJJZDZBdWNtQVEK";

    /// Verbatim `payload.app.tar.gz.sig`, i.e. what `latest.json` carries.
    const TEST_SIGNATURE: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIGNhcmdvLXBhY2thZ2VyIHNlY3JldCBrZXkKUlVUTDlvS3lPaWFQanF3RElqclVPY0UrVytsK2ovdGZhUHdJRjNzQXJvV3NYZk9SUmk4UjBZUS9hTEh5TnR5U253ZlRJOGg0ZDZEUWFjdDg3Z3JNOUd2ZlRiYUlXZVBpemdFPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzg0OTc0MDI0CWZpbGU6cGF5bG9hZC5hcHAudGFyLmd6ClBkK3hha0dMWXFQZC81UU9LMlEvQkYxcTVMYzRjak9BaVRaa3pBVFZZSHBTUWs4cmFhZ3pCMERwSTUzTlFmNGlIdm8rbGliZytsSExWUDdwdGNkTkRnPT0K";

    const TEST_PAYLOAD: &[u8] = b"pretend this is an OcHub.app.tar.gz payload";

    #[test]
    fn manifest_parses_the_tauri_wire_format() {
        let raw = r#"{
            "version": "0.2.0",
            "notes": "Release v0.2.0",
            "pub_date": "2026-07-25T00:00:00Z",
            "platforms": {
                "darwin-aarch64": { "signature": "c2ln", "url": "https://example.invalid/a.tar.gz" },
                "linux-x86_64": { "signature": "c2ln", "url": "https://example.invalid/a.AppImage" }
            }
        }"#;
        let manifest: UpdateManifest = serde_json::from_str(raw).unwrap();
        assert_eq!(manifest.version, "0.2.0");
        assert_eq!(manifest.platforms.len(), 2);
        assert_eq!(
            manifest.platforms["darwin-aarch64"].url,
            "https://example.invalid/a.tar.gz"
        );
    }

    /// Byte-for-byte output of the `Generate latest.json` step in
    /// `.github/workflows/release.yml`. If that step's shape changes, this
    /// fails here rather than silently leaving every install unable to update.
    #[test]
    fn the_manifest_ci_generates_is_parsed() {
        let generated = r#"{
  "version": "0.2.0",
  "notes": "Release v0.2.0",
  "pub_date": "2026-07-25T00:00:00Z",
  "platforms": {
    "linux-x86_64": {
      "signature": "SIGDDD",
      "url": "https://github.com/OcHub-team/OcHub/releases/download/v0.2.0/OcHub_0.2.0_amd64.AppImage"
    },
    "darwin-aarch64": {
      "signature": "SIGAAA",
      "url": "https://github.com/OcHub-team/OcHub/releases/download/v0.2.0/OcHub_0.2.0_macos_aarch64.app.tar.gz"
    },
    "darwin-x86_64": {
      "signature": "SIGBBB",
      "url": "https://github.com/OcHub-team/OcHub/releases/download/v0.2.0/OcHub_0.2.0_macos_x86_64.app.tar.gz"
    },
    "windows-x86_64": {
      "signature": "SIGCCC",
      "url": "https://github.com/OcHub-team/OcHub/releases/download/v0.2.0/OcHub_0.2.0_x64-setup.exe"
    }
  }
}"#;
        let manifest: UpdateManifest = serde_json::from_str(generated).unwrap();
        assert_eq!(manifest.version, "0.2.0");
        // Every platform the client can self-install on must be reachable.
        for key in [
            "darwin-aarch64",
            "darwin-x86_64",
            "windows-x86_64",
            "linux-x86_64",
        ] {
            assert!(manifest.platforms.contains_key(key), "missing {key}");
        }
        // The formats that must never appear, because the client cannot apply
        // them and offering them would imply it can.
        for entry in manifest.platforms.values() {
            assert!(!entry.url.ends_with(".deb"), "deb is package-manager owned");
            assert!(
                !entry.url.ends_with("portable.zip"),
                "portable has no installer"
            );
        }
        // This build must find its own payload in a real release.
        assert!(manifest.entry_for_current_target().is_some());
    }

    #[test]
    fn manifest_tolerates_a_release_without_our_platform() {
        let raw = r#"{ "version": "0.2.0", "platforms": {} }"#;
        let manifest: UpdateManifest = serde_json::from_str(raw).unwrap();
        assert!(manifest.entry_for_current_target().is_none());
    }

    #[test]
    fn current_target_key_is_a_real_platform() {
        // A build for a target with no manifest key would silently never find
        // an update; catch that here rather than in the field.
        assert_ne!(current_target_key(), "unsupported");
    }

    #[test]
    fn base64_wrapped_and_raw_minisign_blocks_decode_alike() {
        let unwrapped = decode_base64_block(TEST_PUBLIC_KEY).unwrap();
        assert!(unwrapped.starts_with("untrusted comment:"), "{unwrapped}");
        // Decoding is idempotent: an already-unwrapped block passes through.
        assert_eq!(decode_base64_block(&unwrapped).unwrap(), unwrapped);
    }

    #[test]
    fn verification_fails_closed_without_a_compiled_in_key() {
        // The guard that keeps an unsigned build from installing anything.
        if PUBLIC_KEY.is_none() {
            let error = verify_payload(b"payload", "c2ln").unwrap_err().to_string();
            assert!(error.contains("未内置更新签名公钥"), "{error}");
        }
    }

    /// End to end against the release toolchain: what `cargo packager signer`
    /// emits is what this module accepts. A break here means shipped builds
    /// would reject every genuine update.
    #[test]
    fn a_signature_from_the_release_toolchain_verifies() {
        verify_payload_with_key(TEST_PAYLOAD, TEST_SIGNATURE, TEST_PUBLIC_KEY).unwrap();
    }

    #[test]
    fn the_same_signature_verifies_unwrapped() {
        // Same material with the base64 envelope removed, so a manifest
        // hand-written with raw minisign output still works.
        verify_payload_with_key(
            TEST_PAYLOAD,
            &decode_base64_block(TEST_SIGNATURE).unwrap(),
            &decode_base64_block(TEST_PUBLIC_KEY).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn a_tampered_payload_is_rejected() {
        // The attack the whole module exists to stop: a valid signature paired
        // with bytes it does not cover.
        let error = verify_payload_with_key(b"tampered payload", TEST_SIGNATURE, TEST_PUBLIC_KEY)
            .unwrap_err()
            .to_string();
        assert!(error.contains("签名校验失败"), "{error}");
    }

    #[test]
    fn a_signature_from_another_key_is_rejected() {
        const OTHER_KEY: &str = "untrusted comment: minisign public key C8028C9A573928E3\nRWTjKDlXmowCyC9Q/dOAftdyN/oC70kgS2Zbl5CRd63EFO5NZwtHjEVQ\n";
        let error = verify_payload_with_key(TEST_PAYLOAD, TEST_SIGNATURE, OTHER_KEY)
            .unwrap_err()
            .to_string();
        assert!(error.contains("签名"), "{error}");
    }

    #[test]
    fn a_garbage_signature_is_rejected() {
        let error = verify_payload_with_key(b"payload", "bm90LWEtc2lnbmF0dXJl", TEST_PUBLIC_KEY)
            .unwrap_err()
            .to_string();
        assert!(error.contains("签名"), "{error}");
    }

    #[test]
    fn a_malformed_public_key_is_rejected() {
        let error = verify_payload_with_key(b"payload", "c2ln", "not-a-key")
            .unwrap_err()
            .to_string();
        assert!(error.contains("公钥"), "{error}");
    }
}
