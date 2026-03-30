/// Enterprise license verification.
///
/// The rate limit and HA rights are gated behind a cryptographically signed
/// license key. The `--features managed` build flag alone is not sufficient —
/// a valid `SKIALITH_LICENSE_KEY` must be present at runtime.
///
/// License key format:
///   <base64url(json_payload)>.<base64url(ed25519_signature)>
///
/// The signature covers the raw UTF-8 bytes of the JSON payload.
/// Only Durable's private signing key can produce a valid signature.
/// That key never appears in this repository or any binary we ship.
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;

use crate::limits::COMMUNITY_RATE_LIMIT;

/// Our embedded Ed25519 public key (32 bytes).
///
/// The corresponding private key lives in Skialith's offline signing
/// infrastructure. It is used to sign license payloads for paying customers
/// and is never stored in source control or shipped binaries.
///
/// To generate a real key pair (one-time, offline):
///   let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
///   println!("{:?}", sk.verifying_key().to_bytes()); // embed below
///   // Store sk securely in your HSM / signing service.
///
/// PLACEHOLDER: zeroed key. Replace before first production release.
const VERIFYING_KEY: [u8; 32] = [0u8; 32];

#[derive(Deserialize)]
struct LicensePayload {
    customer_id: String,
    /// events/sec ceiling; None = unlimited.
    max_events_per_second: Option<u32>,
    /// Whether this license permits running multiple instances (HA).
    allow_ha: bool,
    /// Unix timestamp after which this license is no longer valid.
    expires_at: u64,
}

#[derive(Debug)]
pub struct License {
    pub customer_id: String,
    pub max_events_per_second: Option<u32>,
    pub allow_ha: bool,
    pub expires_at: u64,
}

impl License {
    pub fn is_expired(&self) -> bool {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            >= self.expires_at
    }

    /// Effective rate limit. Falls back to the Community ceiling on expiry.
    pub fn effective_rate_limit(&self) -> u32 {
        if self.is_expired() {
            COMMUNITY_RATE_LIMIT
        } else {
            self.max_events_per_second.unwrap_or(u32::MAX)
        }
    }

    pub fn allows_ha(&self) -> bool {
        !self.is_expired() && self.allow_ha
    }
}

/// Load and verify the license from `SKIALITH_LICENSE_KEY`.
/// Returns `None` if the variable is absent, the format is wrong,
/// or the Ed25519 signature does not verify.
pub fn load() -> Option<License> {
    let raw = std::env::var("SKIALITH_LICENSE_KEY").ok()?;
    parse_and_verify(raw.trim())
}

/// Effective rate limit given an optional license.
/// Always clamps to `COMMUNITY_RATE_LIMIT` when no valid license is present,
/// regardless of build flags.
pub fn effective_rate_limit(license: &Option<License>) -> u32 {
    license
        .as_ref()
        .map(|l| l.effective_rate_limit())
        .unwrap_or(COMMUNITY_RATE_LIMIT)
}

/// Whether HA / multi-instance is permitted.
pub fn allows_ha(license: &Option<License>) -> bool {
    license.as_ref().map(|l| l.allows_ha()).unwrap_or(false)
}

fn parse_and_verify(key_str: &str) -> Option<License> {
    let (payload_b64, sig_b64) = key_str.split_once('.')?;

    let payload_bytes = b64_decode(payload_b64)?;
    let sig_bytes: [u8; 64] = b64_decode(sig_b64)?.try_into().ok()?;

    let verifying_key = VerifyingKey::from_bytes(&VERIFYING_KEY).ok()?;
    let signature = Signature::from_bytes(&sig_bytes);
    verifying_key.verify(&payload_bytes, &signature).ok()?;

    let p: LicensePayload = serde_json::from_slice(&payload_bytes).ok()?;

    Some(License {
        customer_id: p.customer_id,
        max_events_per_second: p.max_events_per_second,
        allow_ha: p.allow_ha,
        expires_at: p.expires_at,
    })
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .ok()
}
