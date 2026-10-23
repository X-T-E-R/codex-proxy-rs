//! 验证 Responses 请求解压、大小限制与原始字段保留

use std::io::{Read, Write, copy, repeat};

use axum::http::{HeaderMap, HeaderValue, header::CONTENT_ENCODING};
use gateway_api::openai::responses::{RequestDecodeError, decode_request_with_headers};
use serde_json::json;

use super::openai_wire_body;

#[test]
fn cyber_identity_matches_http_sse_and_each_websocket_frame_without_parent_leakage() {
    use gateway_api::openai::responses::{
        OpenAiRequestHeaders, decode_response_create_with_context,
    };
    use gateway_core::policy::ClientApiKeyId;
    let key = ClientApiKeyId::new("key-a").unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-codex-turn-metadata",
        HeaderValue::from_static(r#"{"thread_id":"parent","session_id":"shared"}"#),
    );
    let opening = OpenAiRequestHeaders::from_headers(&headers);
    let body = json!({"model":"gpt-test","input":"hello","client_metadata":{"session_id":"shared", "x-codex-turn-metadata":{"thread_id":"child-a"}}});
    let marker = decode_request_with_headers(body.to_string().as_bytes(), &headers, 65536)
        .unwrap()
        .metadata()
        .cyber_session()
        .refusal_key(&key)
        .unwrap();
    for stream in [false, true] {
        let mut http = body.clone();
        http["stream"] = json!(stream);
        let decoded =
            decode_request_with_headers(http.to_string().as_bytes(), &headers, 65536).unwrap();
        assert_eq!(
            decoded.metadata().cyber_session().refusal_key(&key),
            Some(marker.clone())
        );
    }
    for thread in ["child-a", "child-b", "parent"] {
        let mut frame = body.clone();
        frame["type"] = json!("response.create");
        frame["previous_response_id"] = json!("resp_shared_parent");
        frame["client_metadata"]["x-codex-turn-metadata"]["thread_id"] = json!(thread);
        let decoded = decode_response_create_with_context(&frame.to_string(), &opening).unwrap();
        assert_eq!(
            decoded
                .metadata()
                .cyber_session()
                .lookup_keys(&key)
                .contains(&marker),
            thread == "child-a"
        );
    }
}

const ENCODINGS: [&str; 3] = ["gzip", "deflate", "zstd"];
const REQUEST: &[u8] = br#"{"model":"gpt-test","input":"hello","future_field":{"value":1}}"#;
const DECOMPRESSED_LIMIT: u64 = 64 * 1024 * 1024;

pub(super) fn encode_body(encoding: &str, mut data: impl Read) -> Vec<u8> {
    match encoding {
        "gzip" => {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            copy(&mut data, &mut encoder).expect("gzip encode");
            encoder.finish().expect("gzip finish")
        }
        "deflate" => {
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            copy(&mut data, &mut encoder).expect("deflate encode");
            encoder.finish().expect("deflate finish")
        }
        "zstd" => zstd::stream::encode_all(data, 3).expect("zstd encode"),
        _ => panic!("unsupported fixture encoding"),
    }
}

fn headers(encoding: &str) -> HeaderMap {
    HeaderMap::from_iter([(
        CONTENT_ENCODING,
        HeaderValue::from_str(encoding).expect("encoding header"),
    )])
}

#[test]
fn http_decode_should_preserve_compressed_request_fields() {
    let expected = serde_json::from_slice::<serde_json::Value>(REQUEST).expect("fixture JSON");
    for encoding in ENCODINGS {
        let compressed = encode_body(encoding, REQUEST);
        let decoded =
            decode_request_with_headers(&compressed, &headers(encoding), 64 * 1024 * 1024)
                .expect(encoding);
        assert_eq!(
            openai_wire_body(&decoded),
            expected.as_object().expect("object")
        );
    }
}

#[test]
fn http_decode_should_accept_plain_identity_and_case_insensitive_encodings() {
    for request_headers in [HeaderMap::new(), headers(""), headers(" identity ")] {
        assert!(decode_request_with_headers(REQUEST, &request_headers, 64 * 1024 * 1024).is_ok());
    }
    let compressed = encode_body("gzip", REQUEST);
    assert!(decode_request_with_headers(&compressed, &headers(" GZip "), 64 * 1024 * 1024).is_ok());
}

