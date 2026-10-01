// Copied by wuapi-codegen from packages/sdk-codegen/templates/rust/src/webhook.rs. Do not edit here.

//! Webhook signature verification.
//!
//! The signature header is `t=<unix seconds>,v1=<hex>`, where the hex is
//! HMAC-SHA256 of `"<t>.<rawBody>"` keyed with the endpoint's signing secret.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::error::WebhookVerificationError;
use crate::types::WebhookEvent;

/// How far a webhook's timestamp may be from now by default.
pub const DEFAULT_TOLERANCE: Duration = Duration::from_secs(300);

fn mac(secret: &str, timestamp: &str, raw_body: &[u8]) -> Hmac<Sha256> {
    // HMAC accepts a key of any length, so this cannot fail.
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes())
        .unwrap_or_else(|_| unreachable!("HMAC accepts keys of any length"));
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(raw_body);
    mac
}

/// Computes the `v1` signature (lowercase hex) for a timestamp and raw body.
#[must_use]
pub fn compute_signature(secret: &str, timestamp: &str, raw_body: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = mac(secret, timestamp, raw_body.as_ref())
        .finalize()
        .into_bytes();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0F)]));
    }
    out
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    let nibble = |b: u8| {
        char::from(b)
            .to_digit(16)
            .and_then(|d| u8::try_from(d).ok())
    };
    bytes
        .chunks_exact(2)
        .map(|pair| Some(nibble(pair[0])? << 4 | nibble(pair[1])?))
        .collect()
}

/// Checks a webhook request's signature and timestamp without parsing the
/// body.
///
/// - `raw_body`: the request body exactly as received, before any JSON parsing.
/// - `header`: the signature header (`t=<unix seconds>,v1=<hex>`).
/// - `secret`: the endpoint's signing secret.
/// - `tolerance`: the maximum age (and clock skew) of the timestamp.
///
/// The comparison takes the same time whether or not the signature matches.
///
/// # Errors
///
/// [`WebhookVerificationError`] when the header or secret is empty, the
/// header is malformed, the timestamp is outside the tolerance, or no
/// signature matches.
pub fn verify_signature(
    raw_body: impl AsRef<[u8]>,
    header: &str,
    secret: &str,
    tolerance: Duration,
) -> Result<(), WebhookVerificationError> {
    if header.trim().is_empty() {
        return Err(WebhookVerificationError::MissingHeader);
    }
    if secret.is_empty() {
        return Err(WebhookVerificationError::MissingSecret);
    }
    let mut timestamp = None;
    let mut signatures = Vec::new();
    for part in header.split(',') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key.trim() {
            "t" => timestamp = Some(value.trim()),
            "v1" => signatures.push(value.trim()),
            _ => {}
        }
    }
    let timestamp = timestamp
        .filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))
        .ok_or(WebhookVerificationError::MalformedHeader)?;
    let seconds: u64 = timestamp
        .parse()
        .map_err(|_| WebhookVerificationError::MalformedHeader)?;
    if signatures.is_empty() {
        return Err(WebhookVerificationError::MalformedHeader);
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if now.abs_diff(seconds) > tolerance.as_secs() {
        return Err(WebhookVerificationError::TimestampOutOfTolerance);
    }

    let expected = mac(secret, timestamp, raw_body.as_ref());
    let matches = signatures.iter().any(|signature| {
        decode_hex(signature).is_some_and(|bytes| expected.clone().verify_slice(&bytes).is_ok())
    });
    if matches {
        Ok(())
    } else {
        Err(WebhookVerificationError::SignatureMismatch)
    }
}

/// Verifies a webhook request and returns its parsed event, rejecting
/// timestamps more than 300 seconds from now.
///
/// See [`verify_signature`] for the arguments.
///
/// # Errors
///
/// [`WebhookVerificationError`] when the signature does not verify or the
/// body is not a valid event.
pub fn verify_webhook(
    raw_body: impl AsRef<[u8]>,
    header: &str,
    secret: &str,
) -> Result<WebhookEvent, WebhookVerificationError> {
    verify_webhook_with_tolerance(raw_body, header, secret, DEFAULT_TOLERANCE)
}

/// [`verify_webhook`] with your own timestamp tolerance.
///
/// # Errors
///
/// The same as [`verify_webhook`].
pub fn verify_webhook_with_tolerance(
    raw_body: impl AsRef<[u8]>,
    header: &str,
    secret: &str,
    tolerance: Duration,
) -> Result<WebhookEvent, WebhookVerificationError> {
    let raw_body = raw_body.as_ref();
    verify_signature(raw_body, header, secret, tolerance)?;
    serde_json::from_slice(raw_body).map_err(WebhookVerificationError::InvalidBody)
}
