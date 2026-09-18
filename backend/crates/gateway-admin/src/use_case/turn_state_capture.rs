//! 模型 Turn State 的有界诊断队列；不经过普通请求执行、计量与反馈链路。

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, Utc};
use gateway_core::{
    account::ProviderAccountId,
    lifecycle::CancellationToken,
    provider_ports::turn_state::{
        ActiveModelTurnStatePin, ModelTurnStateCaptureActivation,
        ModelTurnStateCaptureCommitOutcome, ModelTurnStateCaptureCoordinator,
        ModelTurnStateCaptureCursor, ModelTurnStateCaptureRequest, ModelTurnStateCaptureScope,
        ModelTurnStateCaptureWaitError, ModelTurnStateView, TurnStateStore, TurnStateStoreError,
        valid_model_turn_state,
    },
    task::{DaemonTask, WorkerTaskError},
};
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc};
use uuid::Uuid;

use crate::{
    model::{
        AdminError,
        accounts::{ModelTurnStateCaptureJob, ModelTurnStateCaptureStatus},
        proxies::ProxyRecord,
    },
    ports::{
        provider::{ProviderAdmin, ProviderTurnStateCaptureRequest},
        proxy::ProxyStore,
    },
};

const QUEUE_CAPACITY: usize = 64;
const GLOBAL_CONCURRENCY: usize = 2;
const AUTO_SCAN_INTERVAL: Duration = Duration::from_secs(30);
const AUTO_SCAN_LIMIT: u16 = 64;
const PROXY_TEST_MAX_AGE: chrono::Duration = chrono::Duration::hours(24);
const FINISHED_JOB_LIMIT: usize = 512;

#[derive(Clone)]
pub(crate) struct ModelTurnStateCaptureManager {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<dyn TurnStateStore>,
    proxies: Arc<dyn ProxyStore>,
    provider: Arc<dyn ProviderAdmin>,
    sender: mpsc::Sender<String>,
    receiver: AsyncMutex<mpsc::Receiver<String>>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    jobs: HashMap<String, JobState>,
    active_accounts: HashMap<String, String>,
    latest_by_scope: HashMap<ScopeKey, String>,
    cooldowns: HashMap<ScopeKey, DateTime<Utc>>,
    scan_cursor: Option<ModelTurnStateCaptureCursor>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ScopeKey {
    account_id: String,
    identity_revision: u64,
    effective_model: String,
}

struct JobState {
    view: ModelTurnStateCaptureJob,
    scope: ModelTurnStateCaptureScope,
    activation: ModelTurnStateCaptureActivation,
    cancellation: CancellationToken,
    commit_guard: Arc<AsyncMutex<()>>,
    completion: Arc<Notify>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CaptureOrigin {
    Manual,
    Automatic,
    Request,
}

impl ModelTurnStateCaptureManager {
    pub(crate) fn new(
        store: Arc<dyn TurnStateStore>,
        proxies: Arc<dyn ProxyStore>,
        provider: Arc<dyn ProviderAdmin>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        Self {
            inner: Arc::new(Inner {
                store,
                proxies,
                provider,
                sender,
                receiver: AsyncMutex::new(receiver),
                state: Mutex::new(State::default()),
            }),
        }
    }

    pub(crate) async fn start(
        &self,
        account_id: &ProviderAccountId,
        requested_model: &str,
        expected_revision: u64,
        expected_policy_revision: u64,
        expected_identity_revision: u64,
        expected_effective_model: &str,
    ) -> Result<ModelTurnStateCaptureJob, AdminError> {
        let view = self
            .inner
            .store
            .load_model_state(account_id.as_str(), requested_model)
            .await
            .map_err(map_store_error)?;
        if view.config_revision != expected_revision
            || view.policy_revision != expected_policy_revision
            || view.identity_revision != expected_identity_revision
            || view.effective_model != expected_effective_model
        {
            return Err(AdminError::conflict("Turn State 已变化，请刷新后重试"));
        }
        self.enqueue(scope_from_view(&view), CaptureOrigin::Manual)
            .await
    }

