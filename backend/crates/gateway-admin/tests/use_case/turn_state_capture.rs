use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use gateway_admin::{
    AdminBundle, AdminServices,
    model::{
        accounts::ModelTurnStateCaptureStatus,
        proxies::{ProxyRecord, ProxyTestResult},
    },
};
use gateway_core::{
    account::{OutboundProxy, ProviderAccountId},
    lifecycle::CancellationToken,
    provider_ports::turn_state::{
        ActiveModelTurnStatePin, ModelTurnStateCaptureActivation,
        ModelTurnStateCaptureCommitOutcome, ModelTurnStateCaptureCursor,
        ModelTurnStateCapturePolicy, ModelTurnStateCaptureScope, ModelTurnStateCaptureTriggerMode,
        ModelTurnStatePin, ModelTurnStatePinAction, ModelTurnStateUpdate, ModelTurnStateView,
        TurnStateObservation, TurnStateStore, TurnStateStoreError, TurnStateView,
    },
    task::{WorkerContribution, WorkerKind, WorkerRunnable, WorkerTaskError},
};

use super::{
    AdminHarness,
    accounts::{FakeProviderAdmin, events, revision},
    proxies::TestProxies,
};

struct CaptureStore {
    views: Mutex<HashMap<String, ModelTurnStateView>>,
    candidates: Vec<ModelTurnStateCaptureScope>,
    candidate_queries: Mutex<Vec<Option<ModelTurnStateCaptureCursor>>>,
    commit_delay: Duration,
    write_before_commit_delay: bool,
    commit_calls: AtomicUsize,
    commits: Mutex<Vec<String>>,
    activation_intents: Mutex<Vec<ModelTurnStateCaptureActivation>>,
    rejected_values: Mutex<Vec<String>>,
    conflict_values: Mutex<Vec<String>>,
    published: AtomicUsize,
    maintenance_calls: AtomicUsize,
}

impl CaptureStore {
    fn new(views: impl IntoIterator<Item = ModelTurnStateView>) -> Arc<Self> {
        Arc::new(Self {
            views: Mutex::new(
                views
                    .into_iter()
                    .map(|view| (view.requested_model.clone(), view))
                    .collect(),
            ),
            candidates: Vec::new(),
            candidate_queries: Mutex::new(Vec::new()),
            commit_delay: Duration::ZERO,
            write_before_commit_delay: false,
            commit_calls: AtomicUsize::new(0),
            commits: Mutex::new(Vec::new()),
            activation_intents: Mutex::new(Vec::new()),
            rejected_values: Mutex::new(Vec::new()),
            conflict_values: Mutex::new(Vec::new()),
            published: AtomicUsize::new(0),
            maintenance_calls: AtomicUsize::new(0),
        })
    }

    fn with_candidates(candidates: Vec<ModelTurnStateCaptureScope>) -> Arc<Self> {
        Arc::new(Self {
            views: Mutex::new(HashMap::new()),
            candidates,
            candidate_queries: Mutex::new(Vec::new()),
            commit_delay: Duration::ZERO,
            write_before_commit_delay: false,
            commit_calls: AtomicUsize::new(0),
            commits: Mutex::new(Vec::new()),
            activation_intents: Mutex::new(Vec::new()),
            rejected_values: Mutex::new(Vec::new()),
            conflict_values: Mutex::new(Vec::new()),
            published: AtomicUsize::new(0),
            maintenance_calls: AtomicUsize::new(0),
        })
    }

    fn with_view_and_candidates(
        view: ModelTurnStateView,
        candidates: Vec<ModelTurnStateCaptureScope>,
    ) -> Arc<Self> {
        Arc::new(Self {
            views: Mutex::new(HashMap::from([(view.requested_model.clone(), view)])),
            candidates,
            candidate_queries: Mutex::new(Vec::new()),
            commit_delay: Duration::ZERO,
            write_before_commit_delay: false,
            commit_calls: AtomicUsize::new(0),
            commits: Mutex::new(Vec::new()),
            activation_intents: Mutex::new(Vec::new()),
            rejected_values: Mutex::new(Vec::new()),
            conflict_values: Mutex::new(Vec::new()),
            published: AtomicUsize::new(0),
            maintenance_calls: AtomicUsize::new(0),
        })
    }

