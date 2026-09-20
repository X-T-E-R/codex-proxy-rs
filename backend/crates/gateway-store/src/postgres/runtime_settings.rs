//! `runtime_settings` 单例与 config revision 的 PostgreSQL owner。

use std::num::NonZeroU32;
use std::time::Duration;
use std::{collections::BTreeMap, fmt};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};

use gateway_core::account::RotationStrategy;
use gateway_core::policy::CodexClientVersion;
use gateway_core::provider_ports::{
    OpenAiRequestBodyOverride, ProviderRefreshPolicy, ProviderRuntimePolicyPort,
    ProviderStoreError, ProviderStoreErrorKind, ProviderWebSocketPoolPolicy,
    ProviderWebSocketPoolPolicyPort,
};

use crate::{Revision, StoreError, StoreResult, postgres_unavailable};

#[derive(Clone, PartialEq, Eq)]
pub struct RuntimeSettings {
    pub config_revision: Revision,
    pub admin_api_key: Option<String>,
    pub refresh_margin_seconds: u64,
    pub refresh_concurrency: u32,
    pub max_concurrent_per_account: u32,
    pub request_interval_ms: u64,
    pub capacity_queue_retry_seconds: u32,
    pub capacity_queue_timeout_seconds: u32,
    pub rotation_strategy: String,
    pub model_mappings: BTreeMap<String, String>,
    pub min_codex_desktop_version: Option<String>,
    pub min_codex_cli_version: Option<String>,
    pub usage_retention_days: u32,
    pub ops_event_retention_days: u32,
    pub audit_retention_days: u32,
    pub ws_pool_enabled: bool,
    pub ws_pool_max_age_ms: u64,
    pub ws_pool_max_connecting: u32,
    pub ws_pool_stream_idle_timeout_ms: u64,
    pub ws_pool_fast_path_budget_ms: u64,
    pub overload_cooldown_enabled: bool,
    pub overload_cooldown_threshold: u32,
    pub overload_cooldown_seconds: u32,
    pub cyber_session_block_enabled: bool,
    pub cyber_session_block_ttl_seconds: u32,
    pub openai_user_agent: Option<String>,
    pub openai_request_body_override_enabled: bool,
    pub openai_request_timezone: String,
    pub openai_search_country: String,
    pub updated_at: DateTime<Utc>,
}

impl fmt::Debug for RuntimeSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeSettings")
            .field("config_revision", &self.config_revision)
            .field(
                "admin_api_key",
                &self.admin_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field("refresh_margin_seconds", &self.refresh_margin_seconds)
            .field("refresh_concurrency", &self.refresh_concurrency)
            .field(
                "max_concurrent_per_account",
                &self.max_concurrent_per_account,
            )
            .field("request_interval_ms", &self.request_interval_ms)
            .field(
                "capacity_queue_retry_seconds",
                &self.capacity_queue_retry_seconds,
            )
            .field(
                "capacity_queue_timeout_seconds",
                &self.capacity_queue_timeout_seconds,
            )
            .field("rotation_strategy", &self.rotation_strategy)
            .field("model_mappings", &self.model_mappings)
            .field("min_codex_desktop_version", &self.min_codex_desktop_version)
            .field("min_codex_cli_version", &self.min_codex_cli_version)
            .field("usage_retention_days", &self.usage_retention_days)
            .field("ops_event_retention_days", &self.ops_event_retention_days)
            .field("audit_retention_days", &self.audit_retention_days)
            .field("ws_pool_enabled", &self.ws_pool_enabled)
            .field("ws_pool_max_age_ms", &self.ws_pool_max_age_ms)
            .field("ws_pool_max_connecting", &self.ws_pool_max_connecting)
            .field(
                "ws_pool_stream_idle_timeout_ms",
                &self.ws_pool_stream_idle_timeout_ms,
            )
            .field(
                "ws_pool_fast_path_budget_ms",
                &self.ws_pool_fast_path_budget_ms,
            )
            .field("updated_at", &self.updated_at)
            .field("overload_cooldown_enabled", &self.overload_cooldown_enabled)
            .field(
                "overload_cooldown_threshold",
                &self.overload_cooldown_threshold,
            )
            .field("overload_cooldown_seconds", &self.overload_cooldown_seconds)
            .field(
                "cyber_session_block_enabled",
                &self.cyber_session_block_enabled,
            )
            .field(
                "cyber_session_block_ttl_seconds",
                &self.cyber_session_block_ttl_seconds,
            )
            .field("openai_user_agent", &self.openai_user_agent)
            .field(
                "openai_request_body_override_enabled",
                &self.openai_request_body_override_enabled,
            )
            .field("openai_request_timezone", &self.openai_request_timezone)
            .field("openai_search_country", &self.openai_search_country)
            .finish()
    }
}