    pub(crate) fn get(&self, job_id: &str) -> Result<ModelTurnStateCaptureJob, AdminError> {
        let state = lock(&self.inner.state);
        state
            .jobs
            .get(job_id)
            .map(|job| job.view.clone())
            .ok_or_else(|| AdminError::not_found("捕获任务不存在"))
    }

    pub(crate) fn current(&self, view: &ModelTurnStateView) -> Option<ModelTurnStateCaptureJob> {
        let key = ScopeKey::from_view(view);
        let state = lock(&self.inner.state);
        let id = state.latest_by_scope.get(&key)?;
        state.jobs.get(id).map(|job| job.view.clone())
    }

    pub(crate) async fn cancel(
        &self,
        job_id: &str,
    ) -> Result<ModelTurnStateCaptureJob, AdminError> {
        let (commit_guard, cancellation) = {
            let state = lock(&self.inner.state);
            let job = state
                .jobs
                .get(job_id)
                .ok_or_else(|| AdminError::not_found("捕获任务不存在"))?;
            if matches!(
                job.view.status,
                ModelTurnStateCaptureStatus::Succeeded
                    | ModelTurnStateCaptureStatus::Failed
                    | ModelTurnStateCaptureStatus::Cancelled
            ) {
                return Ok(job.view.clone());
            }
            (Arc::clone(&job.commit_guard), job.cancellation.clone())
        };
        cancellation.cancel();
        let _commit = commit_guard.lock().await;
        let completion = {
            let mut state = lock(&self.inner.state);
            let job = state
                .jobs
                .get_mut(job_id)
                .ok_or_else(|| AdminError::not_found("捕获任务不存在"))?;
            if matches!(
                job.view.status,
                ModelTurnStateCaptureStatus::Succeeded
                    | ModelTurnStateCaptureStatus::Failed
                    | ModelTurnStateCaptureStatus::Cancelled
            ) {
                return Ok(job.view.clone());
            }
            job.view.status = ModelTurnStateCaptureStatus::Cancelled;
            job.view.reason = Some("cancelled".to_owned());
            job.view.finished_at = Some(Utc::now());
            Arc::clone(&job.completion)
        };
        completion.notify_waiters();
        self.release_active(job_id);
        self.get(job_id)
    }

    pub(crate) async fn cancel_scope(&self, view: &ModelTurnStateView) {
        let key = ScopeKey::from_view(view);
        let job_id = {
            let state = lock(&self.inner.state);
            state
                .active_accounts
                .get(&view.account_id)
                .and_then(|job_id| {
                    state
                        .jobs
                        .get(job_id)
                        .filter(|job| ScopeKey::from_scope(&job.scope) == key)
                        .map(|_| job_id.clone())
                })
        };
        if let Some(job_id) = job_id {
            let _ = self.cancel(&job_id).await;
        }
    }

    pub(crate) async fn cancel_account(&self, account_id: &str) {
        let job_id = lock(&self.inner.state)
            .active_accounts
            .get(account_id)
            .cloned();
        if let Some(job_id) = job_id {
            let _ = self.cancel(&job_id).await;
        }
    }

    pub(crate) async fn capture_proxy(
        &self,
        view: &ModelTurnStateView,
    ) -> Option<gateway_core::provider_ports::turn_state::ModelTurnStateCaptureProxy> {
        self.capture_proxy_by_id(view.capture_proxy_id.as_deref())
            .await
    }

    pub(crate) async fn capture_proxy_by_id(
        &self,
        id: Option<&str>,
    ) -> Option<gateway_core::provider_ports::turn_state::ModelTurnStateCaptureProxy> {
        let id = id?;
        let record = self.inner.proxies.get(id).await.ok()?;
        let ready = validate_proxy(&record).is_ok();
        Some(
            gateway_core::provider_ports::turn_state::ModelTurnStateCaptureProxy {
                id: record.id,
                name: record.name,
                endpoint: safe_proxy_endpoint(&record.proxy),
                last_test_at: record.last_test_at,
                ready,
            },
        )
    }