    fn with_commit_ack_delay(view: ModelTurnStateView, commit_delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            views: Mutex::new(HashMap::from([(view.requested_model.clone(), view)])),
            candidates: Vec::new(),
            candidate_queries: Mutex::new(Vec::new()),
            commit_delay,
            write_before_commit_delay: true,
            commit_calls: AtomicUsize::new(0),
            commits: Mutex::new(Vec::new()),
            activation_intents: Mutex::new(Vec::new()),
            rejected_values: Mutex::new(Vec::new()),
            conflict_values: Mutex::new(Vec::new()),
            published: AtomicUsize::new(0),
            maintenance_calls: AtomicUsize::new(0),
        })
    }

    fn commits(&self) -> Vec<String> {
        self.commits.lock().expect("capture commits").clone()
    }

    fn candidate_queries(&self) -> Vec<Option<ModelTurnStateCaptureCursor>> {
        self.candidate_queries
            .lock()
            .expect("candidate queries")
            .clone()
    }

    fn reject_value(&self, value: String) {
        self.rejected_values
            .lock()
            .expect("rejected capture values")
            .push(value);
    }

    fn conflict_value(&self, value: String) {
        self.conflict_values
            .lock()
            .expect("conflicting capture values")
            .push(value);
    }

    fn activation_intents(&self) -> Vec<ModelTurnStateCaptureActivation> {
        self.activation_intents
            .lock()
            .expect("capture activation intents")
            .clone()
    }
}

#[async_trait]
impl TurnStateStore for CaptureStore {
    async fn load(&self, _: &str) -> Result<TurnStateView, TurnStateStoreError> {
        Err(TurnStateStoreError::Unavailable)
    }

    async fn update(
        &self,
        _: &str,
        _: bool,
        _: Option<Option<String>>,
        _: u64,
    ) -> Result<TurnStateView, TurnStateStoreError> {
        Err(TurnStateStoreError::Unavailable)
    }

    async fn use_observed(
        &self,
        _: &str,
        _: &str,
        _: bool,
        _: u64,
    ) -> Result<TurnStateView, TurnStateStoreError> {
        Err(TurnStateStoreError::Unavailable)
    }

    fn enqueue_observation(&self, _: TurnStateObservation) {}

    fn active_override(&self, _: &str) -> Option<String> {
        None
    }

    async fn load_model_state(
        &self,
        _: &str,
        requested_model: &str,
    ) -> Result<ModelTurnStateView, TurnStateStoreError> {
        self.views
            .lock()
            .expect("model views")
            .get(requested_model)
            .cloned()
            .ok_or(TurnStateStoreError::NotFound)
    }

    async fn update_model_state(
        &self,
        _: &str,
        requested_model: &str,
        update: ModelTurnStateUpdate,
    ) -> Result<ModelTurnStateView, TurnStateStoreError> {
        let mut views = self.views.lock().expect("model views");
        let view = views
            .get_mut(requested_model)
            .ok_or(TurnStateStoreError::NotFound)?;
        if view.identity_revision != update.expected_identity_revision
            || view.effective_model != update.expected_effective_model
            || view.config_revision != update.expected_revision
        {
            return Err(TurnStateStoreError::Conflict);
        }
        view.lock_enabled = update.lock_enabled;
        view.capture_enabled = update.capture_enabled;
        view.config_revision += 1;
        Ok(view.clone())
    }

    fn active_model_pin(&self, _: &str, _: u64, _: &str) -> Option<ActiveModelTurnStatePin> {
        None
    }

