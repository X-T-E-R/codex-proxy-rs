//! Codex WebSocket 连接池。

mod lease;
mod state;
mod supervisor;

use std::{
    future::Future,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{
            AtomicBool, AtomicUsize,
            Ordering::{AcqRel, Acquire},
        },
    },
    time::Duration,
};

use gateway_core::provider_ports::ProviderWebSocketPoolPolicy;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use uuid::Uuid;

use self::state::{
    WebSocketPoolConnecting, WebSocketPoolSlot, WebSocketPoolState, close_pooled_connection,
    close_pooled_connections,
};
use super::pump::PumpKeepalive;
use super::pump::WebSocketConnectionObservation;

pub use self::state::CodexWebSocketPoolKey;
pub(crate) use self::{
    lease::{
        WebSocketPoolAcquire, WebSocketPoolConnectLease, WebSocketPoolConnectOutcome,
        WebSocketPoolConnectWaiter, WebSocketPoolLease,
    },
    state::{
        CodexWebSocketConnectionMetadata, PooledWebSocketConnection, WebSocketContinuationState,
    },
};

const DEFAULT_MAX_CONNECTING: usize = 16;
const DEFAULT_MAX_AGE: Duration = Duration::from_mins(55);
const DEFAULT_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(25);
const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(25);
// 心跳也覆盖正在生成的连接，给短时链路停顿留出恢复余量。
const DEFAULT_PING_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// 快路径预算的代码默认值；启动配置与 DB 运行参数的缺省都锚定它。
pub(crate) const DEFAULT_FAST_PATH_BUDGET_MS: u64 = 800;

/// WebSocket 连接池。
#[derive(Clone)]
pub struct CodexWebSocketPool {
    inner: Arc<Mutex<WebSocketPoolState>>,
    config: Arc<Mutex<CodexWebSocketPoolConfig>>,
    tasks: TaskTracker,
    shutdown: CancellationToken,
    connect_permits: Arc<WebSocketConnectPermitLimiter>,
    maintenance_started: Arc<AtomicBool>,
}

impl Default for CodexWebSocketPool {
    fn default() -> Self {
        Self::with_config(CodexWebSocketPoolConfig::default())
    }
}

/// WebSocket 连接池配置。
#[derive(Debug, Clone, Copy)]
pub struct CodexWebSocketPoolConfig {
    /// 是否启用连接池。
    pub enabled: bool,
    /// 单个 socket 的最大生命周期。
    pub max_age: Duration,
    /// 所有账号合计允许并发执行的 opening 数。
    pub max_connecting: usize,
    /// 后台维护间隔；`None` 表示不启动后台任务。
    pub maintenance_interval: Option<Duration>,
    /// 池化连接的探活 ping 间隔（包括正在生成的连接）；`None` 表示不主动 ping。
    pub ping_interval: Option<Duration>,
    /// 发送 ping 后等待任意入站帧的超时时间；零值表示不校验 Pong deadline。
    pub ping_timeout: Duration,
    /// idle socket 无活动多久后视为失活。
    pub liveness_timeout: Option<Duration>,
    /// 等待下一条上游消息的空闲超时；`None` 或零值使用默认 300 秒。
    pub stream_idle_timeout: Option<Duration>,
    /// 前台等待池化 WebSocket 就绪的预算，超时回退 HTTP/2 SSE。
    pub fast_path_budget: Duration,
}

impl Default for CodexWebSocketPoolConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_age: DEFAULT_MAX_AGE,
            max_connecting: DEFAULT_MAX_CONNECTING,
            maintenance_interval: Some(DEFAULT_MAINTENANCE_INTERVAL),
            ping_interval: Some(DEFAULT_PING_INTERVAL),
            ping_timeout: DEFAULT_PING_TIMEOUT,
            // idle 连接不设失活截断：靠 ping/pong 保活，只在 max_age（55 分钟）
            // 或 ping 失败时关闭，维持跨轮可复用连接。
            liveness_timeout: None,
            stream_idle_timeout: Some(DEFAULT_STREAM_IDLE_TIMEOUT),
            fast_path_budget: Duration::from_millis(DEFAULT_FAST_PATH_BUDGET_MS),
        }
    }
}

