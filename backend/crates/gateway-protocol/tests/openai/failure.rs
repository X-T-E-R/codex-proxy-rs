use gateway_protocol::openai::is_cyber_policy_refusal_json;

#[test]
fn cyber_policy_classifier_is_exact_and_precedence_sensitive() {
    for body in [
        r#"{"error":{"code":" cyber_policy "}}"#,
        r#"{"error":{"code":"CYBER_POLICY"}}"#,
        r#"{"response":{"error":{"code":"cyber_policy"}}}"#,
        r#"{"error":{"code":" "},"response":{"error":{"code":"cyber_policy"}}}"#,
    ] {
        assert!(is_cyber_policy_refusal_json(body), "{body}");
    }
    for body in [
        "cyber_policy",
        r#"{"error":{"message":"cyber_policy"}}"#,
        r#"{"error":{"type":"cyber_policy"}}"#,
        r#"{"error":{"code":"other"},"response":{"error":{"code":"cyber_policy"}}}"#,
        r#"{"output":[{"content":"cyber_policy"}]}"#,
    ] {
        assert!(!is_cyber_policy_refusal_json(body), "{body}");
    }
}