    async fn model_capture_candidates(
        &self,
        after: Option<&ModelTurnStateCaptureCursor>,
        limit: u16,
    ) -> Result<Vec<ModelTurnStateCaptureScope>, TurnStateStoreError> {
        self.candidate_queries
            .lock()
            .expect("candidate queries")
            .push(after.cloned());
        Ok(self
            .candidates
            .iter()
            .filter(|scope| {
                after.is_none_or(|cursor| {
                    (
                        &scope.account_id,
                        scope.identity_revision,
                        &scope.effective_model,
                    ) > (
                        &cursor.account_id,
                        cursor.identity_revision,
                        &cursor.effective_model,
                    )
                })
            })
            .take(usize::from(limit))
            .cloned()
            .collect())
    }

    async fn maintain_model_turn_state_candidates(
        &self,
        _: u16,
    ) -> Result<(), TurnStateStoreError> {
        self.maintenance_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn commit_model_capture(
        &self,
        scope: &ModelTurnStateCaptureScope,
        value: &str,
        activation: ModelTurnStateCaptureActivation,
    ) -> Result<ModelTurnStateCaptureCommitOutcome, TurnStateStoreError> {
        self.commit_calls.fetch_add(1, Ordering::SeqCst);
        self.activation_intents
            .lock()
            .expect("capture activation intents")
            .push(activation);
        if self
            .rejected_values
            .lock()
            .expect("rejected capture values")
            .iter()
            .any(|rejected| rejected == value)
        {
            return Ok(ModelTurnStateCaptureCommitOutcome::RejectedValue);
        }
        if self
            .conflict_values
            .lock()
            .expect("conflicting capture values")
            .iter()
            .any(|conflicting| conflicting == value)
        {
            return Err(TurnStateStoreError::Conflict);
        }
        if let Some(view) = self
            .views
            .lock()
            .expect("capture views")
            .get(&scope.requested_model)
            .filter(|view| {
                view.pin
                    .as_ref()
                    .is_some_and(|pin| !pin.invalidated && pin.value == value)
            })
            .cloned()
        {
            return Ok(ModelTurnStateCaptureCommitOutcome::Unchanged(view));
        }
        if self.write_before_commit_delay {
            self.commits
                .lock()
                .expect("capture commits")
                .push(scope.effective_model.clone());
        }
        tokio::time::sleep(self.commit_delay).await;
        self.published.fetch_add(1, Ordering::SeqCst);
        if !self.write_before_commit_delay {
            self.commits
                .lock()
                .expect("capture commits")
                .push(scope.effective_model.clone());
        }
        Ok(ModelTurnStateCaptureCommitOutcome::Committed(view(
            &scope.account_id,
            &scope.requested_model,
            &scope.effective_model,
            scope.capture_policy.clone(),
        )))
    }
}

#[tokio::test(start_paused = true)]
async fn invalid_candidate_waits_for_backoff_before_the_next_attempt() {
    let policy = policy(2, 10, 30, 1, 1);
    let store = CaptureStore::new([view(
        "acct_backoff",
        "model-backoff",
        "model-backoff",
        policy,
    )]);
    let provider = FakeProviderAdmin::new("openai", events());
    provider.set_capture_values(["short".to_owned(), "V".repeat(292)]);
    let (mut bundle, services) = bundle(Arc::clone(&store), Arc::clone(&provider)).await;
    let job = start(&services, "acct_backoff", "model-backoff").await;
    let (shutdown, worker) = spawn_capture_worker(&mut bundle);

    spin_until("first provider attempt", || {
        provider.capture_calls().len() == 1
    })
    .await;
    tokio::time::advance(Duration::from_millis(999)).await;
    tokio::task::yield_now().await;
    assert_eq!(provider.capture_calls().len(), 1);
    tokio::time::advance(Duration::from_millis(1)).await;
    spin_until("second provider attempt", || {
        provider.capture_calls().len() == 2
    })
    .await;
    spin_until("backoff job success", || {
        services
            .accounts()
            .model_turn_state_capture(&job.job_id)
            .is_ok_and(|job| job.status == ModelTurnStateCaptureStatus::Succeeded)
    })
    .await;
    let calls = provider.capture_calls();
    assert!(calls[1].duration_since(calls[0]) >= Duration::from_secs(1));
    assert_eq!(store.commits(), vec!["model-backoff"]);
    assert_eq!(
        store.activation_intents(),
        vec![ModelTurnStateCaptureActivation::ActivateImmediately]
    );
    stop_worker(shutdown, worker).await;
}

#[tokio::test]
async fn manual_same_active_value_finishes_unchanged_without_another_attempt() {
    let policy = policy(2, 10, 30, 0, 0);
    let rejected = "R".repeat(292);
    let mut model_view = view(
        "acct_rejected_retry",
        "model-rejected-retry",
        "model-rejected-retry",
        policy,
    );
    model_view.pin = Some(active_pin(rejected.clone()));
    let store = CaptureStore::new([model_view]);
    let provider = FakeProviderAdmin::new("openai", events());
    provider.set_capture_values([rejected, "N".repeat(292)]);
    let (mut bundle, services) = bundle(Arc::clone(&store), Arc::clone(&provider)).await;
    let job = start(&services, "acct_rejected_retry", "model-rejected-retry").await;
    let (shutdown, worker) = spawn_capture_worker(&mut bundle);

    spin_until("retry after rejected capture value", || {
        services
            .accounts()
            .model_turn_state_capture(&job.job_id)
            .is_ok_and(|job| job.status == ModelTurnStateCaptureStatus::Succeeded)
    })
    .await;
    let unchanged = services
        .accounts()
        .model_turn_state_capture(&job.job_id)
        .expect("unchanged capture job");
    assert_eq!(unchanged.reason.as_deref(), Some("unchanged_value"));
    assert_eq!(provider.capture_calls().len(), 1);
    assert_eq!(store.commit_calls.load(Ordering::SeqCst), 1);
    assert!(store.commits().is_empty());
    assert_eq!(
        store.activation_intents(),
        vec![ModelTurnStateCaptureActivation::ActivateImmediately]
    );
    stop_worker(shutdown, worker).await;
}

#[tokio::test]
async fn manual_same_active_value_reports_rejected_reason_after_attempts_are_exhausted() {
    let policy = policy(2, 10, 30, 0, 0);
    let rejected = "R".repeat(292);
    let mut model_view = view("acct_rejected", "model-rejected", "model-rejected", policy);
    model_view.pin = Some(active_pin(rejected.clone()));
    let store = CaptureStore::new([model_view]);
    store.reject_value(rejected.clone());
    let provider = FakeProviderAdmin::new("openai", events());
    provider.set_capture_values([rejected.clone(), rejected]);
    let (mut bundle, services) = bundle(Arc::clone(&store), Arc::clone(&provider)).await;
    let job = start(&services, "acct_rejected", "model-rejected").await;
    let (shutdown, worker) = spawn_capture_worker(&mut bundle);

    spin_until("rejected capture failure", || {
        services
            .accounts()
            .model_turn_state_capture(&job.job_id)
            .is_ok_and(|job| job.status == ModelTurnStateCaptureStatus::Failed)
    })
    .await;
    let failed = services
        .accounts()
        .model_turn_state_capture(&job.job_id)
        .expect("rejected capture job");
    assert_eq!(failed.reason.as_deref(), Some("rejected_value"));
    assert_eq!(failed.attempts, 2);
    assert_eq!(provider.capture_calls().len(), 2);
    assert!(store.commits().is_empty());
    stop_worker(shutdown, worker).await;
}

#[tokio::test]
async fn store_scope_conflict_still_terminates_the_job() {
    let policy = policy(2, 10, 30, 0, 0);
    let store = CaptureStore::new([view(
        "acct_scope_changed",
        "model-scope-changed",
        "model-scope-changed",
        policy,
    )]);
    let conflicting = "C".repeat(292);
    store.conflict_value(conflicting.clone());
    let provider = FakeProviderAdmin::new("openai", events());
    provider.set_capture_values([conflicting, "N".repeat(292)]);
    let (mut bundle, services) = bundle(Arc::clone(&store), Arc::clone(&provider)).await;
    let job = start(&services, "acct_scope_changed", "model-scope-changed").await;
    let (shutdown, worker) = spawn_capture_worker(&mut bundle);

    spin_until("scope conflict failure", || {
        services
            .accounts()
            .model_turn_state_capture(&job.job_id)
            .is_ok_and(|job| job.status == ModelTurnStateCaptureStatus::Failed)
    })
    .await;
    let failed = services
        .accounts()
        .model_turn_state_capture(&job.job_id)
        .expect("scope-conflicted capture job");
    assert_eq!(failed.reason.as_deref(), Some("scope_changed"));
    assert_eq!(failed.attempts, 1);
    assert_eq!(provider.capture_calls().len(), 1);
    stop_worker(shutdown, worker).await;
}

#[tokio::test(start_paused = true)]
async fn deadline_and_cancel_after_commit_start_wait_for_ack_and_converge_to_success() {
    let policy = policy(1, 2, 2, 0, 0);
    let store = CaptureStore::with_commit_ack_delay(
        view("acct_timeout", "model-timeout", "model-timeout", policy),
        Duration::from_secs(30),
    );
    let provider = FakeProviderAdmin::new("openai", events());
    provider.set_capture_values(["T".repeat(292)]);
    let (mut bundle, services) = bundle(Arc::clone(&store), provider).await;
    let job = start(&services, "acct_timeout", "model-timeout").await;
    let (shutdown, worker) = spawn_capture_worker(&mut bundle);

    spin_until("database write completed before delayed ack", || {
        !store.commits().is_empty()
    })
    .await;
    let cancel_services = services.clone();
    let cancel_job_id = job.job_id.clone();
    let cancel = tokio::spawn(async move {
        cancel_services
            .accounts()
            .cancel_model_turn_state_capture(&cancel_job_id)
            .await
            .expect("cancel after commit linearization")
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(30)).await;
    let cancelled = cancel.await.expect("cancel join");
    assert_eq!(cancelled.status, ModelTurnStateCaptureStatus::Succeeded);
    let result = services
        .accounts()
        .model_turn_state_capture(&job.job_id)
        .expect("committed job");
    assert_eq!(result.status, ModelTurnStateCaptureStatus::Succeeded);
    assert_eq!(store.commits(), vec!["model-timeout"]);
    assert_eq!(store.published.load(Ordering::SeqCst), 1);
    stop_worker(shutdown, worker).await;
}

#[tokio::test(start_paused = true)]
async fn cancellation_before_commit_start_prevents_the_store_write() {
    let policy = policy(1, 60, 300, 0, 0);
    let store = CaptureStore::new([view("acct_cancel", "model-cancel", "model-cancel", policy)]);
    let provider = FakeProviderAdmin::new("openai", events());
    provider.set_capture_values(["C".repeat(292)]);
    let (mut bundle, services) = bundle(Arc::clone(&store), provider).await;
    let job = start(&services, "acct_cancel", "model-cancel").await;
    let cancelled = services
        .accounts()
        .cancel_model_turn_state_capture(&job.job_id)
        .await
        .expect("cancel capture");
    assert_eq!(cancelled.status, ModelTurnStateCaptureStatus::Cancelled);
    let (shutdown, worker) = spawn_capture_worker(&mut bundle);
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert_eq!(store.commit_calls.load(Ordering::SeqCst), 0);
    assert!(store.commits().is_empty());
    stop_worker(shutdown, worker).await;
}

#[tokio::test]
async fn disabling_model_b_does_not_cancel_the_queued_model_a_job() {
    let policy = policy(1, 10, 30, 0, 0);
    let store = CaptureStore::new([
        view("acct_scope", "model-a", "model-a", policy.clone()),
        view("acct_scope", "model-b", "model-b", policy),
    ]);
    let provider = FakeProviderAdmin::new("openai", events());
    let provider_probe = provider.clone();
    let (_bundle, services) = bundle(store, provider).await;
    assert!(
        services
            .accounts()
            .start_model_turn_state_capture(
                &ProviderAccountId::new("acct_scope").expect("account ID"),
                "model-a",
                1,
                99,
                1,
                "model-a",
            )
            .await
            .is_err(),
        "manual capture start must fence the account policy revision"
    );
    assert!(
        provider_probe.capture_calls().is_empty(),
        "stale policy must be rejected before any provider probe starts"
    );
    assert!(
        services
            .accounts()
            .start_model_turn_state_capture(
                &ProviderAccountId::new("acct_scope").expect("account ID"),
                "model-a",
                1,
                1,
                99,
                "model-a",
            )
            .await
            .is_err(),
        "manual capture start must fence the identity read by the client"
    );
    assert!(
        services
            .accounts()
            .start_model_turn_state_capture(
                &ProviderAccountId::new("acct_scope").expect("account ID"),
                "model-a",
                1,
                1,
                1,
                "stale-effective-model",
            )
            .await
            .is_err(),
        "manual capture start must fence the effective model read by the client"
    );
    let job = start(&services, "acct_scope", "model-a").await;
    services
        .accounts()
        .update_model_turn_state(
            &ProviderAccountId::new("acct_scope").expect("account ID"),
            "model-b",
            disabled_update("model-b"),
        )
        .await
        .expect("disable model B capture");
    assert_eq!(
        services
            .accounts()
            .model_turn_state_capture(&job.job_id)
            .expect("model A job")
            .status,
        ModelTurnStateCaptureStatus::Queued
    );
}

#[tokio::test(start_paused = true)]
async fn manual_start_conflicts_with_an_active_automatic_job_for_the_same_scope() {
    let policy = policy(2, 10, 300, 60, 60);
    let model_view = view(
        "acct_auto_owned",
        "model-auto-owned",
        "model-auto-owned",
        policy.clone(),
    );
    let candidate = ModelTurnStateCaptureScope {
        account_id: model_view.account_id.clone(),
        requested_model: model_view.requested_model.clone(),
        effective_model: model_view.effective_model.clone(),
        identity_revision: model_view.identity_revision,
        config_revision: model_view.config_revision,
        policy_revision: model_view.policy_revision,
        capture_enabled: model_view.capture_enabled,
        capture_proxy_id: model_view.capture_proxy_id.clone(),
        capture_policy: policy,
    };
    let store = CaptureStore::with_view_and_candidates(model_view, vec![candidate]);
    let provider = FakeProviderAdmin::new("openai", events());
    let (mut bundle, services) = bundle(Arc::clone(&store), Arc::clone(&provider)).await;
    let (shutdown, worker) = spawn_capture_worker(&mut bundle);

    spin_until("automatic capture attempt", || {
        provider.capture_calls().len() == 1
    })
    .await;
    let error = services
        .accounts()
        .start_model_turn_state_capture(
            &ProviderAccountId::new("acct_auto_owned").expect("account ID"),
            "model-auto-owned",
            1,
            1,
            1,
            "model-auto-owned",
        )
        .await
        .expect_err("manual start must not reuse an automatic job");
    assert_eq!(error.message(), "账号已有自动捕获任务，请先取消后重试");
    assert!(store.activation_intents().is_empty());

    stop_worker(shutdown, worker).await;
}

#[tokio::test(start_paused = true)]
async fn automatic_scan_advances_past_sixty_four_unusable_candidates() {
    let policy = policy(1, 10, 30, 0, 0);
    let candidates = (0..65)
        .map(|index| ModelTurnStateCaptureScope {
            account_id: format!("acct_fair_{index:03}"),
            requested_model: format!("model-{index:03}"),
            effective_model: format!("model-{index:03}"),
            identity_revision: 1,
            config_revision: 1,
            policy_revision: 1,
            capture_enabled: true,
            capture_proxy_id: Some(if index == 64 {
                "proxy_capture".to_owned()
            } else {
                "missing_proxy".to_owned()
            }),
            capture_policy: policy.clone(),
        })
        .collect();
    let store = CaptureStore::with_candidates(candidates);
    let provider = FakeProviderAdmin::new("openai", events());
    provider.set_capture_values(["F".repeat(292)]);
    let (mut bundle, _services) = bundle(Arc::clone(&store), Arc::clone(&provider)).await;
    let (shutdown, worker) = spawn_capture_worker(&mut bundle);

    spin_until("first automatic page", || {
        store.candidate_queries().len() == 1
    })
    .await;
    assert!(store.commits().is_empty());
    for _ in 0..128 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(30)).await;
    spin_until("second automatic page", || {
        store.candidate_queries().len() == 2
    })
    .await;
    spin_until("fair provider attempt", || {
        provider.capture_calls().len() == 1
    })
    .await;
    spin_until("fair commit started", || {
        store.commit_calls.load(Ordering::SeqCst) == 1
    })
    .await;
    spin_until("fair candidate commit", || !store.commits().is_empty()).await;
    let queries = store.candidate_queries();
    assert_eq!(queries.len(), 2);
    assert!(store.maintenance_calls.load(Ordering::SeqCst) >= 2);
    assert_eq!(
        queries[1].as_ref().map(|cursor| cursor.account_id.as_str()),
        Some("acct_fair_063")
    );
    assert_eq!(store.commits(), vec!["model-064"]);
    assert_eq!(
        store.activation_intents(),
        vec![ModelTurnStateCaptureActivation::StageIfActiveFresh]
    );
    stop_worker(shutdown, worker).await;
}

async fn bundle(
    store: Arc<CaptureStore>,
    provider: Arc<FakeProviderAdmin>,
) -> (AdminBundle, AdminServices) {
    let proxy_events = events();
    let bundle = AdminHarness::new()
        .provider(provider)
        .proxies(Arc::new(TestProxies {
            events: Some(proxy_events.clone()),
            capture_proxy: Some(capture_proxy()),
            ..Default::default()
        }))
        .turn_state(store)
        .build_bundle()
        .await;
    let services = bundle.services();
    (bundle, services)
}

fn spawn_capture_worker(
    bundle: &mut AdminBundle,
) -> (
    CancellationToken,
    tokio::task::JoinHandle<Result<(), WorkerTaskError>>,
) {
    let registration = bundle
        .take_worker_contributions()
        .into_iter()
        .find_map(|contribution| match contribution {
            WorkerContribution::Registration(registration)
                if registration.id.kind() == WorkerKind::TurnStateCapture =>
            {
                Some(registration)
            }
            _ => None,
        })
        .expect("turn-state capture worker");
    let WorkerRunnable::Daemon { task, .. } = registration.runnable else {
        panic!("capture worker must be a daemon");
    };
    let shutdown = CancellationToken::new();
    let worker_shutdown = shutdown.clone();
    let worker = tokio::spawn(async move { task.run(worker_shutdown).await });
    (shutdown, worker)
}

async fn stop_worker(
    shutdown: CancellationToken,
    worker: tokio::task::JoinHandle<Result<(), WorkerTaskError>>,
) {
    shutdown.cancel();
    worker
        .await
        .expect("capture worker join")
        .expect("capture worker");
}

async fn start(
    services: &AdminServices,
    account_id: &str,
    model: &str,
) -> gateway_admin::model::accounts::ModelTurnStateCaptureJob {
    services
        .accounts()
        .start_model_turn_state_capture(
            &ProviderAccountId::new(account_id).expect("account ID"),
            model,
            1,
            1,
            1,
            model,
        )
        .await
        .expect("start capture")
}

async fn spin_until(label: &str, mut predicate: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if predicate() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition did not become true: {label}");
}

fn view(
    account_id: &str,
    requested_model: &str,
    effective_model: &str,
    capture_policy: ModelTurnStateCapturePolicy,
) -> ModelTurnStateView {
    ModelTurnStateView {
        account_id: account_id.to_owned(),
        requested_model: requested_model.to_owned(),
        effective_model: effective_model.to_owned(),
        identity_revision: 1,
        config_revision: 1,
        policy_revision: 1,
        lock_enabled: false,
        capture_enabled: true,
        reuse_window_seconds: 3_600,
        refresh_lead_seconds: 900,
        capture_trigger_mode: ModelTurnStateCaptureTriggerMode::OnAttributedFailure,
        missing_state_action: Default::default(),
        capture_proxy_id: Some("proxy_capture".to_owned()),
        capture_proxy: None,
        capture_policy,
        pin: None,
        candidate: None,
        next_capture_at: None,
        next_activation_at: None,
        capture_not_before: None,
        waiting_reason: None,
        legacy_override_enabled: false,
        legacy_override_configured: false,
        legacy_override_value: None,
    }
}

fn policy(
    max_attempts: u8,
    attempt_timeout_seconds: u16,
    job_timeout_seconds: u16,
    backoff_seconds: u8,
    max_backoff_seconds: u8,
) -> ModelTurnStateCapturePolicy {
    ModelTurnStateCapturePolicy {
        max_attempts,
        attempt_timeout_seconds,
        job_timeout_seconds,
        backoff_seconds,
        max_backoff_seconds,
        cooldown_seconds: 0,
    }
}

fn active_pin(value: String) -> ModelTurnStatePin {
    let captured_at = Utc::now();
    ModelTurnStatePin {
        encoded_bytes: value.len(),
        value,
        raw_bytes: None,
        ciphertext_bytes: None,
        envelope_format: None,
        token_version: None,
        issued_at: None,
        timestamp_verified: false,
        sha256: "active-sha256".to_owned(),
        captured_at,
        reuse_deadline: captured_at + chrono::Duration::hours(1),
        source: "capture".to_owned(),
        compatible_transports: vec!["http".to_owned(), "websocket".to_owned()],
        sent_count: 0,
        last_sent_at: None,
        invalidated: false,
        generation: 1,
        id: None,
    }
}

fn disabled_update(effective_model: &str) -> ModelTurnStateUpdate {
    ModelTurnStateUpdate {
        expected_identity_revision: 1,
        expected_effective_model: effective_model.to_owned(),
        lock_enabled: false,
        capture_enabled: false,
        reuse_window_seconds: 3_600,
        capture_proxy_id: Some("proxy_capture".to_owned()),
        max_attempts: 1,
        attempt_timeout_seconds: 10,
        job_timeout_seconds: 30,
        backoff_seconds: 0,
        max_backoff_seconds: 0,
        cooldown_seconds: 0,
        pin_action: ModelTurnStatePinAction::Keep,
        value: None,
        expected_revision: 1,
    }
}

fn capture_proxy() -> ProxyRecord {
    let now = Utc::now();
    ProxyRecord {
        location: None,
        id: "proxy_capture".to_owned(),
        name: "Capture".to_owned(),
        proxy: OutboundProxy::parse("http://127.0.0.1:8080").expect("proxy"),
        revision: revision(1),
        account_count: 0,
        last_test_at: Some(now),
        last_test: Some(ProxyTestResult {
            success: true,
            latency_ms: 1,
            exit_ip: None,
            exit_ipv4: None,
            exit_ipv6: None,
            message: "ok".to_owned(),
        }),
        created_at: now,
        updated_at: now,
    }
}