#[test]
fn http_decode_should_reject_unknown_and_stacked_encodings() {
    let mut repeated = headers("identity");
    repeated.append(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    for request_headers in [headers("br"), headers("gzip, zstd"), repeated] {
        let error = decode_request_with_headers(REQUEST, &request_headers, 64 * 1024 * 1024)
            .expect_err("unsupported encoding");
        assert!(matches!(
            error,
            RequestDecodeError::UnsupportedContentEncoding { .. }
        ));
        assert_eq!(
            error.protocol_body().into_value()["error"]["code"],
            "unsupported_content_encoding"
        );
    }
}

#[test]
fn http_decode_should_reject_corrupted_and_truncated_compressed_bodies() {
    for encoding in ENCODINGS {
        let mut truncated = encode_body(encoding, REQUEST);
        truncated.truncate(truncated.len() - 4);
        for body in [b"not-a-compressed-stream".as_slice(), truncated.as_slice()] {
            let error = decode_request_with_headers(body, &headers(encoding), 64 * 1024 * 1024)
                .expect_err(encoding);
            assert_eq!(
                error.protocol_body().into_value(),
                json!({"error": {
                    "type": "invalid_request_error", "code": "invalid_json",
                    "message": "Request body must be valid JSON."
                }})
            );
        }
    }
}

#[test]
fn http_decode_should_read_all_gzip_members_and_zstd_frames() {
    for encoding in ["gzip", "zstd"] {
        let mut compressed = encode_body(encoding, &REQUEST[..12]);
        compressed.extend(encode_body(encoding, &REQUEST[12..]));
        let decoded =
            decode_request_with_headers(&compressed, &headers(encoding), 64 * 1024 * 1024)
                .expect(encoding);
        assert_eq!(openai_wire_body(&decoded)["input"], "hello");
    }
}

#[test]
fn http_decode_should_not_ignore_invalid_trailing_members() {
    for encoding in ["gzip", "zstd"] {
        let mut compressed = encode_body(encoding, REQUEST);
        compressed.extend(encode_body(encoding, b"invalid trailing JSON".as_slice()));
        assert_eq!(
            decode_request_with_headers(&compressed, &headers(encoding), 64 * 1024 * 1024)
                .expect_err(encoding),
            RequestDecodeError::MalformedJson
        );
    }
}

#[test]
fn http_decode_should_reject_zstd_frames_with_an_excessive_window() {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).expect("zstd encoder");
    encoder.window_log(27).expect("128 MiB window");
    encoder.write_all(REQUEST).expect("write JSON");
    // 提前输出帧头，避免编码器根据最终小正文缩减窗口
    encoder.flush().expect("flush frame header");
    let compressed = encoder.finish().expect("finish frame");
    assert_eq!(
        zstd::stream::decode_all(compressed.as_slice()).expect("valid zstd frame"),
        REQUEST
    );
    assert_eq!(
        decode_request_with_headers(&compressed, &headers("zstd"), 128 * 1024 * 1024)
            .expect_err("oversized window"),
        RequestDecodeError::MalformedJson
    );
}

#[test]
fn http_decode_should_accept_the_exact_decompressed_limit() {
    let padding = repeat(b' ').take(DECOMPRESSED_LIMIT - REQUEST.len() as u64);
    let compressed = encode_body("zstd", REQUEST.chain(padding));
    let decoded = decode_request_with_headers(&compressed, &headers("zstd"), 64 * 1024 * 1024)
        .expect("exact limit");
    assert_eq!(decoded.metadata().requested_model(), "gpt-test");
}

#[test]
fn http_decode_should_reject_bodies_above_the_decompressed_limit() {
    for encoding in ENCODINGS {
        let compressed = encode_body(encoding, repeat(0).take(DECOMPRESSED_LIMIT + 1));
        let error = decode_request_with_headers(&compressed, &headers(encoding), 64 * 1024 * 1024)
            .expect_err(encoding);
        assert_eq!(
            error,
            RequestDecodeError::DecompressedBodyTooLarge {
                limit_bytes: 64 * 1024 * 1024
            },
            "{encoding}"
        );
    }
}

#[test]
fn http_decode_should_stop_at_limit_before_reading_invalid_trailing_frames() {
    for encoding in ["zstd", "gzip"] {
        // 尾部故意损坏：若先完整解压再检查长度，就会错误地返回 invalid_json，
        // 这个断言验证越界时已经停止读取，而不只是最终返回了某个超限错误
        let mut compressed = encode_body(encoding, repeat(0).take(DECOMPRESSED_LIMIT + 1));
        compressed.extend_from_slice(b"invalid trailing frame");
        let error = decode_request_with_headers(&compressed, &headers(encoding), 64 * 1024 * 1024)
            .expect_err(encoding);
        assert_eq!(
            error,
            RequestDecodeError::DecompressedBodyTooLarge {
                limit_bytes: 64 * 1024 * 1024
            },
            "{encoding}"
        );
    }
}