#[derive(Clone)]
pub struct RuntimeSettingsUpdate {
    pub admin_api_key: Option<String>,
    pub refresh_margin_seconds: u64,
    pub refresh_concurrency: u32,
    pub max_concurrent_per_account: u32,
    pub request_interval_ms: u64,
    pub capacity_queue_retry_seconds: Option<u32>,
    pub capacity_queue_timeout_seconds: Option<u32>,
    pub rotation_strategy: String,
    pub model_mappings: BTreeMap<String, String>,
    pub min_codex_desktop_version: Option<String>,
    pub min_codex_cli_version: Option<String>,
    pub usage_retention_days: u32,
    pub ops_event_retention_days: u32,
    pub audit_retention_days: u32,
    pub ws_pool_enabled: bool,
    pub ws_pool_max_age_ms: u64,
    pub ws_pool_max_connecting: u32,
    pub ws_pool_stream_idle_timeout_ms: u64,
    pub ws_pool_fast_path_budget_ms: u64,
    pub overload_cooldown_enabled: bool,
    pub overload_cooldown_threshold: u32,
    pub overload_cooldown_seconds: u32,
    pub cyber_session_block_enabled: Option<bool>,
    pub cyber_session_block_ttl_seconds: Option<u32>,
    pub openai_user_agent: Option<String>,
    pub openai_request_body_override_enabled: Option<bool>,
    pub openai_request_timezone: Option<String>,
    pub openai_search_country: Option<String>,
}

impl fmt::Debug for RuntimeSettingsUpdate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeSettingsUpdate")
            .field(
                "admin_api_key",
                &self.admin_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field("rotation_strategy", &self.rotation_strategy)
            .field("model_mappings", &self.model_mappings)
            .finish_non_exhaustive()
    }
}

impl RuntimeSettingsUpdate {
    pub fn validate(&self) -> StoreResult<()> {
        let valid_capacity_queue = match (
            self.capacity_queue_retry_seconds,
            self.capacity_queue_timeout_seconds,
        ) {
            (None, None) => true,
            (Some(retry), Some(timeout)) => retry > 0 && timeout >= retry,
            _ => false,
        };
        if self.refresh_margin_seconds == 0
            || self.refresh_concurrency == 0
            || self.max_concurrent_per_account == 0
            || !valid_capacity_queue
            || self.usage_retention_days < 31
            || self.ops_event_retention_days == 0
            || self.audit_retention_days == 0
            || self.ws_pool_max_age_ms == 0
            || self.ws_pool_max_connecting == 0
            || self.ws_pool_stream_idle_timeout_ms == 0
            || self.ws_pool_fast_path_budget_ms == 0
            || self.overload_cooldown_threshold == 0
            || self.overload_cooldown_seconds == 0
            || self
                .cyber_session_block_ttl_seconds
                .is_some_and(|seconds| seconds == 0)
            || !gateway_core::provider_ports::valid_user_agent_override(
                self.openai_user_agent.as_deref(),
            )
            || self
                .openai_request_timezone
                .as_deref()
                .is_some_and(|value| {
                    gateway_core::provider_ports::canonical_openai_request_timezone(value).is_none()
                })
            || self.openai_search_country.as_deref().is_some_and(|value| {
                gateway_core::provider_ports::canonical_openai_search_country(value).is_none()
            })
            || !valid_model_mappings(&self.model_mappings)
            || !valid_client_version(self.min_codex_desktop_version.as_deref())
            || !valid_client_version(self.min_codex_cli_version.as_deref())
            || RotationStrategy::parse(&self.rotation_strategy).is_none()
        {
            return Err(StoreError::InvalidData {
                entity: "runtime settings",
                message: "settings violate the frozen runtime constraints".to_owned(),
            });
        }
        Ok(())
    }
}

#[async_trait]
pub trait RuntimeSettingsRepository: Send + Sync {
    async fn load_runtime_settings(&self) -> StoreResult<RuntimeSettings>;

    async fn update_runtime_settings(&self, update: RuntimeSettingsUpdate)
    -> StoreResult<Revision>;
}

