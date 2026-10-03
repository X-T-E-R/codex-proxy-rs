use gateway_core::policy::{ClientApiKeyId, CyberSessionRequest, MAX_TRANSCRIPT_LOOKUPS};
use serde_json::{Map, Value, json};

fn request(body: Value, turn_metadata: Option<&str>, headers: &[&str]) -> CyberSessionRequest {
    let Value::Object(body) = body else {
        panic!("fixture must be an object");
    };
    CyberSessionRequest::from_responses(&body, turn_metadata, headers.iter().copied(), None)
}

fn key(value: &str) -> ClientApiKeyId {
    ClientApiKeyId::new(value).expect("valid key")
}

#[test]
fn explicit_identity_is_key_scoped_and_ignores_history() {
    let first = request(
        json!({"client_metadata":{"session_id":" session-a "},"input":[{"role":"user","content":"first"}]}),
        None,
        &[],
    );
    let same_session = request(
        json!({"client_metadata":{"session_id":"session-a"},"input":[{"role":"user","content":"changed"}]}),
        None,
        &[],
    );
    let other_session = request(
        json!({"client_metadata":{"session_id":"session-b"},"input":[{"role":"user","content":"first"}]}),
        None,
        &[],
    );

    assert_eq!(
        first.lookup_keys(&key("key-a"))[0].expose_to_store(),
        same_session.lookup_keys(&key("key-a"))[0].expose_to_store()
    );
    assert_ne!(
        first.lookup_keys(&key("key-a"))[0].expose_to_store(),
        other_session.lookup_keys(&key("key-a"))[0].expose_to_store()
    );
    assert_ne!(
        first.lookup_keys(&key("key-a"))[0].expose_to_store(),
        first.lookup_keys(&key("key-b"))[0].expose_to_store()
    );
}

#[test]
fn body_turn_metadata_precedes_header_aliases() {
    let nested = request(
        json!({
            "client_metadata": {
                "X-Codex-Turn-Metadata": "{\"session_id\":\"body-session\"}"
            },
            "input": "hello"
        }),
        Some("{\"session_id\":\"turn-header\"}"),
        &["generic-header"],
    );
    let expected = request(
        json!({"client_metadata":{"session_id":"body-session"},"input":"different"}),
        None,
        &[],
    );
    assert_eq!(
        nested.lookup_keys(&key("key"))[0].expose_to_store(),
        expected.lookup_keys(&key("key"))[0].expose_to_store()
    );
}

#[test]
fn shared_session_does_not_merge_parent_and_sibling_threads() {
    let identity = |thread: &str, input: &str| {
        request(
            json!({"client_metadata":{"session_id":"shared-session","thread_id":thread},"input":input}),
            None,
            &[],
        )
    };
    let refused = identity("child-a", "refused turn")
        .refusal_key(&key("key"))
        .expect("refusal key");
    assert!(
        identity("child-a", "retry")
            .lookup_keys(&key("key"))
            .contains(&refused)
    );
    for thread in ["parent", "child-b"] {
        assert!(
            !identity(thread, "continue")
                .lookup_keys(&key("key"))
                .contains(&refused)
        );
    }
    let legacy = request(
        json!({"client_metadata":{"session_id":"shared-session"},"input":"old turn"}),
        None,
        &[],
    )
    .refusal_key(&key("key"))
    .expect("legacy marker");
    assert!(
        !identity("shared-session", "continue")
            .lookup_keys(&key("key"))
            .contains(&legacy)
    );
}

#[test]
fn canonical_turn_thread_precedes_flat_metadata_and_shared_session() {
    for metadata in [
        json!({"thread_id":"child-a","session_id":"shared-session"}),
        json!("{\"thread_id\":\"child-a\",\"session_id\":\"shared-session\"}"),
    ] {
        let observed = request(
            json!({"client_metadata":{
                "session_id":"shared-session",
                "thread_id":"stale-flat-thread",
                "x-codex-turn-metadata":metadata
            },"input":"turn"}),
            Some("{\"thread_id\":\"connection-thread\",\"session_id\":\"shared-session\"}"),
            &["shared-session"],
        );
        let expected = request(
            json!({"client_metadata":{"thread_id":"child-a"},"input":"other turn"}),
            None,
            &[],
        );
        assert_eq!(
            observed.lookup_keys(&key("key")),
            expected.lookup_keys(&key("key"))
        );
    }
}

#[test]
fn turn_header_thread_precedes_body_session_and_ignores_parent_lineage() {
    let observed = request(
        json!({"client_metadata":{"session_id":"shared-session","x-codex-parent-thread-id":"parent"},"input":"turn"}),
        Some(
            "{\"thread_id\":\"child-a\",\"session_id\":\"shared-session\",\"parent_thread_id\":\"parent\"}",
        ),
        &["shared-session"],
    );
    let expected = request(
        json!({"client_metadata":{"thread_id":"child-a"},"input":"other turn"}),
        None,
        &[],
    );
    assert_eq!(
        observed.lookup_keys(&key("key")),
        expected.lookup_keys(&key("key"))
    );
}

