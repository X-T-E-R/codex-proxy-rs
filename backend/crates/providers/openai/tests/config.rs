use std::path::Path;

use chrono::{TimeZone as _, Utc};
use provider_openai::config::{
    CodexWebSocketPoolSettings, CodexWireProfileConfig, DEFAULT_STREAM_MAX_RETRIES,
    MAX_STREAM_MAX_RETRIES, OpenAiConfig,
};

#[test]
fn openai_config_builds_the_audited_wire_profile() {
    let mut config = valid_config();
    config
        .resolve_and_validate(Path::new("/srv/gateway"))
        .expect("valid OpenAI config");
    let profile = config.wire_profile_state().snapshot();

    assert_eq!(
        profile.user_agent(),
        "Codex Desktop/0.102.0 (Mac OS 15.5.0; arm64) xterm-256color (Codex Desktop; 1.2026.190)"
    );
    assert_eq!(profile.desktop_build, "19012345678");
}

#[test]
fn openai_config_derives_identity_secret_from_runtime_data_dir() {
    let mut config = valid_config();
    config
        .resolve_and_validate(Path::new("/srv/gateway/runtime-data"))
        .expect("valid OpenAI config");

    assert!(format!("{config:?}").contains("/srv/gateway/runtime-data/identity_hmac_secret"));
}

#[test]
fn openai_config_rejects_noncanonical_versions_and_empty_fields() {
    let mut config = valid_config();
    config.wire_profile.codex_version = "latest".to_owned();
    assert!(
        config
            .resolve_and_validate(Path::new("/srv/gateway"))
            .is_err()
    );

    let mut config = valid_config();
    config.wire_profile.desktop_version = "1.preview".to_owned();
    assert!(
        config
            .resolve_and_validate(Path::new("/srv/gateway"))
            .is_err()
    );

    let mut config = valid_config();
    config.wire_profile.originator.clear();
    assert!(
        config
            .resolve_and_validate(Path::new("/srv/gateway"))
            .is_err()
    );
}

#[test]
fn openai_config_restricts_upstream_base_url_to_https_or_loopback_http() {
    for base_url in [
        "http://internal.example.com/backend-api",
        "http://10.0.0.7/backend-api",
        "https://chatgpt.com/backend-api?debug=1",
        "https://user:pass@chatgpt.com/backend-api",
        "https://chatgpt.com/backend-api#fragment",
        "ftp://chatgpt.com/backend-api",
    ] {
        let mut config = valid_config();
        config.api.base_url = base_url.to_owned();
        assert!(
            config
                .resolve_and_validate(Path::new("/srv/gateway"))
                .is_err(),
            "expected {base_url} to be rejected"
        );
    }

    for base_url in [
        "https://chatgpt.com/backend-api",
        "http://127.0.0.1:8080/backend-api",
        "http://localhost:8080/backend-api",
        "http://[::1]:8080/backend-api",
    ] {
        let mut config = valid_config();
        config.api.base_url = base_url.to_owned();
        assert!(
            config
                .resolve_and_validate(Path::new("/srv/gateway"))
                .is_ok(),
            "expected {base_url} to be accepted"
        );
    }
}

#[test]
fn openai_config_defaults_to_the_provider_owned_operating_values() {
    let config = OpenAiConfig::default();
    assert_eq!(DEFAULT_STREAM_MAX_RETRIES, 5);

    assert_eq!(
        (
            config.api.base_url.as_str(),
            config.ws_pool.enabled,
            config.ws_pool.max_age_ms,
            config.ws_pool.max_connecting,
            config.ws_pool.stream_idle_timeout_ms,
            config.ws_pool.fast_path_budget_ms,
            config.quota.refresh_interval_minutes,
            config.auth.refresh_enabled,
            config.auth.oauth_client_id.as_str(),
            config.auth.oauth_token_endpoint.as_str(),
            config.stream_max_retries(),
        ),
        (
            "https://chatgpt.com/backend-api",
            true,
            3_300_000,
            8,
            300_000,
            800,
            15,
            true,
            "app_EMoamEEZ73f0CkXaXp7hrann",
            "https://auth.openai.com/oauth/token",
            u32::try_from(DEFAULT_STREAM_MAX_RETRIES).expect("default retry budget fits u32"),
        )
    );
    assert_eq!(
        config.wire_profile_state().snapshot().user_agent(),
        "Codex Desktop/0.153.4 (Mac OS 15.7.1; arm64) unknown (Codex Desktop; 26.901.51231)"
    );
}

#[test]
fn openai_config_rejects_zero_ws_pool_numeric_fields() {
    // max_age / max_connecting / fast_path_budget 为 0 时池语义不成立，启动即拒绝。
    for mutate in [
        (|config: &mut OpenAiConfig| config.ws_pool.max_age_ms = 0) as fn(&mut OpenAiConfig),
        (|config: &mut OpenAiConfig| config.ws_pool.max_connecting = 0) as fn(&mut OpenAiConfig),
        (|config: &mut OpenAiConfig| config.ws_pool.fast_path_budget_ms = 0)
            as fn(&mut OpenAiConfig),
    ] {
        let mut config = valid_config();
        mutate(&mut config);
        assert!(
            config
                .resolve_and_validate(Path::new("/srv/gateway"))
                .is_err()
        );
    }
}

#[test]
fn openai_ws_pool_settings_parse_without_fast_path_budget_field() {
    // 已部署 config.yaml 的 ws_pool 段没有 fast_path_budget_ms；缺少该字段时必须
    // 回落到代码默认值，否则旧配置无法启动。
    let settings: CodexWebSocketPoolSettings = serde_json::from_value(serde_json::json!({
        "enabled": true,
        "max_age_ms": 3_300_000,
        "max_connecting": 8,
        "stream_idle_timeout_ms": 300_000
    }))
    .expect("ws_pool settings without fast_path_budget_ms should still parse");

    assert_eq!(settings.fast_path_budget_ms, 800);
}

#[test]
fn openai_stream_retry_budget_uses_the_official_hard_cap() {
    let mut config = OpenAiConfig::default();
    config.stream_max_retries = MAX_STREAM_MAX_RETRIES + 1;

    assert_eq!(
        config.stream_max_retries(),
        u32::try_from(MAX_STREAM_MAX_RETRIES).expect("retry cap fits u32")
    );
}

fn valid_config() -> OpenAiConfig {
    let mut config = OpenAiConfig::default();
    config.wire_profile = CodexWireProfileConfig {
        originator: "Codex Desktop".to_owned(),
        codex_version: "0.102.0".to_owned(),
        desktop_version: "1.2026.190".to_owned(),
        desktop_build: "19012345678".to_owned(),
        os_type: "Mac OS".to_owned(),
        os_version: "15.5.0".to_owned(),
        arch: "arm64".to_owned(),
        terminal: "xterm-256color".to_owned(),
        residency: None,
        verified_at: Utc
            .with_ymd_and_hms(2026, 7, 19, 0, 0, 0)
            .single()
            .expect("valid test time"),
    };
    config
}
