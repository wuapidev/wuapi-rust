// Copied by wuapi-codegen from packages/sdk-codegen/templates/rust/tests/api/webhook.rs. Do not edit here.

//! Webhook signature verification.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::sdk::WebhookVerificationError as E;
use crate::sdk::webhook::{DEFAULT_TOLERANCE, compute_signature, verify_signature, verify_webhook};

const SECRET: &str = "whsec_test";
const BODY: &str = r#"{"id":"evt_1","type":"message.received"}"#;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn header(timestamp: u64, body: &str) -> String {
    let t = timestamp.to_string();
    format!("t={t},v1={}", compute_signature(SECRET, &t, body))
}

#[test]
fn computes_hmac_sha256_of_timestamp_dot_body() {
    // Computed independently: HMAC-SHA256("whsec_test", "1700000000." + BODY).
    assert_eq!(
        compute_signature(SECRET, "1700000000", BODY),
        "0e10e8ef75be685bafd4931aeee59cfb13f2f0625fa89c9eced763c6b6d5e9a8"
    );
    let signature = compute_signature(SECRET, "1700000000", BODY);
    assert_eq!(signature.len(), 64);
    assert!(
        signature
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    assert_ne!(signature, compute_signature(SECRET, "1700000001", BODY));
    assert_ne!(
        signature,
        compute_signature("whsec_other", "1700000000", BODY)
    );
}

#[test]
fn accepts_a_valid_signature() {
    verify_signature(BODY, &header(now(), BODY), SECRET, DEFAULT_TOLERANCE).unwrap();
    // Bytes work as well as text, and spaces around the parts are ignored.
    let spaced = header(now(), BODY).replace(',', " , ");
    verify_signature(BODY.as_bytes(), &spaced, SECRET, DEFAULT_TOLERANCE).unwrap();
    // Any matching v1 signature is enough (secret rotation), in either case.
    let t = now().to_string();
    let good = compute_signature(SECRET, &t, BODY).to_uppercase();
    let rotated = format!("t={t},v1={},v0=ignored,v1={good}", "0".repeat(64));
    verify_signature(BODY, &rotated, SECRET, DEFAULT_TOLERANCE).unwrap();
}

#[test]
fn rejects_a_wrong_signature() {
    let valid = header(now(), BODY);
    for (body, header, secret) in [
        (r#"{"id":"evt_2"}"#, valid.as_str(), SECRET),
        (BODY, valid.as_str(), "whsec_other"),
        (BODY, &format!("t={},v1={}", now(), "ab".repeat(32)), SECRET),
        (BODY, &format!("t={},v1=not-hex", now()), SECRET),
        (BODY, &format!("t={},v1=abc", now()), SECRET),
    ] {
        let error = verify_signature(body, header, secret, DEFAULT_TOLERANCE).unwrap_err();
        assert!(matches!(error, E::SignatureMismatch), "{header}: {error:?}");
    }
}

#[test]
fn rejects_missing_and_malformed_input() {
    let valid = header(now(), BODY);
    let check = |header: &str, secret: &str| {
        verify_signature(BODY, header, secret, DEFAULT_TOLERANCE).unwrap_err()
    };
    assert!(matches!(check("", SECRET), E::MissingHeader));
    assert!(matches!(check("  ", SECRET), E::MissingHeader));
    assert!(matches!(check(&valid, ""), E::MissingSecret));
    let signature = valid.split_once(",").unwrap().1;
    for malformed in [
        "nonsense".to_owned(),
        signature.to_owned(),
        format!("t=,{signature}"),
        format!("t=12x,{signature}"),
        format!("t=-5,{signature}"),
        format!("t={},{signature}", "9".repeat(40)),
        format!("t={}", now()),
    ] {
        assert!(
            matches!(check(&malformed, SECRET), E::MalformedHeader),
            "{malformed}"
        );
    }
    assert_eq!(
        check("", SECRET).to_string(),
        "missing webhook signature header"
    );
}

#[test]
fn rejects_timestamps_outside_the_tolerance() {
    let old = header(now() - 301, BODY);
    let error = verify_signature(BODY, &old, SECRET, DEFAULT_TOLERANCE).unwrap_err();
    assert!(matches!(error, E::TimestampOutOfTolerance), "{error:?}");
    let future = header(now() + 400, BODY);
    let error = verify_signature(BODY, &future, SECRET, DEFAULT_TOLERANCE).unwrap_err();
    assert!(matches!(error, E::TimestampOutOfTolerance), "{error:?}");
    // A wider tolerance accepts it.
    verify_signature(BODY, &old, SECRET, Duration::from_secs(600)).unwrap();
}

#[test]
fn a_signed_body_that_is_not_json_is_rejected() {
    let body = "not json";
    let error = verify_webhook(body, &header(now(), body), SECRET).unwrap_err();
    assert!(matches!(error, E::InvalidBody(_)), "{error:?}");
    assert!(std::error::Error::source(&error).is_some());
    // The signature is checked first.
    let error = verify_webhook(body, &header(now(), "other"), SECRET).unwrap_err();
    assert!(matches!(error, E::SignatureMismatch), "{error:?}");
}