#[test]
fn transcript_matches_exact_append_but_not_rewritten_latest_turn() {
    let refused = request(
        json!({
            "instructions":"system contract",
            "input":[
                {"role":"user","content":"one","unknown":{"b":2,"a":1}},
                {"role":"assistant","content":"two"}
            ]
        }),
        None,
        &[],
    );
    let appended = request(
        json!({
            "instructions":"system contract",
            "input":[
                {"unknown":{"a":1,"b":2},"content":"one","role":"user"},
                {"role":"assistant","content":"two"},
                {"role":"user","content":"three"}
            ]
        }),
        None,
        &[],
    );
    let rewritten = request(
        json!({
            "instructions":"system contract",
            "input":[
                {"role":"user","content":"one","unknown":{"a":1,"b":2}},
                {"role":"assistant","content":"changed"},
                {"role":"user","content":"three"}
            ]
        }),
        None,
        &[],
    );
    let refused_key = refused.refusal_key(&key("key")).expect("refusal key");
    assert!(appended.lookup_keys(&key("key")).contains(&refused_key));
    assert!(!rewritten.lookup_keys(&key("key")).contains(&refused_key));
}

#[test]
fn transcript_lookup_keeps_only_the_newest_256_prefixes() {
    let items = (0..300)
        .map(|index| json!({"role":"user","content":index.to_string()}))
        .collect::<Vec<_>>();
    let mut body = Map::new();
    body.insert("input".to_owned(), Value::Array(items));
    let request = CyberSessionRequest::from_responses(&body, None, std::iter::empty(), None);

    assert_eq!(
        request.lookup_keys(&key("key")).len(),
        MAX_TRANSCRIPT_LOOKUPS
    );
}

#[test]
fn invalid_explicit_candidates_fall_back_to_transcript_without_using_affinity_fields() {
    let observed = request(
        json!({
            "client_metadata":{"session_id":12},
            "prompt_cache_key":"shared-cache",
            "thread_id":"thread-a",
            "input":"hello"
        }),
        None,
        &[],
    );
    let plain = request(json!({"input":"hello"}), None, &[]);
    assert_eq!(
        observed.lookup_keys(&key("key"))[0].expose_to_store(),
        plain.lookup_keys(&key("key"))[0].expose_to_store()
    );
}

#[test]
fn invalid_thread_candidates_preserve_session_fallback() {
    let expected = request(
        json!({"client_metadata":{"session_id":"session-a"},"input":"changed"}),
        None,
        &[],
    );
    for invalid in [
        Value::Null,
        json!(12),
        json!(" "),
        json!("bad\nthread"),
        json!("好".repeat(256)),
    ] {
        let observed = request(
            json!({"client_metadata":{
                "session_id":"session-a", "thread_id":invalid,
                "x-codex-turn-metadata":{"thread_id":invalid}
            },"input":"turn"}),
            Some("{\"thread_id\":12}"),
            &[],
        );
        assert_eq!(
            observed.lookup_keys(&key("key")),
            expected.lookup_keys(&key("key"))
        );
    }
    let accepted = request(
        json!({"client_metadata":{"thread_id":"好".repeat(255)},"input":"turn"}),
        None,
        &[],
    );
    let same = request(
        json!({"client_metadata":{"thread_id":"好".repeat(255)},"input":"other turn"}),
        None,
        &[],
    );
    assert_eq!(
        accepted.lookup_keys(&key("key")),
        same.lookup_keys(&key("key"))
    );
}

#[test]
fn explicit_identity_bounds_unicode_characters_without_byte_truncation() {
    let accepted = "好".repeat(255);
    let accepted_request = request(
        json!({"client_metadata":{"session_id":accepted},"input":"first"}),
        None,
        &[],
    );
    let same_session = request(
        json!({"client_metadata":{"session_id":"好".repeat(255)},"input":"changed"}),
        None,
        &[],
    );
    assert_eq!(
        accepted_request.lookup_keys(&key("key"))[0].expose_to_store(),
        same_session.lookup_keys(&key("key"))[0].expose_to_store()
    );

    let overlong = request(
        json!({"client_metadata":{"session_id":"好".repeat(256)},"input":"fallback"}),
        None,
        &[],
    );
    let transcript = request(json!({"input":"fallback"}), None, &[]);
    assert_eq!(
        overlong.lookup_keys(&key("key"))[0].expose_to_store(),
        transcript.lookup_keys(&key("key"))[0].expose_to_store()
    );
}