impl CodexWebSocketPoolConfig {
    /// pump 后台任务的保活策略：从连接池配置派生出 ping/pong 与 liveness 策略。
    pub(crate) fn keepalive(&self) -> PumpKeepalive {
        PumpKeepalive {
            ping_interval: self.ping_interval,
            ping_timeout: (!self.ping_timeout.is_zero()).then_some(self.ping_timeout),
            liveness_timeout: self.liveness_timeout,
        }
    }
}

/// 只统计仍持有的建连名额；每次申请使用当前配置上限，缩容不撤销在途建连。
#[derive(Debug)]
struct WebSocketConnectPermitLimiter {
    in_flight: AtomicUsize,
}

impl WebSocketConnectPermitLimiter {
    fn new() -> Self {
        Self {
            in_flight: AtomicUsize::new(0),
        }
    }

    fn try_acquire(self: &Arc<Self>, limit: usize) -> Option<WebSocketConnectPermit> {
        let mut observed = self.in_flight.load(Acquire);
        loop {
            if observed >= limit {
                return None;
            }
            match self
                .in_flight
                .compare_exchange_weak(observed, observed + 1, AcqRel, Acquire)
            {
                Ok(_) => {
                    return Some(WebSocketConnectPermit {
                        limiter: Arc::clone(self),
                    });
                }
                Err(actual) => observed = actual,
            }
        }
    }
}

/// 语义等价于 `OwnedSemaphorePermit`：drop 时归还一个 opening 名额。
#[derive(Debug)]
pub(crate) struct WebSocketConnectPermit {
    limiter: Arc<WebSocketConnectPermitLimiter>,
}

impl Drop for WebSocketConnectPermit {
    fn drop(&mut self) {
        self.limiter.in_flight.fetch_sub(1, AcqRel);
    }
}

impl CodexWebSocketPool {
    /// 构造不限制累计 slot 数的连接池策略和状态。
    pub fn new(max_age: Duration) -> Self {
        Self::with_config(CodexWebSocketPoolConfig {
            max_age,
            maintenance_interval: None,
            ping_interval: None,
            liveness_timeout: None,
            ..CodexWebSocketPoolConfig::default()
        })
    }

    /// 使用完整配置构造连接池。
    pub fn with_config(config: CodexWebSocketPoolConfig) -> Self {
        let pool = Self {
            inner: Arc::new(Mutex::new(WebSocketPoolState::default())),
            config: Arc::new(Mutex::new(config)),
            tasks: TaskTracker::new(),
            shutdown: CancellationToken::new(),
            connect_permits: Arc::new(WebSocketConnectPermitLimiter::new()),
            maintenance_started: Arc::new(AtomicBool::new(false)),
        };
        pool.spawn_maintenance_task();
        pool
    }

    /// pump 后台任务的保活策略（供建连时传入）。
    pub(crate) fn keepalive(&self) -> PumpKeepalive {
        self.config_snapshot().keepalive()
    }