#[derive(Clone)]
pub struct PgRuntimeSettingsRepository {
    pool: PgPool,
}

impl PgRuntimeSettingsRepository {
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl RuntimeSettingsRepository for PgRuntimeSettingsRepository {
    async fn load_runtime_settings(&self) -> StoreResult<RuntimeSettings> {
        load_runtime_settings_from_pool(&self.pool).await
    }

    async fn update_runtime_settings(
        &self,
        update: RuntimeSettingsUpdate,
    ) -> StoreResult<Revision> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| postgres_unavailable("begin runtime settings update"))?;
        let revision = update_runtime_settings_in_transaction(&mut transaction, &update).await?;
        transaction
            .commit()
            .await
            .map_err(|_| postgres_unavailable("commit runtime settings update"))?;
        Ok(revision)
    }
}

pub(crate) async fn load_runtime_settings_from_pool(pool: &PgPool) -> StoreResult<RuntimeSettings> {
    let row = sqlx::query_as::<_, RuntimeSettingsRow>(
            "select config_revision, admin_api_key, refresh_margin_seconds,
                    refresh_concurrency, max_concurrent_per_account, request_interval_ms,
                    capacity_queue_retry_seconds, capacity_queue_timeout_seconds,
                    rotation_strategy, model_mappings_json, usage_retention_days, ops_event_retention_days,
                    audit_retention_days, min_codex_desktop_version,
                    min_codex_cli_version, ws_pool_enabled, ws_pool_max_age_ms,
                    ws_pool_max_connecting, ws_pool_stream_idle_timeout_ms,
                    ws_pool_fast_path_budget_ms, overload_cooldown_enabled,
                    overload_cooldown_threshold, overload_cooldown_seconds,
                    cyber_session_block_enabled, cyber_session_block_ttl_seconds,
                    openai_user_agent, openai_request_body_override_enabled,
                    openai_request_timezone, openai_search_country, updated_at
             from runtime_settings where id = 1",
        )
    .fetch_optional(pool)
    .await
    .map_err(|_| postgres_unavailable("load runtime settings"))?
    .ok_or_else(|| StoreError::NotFound {
        entity: "runtime settings",
        id: "1".to_owned(),
    })?;
    runtime_settings_from_row(row)
}

impl ProviderRuntimePolicyPort for PgRuntimeSettingsRepository {
    fn load_user_agent_override(
        &self,
    ) -> futures::future::BoxFuture<'_, Result<Option<String>, ProviderStoreError>> {
        Box::pin(async move {
            let settings = RuntimeSettingsRepository::load_runtime_settings(self)
                .await
                .map_err(|_| provider_unavailable("load user agent override"))?;
            if !gateway_core::provider_ports::valid_user_agent_override(
                settings.openai_user_agent.as_deref(),
            ) {
                return Err(provider_invalid("decode user agent override"));
            }
            Ok(settings.openai_user_agent)
        })
    }

    fn load_refresh_policy(
        &self,
    ) -> futures::future::BoxFuture<'_, Result<ProviderRefreshPolicy, ProviderStoreError>> {
        Box::pin(async move {
            let settings = RuntimeSettingsRepository::load_runtime_settings(self)
                .await
                .map_err(|_| provider_unavailable("load refresh policy"))?;
            let concurrency = NonZeroU32::new(settings.refresh_concurrency)
                .ok_or_else(|| provider_invalid("decode refresh policy"))?;
            ProviderRefreshPolicy::try_new(
                Duration::from_secs(settings.refresh_margin_seconds),
                concurrency,
            )
        })
    }

    fn load_openai_request_body_override(
        &self,
    ) -> futures::future::BoxFuture<'_, Result<OpenAiRequestBodyOverride, ProviderStoreError>> {
        Box::pin(async move {
            let settings = RuntimeSettingsRepository::load_runtime_settings(self)
                .await
                .map_err(|_| provider_unavailable("load OpenAI request body override"))?;
            OpenAiRequestBodyOverride::try_new(
                settings.openai_request_body_override_enabled,
                settings.openai_request_timezone,
                settings.openai_search_country,
            )
        })
    }
}