    async fn enqueue(
        &self,
        scope: ModelTurnStateCaptureScope,
        origin: CaptureOrigin,
    ) -> Result<ModelTurnStateCaptureJob, AdminError> {
        let proxy_id = scope
            .capture_proxy_id
            .as_deref()
            .ok_or_else(|| AdminError::invalid("请先选择捕获代理"))?;
        let proxy = self
            .inner
            .proxies
            .get(proxy_id)
            .await
            .map_err(|_| AdminError::conflict("捕获代理不存在或已变化"))?;
        validate_proxy(&proxy)?;
        let key = ScopeKey::from_scope(&scope);
        let now = Utc::now();
        let job_id = Uuid::now_v7().to_string();
        let view = ModelTurnStateCaptureJob {
            job_id: job_id.clone(),
            status: ModelTurnStateCaptureStatus::Queued,
            attempts: 0,
            reason: None,
            created_at: now,
            started_at: None,
            finished_at: None,
        };
        {
            let mut state = lock(&self.inner.state);
            if let Some(active) = state.active_accounts.get(&scope.account_id) {
                let active = state
                    .jobs
                    .get(active)
                    .ok_or_else(|| AdminError::conflict("账号已有捕获任务"))?;
                if ScopeKey::from_scope(&active.scope) == key {
                    if origin == CaptureOrigin::Manual
                        && active.activation == ModelTurnStateCaptureActivation::StageIfActiveFresh
                    {
                        return Err(AdminError::conflict("账号已有自动捕获任务，请先取消后重试"));
                    }
                    return Ok(active.view.clone());
                }
                return Err(AdminError::conflict("账号已有其他模型的捕获任务"));
            }
            if origin != CaptureOrigin::Manual
                && state.cooldowns.get(&key).is_some_and(|finished_at| {
                    *finished_at
                        + chrono::Duration::seconds(i64::from(
                            scope.capture_policy.cooldown_seconds,
                        ))
                        > now
                })
            {
                return Err(AdminError::conflict("捕获任务仍在冷却期"));
            }
            state
                .active_accounts
                .insert(scope.account_id.clone(), job_id.clone());
            state.latest_by_scope.insert(key, job_id.clone());
            state.jobs.insert(
                job_id.clone(),
                JobState {
                    view: view.clone(),
                    scope,
                    activation: if matches!(origin, CaptureOrigin::Manual | CaptureOrigin::Request)
                    {
                        ModelTurnStateCaptureActivation::ActivateImmediately
                    } else {
                        ModelTurnStateCaptureActivation::StageIfActiveFresh
                    },
                    cancellation: CancellationToken::new(),
                    commit_guard: Arc::new(AsyncMutex::new(())),
                    completion: Arc::new(Notify::new()),
                },
            );
            trim_finished(&mut state);
        }
        if self.inner.sender.try_send(job_id.clone()).is_err() {
            self.finish(
                &job_id,
                ModelTurnStateCaptureStatus::Failed,
                Some("queue_full"),
            )
            .await;
            return Err(AdminError::unavailable("捕获任务队列已满"));
        }
        Ok(view)
    }

    fn release_active(&self, job_id: &str) {
        let mut state = lock(&self.inner.state);
        let Some(account_id) = state
            .jobs
            .get(job_id)
            .map(|job| job.scope.account_id.clone())
        else {
            return;
        };
        if state.active_accounts.get(&account_id).map(String::as_str) == Some(job_id) {
            state.active_accounts.remove(&account_id);
        }
    }

    async fn finish(
        &self,
        job_id: &str,
        status: ModelTurnStateCaptureStatus,
        reason: Option<&str>,
    ) {
        let now = Utc::now();
        let scope = lock(&self.inner.state)
            .jobs
            .get(job_id)
            .map(|job| job.scope.clone());
        if status == ModelTurnStateCaptureStatus::Failed
            && let (Some(scope), Some(reason)) = (scope.as_ref(), reason)
            && let Err(error) = self
                .inner
                .store
                .record_model_capture_failure(scope, reason, now)
                .await
        {
            tracing::warn!(
                account_id = scope.account_id,
                effective_model = scope.effective_model,
                error_kind = ?error,
                "model turn state capture cooldown could not be persisted"
            );
        }
        let (key, completion) = {
            let mut state = lock(&self.inner.state);
            let Some(job) = state.jobs.get_mut(job_id) else {
                return;
            };
            if job.view.status == ModelTurnStateCaptureStatus::Cancelled {
                return;
            }
            job.view.status = status;
            job.view.reason = reason.map(str::to_owned);
            job.view.finished_at = Some(now);
            (
                ScopeKey::from_scope(&job.scope),
                Arc::clone(&job.completion),
            )
        };
        completion.notify_waiters();
        let mut state = lock(&self.inner.state);
        state.cooldowns.insert(key, now);
        drop(state);
        self.release_active(job_id);
    }

