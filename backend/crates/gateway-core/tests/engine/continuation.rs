//! 验证原生续接标识的脱敏、账号绑定与 Client Key 隔离

use gateway_core::account::ProviderAccountId;
use gateway_core::engine::continuation::{
    ContinuationBinding, NativeContinuationPin, PreviousResponseId, is_opencode_public_response_id,
};
use gateway_core::policy::ClientApiKeyId;
use gateway_core::routing::ProviderKind;

fn pin() -> NativeContinuationPin {
    NativeContinuationPin::new(
        PreviousResponseId::new("response-private"),
        PreviousResponseId::new("upstream-private"),
        ClientApiKeyId::new("key_client").expect("valid client key"),
        ProviderKind::new("openai").expect("valid provider"),
        ProviderAccountId::new("acct_codex").expect("valid account"),
    )
}

#[test]
fn native_pin_debug_should_redact_previous_response_id() {
    let debug = format!("{:?}", pin());
    assert!(!debug.contains("response-private"));
    assert!(!debug.contains("upstream-private"));
}

#[test]
fn external_binding_debug_should_redact_previous_response_id() {
    let binding = ContinuationBinding::External(PreviousResponseId::new("external-private"));

    assert!(!format!("{binding:?}").contains("external-private"));
}

#[test]
fn opaque_response_id_preserves_any_codex_string_without_debug_disclosure() {
    let value = format!("resp_{}\0opaque", "x".repeat(257));
    let response_id = PreviousResponseId::new(value.clone());

    assert_eq!(response_id.as_str(), value);
    assert!(!format!("{response_id:?}").contains(&value));
}

#[test]
fn only_complete_versioned_opencode_ids_claim_the_public_domain() {
    assert!(is_opencode_public_response_id(
        "cpr_oc_v1_123e4567-e89b-42d3-a456-426614174000"
    ));
    for opaque in [
        "cpr_oc_v1_",
        "cpr_oc_v1_legacy-opaque",
        "cpr_oc_v1_123e4567-e89b-12d3-a456-426614174000",
        "cpr_oc_v1_123E4567-E89B-42D3-A456-426614174000",
        "cpr_oc_v2_123e4567-e89b-42d3-a456-426614174000",
        "cpr_oc_v1_123e4567-e89b-42d3-a456-426614174000-extra",
    ] {
        assert!(!is_opencode_public_response_id(opaque), "{opaque}");
    }
}

#[test]
fn native_pin_should_reject_different_account() {
    assert!(!pin().matches(
        &ProviderKind::new("openai").expect("valid provider"),
        &ProviderAccountId::new("acct_other").expect("valid account"),
    ));
}

#[test]
fn native_pin_should_reject_different_client_api_key() {
    assert!(!pin().matches_client(&ClientApiKeyId::new("key_other").expect("valid client key")));
}
