//! Cyber 会话键的线程优先、Key 隔离、历史前缀和不透明边界

use gateway_core::{
    policy::ClientApiKeyId,
    provider_ports::{CyberSessionRequest, MAX_TRANSCRIPT_LOOKUPS},
};
use serde_json::{Value, json};

fn request(body: Value, session: &str) -> CyberSessionRequest {
    CyberSessionRequest::from_responses(body.as_object().unwrap(), None, [session], None)
}

#[test]
fn thread_identity_isolates_parent_and_siblings_even_with_shared_session_and_continuation() {
    let key = ClientApiKeyId::new("key-a").unwrap();
    let other = ClientApiKeyId::new("key-b").unwrap();
    let identity = |thread: &str| {
        request(
            json!({
                "previous_response_id": "response-shared",
                "client_metadata": {"session_id": "shared", "x-codex-turn-metadata": {"thread_id": thread}}
            }),
            "shared",
        )
    };
    let child = identity("child-a").refusal_key(&key).unwrap();
    for thread in ["parent", "child-b"] {
        assert!(!identity(thread).lookup_keys(&key).contains(&child));
    }
    assert!(identity("child-a").lookup_keys(&key).contains(&child));
    assert!(!identity("child-a").lookup_keys(&other).contains(&child));
    assert!(!format!("{child:?}").contains("child-a"));
}

#[test]
fn body_thread_precedes_stale_opening_headers_and_session_fallback_keeps_legacy_headers() {
    let key = ClientApiKeyId::new("key-a").unwrap();
    let body = json!({"client_metadata": {"x-codex-turn-metadata": "{\"thread_id\":\"child-a\"}"}});
    let child = CyberSessionRequest::from_responses(
        body.as_object().unwrap(),
        Some("{\"thread_id\":\"parent\"}"),
        ["shared"],
        Some("parent"),
    );
    assert_eq!(
        child.refusal_key(&key),
        request(body, "different-session").refusal_key(&key)
    );
    assert_eq!(
        request(json!({}), "legacy-session").lookup_keys(&key).len(),
        1
    );
    assert!(request(json!({}), "\n").lookup_keys(&key).is_empty());
}

#[test]
fn transcript_prefixes_block_only_matching_descendants_with_bounded_lookups() {
    let key = ClientApiKeyId::new("key-a").unwrap();
    let first = request(
        json!({"instructions":"same", "input":[{"role":"user","content":"blocked"}]}),
        "",
    );
    let marker = first.refusal_key(&key).unwrap();
    let descendant = request(
        json!({"instructions":"same", "input":[{"content":"blocked","role":"user"},{"role":"assistant","content":"next"}]}),
        "",
    );
    assert!(descendant.lookup_keys(&key).contains(&marker));
    assert!(
        !request(
            json!({"instructions":"different", "input":[{"role":"user","content":"blocked"}]}),
            ""
        )
        .lookup_keys(&key)
        .contains(&marker)
    );
    let long = request(
        json!({"input": (0..300).map(|n| json!({"content":n})).collect::<Vec<_>>()}),
        "",
    );
    assert_eq!(long.lookup_keys(&key).len(), MAX_TRANSCRIPT_LOOKUPS);
}