    async fn run_job(&self, job_id: String) {
        let (scope, activation, cancellation, commit_guard, created_at) = {
            let mut state = lock(&self.inner.state);
            let Some(job) = state.jobs.get_mut(&job_id) else {
                return;
            };
            if job.view.status == ModelTurnStateCaptureStatus::Cancelled {
                return;
            }
            job.view.status = ModelTurnStateCaptureStatus::Running;
            job.view.started_at = Some(Utc::now());
            (
                job.scope.clone(),
                job.activation,
                job.cancellation.clone(),
                Arc::clone(&job.commit_guard),
                job.view.created_at,
            )
        };
        let job_timeout = Duration::from_secs(u64::from(scope.capture_policy.job_timeout_seconds));
        let queued_for = (Utc::now() - created_at).to_std().unwrap_or_default();
        let deadline = tokio::time::Instant::now() + job_timeout.saturating_sub(queued_for);
        if tokio::time::Instant::now() >= deadline {
            self.finish(
                &job_id,
                ModelTurnStateCaptureStatus::Failed,
                Some("job_timeout"),
            )
            .await;
            return;
        }
        let Some(proxy_id) = scope.capture_proxy_id.as_deref() else {
            self.finish(
                &job_id,
                ModelTurnStateCaptureStatus::Failed,
                Some("proxy_unavailable"),
            )
            .await;
            return;
        };
        let proxy_record = match tokio::select! {
            () = cancellation.cancelled() => None,
            result = tokio::time::timeout_at(deadline, self.inner.proxies.get(proxy_id)) => Some(result),
        } {
            Some(Ok(Ok(proxy))) if validate_proxy(&proxy).is_ok() => proxy,
            None => {
                let _ = self.cancel(&job_id).await;
                return;
            }
            Some(Err(_)) => {
                self.finish(
                    &job_id,
                    ModelTurnStateCaptureStatus::Failed,
                    Some("job_timeout"),
                )
                .await;
                return;
            }
            _ => {
                self.finish(
                    &job_id,
                    ModelTurnStateCaptureStatus::Failed,
                    Some("proxy_unavailable"),
                )
                .await;
                return;
            }
        };
        let reservation = match tokio::select! {
            () = cancellation.cancelled() => None,
            result = tokio::time::timeout_at(deadline, self.inner.proxies.reserve_import(proxy_id)) => Some(result),
        } {
            Some(Ok(Ok(reservation)))
                if reservation.binding.id == proxy_record.id
                    && reservation.binding.proxy == proxy_record.proxy =>
            {
                reservation
            }
            None => {
                let _ = self.cancel(&job_id).await;
                return;
            }
            Some(Err(_)) => {
                self.finish(
                    &job_id,
                    ModelTurnStateCaptureStatus::Failed,
                    Some("job_timeout"),
                )
                .await;
                return;
            }
            _ => {
                self.finish(
                    &job_id,
                    ModelTurnStateCaptureStatus::Failed,
                    Some("proxy_changed"),
                )
                .await;
                return;
            }
        };
        let proxy = reservation.binding.proxy.clone();
        let _proxy_guard = reservation.guard;
        let mut reason = "no_turn_state";
        for attempt in 1..=scope.capture_policy.max_attempts {
            if cancellation.is_cancelled() || tokio::time::Instant::now() >= deadline {
                break;
            }
            if let Some(job) = lock(&self.inner.state).jobs.get_mut(&job_id) {
                job.view.attempts = attempt;
            }
            let request = ProviderTurnStateCaptureRequest {
                account_id: match ProviderAccountId::new(scope.account_id.clone()) {
                    Ok(account_id) => account_id,
                    Err(_) => {
                        reason = "invalid_scope";
                        break;
                    }
                },
                identity_revision: scope.identity_revision,
                effective_model: scope.effective_model.clone(),
                proxy: proxy.clone(),
            };
            let attempt_timeout =
                Duration::from_secs(u64::from(scope.capture_policy.attempt_timeout_seconds));
            let attempt_result = tokio::select! {
                () = cancellation.cancelled() => break,
                result = tokio::time::timeout_at(
                    deadline.min(tokio::time::Instant::now() + attempt_timeout),
                    self.inner.provider.capture_turn_state(request),
                ) => result,
            };
            match attempt_result {
                Ok(Ok(candidate)) => {
                    let candidate = candidate.into_value();
                    if !valid_model_turn_state(&candidate) {
                        reason = "invalid_length";
                    } else {
                        let commit_lock = tokio::select! {
                            () = cancellation.cancelled() => break,
                            result = tokio::time::timeout_at(deadline, commit_guard.lock()) => result,
                        };
                        let Ok(_commit) = commit_lock else {
                            reason = "job_timeout";
                            break;
                        };
                        if cancellation.is_cancelled() {
                            break;
                        }
                        if tokio::time::Instant::now() >= deadline {
                            reason = "job_timeout";
                            break;
                        }
                        // 取得 guard 并通过最后一次 cancel/deadline 检查后，Store commit
                        // 就是不可取消的线性化区。必须等待数据库结果与缓存发布完成，避免
                        // PostgreSQL 已提交但 future 被丢弃，留下 job/cached pin 分裂。
                        let commit_result = self
                            .inner
                            .store
                            .commit_model_capture(&scope, &candidate, activation)
                            .await;
                        match commit_result {
                            Ok(ModelTurnStateCaptureCommitOutcome::Committed(_)) => {
                                self.finish(&job_id, ModelTurnStateCaptureStatus::Succeeded, None)
                                    .await;
                                return;
                            }
                            Ok(ModelTurnStateCaptureCommitOutcome::Unchanged(_)) => {
                                self.finish(
                                    &job_id,
                                    ModelTurnStateCaptureStatus::Succeeded,
                                    Some("unchanged_value"),
                                )
                                .await;
                                return;
                            }
                            Ok(ModelTurnStateCaptureCommitOutcome::RejectedValue) => {
                                reason = "rejected_value";
                            }
                            Err(TurnStateStoreError::Conflict | TurnStateStoreError::NotFound) => {
                                self.finish(
                                    &job_id,
                                    ModelTurnStateCaptureStatus::Failed,
                                    Some("scope_changed"),
                                )
                                .await;
                                return;
                            }
                            Err(_)
                                if cancellation.is_cancelled()
                                    || tokio::time::Instant::now() >= deadline =>
                            {
                                self.finish(
                                    &job_id,
                                    ModelTurnStateCaptureStatus::Failed,
                                    Some("store_unavailable"),
                                )
                                .await;
                                return;
                            }
                            Err(_) => reason = "store_unavailable",
                        }
                    }
                }
                Ok(Err(_)) => reason = "capture_failed",
                Err(_) => reason = "attempt_timeout",
            }
            if attempt < scope.capture_policy.max_attempts {
                let shift = u32::from(attempt.saturating_sub(1)).min(31);
                let multiplier = 1_u64.checked_shl(shift).unwrap_or(u64::MAX);
                let delay = u64::from(scope.capture_policy.backoff_seconds)
                    .saturating_mul(multiplier)
                    .min(u64::from(scope.capture_policy.max_backoff_seconds));
                tokio::select! {
                    () = cancellation.cancelled() => break,
                    () = tokio::time::sleep_until(
                        deadline.min(tokio::time::Instant::now() + Duration::from_secs(delay)),
                    ) => {}
                }
            }
        }
        if cancellation.is_cancelled() {
            let _ = self.cancel(&job_id).await;
        } else {
            if tokio::time::Instant::now() >= deadline {
                reason = "job_timeout";
            }
            self.finish(&job_id, ModelTurnStateCaptureStatus::Failed, Some(reason))
                .await;
        }
    }

