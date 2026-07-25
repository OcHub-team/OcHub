//! Tells Cargo that the updater's public key is a build input.
//!
//! `services::update::manifest::PUBLIC_KEY` reads `OCHUB_UPDATER_PUBKEY` via
//! `option_env!`, which is resolved at compile time. Without this, Cargo has no
//! idea the value participates in the build and will happily reuse a cached
//! object file after the key changes — producing a binary that silently
//! verifies updates against the wrong key, or against none at all.

fn main() {
    println!("cargo:rerun-if-env-changed=OCHUB_UPDATER_PUBKEY");
}
