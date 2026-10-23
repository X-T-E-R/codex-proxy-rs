//! 从 Responses 入口身份和逐帧历史提取不可逆的 Cyber 语义摘要

use gateway_core::provider_ports::{CyberSessionRequest, MAX_TRANSCRIPT_LOOKUPS};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;

pub(super) fn from_responses<'a>(
    body: &Map<String, Value>,
    turn_metadata_header: Option<&str>,
    semantic_headers: impl IntoIterator<Item = &'a str>,
    thread_id_header: Option<&str>,
) -> CyberSessionRequest {
    let metadata = body.get("client_metadata").and_then(Value::as_object);
    let turn = metadata.and_then(|value| {
        value
            .get("x-codex-turn-metadata")
            .or_else(|| value.get("X-Codex-Turn-Metadata"))
    });
    let thread = metadata_identity(turn, "thread_id")
        .or_else(|| metadata.and_then(|value| valid_value(value.get("thread_id"))))
        .or_else(|| turn_metadata_header.and_then(|value| encoded_identity(value, "thread_id")))
        .or_else(|| thread_id_header.and_then(valid_identity));
    let thread_scoped = thread.is_some();
    let explicit = thread.or_else(|| {
        metadata
            .and_then(|value| valid_value(value.get("session_id")))
            .or_else(|| metadata_identity(turn, "session_id"))
            .or_else(|| {
                turn_metadata_header.and_then(|value| encoded_identity(value, "session_id"))
            })
            .or_else(|| semantic_headers.into_iter().find_map(valid_identity))
    });
    if let Some(identity) = explicit {
        let mut hasher = Sha256::new();
        // 保持旧线程域，避免继承曾经按共享 session 误伤兄弟线程的 marker
        hash_field(
            &mut hasher,
            if thread_scoped {
                b"codex-proxy-rs.cyber-session.thread.v1"
            } else {
                b"codex-proxy-rs.cyber-session.explicit.v1"
            },
        );
        hash_field(&mut hasher, identity.as_bytes());
        return CyberSessionRequest::from_digests(Some(hasher.finalize().into()), Vec::new());
    }
    CyberSessionRequest::from_digests(None, transcript_digests(body).unwrap_or_default())
}

fn valid_value(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).and_then(valid_identity)
}

fn metadata_identity(value: Option<&Value>, field: &str) -> Option<String> {
    match value? {
        Value::Object(metadata) => valid_value(metadata.get(field)),
        Value::String(encoded) => encoded_identity(encoded, field),
        _ => None,
    }
}

fn encoded_identity(encoded: &str, field: &str) -> Option<String> {
    serde_json::from_str::<Value>(encoded)
        .ok()
        .and_then(|value| valid_value(value.as_object()?.get(field)))
}

fn valid_identity(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.chars().take(256).count() <= 255
        && !value
            .chars()
            .any(|character| character < '\u{20}' || character == '\u{7f}'))
    .then(|| value.to_owned())
}

fn transcript_digests(body: &Map<String, Value>) -> Option<Vec<[u8; 32]>> {
    let history: Vec<&Value> = if let Some(messages) = body.get("messages") {
        messages.as_array()?.iter().collect()
    } else {
        let input = body.get("input")?;
        match input {
            Value::Array(items) => items.iter().collect(),
            Value::String(value) if !value.is_empty() => vec![input],
            _ => return None,
        }
    };
    if history.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"codex-proxy-rs.cyber-session.transcript.v1");
    for field in ["instructions", "system"] {
        if let Some(value) = body
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            hash_field(&mut hasher, field.as_bytes());
            hash_field(&mut hasher, value.as_bytes());
        }
    }
    let mut digests = VecDeque::with_capacity(MAX_TRANSCRIPT_LOOKUPS);
    for item in history {
        let mut canonical = Vec::new();
        write_canonical_json(item, &mut canonical);
        hash_field(&mut hasher, &canonical);
        if digests.len() == MAX_TRANSCRIPT_LOOKUPS {
            digests.pop_front();
        }
        digests.push_back(hasher.clone().finalize().into());
    }
    Some(digests.into())
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(value.len().to_string().as_bytes());
    hasher.update(b":");
    hasher.update(value);
}

fn write_canonical_json(value: &Value, output: &mut Vec<u8>) {
    match value {
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical_json(value, output);
            }
            output.push(b']');
        }
        Value::Object(values) => {
            output.push(b'{');
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                serde_json::to_writer(&mut *output, key).expect("writing JSON to Vec cannot fail");
                output.push(b':');
                write_canonical_json(&values[key], output);
            }
            output.push(b'}');
        }
        _ => serde_json::to_writer(output, value).expect("writing JSON to Vec cannot fail"),
    }
}