    /// 一次读取完整配置，避免请求准备时混用两次设置更新的超时参数。
    pub(crate) fn config_snapshot(&self) -> CodexWebSocketPoolConfig {
        *self
            .config
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// 把 DB 下发的池策略换入为当前值；无共享状态重建，进行中的连接不受影响。
    pub fn apply_runtime_policy(&self, policy: ProviderWebSocketPoolPolicy) {
        let mut config = self
            .config
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        config.enabled = policy.enabled();
        config.max_age = policy.max_age();
        config.stream_idle_timeout = Some(policy.stream_idle_timeout());
        config.fast_path_budget = policy.fast_path_budget();
        config.max_connecting =
            usize::try_from(policy.max_connecting().get()).unwrap_or(usize::MAX);
    }

    /// 注册由连接池生命周期托管的 opening 任务。
    pub(crate) fn spawn_connect_task(&self, future: impl Future<Output = ()> + Send + 'static) {
        drop(self.tasks.spawn(future));
    }

    pub(crate) async fn acquire(
        &self,
        key: &CodexWebSocketPoolKey,
        required_response_id: Option<&str>,
    ) -> WebSocketPoolAcquire {
        self.spawn_maintenance_task();
        let mut connections_to_close = Vec::new();
        let acquire = {
            let mut state = self.lock_state();
            let config = self.config_snapshot();
            let mut continuation_loss = None;
            if !config.enabled || state.shutting_down {
                return WebSocketPoolAcquire::Bypass(WebSocketPoolBypassReason::Disabled);
            }
            let key = if let Some(response_id) = required_response_id {
                let Some(key) = state.slots.iter().find_map(|(candidate, slot)| {
                    (candidate.same_logical_connection(key)
                        && slot.latest_response_id() == Some(response_id))
                    .then(|| candidate.clone())
                }) else {
                    return state
                        .continuation_loss(key, response_id, tokio::time::Instant::now())
                        .map_or(
                            WebSocketPoolAcquire::Bypass(
                                WebSocketPoolBypassReason::ContinuationNotFound,
                            ),
                            WebSocketPoolAcquire::ContinuationLost,
                        );
                };
                key
            } else {
                key.clone()
            };
            match state.slots.get(&key) {
                Some(WebSocketPoolSlot::Busy(_)) => {
                    return WebSocketPoolAcquire::Bypass(WebSocketPoolBypassReason::Busy);
                }
                Some(WebSocketPoolSlot::Connecting(connecting)) => {
                    return WebSocketPoolAcquire::Wait(WebSocketPoolConnectWaiter {
                        receiver: connecting.outcome.subscribe(),
                        started_at: connecting.started_at,
                    });
                }
                Some(WebSocketPoolSlot::Idle { .. }) => {
                    let Some(WebSocketPoolSlot::Idle { connection, .. }) = state.slots.remove(&key)
                    else {
                        return WebSocketPoolAcquire::Bypass(WebSocketPoolBypassReason::Busy);
                    };
                    // 零成本探活：后台 pump 已实时感知连接死亡（RST/Close/EOF/失活），
                    // 复用前只需读取 is_closed 标志，避免复用到静默死连接后卡到超时。
                    let expired = connection.created_at.elapsed() >= config.max_age;
                    let closed = connection.websocket.is_closed();
                    if !expired && !closed {
                        let lease = WebSocketPoolLease::reserve(
                            self.clone(),
                            key.clone(),
                            connection.continuation.latest_response_id(),
                        );
                        state.slots.insert(
                            key.clone(),
                            WebSocketPoolSlot::Busy(lease.reservation.clone()),
                        );
                        return WebSocketPoolAcquire::Reused { connection, lease };
                    }
                    let observation = if expired && !closed {
                        connection
                            .websocket
                            .observation()
                            .with_exit_reason("max_age_expired")
                    } else {
                        connection.websocket.observation()
                    };
                    state.remember_continuation_loss(
                        &key,
                        connection.continuation.latest_response_id(),
                        observation.clone(),
                        tokio::time::Instant::now(),
                    );
                    connections_to_close.push(*connection);
                    if required_response_id.is_some() {
                        continuation_loss = Some(observation);
                    }
                }
                None => {}
            }

            let acquire = if let Some(observation) = continuation_loss {
                WebSocketPoolAcquire::ContinuationLost(observation)
            } else if required_response_id.is_some() {
                WebSocketPoolAcquire::Bypass(WebSocketPoolBypassReason::ContinuationNotFound)
            } else {
                let connect_permit = self.connect_permits.try_acquire(config.max_connecting);
                match connect_permit {
                    Some(connect_permit) => {
                        let lease = WebSocketPoolConnectLease::reserve(
                            self.clone(),
                            key.clone(),
                            connect_permit,
                        );
                        state.slots.insert(
                            key,
                            WebSocketPoolSlot::Connecting(WebSocketPoolConnecting {
                                id: lease.id,
                                started_at: lease.started_at,
                                outcome: lease.outcome.clone(),
                                cancellation: lease.cancellation.clone(),
                            }),
                        );
                        WebSocketPoolAcquire::Connect(lease)
                    }
                    None => WebSocketPoolAcquire::Bypass(WebSocketPoolBypassReason::Cap),
                }
            };
            drop(state);
            acquire
        };

        close_pooled_connections(connections_to_close).await;

        acquire
    }

    async fn put_reserved(
        &self,
        key: &CodexWebSocketPoolKey,
        reservation_id: Uuid,
        connection: PooledWebSocketConnection,
    ) {
        let mut connection = Some(connection);
        {
            let mut state = self.lock_state();
            let config = self.config_snapshot();
            let expired = connection
                .as_ref()
                .is_some_and(|connection| connection.created_at.elapsed() >= config.max_age);
            let owns_reservation = matches!(
                state.slots.get(key),
                Some(WebSocketPoolSlot::Busy(reservation))
                    if reservation.id == reservation_id
            );
            if owns_reservation && (expired || state.shutting_down || !config.enabled) {
                if expired && let Some(connection) = connection.as_ref() {
                    state.remember_continuation_loss(
                        key,
                        connection.continuation.latest_response_id(),
                        connection
                            .websocket
                            .observation()
                            .with_exit_reason("max_age_expired"),
                        tokio::time::Instant::now(),
                    );
                }
                state.slots.remove(key);
            } else if owns_reservation && let Some(connection) = connection.take() {
                state.slots.insert(
                    key.clone(),
                    WebSocketPoolSlot::Idle {
                        connection: Box::new(connection),
                    },
                );
            }
        }
        if let Some(connection) = connection {
            close_pooled_connection(connection).await;
        }
    }

    fn discard_reserved_now(&self, key: &CodexWebSocketPoolKey, reservation_id: Uuid) {
        let mut state = self.lock_state();
        if matches!(
            state.slots.get(key),
            Some(WebSocketPoolSlot::Busy(reservation)) if reservation.id == reservation_id
        ) {
            state.slots.remove(key);
        }
    }

    fn discard_reserved_with_observation(
        &self,
        key: &CodexWebSocketPoolKey,
        reservation_id: Uuid,
        observation: WebSocketConnectionObservation,
    ) {
        let mut state = self.lock_state();
        let latest_response_id = match state.slots.get(key) {
            Some(WebSocketPoolSlot::Busy(reservation)) if reservation.id == reservation_id => {
                reservation.latest_response_id.clone()
            }
            _ => return,
        };
        state.remember_continuation_loss(
            key,
            latest_response_id.as_deref(),
            observation,
            tokio::time::Instant::now(),
        );
        state.slots.remove(key);
    }

    async fn finish_connect_reserved(
        &self,
        key: &CodexWebSocketPoolKey,
        connect_id: Uuid,
        connection: PooledWebSocketConnection,
    ) -> Result<(Box<PooledWebSocketConnection>, WebSocketPoolLease), Box<PooledWebSocketConnection>>
    {
        let mut state = self.lock_state();
        let owns_connect = matches!(
            state.slots.get(key),
            Some(WebSocketPoolSlot::Connecting(connecting)) if connecting.id == connect_id
        );
        if owns_connect && !state.shutting_down && self.config_snapshot().enabled {
            let lease = WebSocketPoolLease::reserve(self.clone(), key.clone(), None);
            state.slots.insert(
                key.clone(),
                WebSocketPoolSlot::Busy(lease.reservation.clone()),
            );
            Ok((Box::new(connection), lease))
        } else {
            if owns_connect {
                state.slots.remove(key);
            }
            Err(Box::new(connection))
        }
    }

    async fn fail_connect(&self, key: &CodexWebSocketPoolKey, connect_id: Uuid) {
        self.fail_connect_now(key, connect_id);
    }

    fn fail_connect_now(&self, key: &CodexWebSocketPoolKey, connect_id: Uuid) {
        let mut state = self.lock_state();
        if matches!(
            state.slots.get(key),
            Some(WebSocketPoolSlot::Connecting(connecting)) if connecting.id == connect_id
        ) {
            state.slots.remove(key);
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, WebSocketPoolState> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSocketPoolBypassReason {
    Disabled,
    Busy,
    Cap,
    ContinuationNotFound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebSocketPoolDecision {
    kind: WebSocketPoolDecisionKind,
}

impl WebSocketPoolDecision {
    pub fn new() -> Self {
        Self {
            kind: WebSocketPoolDecisionKind::New,
        }
    }

    pub fn reuse() -> Self {
        Self {
            kind: WebSocketPoolDecisionKind::Reuse,
        }
    }

    pub fn kind(self) -> &'static str {
        self.kind.as_str()
    }

    pub const fn is_reuse(self) -> bool {
        matches!(self.kind, WebSocketPoolDecisionKind::Reuse)
    }
}

impl Default for WebSocketPoolDecision {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebSocketPoolDecisionKind {
    New,
    Reuse,
}

impl WebSocketPoolDecisionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Reuse => "reuse",
        }
    }
}
