use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::time::{Duration, SystemTime};

use gateway_core::account::{
    AccountRuntimeSignals, CredentialRevision, OpaqueProviderData, ProviderAccountId,
};
use gateway_core::provider_ports::turn_state::model_turn_state_token_metadata;
use gateway_core::provider_ports::{
    NewOAuthPendingFlow, OAuthPendingBinding, ProviderRefreshPolicy, ProviderSchedulingState,
    ProviderSessionAffinityKey, ProviderStoreErrorKind, ProviderWebSocketPoolPolicy,
};
use gateway_core::routing::ProviderKind;

#[test]
fn oauth_pending_binding_debug_redacts_raw_value() {
    let binding = OAuthPendingBinding::try_new("must-not-appear").expect("valid binding");

    assert_eq!(format!("{binding:?}"), "OAuthPendingBinding([REDACTED])");
}

#[test]
fn model_turn_state_reads_only_public_fernet_envelope_metadata() {
    let token = synthetic_fernet_candidate();
    let metadata = model_turn_state_token_metadata(&token);

    assert_eq!(metadata.encoded_bytes, 292);
    assert_eq!(metadata.raw_bytes, Some(217));
    assert_eq!(metadata.ciphertext_bytes, Some(160));
    assert_eq!(metadata.envelope_format, Some("fernet_v0x80_candidate"));
    assert_eq!(metadata.token_version, Some(0x80));
    assert_eq!(
        metadata.issued_at.map(|value| value.timestamp()),
        Some(1_789_650_773)
    );
    assert!(!metadata.timestamp_verified);
}

fn synthetic_fernet_candidate() -> String {
    let mut raw = Vec::with_capacity(217);
    raw.push(0x80);
    raw.extend_from_slice(&1_789_650_773_u64.to_be_bytes());
    raw.extend_from_slice(&[0x11; 16]);
    raw.extend_from_slice(&[0x22; 160]);
    raw.extend_from_slice(&[0x33; 32]);
    assert_eq!(raw.len(), 217);
    url_safe_base64(&raw)
}

fn url_safe_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut chunks = bytes.chunks_exact(3);
    for chunk in &mut chunks {
        encoded.push(char::from(ALPHABET[usize::from(chunk[0] >> 2)]));
        encoded.push(char::from(
            ALPHABET[usize::from((chunk[0] & 0x03) << 4 | chunk[1] >> 4)],
        ));
        encoded.push(char::from(
            ALPHABET[usize::from((chunk[1] & 0x0f) << 2 | chunk[2] >> 6)],
        ));
        encoded.push(char::from(ALPHABET[usize::from(chunk[2] & 0x3f)]));
    }
    match chunks.remainder() {
        [first] => {
            encoded.push(char::from(ALPHABET[usize::from(*first >> 2)]));
            encoded.push(char::from(ALPHABET[usize::from((*first & 0x03) << 4)]));
            encoded.push_str("==");
        }
        [first, second] => {
            encoded.push(char::from(ALPHABET[usize::from(*first >> 2)]));
            encoded.push(char::from(
                ALPHABET[usize::from((*first & 0x03) << 4 | *second >> 4)],
            ));
            encoded.push(char::from(ALPHABET[usize::from((*second & 0x0f) << 2)]));
            encoded.push('=');
        }
        [] => {}
        _ => unreachable!("chunks_exact remainder is shorter than three bytes"),
    }
    encoded
}

#[test]
fn provider_session_affinity_key_debug_is_opaque() {
    let key = ProviderSessionAffinityKey::try_new("opaque-session-key").expect("valid key");

    assert_eq!(format!("{key:?}"), "ProviderSessionAffinityKey([OPAQUE])");
}

#[test]
fn oauth_pending_ttl_rejects_zero_and_more_than_thirty_minutes() {
    let provider = ProviderKind::new("fixture").expect("valid provider");
    let flow = OAuthPendingBinding::try_new("flow").expect("valid flow");
    let owner = OAuthPendingBinding::try_new("owner").expect("valid owner");
    let payload = OpaqueProviderData::new(serde_json::Map::new());

    for ttl in [Duration::ZERO, Duration::from_secs(30 * 60 + 1)] {
        let error = NewOAuthPendingFlow::try_new(
            provider.clone(),
            flow.clone(),
            owner.clone(),
            ttl,
            payload.clone(),
        )
        .expect_err("invalid TTL must fail");
        assert_eq!(error.kind(), ProviderStoreErrorKind::InvalidData);
    }
}