    async fn enqueue_automatic(&self) {
        if let Err(error) = self
            .inner
            .store
            .maintain_model_turn_state_candidates(AUTO_SCAN_LIMIT)
            .await
        {
            tracing::warn!(error_kind = ?error, "model turn state candidate maintenance failed");
        }
        let after = lock(&self.inner.state).scan_cursor.clone();
        let Ok(scopes) = self
            .inner
            .store
            .model_capture_candidates(after.as_ref(), AUTO_SCAN_LIMIT)
            .await
        else {
            tracing::warn!("automatic model turn state capture scan failed");
            return;
        };
        let next_cursor = (scopes.len() == usize::from(AUTO_SCAN_LIMIT))
            .then(|| scopes.last().map(ModelTurnStateCaptureScope::cursor))
            .flatten();
        lock(&self.inner.state).scan_cursor = next_cursor;
        for scope in scopes {
            let account_id = scope.account_id.clone();
            let effective_model = scope.effective_model.clone();
            if let Err(error) = self.enqueue(scope, CaptureOrigin::Automatic).await {
                tracing::debug!(
                    account_id,
                    effective_model,
                    reason = error.message(),
                    "automatic model turn state capture is waiting for prerequisites"
                );
            }
        }
    }
}

#[async_trait::async_trait]
impl ModelTurnStateCaptureCoordinator for ModelTurnStateCaptureManager {
    async fn capture_for_request(
        &self,
        request: ModelTurnStateCaptureRequest,
    ) -> Result<ActiveModelTurnStatePin, ModelTurnStateCaptureWaitError> {
        let scope = self
            .inner
            .store
            .load_model_capture_scope(
                &request.account_id,
                request.identity_revision,
                &request.effective_model,
            )
            .await
            .map_err(|_| ModelTurnStateCaptureWaitError::Unavailable)?;
        if scope.identity_revision != request.identity_revision
            || scope.effective_model != request.effective_model
        {
            return Err(ModelTurnStateCaptureWaitError::ScopeChanged);
        }
        if !scope.capture_enabled {
            return Err(ModelTurnStateCaptureWaitError::Disabled);
        }
        if let Some(pin) = self.inner.store.active_model_pin(
            &request.account_id,
            request.identity_revision,
            &request.effective_model,
        ) {
            return Ok(pin);
        }
        let job = self
            .enqueue(scope, CaptureOrigin::Request)
            .await
            .map_err(|_| ModelTurnStateCaptureWaitError::Unavailable)?;
        let completion = {
            let state = lock(&self.inner.state);
            Arc::clone(
                &state
                    .jobs
                    .get(&job.job_id)
                    .ok_or(ModelTurnStateCaptureWaitError::Unavailable)?
                    .completion,
            )
        };
        loop {
            let notified = completion.notified();
            let status = self
                .get(&job.job_id)
                .map_err(|_| ModelTurnStateCaptureWaitError::Unavailable)?
                .status;
            match status {
                ModelTurnStateCaptureStatus::Succeeded => {
                    return self
                        .inner
                        .store
                        .active_model_pin(
                            &request.account_id,
                            request.identity_revision,
                            &request.effective_model,
                        )
                        .ok_or(ModelTurnStateCaptureWaitError::Unavailable);
                }
                ModelTurnStateCaptureStatus::Failed | ModelTurnStateCaptureStatus::Cancelled => {
                    return Err(ModelTurnStateCaptureWaitError::Unavailable);
                }
                ModelTurnStateCaptureStatus::Queued | ModelTurnStateCaptureStatus::Running => {
                    notified.await;
                }
            }
        }
    }
}

impl DaemonTask for ModelTurnStateCaptureManager {
    fn run(
        &self,
        shutdown: CancellationToken,
    ) -> futures::future::BoxFuture<'_, Result<(), WorkerTaskError>> {
        Box::pin(async move {
            let mut receiver = self.inner.receiver.lock().await;
            let mut active = tokio::task::JoinSet::new();
            let mut scan = tokio::time::interval(AUTO_SCAN_INTERVAL);
            scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    _ = scan.tick() => self.enqueue_automatic().await,
                    Some(_) = active.join_next(), if !active.is_empty() => {}
                    job_id = receiver.recv(), if active.len() < GLOBAL_CONCURRENCY => {
                        let Some(job_id) = job_id else {
                            return Err(WorkerTaskError::safe("model turn state capture queue closed"));
                        };
                        let manager = self.clone();
                        active.spawn(async move { manager.run_job(job_id).await });
                    }
                }
            }
            let cancellations = {
                let state = lock(&self.inner.state);
                state
                    .jobs
                    .values()
                    .filter(|job| {
                        matches!(
                            job.view.status,
                            ModelTurnStateCaptureStatus::Queued
                                | ModelTurnStateCaptureStatus::Running
                        )
                    })
                    .map(|job| job.cancellation.clone())
                    .collect::<Vec<_>>()
            };
            for cancellation in cancellations {
                cancellation.cancel();
            }
            while active.join_next().await.is_some() {}
            Ok(())
        })
    }
}