#[test]
fn http_decode_should_apply_configured_limits_to_every_encoding_and_frame() {
    for encoding in ENCODINGS {
        let compressed = encode_body(encoding, REQUEST);
        assert!(
            decode_request_with_headers(&compressed, &headers(encoding), REQUEST.len()).is_ok()
        );
        let error = decode_request_with_headers(&compressed, &headers(encoding), REQUEST.len() - 1)
            .unwrap_err();
        assert_eq!(
            error,
            RequestDecodeError::DecompressedBodyTooLarge {
                limit_bytes: REQUEST.len() - 1
            }
        );
        assert!(
            error
                .protocol_body()
                .error
                .message
                .contains(&(REQUEST.len() - 1).to_string())
        );
    }
    for encoding in ["gzip", "zstd"] {
        let split = REQUEST.len() / 2;
        let mut compressed = encode_body(encoding, &REQUEST[..split]);
        compressed.extend(encode_body(encoding, &REQUEST[split..]));
        assert!(
            decode_request_with_headers(&compressed, &headers(encoding), REQUEST.len()).is_ok()
        );
        assert!(matches!(
            decode_request_with_headers(&compressed, &headers(encoding), REQUEST.len() - 1),
            Err(RequestDecodeError::DecompressedBodyTooLarge { .. })
        ));
    }
    for request_headers in [HeaderMap::new(), headers("identity")] {
        assert!(decode_request_with_headers(REQUEST, &request_headers, 1).is_ok());
    }
}

fn cyber_request(
    mut body: serde_json::Value,
    session: &str,
) -> gateway_core::provider_ports::CyberSessionRequest {
    body["model"] = json!("gpt-test");
    let mut headers = HeaderMap::new();
    if !session.is_empty() {
        headers.insert("session_id", HeaderValue::from_str(session).unwrap());
    }
    decode_request_with_headers(body.to_string().as_bytes(), &headers, 65536)
        .unwrap()
        .metadata()
        .cyber_session()
        .clone()
}

#[test]
fn cyber_thread_identity_is_key_scoped_and_never_uses_shared_parent_continuation() {
    let key = gateway_core::policy::ClientApiKeyId::new("key-a").unwrap();
    let other = gateway_core::policy::ClientApiKeyId::new("key-b").unwrap();
    let identity = |thread: &str| {
        cyber_request(
            json!({
                "input":"hello", "previous_response_id":"resp_shared",
                "client_metadata":{"session_id":"shared", "x-codex-turn-metadata":{"thread_id":thread}}
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
    let encoded = cyber_request(
        json!({"input":"hello","client_metadata":{"x-codex-turn-metadata":"{\"thread_id\":\"child-a\"}"}}),
        "different",
    );
    assert_eq!(encoded.refusal_key(&key), Some(child));
    assert_eq!(
        cyber_request(json!({}), "legacy-session")
            .lookup_keys(&key)
            .len(),
        1
    );
}

#[test]
fn cyber_transcript_prefixes_preserve_descendants_and_bound_lookups() {
    let key = gateway_core::policy::ClientApiKeyId::new("key-a").unwrap();
    let first = cyber_request(
        json!({"instructions":"same","input":[{"role":"user","content":"blocked"}]}),
        "",
    );
    let marker = first.refusal_key(&key).unwrap();
    let descendant = cyber_request(
        json!({"instructions":"same","input":[{"content":"blocked","role":"user"},{"role":"assistant","content":"next"}]}),
        "",
    );
    assert!(descendant.lookup_keys(&key).contains(&marker));
    assert!(
        !cyber_request(
            json!({"instructions":"different","input":[{"role":"user","content":"blocked"}]}),
            ""
        )
        .lookup_keys(&key)
        .contains(&marker)
    );
    let long = cyber_request(
        json!({"input":(0..300).map(|n| json!({"content":n})).collect::<Vec<_>>()}),
        "",
    );
    assert_eq!(
        long.lookup_keys(&key).len(),
        gateway_core::provider_ports::MAX_TRANSCRIPT_LOOKUPS
    );
}
