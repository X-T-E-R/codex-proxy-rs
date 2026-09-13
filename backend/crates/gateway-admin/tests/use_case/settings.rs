use async_trait::async_trait;

use gateway_admin::{
    model::{
        AdminErrorKind, MutationContext,
        settings::{
            AdminApiKey, AdminApiKeyMutation, ReplaceRuntimeSettings, RotationStrategy,
            RuntimeSettings,
        },
    },
    ports::store::{AdminStoreError, AdminStoreErrorKind, AdminStoreResult, SettingsStore},
};

struct UnusedSettingsStore;

#[async_trait]
impl SettingsStore for UnusedSettingsStore {
    async fn load_runtime_settings(&self) -> AdminStoreResult<RuntimeSettings> {
        Err(unused())
    }

    async fn admin_api_key_exists(&self) -> AdminStoreResult<bool> {
        Err(unused())
    }

    async fn replace_runtime_settings(
        &self,
        _: ReplaceRuntimeSettings,
        _: &MutationContext,
    ) -> AdminStoreResult<RuntimeSettings> {
        Err(unused())
    }

    async fn replace_admin_api_key(
        &self,
        _: AdminApiKey,
        _: &MutationContext,
    ) -> AdminStoreResult<AdminApiKeyMutation> {
        Err(unused())
    }

    async fn delete_admin_api_key(
        &self,
        _: &MutationContext,
    ) -> AdminStoreResult<AdminApiKeyMutation> {
        Err(unused())
    }
}

fn valid_replace_command() -> ReplaceRuntimeSettings {
    ReplaceRuntimeSettings {
        model_mappings: Default::default(),
        refresh_margin_seconds: 3_600,
        refresh_concurrency: 1,
        max_concurrent_per_account: 1,
        request_interval_ms: 0,
        rotation_strategy: RotationStrategy::Smart,
        min_codex_desktop_version: None,
        min_codex_cli_version: None,
        usage_retention_days: 31,
        ops_event_retention_days: 30,
        audit_retention_days: 30,
        ws_pool_enabled: true,
        ws_pool_max_age_ms: 3_300_000,
        ws_pool_max_connecting: 8,
        ws_pool_stream_idle_timeout_ms: 300_000,
        ws_pool_fast_path_budget_ms: 800,
        overload_cooldown_enabled: false,
        overload_cooldown_threshold: 2,
        overload_cooldown_seconds: 120,
        cyber_session_block_enabled: Some(false),
        cyber_session_block_ttl_seconds: Some(3600),
        openai_user_agent: None,
        openai_request_body_override_enabled: Some(true),
        openai_request_timezone: Some("America/Los_Angeles".to_owned()),
        openai_search_country: Some("US".to_owned()),
    }
}

async fn replace_and_expect_invalid(command: ReplaceRuntimeSettings) {
    let services = super::AdminHarness::new()
        .settings(std::sync::Arc::new(UnusedSettingsStore))
        .build()
        .await;
    let error = services
        .settings()
        .replace(
            &MutationContext {
                actor: gateway_admin::model::MutationActor::System,
                request_id: "request-settings".to_owned(),
            },
            command,
        )
        .await
        .expect_err("invalid settings");

    assert_eq!(error.kind(), AdminErrorKind::Invalid);
}

#[tokio::test]
async fn settings_should_reject_zero_refresh_margin_before_store_call() {
    replace_and_expect_invalid(ReplaceRuntimeSettings {
        refresh_margin_seconds: 0,
        ..valid_replace_command()
    })
    .await;
}

#[tokio::test]
async fn settings_should_reject_zero_ws_pool_values_before_store_call() {
    // 四个 ws_pool 数值字段任一归零都必须在到达 store 前拒绝。
    for mutate in [
        (|command: &mut ReplaceRuntimeSettings| command.ws_pool_max_age_ms = 0)
            as fn(&mut ReplaceRuntimeSettings),
        (|command: &mut ReplaceRuntimeSettings| command.ws_pool_max_connecting = 0)
            as fn(&mut ReplaceRuntimeSettings),
        (|command: &mut ReplaceRuntimeSettings| {
            command.ws_pool_stream_idle_timeout_ms = 0;
        }) as fn(&mut ReplaceRuntimeSettings),
        (|command: &mut ReplaceRuntimeSettings| {
            command.ws_pool_fast_path_budget_ms = 0;
        }) as fn(&mut ReplaceRuntimeSettings),
    ] {
        let mut command = valid_replace_command();
        mutate(&mut command);
        replace_and_expect_invalid(command).await;
    }
}

fn unused() -> AdminStoreError {
    AdminStoreError::new(
        AdminStoreErrorKind::Unavailable,
        "settings",
        "unused in this test",
    )
}

#[tokio::test]
async fn overload_cooldown_settings_reject_zero_even_when_disabled() {
    replace_and_expect_invalid(ReplaceRuntimeSettings {
        overload_cooldown_threshold: 0,
        ..valid_replace_command()
    })
    .await;
    replace_and_expect_invalid(ReplaceRuntimeSettings {
        overload_cooldown_seconds: 0,
        ..valid_replace_command()
    })
    .await;
}

#[tokio::test]
async fn cyber_session_ttl_rejects_zero_even_when_disabled() {
    replace_and_expect_invalid(ReplaceRuntimeSettings {
        cyber_session_block_ttl_seconds: Some(0),
        ..valid_replace_command()
    })
    .await;
}

#[tokio::test]
async fn openai_request_locale_rejects_invalid_values_before_store_call() {
    replace_and_expect_invalid(ReplaceRuntimeSettings {
        openai_request_timezone: Some("Pacific".to_owned()),
        ..valid_replace_command()
    })
    .await;
    replace_and_expect_invalid(ReplaceRuntimeSettings {
        openai_search_country: Some("USA".to_owned()),
        ..valid_replace_command()
    })
    .await;
}