fn scope_from_view(view: &ModelTurnStateView) -> ModelTurnStateCaptureScope {
    ModelTurnStateCaptureScope {
        account_id: view.account_id.clone(),
        requested_model: view.requested_model.clone(),
        effective_model: view.effective_model.clone(),
        identity_revision: view.identity_revision,
        config_revision: view.config_revision,
        policy_revision: view.policy_revision,
        capture_enabled: view.capture_enabled,
        capture_proxy_id: view.capture_proxy_id.clone(),
        capture_policy: view.capture_policy.clone(),
    }
}

impl ScopeKey {
    fn from_scope(scope: &ModelTurnStateCaptureScope) -> Self {
        Self {
            account_id: scope.account_id.clone(),
            identity_revision: scope.identity_revision,
            effective_model: scope.effective_model.clone(),
        }
    }

    fn from_view(view: &ModelTurnStateView) -> Self {
        Self {
            account_id: view.account_id.clone(),
            identity_revision: view.identity_revision,
            effective_model: view.effective_model.clone(),
        }
    }
}

fn validate_proxy(proxy: &ProxyRecord) -> Result<(), AdminError> {
    if !proxy.last_test.as_ref().is_some_and(|test| test.success)
        || proxy
            .last_test_at
            .is_none_or(|tested_at| tested_at < Utc::now() - PROXY_TEST_MAX_AGE)
    {
        return Err(AdminError::conflict("捕获代理需要在 24 小时内测试成功"));
    }
    Ok(())
}