#[test]
fn refresh_policy_requires_a_positive_margin() {
    let error = ProviderRefreshPolicy::try_new(
        Duration::ZERO,
        NonZeroU32::new(1).expect("positive concurrency"),
    )
    .expect_err("zero margin must fail");

    assert_eq!(error.kind(), ProviderStoreErrorKind::InvalidData);
}

#[test]
fn refresh_policy_should_mark_tokens_due_at_the_exact_configured_margin() {
    let policy = ProviderRefreshPolicy::try_new(
        Duration::from_secs(3_600),
        NonZeroU32::new(2).expect("positive concurrency"),
    )
    .expect("valid policy");
    let observed_at = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
    let expires_at = observed_at + Duration::from_secs(7_200);

    assert!(!policy.is_refresh_due(expires_at, observed_at));
    assert!(policy.is_refresh_due(observed_at + Duration::from_secs(3_600), observed_at));
}

#[test]
fn refresh_policy_should_mark_expired_tokens_due() {
    let policy = ProviderRefreshPolicy::try_new(
        Duration::from_secs(3_600),
        NonZeroU32::new(1).expect("positive concurrency"),
    )
    .expect("valid policy");
    let observed_at = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);

    assert!(policy.is_refresh_due(observed_at - Duration::from_secs(1), observed_at));
}

#[test]
fn websocket_pool_policy_rejects_zero_durations() {
    // max_age / stream_idle_timeout / fast_path_budget 为 0 时池语义不成立。
    let valid_max_connecting = NonZeroU32::new(8).expect("positive concurrency");
    for (max_age, idle, budget) in [
        (
            Duration::ZERO,
            Duration::from_secs(300),
            Duration::from_millis(800),
        ),
        (
            Duration::from_secs(3_300),
            Duration::ZERO,
            Duration::from_millis(800),
        ),
        (
            Duration::from_secs(3_300),
            Duration::from_secs(300),
            Duration::ZERO,
        ),
    ] {
        let error =
            ProviderWebSocketPoolPolicy::try_new(true, max_age, valid_max_connecting, idle, budget)
                .expect_err("zero duration must fail");
        assert_eq!(error.kind(), ProviderStoreErrorKind::InvalidData);
    }
}

#[test]
fn websocket_pool_policy_round_trips_stable_values() {
    let policy = ProviderWebSocketPoolPolicy::try_new(
        false,
        Duration::from_millis(3_300_000),
        NonZeroU32::new(4).expect("positive concurrency"),
        Duration::from_millis(120_000),
        Duration::from_millis(3_000),
    )
    .expect("valid policy");

    assert!(!policy.enabled());
    assert_eq!(policy.max_age(), Duration::from_millis(3_300_000));
    assert_eq!(policy.max_connecting().get(), 4);
    assert_eq!(policy.stream_idle_timeout(), Duration::from_millis(120_000));
    assert_eq!(policy.fast_path_budget(), Duration::from_millis(3_000));
}

#[test]
fn scheduling_state_preserves_provider_neutral_signals() {
    let account = ProviderAccountId::new("acct_fixture").expect("valid account");
    let signals = BTreeMap::from([(
        account.clone(),
        AccountRuntimeSignals {
            in_flight: 2,
            last_started_at: None,
            quota_reset_at: None,
            quota_remaining_rank: Some(7),
            cooldown: None,
            failure_rate_basis_points: Some(125),
            first_output_latency_ms: Some(250),
        },
    )]);
    let state = ProviderSchedulingState::new(signals, 9);

    assert_eq!(state.signals()[&account].in_flight, 2);
    assert_eq!(
        state.signals()[&account].failure_rate_basis_points,
        Some(125)
    );
    assert_eq!(state.round_robin_cursor(), 9);
    assert_eq!(
        CredentialRevision::new(1).expect("positive revision").get(),
        1
    );
}