impl ProviderWebSocketPoolPolicyPort for PgRuntimeSettingsRepository {
    fn load_websocket_pool_policy(
        &self,
    ) -> futures::future::BoxFuture<'_, Result<ProviderWebSocketPoolPolicy, ProviderStoreError>>
    {
        Box::pin(async move {
            let settings = RuntimeSettingsRepository::load_runtime_settings(self)
                .await
                .map_err(|_| provider_unavailable("load WebSocket pool policy"))?;
            let max_connecting = NonZeroU32::new(settings.ws_pool_max_connecting)
                .ok_or_else(|| provider_invalid("decode WebSocket pool policy"))?;
            ProviderWebSocketPoolPolicy::try_new(
                settings.ws_pool_enabled,
                Duration::from_millis(settings.ws_pool_max_age_ms),
                max_connecting,
                Duration::from_millis(settings.ws_pool_stream_idle_timeout_ms),
                Duration::from_millis(settings.ws_pool_fast_path_budget_ms),
            )
        })
    }
}

pub(crate) async fn load_runtime_settings_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
) -> StoreResult<RuntimeSettings> {
    let row = sqlx::query_as::<_, RuntimeSettingsRow>(
        "select config_revision, admin_api_key, refresh_margin_seconds,
                refresh_concurrency, max_concurrent_per_account, request_interval_ms,
                capacity_queue_retry_seconds, capacity_queue_timeout_seconds,
                rotation_strategy, model_mappings_json, usage_retention_days, ops_event_retention_days,
                audit_retention_days, min_codex_desktop_version,
                min_codex_cli_version, ws_pool_enabled, ws_pool_max_age_ms,
                ws_pool_max_connecting, ws_pool_stream_idle_timeout_ms,
                ws_pool_fast_path_budget_ms, overload_cooldown_enabled,
                overload_cooldown_threshold, overload_cooldown_seconds,
                cyber_session_block_enabled, cyber_session_block_ttl_seconds,
                openai_user_agent, openai_request_body_override_enabled,
                openai_request_timezone, openai_search_country, updated_at
         from runtime_settings where id = 1",
    )
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| postgres_unavailable("load runtime settings in transaction"))?
    .ok_or_else(|| StoreError::NotFound {
        entity: "runtime settings",
        id: "1".to_owned(),
    })?;
    runtime_settings_from_row(row)
}