fn safe_proxy_endpoint(proxy: &gateway_core::account::OutboundProxy) -> String {
    let Ok(url) = url::Url::parse(proxy.expose_url()) else {
        return "managed proxy".to_owned();
    };
    let host = url.host_str().unwrap_or("managed proxy");
    match url.port_or_known_default() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    }
}

fn trim_finished(state: &mut State) {
    if state.jobs.len() <= FINISHED_JOB_LIMIT {
        return;
    }
    let remove = state
        .jobs
        .iter()
        .filter(|(_, job)| job.view.finished_at.is_some())
        .min_by_key(|(_, job)| job.view.finished_at)
        .map(|(id, _)| id.clone());
    if let Some(id) = remove {
        state.jobs.remove(&id);
    }
}

fn map_store_error(error: TurnStateStoreError) -> AdminError {
    match error {
        TurnStateStoreError::Invalid => AdminError::invalid("Turn State 配置无效"),
        TurnStateStoreError::CaptureProxyRequired => AdminError::invalid("请先选择捕获代理"),
        TurnStateStoreError::CaptureProxyNotReady => {
            AdminError::conflict("捕获代理需要在 24 小时内测试成功")
        }
        TurnStateStoreError::NotFound => AdminError::not_found("OpenAI 账号不存在"),
        TurnStateStoreError::Conflict => AdminError::conflict("Turn State 已变化，请刷新后重试"),
        TurnStateStoreError::Unavailable => AdminError::unavailable("Turn State 服务暂不可用"),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
