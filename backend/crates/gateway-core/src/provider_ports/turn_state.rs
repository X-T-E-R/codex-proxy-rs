//! OpenAI 账号的实验性出站 turn state 与上游原值观测端口。

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use chrono::{DateTime, Utc};

pub const MODEL_TURN_STATE_BYTES: usize = 292;
pub const DEFAULT_MODEL_REUSE_WINDOW_SECONDS: u32 = 7_200;
pub const DEFAULT_CAPTURE_MAX_ATTEMPTS: u8 = 3;
pub const DEFAULT_CAPTURE_ATTEMPT_TIMEOUT_SECONDS: u8 = 8;
pub const DEFAULT_CAPTURE_JOB_TIMEOUT_SECONDS: u8 = 30;
pub const DEFAULT_CAPTURE_BACKOFF_SECONDS: u8 = 1;
pub const DEFAULT_CAPTURE_MAX_BACKOFF_SECONDS: u8 = 4;
pub const DEFAULT_CAPTURE_COOLDOWN_SECONDS: u32 = 900;
pub const DEFAULT_CAPTURE_REFRESH_LEAD_SECONDS: u32 = 900;

#[derive(Clone, PartialEq, Eq)]
pub struct TurnStateObserved {
    pub id: String,
    pub value: String,
    pub bytes: usize,
    pub sha256: String,
    pub observed_at: DateTime<Utc>,
    pub transport: String,
    pub upstream_response_id: Option<String>,
    pub client_turn_id: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct TurnStateOverride {
    pub enabled: bool,
    pub value: Option<String>,
    pub bytes: usize,
    pub sha256: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct TurnStateView {
    pub account_id: String,
    pub observed: Option<TurnStateObserved>,
    pub override_state: TurnStateOverride,
    pub config_revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTurnStatePinAction {
    Keep,
    Replace,
    Clear,
    Invalidate,
    ImportLegacy,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ModelTurnStateUpdate {
    pub expected_identity_revision: u64,
    pub expected_effective_model: String,
    /// 账号策略已迁到独立端点；这些字段仅保留旧管理客户端的 wire 兼容，Store 不采用。
    pub lock_enabled: bool,
    pub capture_enabled: bool,
    pub reuse_window_seconds: u32,
    pub capture_proxy_id: Option<String>,
    pub max_attempts: u8,
    pub attempt_timeout_seconds: u16,
    pub job_timeout_seconds: u16,
    pub backoff_seconds: u8,
    pub max_backoff_seconds: u8,
    pub cooldown_seconds: u32,
    pub pin_action: ModelTurnStatePinAction,
    pub value: Option<String>,
    pub expected_revision: u64,
}

impl std::fmt::Debug for ModelTurnStateUpdate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelTurnStateUpdate")
            .field(
                "expected_identity_revision",
                &self.expected_identity_revision,
            )
            .field("expected_effective_model", &self.expected_effective_model)
            .field("lock_enabled", &self.lock_enabled)
            .field("capture_enabled", &self.capture_enabled)
            .field("reuse_window_seconds", &self.reuse_window_seconds)
            .field("capture_proxy_id", &self.capture_proxy_id)
            .field("max_attempts", &self.max_attempts)
            .field("attempt_timeout_seconds", &self.attempt_timeout_seconds)
            .field("job_timeout_seconds", &self.job_timeout_seconds)
            .field("backoff_seconds", &self.backoff_seconds)
            .field("max_backoff_seconds", &self.max_backoff_seconds)
            .field("cooldown_seconds", &self.cooldown_seconds)
            .field("pin_action", &self.pin_action)
            .field("value", &self.value.as_ref().map(|_| "<redacted>"))
            .field("expected_revision", &self.expected_revision)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct AccountTurnStatePolicyUpdate {
    pub expected_identity_revision: u64,
    pub expected_revision: u64,
    pub lock_enabled: bool,
    pub capture_enabled: bool,
    pub reuse_window_seconds: u32,
    pub refresh_lead_seconds: u32,
    pub capture_proxy_id: Option<String>,
    pub max_attempts: u8,
    pub attempt_timeout_seconds: u16,
    pub job_timeout_seconds: u16,
    pub backoff_seconds: u8,
    pub max_backoff_seconds: u8,
    pub cooldown_seconds: u32,
}

impl std::fmt::Debug for AccountTurnStatePolicyUpdate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccountTurnStatePolicyUpdate")
            .field(
                "expected_identity_revision",
                &self.expected_identity_revision,
            )
            .field("expected_revision", &self.expected_revision)
            .field("lock_enabled", &self.lock_enabled)
            .field("capture_enabled", &self.capture_enabled)
            .field("reuse_window_seconds", &self.reuse_window_seconds)
            .field("refresh_lead_seconds", &self.refresh_lead_seconds)
            .field("capture_proxy_id", &self.capture_proxy_id)
            .field("max_attempts", &self.max_attempts)
            .field("attempt_timeout_seconds", &self.attempt_timeout_seconds)
            .field("job_timeout_seconds", &self.job_timeout_seconds)
            .field("backoff_seconds", &self.backoff_seconds)
            .field("max_backoff_seconds", &self.max_backoff_seconds)
            .field("cooldown_seconds", &self.cooldown_seconds)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ModelTurnStatePin {
    pub value: String,
    pub encoded_bytes: usize,
    pub raw_bytes: Option<usize>,
    pub ciphertext_bytes: Option<usize>,
    pub envelope_format: Option<&'static str>,
    pub token_version: Option<u8>,
    pub issued_at: Option<DateTime<Utc>>,
    pub timestamp_verified: bool,
    pub sha256: String,
    pub captured_at: DateTime<Utc>,
    pub reuse_deadline: DateTime<Utc>,
    pub source: String,
    pub compatible_transport: String,
    pub invalidated: bool,
    pub generation: u64,
    pub id: Option<String>,
}

impl std::fmt::Debug for ModelTurnStatePin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelTurnStatePin")
            .field("value", &"<redacted>")
            .field("encoded_bytes", &self.encoded_bytes)
            .field("raw_bytes", &self.raw_bytes)
            .field("ciphertext_bytes", &self.ciphertext_bytes)
            .field("envelope_format", &self.envelope_format)
            .field("token_version", &self.token_version)
            .field("issued_at", &self.issued_at)
            .field("timestamp_verified", &self.timestamp_verified)
            .field("sha256", &self.sha256)
            .field("captured_at", &self.captured_at)
            .field("reuse_deadline", &self.reuse_deadline)
            .field("source", &self.source)
            .field("compatible_transport", &self.compatible_transport)
            .field("invalidated", &self.invalidated)
            .field("generation", &self.generation)
            .field("id", &self.id)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTurnStateCapturePolicy {
    pub max_attempts: u8,
    pub attempt_timeout_seconds: u16,
    pub job_timeout_seconds: u16,
    pub backoff_seconds: u8,
    pub max_backoff_seconds: u8,
    pub cooldown_seconds: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTurnStateView {
    pub account_id: String,
    pub requested_model: String,
    pub effective_model: String,
    pub identity_revision: u64,
    pub config_revision: u64,
    pub policy_revision: u64,
    pub lock_enabled: bool,
    pub capture_enabled: bool,
    pub reuse_window_seconds: u32,
    pub refresh_lead_seconds: u32,
    pub capture_proxy_id: Option<String>,
    pub capture_proxy: Option<ModelTurnStateCaptureProxy>,
    pub capture_policy: ModelTurnStateCapturePolicy,
    pub pin: Option<ModelTurnStatePin>,
    pub candidate: Option<ModelTurnStatePin>,
    pub next_capture_at: Option<DateTime<Utc>>,
    pub next_activation_at: Option<DateTime<Utc>>,
    pub capture_not_before: Option<DateTime<Utc>>,
    pub waiting_reason: Option<String>,
    pub legacy_override_enabled: bool,
    pub legacy_override_configured: bool,
    pub legacy_override_value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountTurnStatePolicyView {
    pub account_id: String,
    pub identity_revision: u64,
    pub config_revision: u64,
    pub lock_enabled: bool,
    pub capture_enabled: bool,
    pub reuse_window_seconds: u32,
    pub refresh_lead_seconds: u32,
    pub capture_proxy_id: Option<String>,
    pub capture_proxy: Option<ModelTurnStateCaptureProxy>,
    pub capture_policy: ModelTurnStateCapturePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTurnStateCaptureProxy {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub last_test_at: Option<DateTime<Utc>>,
    pub ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTurnStateCaptureScope {
    pub account_id: String,
    pub requested_model: String,
    pub effective_model: String,
    pub identity_revision: u64,
    pub config_revision: u64,
    pub policy_revision: u64,
    pub capture_enabled: bool,
    pub capture_proxy_id: Option<String>,
    pub capture_policy: ModelTurnStateCapturePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTurnStateCaptureCursor {
    pub account_id: String,
    pub identity_revision: u64,
    pub effective_model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTurnStateObservationScope {
    pub account_id: String,
    pub identity_revision: u64,
    pub effective_model: String,
    pub config_revision: u64,
    pub policy_revision: u64,
}

impl ModelTurnStateCaptureScope {
    #[must_use]
    pub fn cursor(&self) -> ModelTurnStateCaptureCursor {
        ModelTurnStateCaptureCursor {
            account_id: self.account_id.clone(),
            identity_revision: self.identity_revision,
            effective_model: self.effective_model.clone(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ActiveModelTurnStatePin {
    pub value: String,
    pub sha256: String,
    pub generation: u64,
    pub candidate_id: Option<String>,
    pub source: String,
}

impl std::fmt::Debug for ActiveModelTurnStatePin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActiveModelTurnStatePin")
            .field("value", &"<redacted>")
            .field("sha256", &self.sha256)
            .field("generation", &self.generation)
            .field("candidate_id", &self.candidate_id)
            .field("source", &self.source)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStateStoreError {
    Invalid,
    CaptureProxyRequired,
    CaptureProxyNotReady,
    NotFound,
    Conflict,
    Unavailable,
}

/// Provider 已在真实上游接收边界确认的一次值；ID 与时间在该边界生成。
#[derive(Clone, PartialEq, Eq)]
pub struct TurnStateObservation {
    pub id: String,
    pub account_id: String,
    pub request_id: Option<String>,
    pub attempt_index: Option<u32>,
    pub value: String,
    pub observed_at: DateTime<Utc>,
    pub transport: String,
    pub upstream_response_id: Option<String>,
    pub client_turn_id: Option<String>,
    /// 普通 HTTP Responses 请求没有注入有效模型锁，且账号启用锁定或捕获时设置。
    pub model_scope: Option<ModelTurnStateObservationScope>,
}

/// Provider 在最终上游 transport 成功建立响应边界后确认的实际发送值。
#[derive(Clone, PartialEq, Eq)]
pub struct TurnStateSent {
    pub request_id: String,
    pub attempt_index: u32,
    pub account_id: String,
    pub identity_revision: u64,
    pub effective_model: String,
    pub value: String,
    pub sent_at: DateTime<Utc>,
    pub transport: String,
    pub source: String,
    pub generation: Option<u64>,
    pub candidate_id: Option<String>,
}

impl TurnStateObservation {
    #[must_use]
    pub fn with_upstream_response_id(mut self, upstream_response_id: Option<String>) -> Self {
        self.upstream_response_id = upstream_response_id;
        self
    }
}

/// 覆盖值必须能无损写入 HTTP header；WS 与 HTTP 共用同一配置边界。
#[must_use]
pub fn valid_turn_state_override(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 16 * 1024
        && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
}

#[must_use]
pub fn valid_model_turn_state(value: &str) -> bool {
    value.len() == MODEL_TURN_STATE_BYTES && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelTurnStateTokenMetadata {
    pub encoded_bytes: usize,
    pub raw_bytes: Option<usize>,
    pub ciphertext_bytes: Option<usize>,
    pub envelope_format: Option<&'static str>,
    pub token_version: Option<u8>,
    pub issued_at: Option<DateTime<Utc>>,
    pub timestamp_verified: bool,
}

/// 无密钥读取 Fernet envelope 的公开结构字段；不验证密文或 HMAC。
#[must_use]
pub fn model_turn_state_token_metadata(value: &str) -> ModelTurnStateTokenMetadata {
    let encoded_bytes = value.len();
    let Ok(raw) = URL_SAFE.decode(value) else {
        return ModelTurnStateTokenMetadata {
            encoded_bytes,
            raw_bytes: None,
            ciphertext_bytes: None,
            envelope_format: None,
            token_version: None,
            issued_at: None,
            timestamp_verified: false,
        };
    };
    // Fernet: version(1) + timestamp(8) + IV(16) + ciphertext(16*n) + HMAC(32).
    if raw.len() < 73 || !(raw.len() - 57).is_multiple_of(16) {
        return ModelTurnStateTokenMetadata {
            encoded_bytes,
            raw_bytes: Some(raw.len()),
            ciphertext_bytes: None,
            envelope_format: None,
            token_version: raw.first().copied(),
            issued_at: None,
            timestamp_verified: false,
        };
    }
    let timestamp = u64::from_be_bytes(raw[1..9].try_into().expect("fixed timestamp slice"));
    let issued_at = i64::try_from(timestamp)
        .ok()
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0));
    ModelTurnStateTokenMetadata {
        encoded_bytes,
        raw_bytes: Some(raw.len()),
        ciphertext_bytes: Some(raw.len() - 57),
        envelope_format: (raw.first() == Some(&0x80)).then_some("fernet_v0x80_candidate"),
        token_version: raw.first().copied(),
        issued_at,
        timestamp_verified: false,
    }
}

#[async_trait]
pub trait TurnStateStore: Send + Sync {
    async fn load(&self, account_id: &str) -> Result<TurnStateView, TurnStateStoreError>;

    /// value: None 保持，Some(None) 清空，Some(Some(value)) 更新。
    async fn update(
        &self,
        account_id: &str,
        enabled: bool,
        value: Option<Option<String>>,
        expected_revision: u64,
    ) -> Result<TurnStateView, TurnStateStoreError>;

    async fn use_observed(
        &self,
        account_id: &str,
        observation_id: &str,
        enabled: bool,
        expected_revision: u64,
    ) -> Result<TurnStateView, TurnStateStoreError>;

    /// 数据面非阻塞入队；队列满或关闭时由实现丢弃并记录，不向请求返回错误。
    fn enqueue_observation(&self, observation: TurnStateObservation);

    /// 最终 transport 已实际返回响应边界后写入；捕获探针不经过该端口。
    fn enqueue_sent(&self, _sent: TurnStateSent) {}

    /// 读取启动时 hydrate、管理提交后同步更新的进程内快照。
    fn active_override(&self, account_id: &str) -> Option<String>;

    async fn load_model_state(
        &self,
        _account_id: &str,
        _requested_model: &str,
    ) -> Result<ModelTurnStateView, TurnStateStoreError> {
        Err(TurnStateStoreError::Unavailable)
    }

    async fn load_account_policy(
        &self,
        _account_id: &str,
    ) -> Result<AccountTurnStatePolicyView, TurnStateStoreError> {
        Err(TurnStateStoreError::Unavailable)
    }

    async fn update_account_policy(
        &self,
        _account_id: &str,
        _update: AccountTurnStatePolicyUpdate,
    ) -> Result<AccountTurnStatePolicyView, TurnStateStoreError> {
        Err(TurnStateStoreError::Unavailable)
    }

    async fn update_model_state(
        &self,
        _account_id: &str,
        _requested_model: &str,
        _update: ModelTurnStateUpdate,
    ) -> Result<ModelTurnStateView, TurnStateStoreError> {
        Err(TurnStateStoreError::Unavailable)
    }

    /// 最终账号与 effective model 已确定后读取；只访问启动 hydrate/提交发布的内存快照。
    fn active_model_pin(
        &self,
        _account_id: &str,
        _identity_revision: u64,
        _effective_model: &str,
    ) -> Option<ActiveModelTurnStatePin> {
        None
    }

    /// 普通 HTTP Responses 请求在未注入模型锁时取得的 CAS scope；只读进程内快照。
    fn model_observation_scope(
        &self,
        _account_id: &str,
        _identity_revision: u64,
        _effective_model: &str,
    ) -> Option<ModelTurnStateObservationScope> {
        None
    }

    /// 自动捕获轮询的有界候选；普通请求数据面不调用此方法。
    async fn model_capture_candidates(
        &self,
        _after: Option<&ModelTurnStateCaptureCursor>,
        _limit: u16,
    ) -> Result<Vec<ModelTurnStateCaptureScope>, TurnStateStoreError> {
        Ok(Vec::new())
    }

    /// 后台整理已到切换点或已过期的 candidate，并同步数据面快照。
    async fn maintain_model_turn_state_candidates(
        &self,
        _limit: u16,
    ) -> Result<(), TurnStateStoreError> {
        Ok(())
    }

    /// 以账号身份、模型与配置 revision fence 提交一次捕获结果。
    async fn commit_model_capture(
        &self,
        _scope: &ModelTurnStateCaptureScope,
        _value: &str,
    ) -> Result<ModelTurnStateView, TurnStateStoreError> {
        Err(TurnStateStoreError::Unavailable)
    }

    async fn record_model_capture_failure(
        &self,
        _scope: &ModelTurnStateCaptureScope,
        _reason: &str,
        _finished_at: DateTime<Utc>,
    ) -> Result<(), TurnStateStoreError> {
        Ok(())
    }

    /// 仅由可归因到当前 HTTP 模型锁版本的结构化上游拒绝调用。
    async fn invalidate_active_model_pin(
        &self,
        _account_id: &str,
        _identity_revision: u64,
        _effective_model: &str,
        _expected_generation: u64,
        _expected_candidate_id: Option<&str>,
        _expected_sha256: &str,
    ) -> Result<bool, TurnStateStoreError> {
        Ok(false)
    }
}