pub(crate) async fn update_runtime_settings_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    update: &RuntimeSettingsUpdate,
) -> StoreResult<Revision> {
    update.validate()?;
    let refresh_margin_seconds =
        i64::try_from(update.refresh_margin_seconds).map_err(|_| invalid_numeric())?;
    let openai_request_timezone = update
        .openai_request_timezone
        .as_deref()
        .map(|value| {
            gateway_core::provider_ports::canonical_openai_request_timezone(value).ok_or(())
        })
        .transpose()
        .map_err(|()| invalid_openai_request_locale())?;
    let openai_search_country = update
        .openai_search_country
        .as_deref()
        .map(|value| gateway_core::provider_ports::canonical_openai_search_country(value).ok_or(()))
        .transpose()
        .map_err(|()| invalid_openai_request_locale())?;
    let next = sqlx::query_scalar::<_, i64>(
        "update runtime_settings
             set config_revision = config_revision + 1,
	                 admin_api_key = $1,
	                 refresh_margin_seconds = $2,
	                 refresh_concurrency = $3,
	                 max_concurrent_per_account = $4,
	                 request_interval_ms = $5,
	                 rotation_strategy = $6,
	                 model_mappings_json = $7,
	                 usage_retention_days = $8,
	                 ops_event_retention_days = $9,
	                 audit_retention_days = $10,
	                 min_codex_desktop_version = $11,
	                 min_codex_cli_version = $12,
	                 ws_pool_enabled = $13,
	                 ws_pool_max_age_ms = $14,
	                 ws_pool_max_connecting = $15,
	                 ws_pool_stream_idle_timeout_ms = $16,
	                 ws_pool_fast_path_budget_ms = $17,
	                 overload_cooldown_enabled = $18,
	                 overload_cooldown_threshold = $19,
	                 overload_cooldown_seconds = $20,
	                 cyber_session_block_enabled = coalesce($21, cyber_session_block_enabled),
	                 cyber_session_block_ttl_seconds = coalesce($22, cyber_session_block_ttl_seconds),
	                 openai_user_agent = $23,
	                 openai_request_body_override_enabled = coalesce($24, openai_request_body_override_enabled),
	                 openai_request_timezone = coalesce($25, openai_request_timezone),
	                 openai_search_country = coalesce($26, openai_search_country),
	                 capacity_queue_retry_seconds = coalesce($27, capacity_queue_retry_seconds),
	                 capacity_queue_timeout_seconds = coalesce($28, capacity_queue_timeout_seconds),
	                 updated_at = now()
	             where id = 1
	             returning config_revision",
    )
    .bind(update.admin_api_key.as_deref())
    .bind(refresh_margin_seconds)
    .bind(i64::from(update.refresh_concurrency))
    .bind(i64::from(update.max_concurrent_per_account))
    .bind(i64::try_from(update.request_interval_ms).map_err(|_| invalid_numeric())?)
    .bind(&update.rotation_strategy)
    .bind(sqlx::types::Json(&update.model_mappings))
    .bind(i64::from(update.usage_retention_days))
    .bind(i64::from(update.ops_event_retention_days))
    .bind(i64::from(update.audit_retention_days))
    .bind(update.min_codex_desktop_version.as_deref())
    .bind(update.min_codex_cli_version.as_deref())
    .bind(update.ws_pool_enabled)
    .bind(i64::try_from(update.ws_pool_max_age_ms).map_err(|_| invalid_numeric())?)
    .bind(i64::from(update.ws_pool_max_connecting))
    .bind(i64::try_from(update.ws_pool_stream_idle_timeout_ms).map_err(|_| invalid_numeric())?)
    .bind(i64::try_from(update.ws_pool_fast_path_budget_ms).map_err(|_| invalid_numeric())?)
    .bind(update.overload_cooldown_enabled)
    .bind(i64::from(update.overload_cooldown_threshold))
    .bind(i64::from(update.overload_cooldown_seconds))
    .bind(update.cyber_session_block_enabled)
    .bind(update.cyber_session_block_ttl_seconds.map(i64::from))
    .bind(update.openai_user_agent.as_deref())
    .bind(update.openai_request_body_override_enabled)
    .bind(openai_request_timezone.as_deref())
    .bind(openai_search_country.as_deref())
    .bind(update.capacity_queue_retry_seconds.map(i64::from))
    .bind(update.capacity_queue_timeout_seconds.map(i64::from))
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| postgres_unavailable("update runtime settings in transaction"))?
    .ok_or_else(|| StoreError::NotFound {
        entity: "runtime settings",
        id: "1".to_owned(),
    })?;
    Revision::new(u64::try_from(next).map_err(|_| invalid_numeric())?)
}

pub(crate) async fn bump_config_revision_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
) -> StoreResult<Revision> {
    let next = sqlx::query_scalar::<_, i64>(
        "update runtime_settings
         set config_revision = config_revision + 1, updated_at = now()
         where id = 1
         returning config_revision",
    )
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|_| postgres_unavailable("bump config revision in transaction"))?
    .ok_or_else(|| StoreError::NotFound {
        entity: "runtime settings",
        id: "1".to_owned(),
    })?;
    Revision::new(u64::try_from(next).map_err(|_| invalid_numeric())?)
}

/// 更新 admin_api_key 字段，config revision 由调用方 bump。
pub(crate) async fn update_admin_api_key_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    admin_api_key: Option<String>,
) -> StoreResult<()> {
    sqlx::query(
        "update runtime_settings
         set admin_api_key = $1,
             updated_at = now()
         where id = 1",
    )
    .bind(admin_api_key.as_deref())
    .execute(&mut **transaction)
    .await
    .map_err(|_| postgres_unavailable("update admin api key in transaction"))?;
    Ok(())
}

// 列数超过 sqlx 元组 FromRow 的元数上限，用具名列结构接收查询结果。
#[derive(sqlx::FromRow)]
struct RuntimeSettingsRow {
    config_revision: i64,
    admin_api_key: Option<String>,
    refresh_margin_seconds: i64,
    refresh_concurrency: i64,
    max_concurrent_per_account: i64,
    request_interval_ms: i64,
    capacity_queue_retry_seconds: i64,
    capacity_queue_timeout_seconds: i64,
    rotation_strategy: String,
    model_mappings_json: sqlx::types::Json<BTreeMap<String, String>>,
    usage_retention_days: i64,
    ops_event_retention_days: i64,
    audit_retention_days: i64,
    min_codex_desktop_version: Option<String>,
    min_codex_cli_version: Option<String>,
    ws_pool_enabled: bool,
    ws_pool_max_age_ms: i64,
    ws_pool_max_connecting: i64,
    ws_pool_stream_idle_timeout_ms: i64,
    ws_pool_fast_path_budget_ms: i64,
    overload_cooldown_enabled: bool,
    overload_cooldown_threshold: i64,
    overload_cooldown_seconds: i64,
    cyber_session_block_enabled: bool,
    cyber_session_block_ttl_seconds: i64,
    openai_user_agent: Option<String>,
    openai_request_body_override_enabled: bool,
    openai_request_timezone: String,
    openai_search_country: String,
    updated_at: DateTime<Utc>,
}

