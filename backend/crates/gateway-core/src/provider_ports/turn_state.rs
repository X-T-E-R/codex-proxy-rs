//! OpenAI 账号的实验性出站 turn state 与上游原值观测端口。

use async_trait::async_trait;
use chrono::{DateTime, Utc};

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
pub enum TurnStateStoreError {
    Invalid,
    NotFound,
    Conflict,
    Unavailable,
}

/// Provider 已在真实上游接收边界确认的一次值；ID 与时间在该边界生成。
#[derive(Clone, PartialEq, Eq)]
pub struct TurnStateObservation {
    pub id: String,
    pub account_id: String,
    pub value: String,
    pub observed_at: DateTime<Utc>,
    pub transport: String,
    pub upstream_response_id: Option<String>,
    pub client_turn_id: Option<String>,
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

    /// 读取启动时 hydrate、管理提交后同步更新的进程内快照。
    fn active_override(&self, account_id: &str) -> Option<String>;
}
