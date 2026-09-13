//! OpenAI-compatible structured failure facts shared by Provider adapters.

use serde_json::Value;

/// Exact cyber policy refusal classification while the original JSON remains available.
#[must_use]
pub fn is_cyber_policy_refusal_json(raw: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    let top = value.pointer("/error/code").and_then(Value::as_str);
    let code = match top.map(str::trim) {
        Some(code) if !code.is_empty() => Some(code),
        _ => value
            .pointer("/response/error/code")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|code| !code.is_empty()),
    };
    code.is_some_and(|code| code.eq_ignore_ascii_case("cyber_policy"))
}