fn runtime_settings_from_row(row: RuntimeSettingsRow) -> StoreResult<RuntimeSettings> {
    Ok(RuntimeSettings {
        config_revision: Revision::new(to_u64(row.config_revision)?)?,
        admin_api_key: row.admin_api_key,
        refresh_margin_seconds: to_u64(row.refresh_margin_seconds)?,
        refresh_concurrency: to_u32(row.refresh_concurrency)?,
        max_concurrent_per_account: to_u32(row.max_concurrent_per_account)?,
        request_interval_ms: to_u64(row.request_interval_ms)?,
        capacity_queue_retry_seconds: to_u32(row.capacity_queue_retry_seconds)?,
        capacity_queue_timeout_seconds: to_u32(row.capacity_queue_timeout_seconds)?,
        rotation_strategy: row.rotation_strategy,
        model_mappings: row.model_mappings_json.0,
        usage_retention_days: to_u32(row.usage_retention_days)?,
        ops_event_retention_days: to_u32(row.ops_event_retention_days)?,
        audit_retention_days: to_u32(row.audit_retention_days)?,
        min_codex_desktop_version: row.min_codex_desktop_version,
        min_codex_cli_version: row.min_codex_cli_version,
        ws_pool_enabled: row.ws_pool_enabled,
        ws_pool_max_age_ms: to_u64(row.ws_pool_max_age_ms)?,
        ws_pool_max_connecting: to_u32(row.ws_pool_max_connecting)?,
        ws_pool_stream_idle_timeout_ms: to_u64(row.ws_pool_stream_idle_timeout_ms)?,
        ws_pool_fast_path_budget_ms: to_u64(row.ws_pool_fast_path_budget_ms)?,
        overload_cooldown_enabled: row.overload_cooldown_enabled,
        overload_cooldown_threshold: to_u32(row.overload_cooldown_threshold)?,
        overload_cooldown_seconds: to_u32(row.overload_cooldown_seconds)?,
        cyber_session_block_enabled: row.cyber_session_block_enabled,
        cyber_session_block_ttl_seconds: to_u32(row.cyber_session_block_ttl_seconds)?,
        openai_user_agent: row.openai_user_agent,
        openai_request_body_override_enabled: row.openai_request_body_override_enabled,
        openai_request_timezone: row.openai_request_timezone,
        openai_search_country: row.openai_search_country,
        updated_at: row.updated_at,
    })
}

fn to_u64(value: i64) -> StoreResult<u64> {
    u64::try_from(value).map_err(|_| invalid_numeric())
}

fn to_u32(value: i64) -> StoreResult<u32> {
    u32::try_from(value).map_err(|_| invalid_numeric())
}

fn invalid_numeric() -> StoreError {
    StoreError::InvalidData {
        entity: "runtime settings",
        message: "numeric field is outside its supported range".to_owned(),
    }
}

fn invalid_openai_request_locale() -> StoreError {
    StoreError::InvalidData {
        entity: "runtime settings",
        message: "OpenAI request locale is invalid".to_owned(),
    }
}

fn provider_unavailable(operation: &'static str) -> ProviderStoreError {
    ProviderStoreError::new(ProviderStoreErrorKind::Unavailable, operation)
}

fn provider_invalid(operation: &'static str) -> ProviderStoreError {
    ProviderStoreError::new(ProviderStoreErrorKind::InvalidData, operation)
}

fn valid_model_mappings(mappings: &BTreeMap<String, String>) -> bool {
    mappings.len() <= 512
        && mappings.iter().all(|(requested, upstream)| {
            valid_model_name(requested, 256) && valid_model_name(upstream, 256)
        })
}

fn valid_model_name(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && !value.bytes().any(|byte| byte.is_ascii_control())
}

fn valid_client_version(value: Option<&str>) -> bool {
    value.is_none_or(|value| CodexClientVersion::parse(value).is_ok())
}
