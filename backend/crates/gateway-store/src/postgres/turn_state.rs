//! OpenAI 账号 turn state 的权威配置、进程内快照与有界观测写入。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gateway_core::lifecycle::CancellationToken;
use gateway_core::provider_ports::turn_state::{
    AccountTurnStatePolicyUpdate, AccountTurnStatePolicyView, ActiveModelTurnStatePin,
    MODEL_TURN_STATE_BYTES, ModelTurnStateCaptureCursor, ModelTurnStateCapturePolicy,
    ModelTurnStateCaptureScope, ModelTurnStateObservationScope, ModelTurnStatePin,
    ModelTurnStatePinAction, ModelTurnStateUpdate, ModelTurnStateView, TurnStateObservation,
    TurnStateObserved, TurnStateOverride, TurnStateSent, TurnStateStore, TurnStateStoreError,
    TurnStateView, model_turn_state_token_metadata, valid_model_turn_state,
    valid_turn_state_override,
};
use gateway_core::routing::resolve_model_mapping;
use gateway_core::task::{DaemonTask, WorkerTaskError};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use tokio::sync::{Mutex, mpsc};

use crate::{StoreBackend, StoreError, StoreResult};

const OVERRIDE_SNAPSHOT_LIMIT: i64 = 65_536;
const OBSERVATION_QUEUE_CAPACITY: usize = 512;
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

pub struct PgTurnStateStore {
    pool: PgPool,
    overrides: Arc<RwLock<HashMap<String, CachedOverride>>>,
    account_policies: Arc<RwLock<HashMap<AccountPolicyKey, CachedAccountPolicy>>>,
    model_pins: Arc<RwLock<HashMap<ModelPinKey, CachedModelPin>>>,
    observations: mpsc::Sender<TurnStateWrite>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ModelPinKey {
    account_id: String,
    identity_revision: u64,
    effective_model: String,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct AccountPolicyKey {
    account_id: String,
    identity_revision: u64,
}

struct CachedAccountPolicy {
    config_revision: u64,
    lock_enabled: bool,
    capture_enabled: bool,
    reuse_window_seconds: u32,
}

struct CachedModelPin {
    config_revision: u64,
    active_generation: u64,
    value: Option<String>,
    source: Option<String>,
    active_candidate_id: Option<String>,
    captured_at: Option<DateTime<Utc>>,
    reuse_deadline: Option<DateTime<Utc>>,
    invalidated: bool,
    candidate_id: Option<String>,
    candidate_value: Option<String>,
    candidate_source: Option<String>,
    candidate_captured_at: Option<DateTime<Utc>>,
    candidate_reuse_deadline: Option<DateTime<Utc>>,
}

struct CachedOverride {
    revision: u64,
    value: Option<String>,
}

impl PgTurnStateStore {
    pub async fn initialize(pool: PgPool) -> StoreResult<(Self, TurnStateObservationWriter)> {
        let rows = sqlx::query(
            "select account_id, enabled, override_value, config_revision
               from openai_turn_states
              order by account_id
              limit $1",
        )
        .bind(OVERRIDE_SNAPSHOT_LIMIT + 1)
        .fetch_all(&pool)
        .await
        .map_err(startup_unavailable)?;
        if i64::try_from(rows.len()).unwrap_or(i64::MAX) > OVERRIDE_SNAPSHOT_LIMIT {
            return Err(StoreError::InvalidData {
                entity: "OpenAI turn state override snapshot",
                message: "enabled override count exceeds the supported limit".to_owned(),
            });
        }
        let mut overrides = HashMap::with_capacity(rows.len());
        for row in rows {
            let account_id: String = row.get("account_id");
            let enabled: bool = row.get("enabled");
            let value: Option<String> = row.get("override_value");
            let revision = u64::try_from(row.get::<i64, _>("config_revision")).map_err(|_| {
                StoreError::InvalidData {
                    entity: "OpenAI turn state override snapshot",
                    message: format!("account {account_id} contains an invalid revision"),
                }
            })?;
            if enabled
                && value
                    .as_deref()
                    .is_none_or(|value| !valid_turn_state_override(value))
            {
                return Err(StoreError::InvalidData {
                    entity: "OpenAI turn state override snapshot",
                    message: format!("account {account_id} contains an invalid override"),
                });
            }
            overrides.insert(
                account_id,
                CachedOverride {
                    revision,
                    value: enabled.then_some(value).flatten(),
                },
            );
        }
        let overrides = Arc::new(RwLock::new(overrides));
        let policy_rows = sqlx::query(
            "select account_id, identity_revision, config_revision, lock_enabled,
                    capture_enabled, reuse_window_seconds
               from openai_account_turn_state_policies
              order by account_id, identity_revision
              limit $1",
        )
        .bind(OVERRIDE_SNAPSHOT_LIMIT + 1)
        .fetch_all(&pool)
        .await
        .map_err(startup_unavailable)?;
        if i64::try_from(policy_rows.len()).unwrap_or(i64::MAX) > OVERRIDE_SNAPSHOT_LIMIT {
            return Err(StoreError::InvalidData {
                entity: "OpenAI account turn state policy snapshot",
                message: "account policy count exceeds the supported limit".to_owned(),
            });
        }
        let mut account_policies = HashMap::with_capacity(policy_rows.len());
        for row in policy_rows {
            let account_id: String = row.get("account_id");
            let identity_revision =
                u64::try_from(row.get::<i64, _>("identity_revision")).map_err(|_| {
                    StoreError::InvalidData {
                        entity: "OpenAI account turn state policy snapshot",
                        message: format!(
                            "account {account_id} contains an invalid identity revision"
                        ),
                    }
                })?;
            account_policies.insert(
                AccountPolicyKey {
                    account_id,
                    identity_revision,
                },
                CachedAccountPolicy {
                    config_revision: u64::try_from(row.get::<i64, _>("config_revision")).map_err(
                        |_| StoreError::InvalidData {
                            entity: "OpenAI account turn state policy snapshot",
                            message: "account policy contains an invalid revision".to_owned(),
                        },
                    )?,
                    lock_enabled: row.get("lock_enabled"),
                    capture_enabled: row.get("capture_enabled"),
                    reuse_window_seconds: u32::try_from(row.get::<i32, _>("reuse_window_seconds"))
                        .map_err(|_| StoreError::InvalidData {
                            entity: "OpenAI account turn state policy snapshot",
                            message: "account policy contains an invalid reuse window".to_owned(),
                        })?,
                },
            );
        }
        let account_policies = Arc::new(RwLock::new(account_policies));
        let model_rows = sqlx::query(
            "select account_id, identity_revision, effective_model, config_revision,
                    active_generation, active_candidate_id, pin_value, pin_source, pin_captured_at,
                    pin_reuse_deadline, pin_invalidated_at, candidate_id, candidate_value,
                    candidate_source, candidate_captured_at, candidate_reuse_deadline
               from openai_model_turn_states
              order by account_id, identity_revision, effective_model
              limit $1",
        )
        .bind(OVERRIDE_SNAPSHOT_LIMIT + 1)
        .fetch_all(&pool)
        .await
        .map_err(startup_unavailable)?;
        if i64::try_from(model_rows.len()).unwrap_or(i64::MAX) > OVERRIDE_SNAPSHOT_LIMIT {
            return Err(StoreError::InvalidData {
                entity: "OpenAI model turn state snapshot",
                message: "model pin count exceeds the supported limit".to_owned(),
            });
        }
        let mut model_pins = HashMap::with_capacity(model_rows.len());
        for row in model_rows {
            let account_id: String = row.get("account_id");
            let identity_revision =
                u64::try_from(row.get::<i64, _>("identity_revision")).map_err(|_| {
                    StoreError::InvalidData {
                        entity: "OpenAI model turn state snapshot",
                        message: format!(
                            "account {account_id} contains an invalid identity revision"
                        ),
                    }
                })?;
            let effective_model: String = row.get("effective_model");
            let config_revision =
                u64::try_from(row.get::<i64, _>("config_revision")).map_err(|_| {
                    StoreError::InvalidData {
                        entity: "OpenAI model turn state snapshot",
                        message: format!("account {account_id} contains an invalid model revision"),
                    }
                })?;
            let value: Option<String> = row.get("pin_value");
            if value
                .as_deref()
                .is_some_and(|value| !valid_turn_state_override(value))
            {
                return Err(StoreError::InvalidData {
                    entity: "OpenAI model turn state snapshot",
                    message: format!("account {account_id} contains an invalid model pin"),
                });
            }
            model_pins.insert(
                ModelPinKey {
                    account_id: account_id.clone(),
                    identity_revision,
                    effective_model,
                },
                CachedModelPin {
                    config_revision,
                    active_generation: u64::try_from(row.get::<i64, _>("active_generation"))
                        .map_err(|_| StoreError::InvalidData {
                            entity: "OpenAI model turn state snapshot",
                            message: format!("account {account_id} contains an invalid generation"),
                        })?,
                    value,
                    source: row.get("pin_source"),
                    active_candidate_id: row.get("active_candidate_id"),
                    captured_at: row.get("pin_captured_at"),
                    reuse_deadline: row.get("pin_reuse_deadline"),
                    invalidated: row
                        .get::<Option<DateTime<Utc>>, _>("pin_invalidated_at")
                        .is_some(),
                    candidate_id: row.get("candidate_id"),
                    candidate_value: row.get("candidate_value"),
                    candidate_source: row.get("candidate_source"),
                    candidate_captured_at: row.get("candidate_captured_at"),
                    candidate_reuse_deadline: row.get("candidate_reuse_deadline"),
                },
            );
        }
        let model_pins = Arc::new(RwLock::new(model_pins));
        let writer_model_pins = Arc::clone(&model_pins);
        let (sender, receiver) = mpsc::channel(OBSERVATION_QUEUE_CAPACITY);
        Ok((
            Self {
                pool: pool.clone(),
                overrides,
                account_policies,
                model_pins,
                observations: sender,
            },
            TurnStateObservationWriter {
                pool,
                model_pins: writer_model_pins,
                receiver: Mutex::new(receiver),
            },
        ))
    }

    #[doc(hidden)]
    pub fn publish_cached_override(&self, view: &TurnStateView) -> Result<()> {
        let mut overrides = self
            .overrides
            .write()
            .map_err(|_| TurnStateStoreError::Unavailable)?;
        let value = if view.override_state.enabled {
            let value = view
                .override_state
                .value
                .as_ref()
                .ok_or(TurnStateStoreError::Unavailable)?;
            Some(value.clone())
        } else {
            None
        };
        match overrides.entry(view.account_id.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(CachedOverride {
                    revision: view.config_revision,
                    value,
                });
            }
            std::collections::hash_map::Entry::Occupied(mut entry)
                if view.config_revision > entry.get().revision =>
            {
                entry.insert(CachedOverride {
                    revision: view.config_revision,
                    value,
                });
            }
            std::collections::hash_map::Entry::Occupied(_) => {}
        }
        Ok(())
    }

    fn publish_cached_model(&self, view: &ModelTurnStateView) -> Result<()> {
        publish_cached_model_snapshot(&self.model_pins, view)
    }

    fn publish_cached_policy(&self, view: &AccountTurnStatePolicyView) -> Result<()> {
        let mut policies = self
            .account_policies
            .write()
            .map_err(|_| TurnStateStoreError::Unavailable)?;
        let key = AccountPolicyKey {
            account_id: view.account_id.clone(),
            identity_revision: view.identity_revision,
        };
        let next = CachedAccountPolicy {
            config_revision: view.config_revision,
            lock_enabled: view.lock_enabled,
            capture_enabled: view.capture_enabled,
            reuse_window_seconds: view.reuse_window_seconds,
        };
        match policies.entry(key) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(next);
            }
            std::collections::hash_map::Entry::Occupied(mut entry)
                if view.config_revision > entry.get().config_revision =>
            {
                entry.insert(next);
            }
            std::collections::hash_map::Entry::Occupied(_) => {}
        }
        Ok(())
    }

    fn publish_cached_policy_from_model(&self, view: &ModelTurnStateView) -> Result<()> {
        self.publish_cached_policy(&AccountTurnStatePolicyView {
            account_id: view.account_id.clone(),
            identity_revision: view.identity_revision,
            config_revision: view.policy_revision,
            lock_enabled: view.lock_enabled,
            capture_enabled: view.capture_enabled,
            reuse_window_seconds: view.reuse_window_seconds,
            refresh_lead_seconds: view.refresh_lead_seconds,
            capture_proxy_id: view.capture_proxy_id.clone(),
            capture_proxy: None,
            capture_policy: view.capture_policy.clone(),
        })
    }
}

fn publish_cached_model_snapshot(
    snapshot: &RwLock<HashMap<ModelPinKey, CachedModelPin>>,
    view: &ModelTurnStateView,
) -> Result<()> {
    let mut model_pins = snapshot
        .write()
        .map_err(|_| TurnStateStoreError::Unavailable)?;
    let key = ModelPinKey {
        account_id: view.account_id.clone(),
        identity_revision: view.identity_revision,
        effective_model: view.effective_model.clone(),
    };
    let next = CachedModelPin {
        config_revision: view.config_revision,
        active_generation: view.pin.as_ref().map_or_else(
            || {
                view.candidate
                    .as_ref()
                    .map_or(1, |pin| pin.generation.saturating_sub(1).max(1))
            },
            |pin| pin.generation,
        ),
        value: view.pin.as_ref().map(|pin| pin.value.clone()),
        source: view.pin.as_ref().map(|pin| pin.source.clone()),
        active_candidate_id: view.pin.as_ref().and_then(|pin| pin.id.clone()),
        captured_at: view.pin.as_ref().map(|pin| pin.captured_at),
        reuse_deadline: view.pin.as_ref().map(|pin| pin.reuse_deadline),
        invalidated: view.pin.as_ref().is_some_and(|pin| pin.invalidated),
        candidate_id: view.candidate.as_ref().and_then(|pin| pin.id.clone()),
        candidate_value: view.candidate.as_ref().map(|pin| pin.value.clone()),
        candidate_source: view.candidate.as_ref().map(|pin| pin.source.clone()),
        candidate_captured_at: view.candidate.as_ref().map(|pin| pin.captured_at),
        candidate_reuse_deadline: view.candidate.as_ref().map(|pin| pin.reuse_deadline),
    };
    match model_pins.entry(key) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(next);
        }
        std::collections::hash_map::Entry::Occupied(mut entry)
            if view.config_revision > entry.get().config_revision =>
        {
            entry.insert(next);
        }
        std::collections::hash_map::Entry::Occupied(_) => {}
    }
    Ok(())
}

pub struct TurnStateObservationWriter {
    pool: PgPool,
    model_pins: Arc<RwLock<HashMap<ModelPinKey, CachedModelPin>>>,
    receiver: Mutex<mpsc::Receiver<TurnStateWrite>>,
}

enum TurnStateWrite {
    Returned(TurnStateObservation),
    Sent(TurnStateSent),
}

impl DaemonTask for TurnStateObservationWriter {
    fn run(
        &self,
        cancellation: CancellationToken,
    ) -> futures::future::BoxFuture<'_, std::result::Result<(), WorkerTaskError>> {
        Box::pin(async move {
            let mut receiver = self.receiver.lock().await;
            loop {
                let observation = tokio::select! {
                    () = cancellation.cancelled() => {
                        drain_observations(&self.pool, &self.model_pins, &mut receiver).await;
                        return Ok(());
                    }
                    observation = receiver.recv() => observation,
                };
                let Some(observation) = observation else {
                    return Err(WorkerTaskError::safe("turn state observation queue closed"));
                };
                persist_turn_state_write(&self.pool, &self.model_pins, observation).await;
            }
        })
    }
}

async fn drain_observations(
    pool: &PgPool,
    model_pins: &RwLock<HashMap<ModelPinKey, CachedModelPin>>,
    receiver: &mut mpsc::Receiver<TurnStateWrite>,
) {
    receiver.close();
    let started = Instant::now();
    while let Some(observation) = receiver.recv().await {
        let remaining = SHUTDOWN_DRAIN_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero()
            || tokio::time::timeout(
                remaining,
                persist_turn_state_write(pool, model_pins, observation),
            )
            .await
            .is_err()
        {
            break;
        }
    }
    let dropped = receiver.len();
    if dropped > 0 {
        tracing::warn!(
            dropped,
            "OpenAI turn state observations were dropped during shutdown"
        );
    }
}

async fn persist_turn_state_write(
    pool: &PgPool,
    model_pins: &RwLock<HashMap<ModelPinKey, CachedModelPin>>,
    write: TurnStateWrite,
) {
    match write {
        TurnStateWrite::Returned(observation) => {
            persist_observation(pool, model_pins, observation).await;
        }
        TurnStateWrite::Sent(sent) => {
            if let Err(error) = persist_sent(pool, &sent).await {
                tracing::warn!(
                    request_id = sent.request_id,
                    attempt_index = sent.attempt_index,
                    error_kind = ?error,
                    "OpenAI sent turn state receipt write failed"
                );
            }
        }
    }
}

async fn persist_sent(pool: &PgPool, sent: &TurnStateSent) -> Result<()> {
    if !valid_turn_state_override(&sent.value) {
        return Err(TurnStateStoreError::Invalid);
    }
    sqlx::query(
        "insert into request_turn_state_observations(
           request_id, attempt_index, observation_id, value, observed_at, source,
           sent_value, sent_at, sent_source, sent_transport, sent_account_id, sent_identity_revision,
           sent_effective_model, sent_generation, sent_candidate_id
         )
         select $1, $2, null, null, null, null, $3, $4, $5, $6, $7, $8, $9, $10, $11
           from runtime_settings settings
          where settings.id = 1
            and $4 >= now() - (settings.usage_retention_days * interval '1 day')
         on conflict (request_id, attempt_index) do update
           set sent_value = excluded.sent_value, sent_at = excluded.sent_at,
               sent_source = excluded.sent_source, sent_transport = excluded.sent_transport,
               sent_account_id = excluded.sent_account_id,
               sent_identity_revision = excluded.sent_identity_revision,
               sent_effective_model = excluded.sent_effective_model,
               sent_generation = excluded.sent_generation,
               sent_candidate_id = excluded.sent_candidate_id",
    )
    .bind(&sent.request_id)
    .bind(i32::try_from(sent.attempt_index).map_err(|_| TurnStateStoreError::Invalid)?)
    .bind(&sent.value)
    .bind(sent.sent_at)
    .bind(&sent.source)
    .bind(&sent.transport)
    .bind(&sent.account_id)
    .bind(i64::try_from(sent.identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
    .bind(&sent.effective_model)
    .bind(sent.generation.and_then(|value| i64::try_from(value).ok()))
    .bind(sent.candidate_id.as_deref())
    .execute(pool)
    .await
    .map_err(unavailable)?;
    Ok(())
}

async fn persist_observation(
    pool: &PgPool,
    model_pins: &RwLock<HashMap<ModelPinKey, CachedModelPin>>,
    observation: TurnStateObservation,
) {
    let account_id = observation.account_id.clone();
    let observation_id = observation.id.clone();
    if let Err(error) = persist_observation_inner(pool, &observation).await {
        tracing::warn!(
            account_id,
            observation_id,
            error_kind = ?error,
            "OpenAI turn state observation write failed"
        );
    }
    if let Some(scope) = observation.model_scope.as_ref()
        && let Err(error) = apply_model_observation(pool, model_pins, scope, &observation).await
    {
        tracing::warn!(
            account_id,
            observation_id,
            effective_model = scope.effective_model,
            error_kind = ?error,
            "OpenAI model turn state observation could not advance capture state"
        );
    }
}

async fn persist_observation_inner(
    pool: &PgPool,
    observation: &TurnStateObservation,
) -> Result<()> {
    if observation.value.len() > 16 * 1024 {
        return Err(TurnStateStoreError::Invalid);
    }
    if let (Some(request_id), Some(attempt_index)) =
        (&observation.request_id, observation.attempt_index)
        && let Err(error) =
            persist_request_observation(pool, request_id, attempt_index, observation).await
    {
        tracing::warn!(request_id, attempt_index, error_kind = ?error,
            "OpenAI request turn state observation write failed");
    }
    if observation.value.is_empty() {
        // 账号最近值保持既有的非空语义；请求级 0 B 观测仍是一次真实上游返回。
        return Ok(());
    }
    sqlx::query(
        "insert into openai_turn_states(
           account_id, observed_id, observed_value, observed_at, observed_transport,
           observed_upstream_response_id, observed_client_turn_id
         )
         select id, $2, $3, $4, $5, $6, $7
           from provider_accounts
          where id = $1 and provider_kind = 'openai'
         on conflict (account_id) do update
           set observed_id = excluded.observed_id,
               observed_value = excluded.observed_value,
               observed_at = excluded.observed_at,
               observed_transport = excluded.observed_transport,
               observed_upstream_response_id = case
                 when excluded.observed_id = openai_turn_states.observed_id
                   then coalesce(excluded.observed_upstream_response_id,
                                 openai_turn_states.observed_upstream_response_id)
                 else excluded.observed_upstream_response_id
               end,
               observed_client_turn_id = case
                 when excluded.observed_id = openai_turn_states.observed_id
                   then coalesce(excluded.observed_client_turn_id,
                                 openai_turn_states.observed_client_turn_id)
                 else excluded.observed_client_turn_id
               end
         where openai_turn_states.observed_at is null
            or openai_turn_states.observed_id is null
            or excluded.observed_id = openai_turn_states.observed_id
            or (excluded.observed_at, excluded.observed_id)
                 > (openai_turn_states.observed_at, openai_turn_states.observed_id)",
    )
    .bind(&observation.account_id)
    .bind(&observation.id)
    .bind(&observation.value)
    .bind(observation.observed_at)
    .bind(&observation.transport)
    .bind(observation.upstream_response_id.as_deref())
    .bind(observation.client_turn_id.as_deref())
    .execute(pool)
    .await
    .map_err(unavailable)?;
    Ok(())
}

async fn apply_model_observation(
    pool: &PgPool,
    model_pins: &RwLock<HashMap<ModelPinKey, CachedModelPin>>,
    scope: &ModelTurnStateObservationScope,
    observation: &TurnStateObservation,
) -> Result<()> {
    if observation.transport != "http" || observation.account_id != scope.account_id {
        return Err(TurnStateStoreError::Invalid);
    }
    let mut tx = pool.begin().await.map_err(unavailable)?;
    let inserted = sqlx::query(
        "insert into openai_model_turn_states(account_id, identity_revision, effective_model)
         select id, identity_revision, $3
           from provider_accounts
          where id = $1 and provider_kind = 'openai' and identity_revision = $2
         on conflict do nothing",
    )
    .bind(&scope.account_id)
    .bind(i64::try_from(scope.identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
    .bind(&scope.effective_model)
    .execute(&mut *tx)
    .await
    .map_err(unavailable)?
    .rows_affected()
        == 1;
    let row = sqlx::query(
        "select p.lock_enabled, p.capture_enabled, p.reuse_window_seconds,
                p.config_revision as policy_revision, s.pin_value,
                s.pin_captured_at, s.pin_reuse_deadline, s.pin_invalidated_at,
                s.config_revision as model_revision
           from openai_model_turn_states s
           join provider_accounts a on a.id = s.account_id
             and a.provider_kind = 'openai'
             and a.identity_revision = s.identity_revision
           join openai_account_turn_state_policies p
             on p.account_id = s.account_id and p.identity_revision = s.identity_revision
          where s.account_id = $1 and s.identity_revision = $2 and s.effective_model = $3
          for update",
    )
    .bind(&scope.account_id)
    .bind(i64::try_from(scope.identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
    .bind(&scope.effective_model)
    .fetch_optional(&mut *tx)
    .await
    .map_err(unavailable)?;
    let Some(row) = row else {
        tx.rollback().await.map_err(unavailable)?;
        return Ok(());
    };
    let config_revision = u64::try_from(row.get::<i64, _>("model_revision"))
        .map_err(|_| TurnStateStoreError::Unavailable)?;
    let policy_revision = u64::try_from(row.get::<i64, _>("policy_revision"))
        .map_err(|_| TurnStateStoreError::Unavailable)?;
    let reuse_window_seconds = u32::try_from(row.get::<i32, _>("reuse_window_seconds"))
        .map_err(|_| TurnStateStoreError::Unavailable)?;
    let active = row.get::<Option<String>, _>("pin_value").is_some()
        && row
            .get::<Option<DateTime<Utc>>, _>("pin_invalidated_at")
            .is_none()
        && row
            .get::<Option<DateTime<Utc>>, _>("pin_reuse_deadline")
            .zip(row.get::<Option<DateTime<Utc>>, _>("pin_captured_at"))
            .is_some_and(|(stored, captured)| {
                stored.min(model_reuse_deadline(captured, reuse_window_seconds)) > Utc::now()
            });
    let lock_enabled = row.get::<bool, _>("lock_enabled");
    let capture_enabled = row.get::<bool, _>("capture_enabled");
    let model_revision_matches = if scope.config_revision == 0 {
        inserted && config_revision == 1
    } else {
        config_revision == scope.config_revision
    };
    if (!lock_enabled && !capture_enabled)
        || !model_revision_matches
        || policy_revision != scope.policy_revision
        || active
    {
        tx.rollback().await.map_err(unavailable)?;
        return Ok(());
    }

    let encoded_bytes = observation.value.len();
    if valid_model_turn_state(&observation.value) {
        let metadata = model_turn_state_token_metadata(&observation.value);
        let reuse_window_seconds = u32::try_from(row.get::<i32, _>("reuse_window_seconds"))
            .map_err(|_| TurnStateStoreError::Unavailable)?;
        let reuse_deadline = model_reuse_deadline(observation.observed_at, reuse_window_seconds);
        let updated = sqlx::query(
            "update openai_model_turn_states
                set pin_value = $4, pin_source = 'observation',
                    pin_compatible_transport = 'http', pin_token_version = $5,
                    pin_issued_at = $6, pin_raw_bytes = $7, pin_captured_at = $8,
                    pin_reuse_deadline = $9, pin_invalidated_at = null,
                    active_activated_at = $8, active_candidate_id = null,
                    active_generation = active_generation + 1,
                    candidate_id = null, candidate_value = null,
                    candidate_token_version = null, candidate_issued_at = null,
                    candidate_raw_bytes = null, candidate_source = null,
                    candidate_captured_at = null, candidate_reuse_deadline = null,
                    capture_requested_at = null,
                    capture_not_before = null,
                    config_revision = config_revision + 1, updated_at = now()
              where account_id = $1 and identity_revision = $2 and effective_model = $3
                and config_revision = $10",
        )
        .bind(&scope.account_id)
        .bind(i64::try_from(scope.identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
        .bind(&scope.effective_model)
        .bind(&observation.value)
        .bind(metadata.token_version.map(i16::from))
        .bind(metadata.issued_at)
        .bind(
            metadata
                .raw_bytes
                .and_then(|value| i32::try_from(value).ok()),
        )
        .bind(observation.observed_at)
        .bind(reuse_deadline)
        .bind(i64::try_from(config_revision).map_err(|_| TurnStateStoreError::Invalid)?)
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        if updated.rows_affected() != 1 {
            tx.rollback().await.map_err(unavailable)?;
            return Ok(());
        }
        let view = model_view_in(
            &mut tx,
            &scope.account_id,
            &scope.effective_model,
            scope.identity_revision,
            &scope.effective_model,
        )
        .await?;
        tx.commit().await.map_err(unavailable)?;
        publish_cached_model_snapshot(model_pins, &view)?;
    } else if capture_enabled && encoded_bytes != MODEL_TURN_STATE_BYTES {
        let updated = sqlx::query(
            "update openai_model_turn_states
                set capture_requested_at = coalesce(capture_requested_at, $4), updated_at = now()
              where account_id = $1 and identity_revision = $2 and effective_model = $3
                and config_revision = $5",
        )
        .bind(&scope.account_id)
        .bind(i64::try_from(scope.identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
        .bind(&scope.effective_model)
        .bind(observation.observed_at)
        .bind(i64::try_from(config_revision).map_err(|_| TurnStateStoreError::Invalid)?)
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        if updated.rows_affected() != 1 {
            tx.rollback().await.map_err(unavailable)?;
            return Ok(());
        }
        let view = model_view_in(
            &mut tx,
            &scope.account_id,
            &scope.effective_model,
            scope.identity_revision,
            &scope.effective_model,
        )
        .await?;
        tx.commit().await.map_err(unavailable)?;
        publish_cached_model_snapshot(model_pins, &view)?;
    } else {
        // 292B 但不可打印的值只作为可疑观测保留；自动捕获关闭时，
        // 其他长度同样只保留请求观测，不能覆盖当前模型锁或请求住宅代理。
        tx.rollback().await.map_err(unavailable)?;
    }
    Ok(())
}

async fn persist_request_observation(
    pool: &PgPool,
    request_id: &str,
    attempt_index: u32,
    observation: &TurnStateObservation,
) -> Result<()> {
    let attempt_index = i32::try_from(attempt_index).map_err(|_| TurnStateStoreError::Invalid)?;
    // 执行观测和账号观测是两个异步队列：此处不依赖请求行先落库。
    // 历史保留期在写入时再次检查，避免迟到写入重新生成已清理的敏感原值。
    sqlx::query(
        "insert into request_turn_state_observations
           (request_id, attempt_index, observation_id, value, observed_at, source,
            upstream_response_id)
         select $1, $2, $3, $4, $5, $6, $7
          from runtime_settings settings
          where settings.id = 1
            and (
              exists (
                select 1 from model_requests request
                 where request.id = $1
                   and (request.outcome = 'running'
                        or request.completed_at >= now()
                           - (settings.usage_retention_days * interval '1 day'))
              )
              or (
                not exists (select 1 from model_requests request where request.id = $1)
                and $5 >= now() - (settings.usage_retention_days * interval '1 day')
              )
            )
         on conflict (request_id, attempt_index) do update
           set changed = request_turn_state_observations.changed
                         or (request_turn_state_observations.value is not null
                             and request_turn_state_observations.value is distinct from excluded.value),
               observation_id = case when request_turn_state_observations.observation_id is null
                                      or (excluded.observed_at, excluded.observation_id)
                                     >= (request_turn_state_observations.observed_at,
                                         request_turn_state_observations.observation_id)
                                     then excluded.observation_id
                                     else request_turn_state_observations.observation_id end,
               value = case when request_turn_state_observations.observation_id is null
                              or (excluded.observed_at, excluded.observation_id)
                              >= (request_turn_state_observations.observed_at,
                                  request_turn_state_observations.observation_id)
                              then excluded.value else request_turn_state_observations.value end,
               observed_at = case when request_turn_state_observations.observed_at is null
                                  then excluded.observed_at
                                  else greatest(request_turn_state_observations.observed_at,
                                                excluded.observed_at) end,
               source = case when request_turn_state_observations.observation_id is null
                               or (excluded.observed_at, excluded.observation_id)
                               >= (request_turn_state_observations.observed_at,
                                   request_turn_state_observations.observation_id)
                               then excluded.source else request_turn_state_observations.source end,
               upstream_response_id = case
                 when request_turn_state_observations.observation_id is null then excluded.upstream_response_id
                 when excluded.observation_id = request_turn_state_observations.observation_id
                   then coalesce(excluded.upstream_response_id,
                                 request_turn_state_observations.upstream_response_id)
                 when (excluded.observed_at, excluded.observation_id)
                       > (request_turn_state_observations.observed_at,
                          request_turn_state_observations.observation_id)
                   then excluded.upstream_response_id
                 else request_turn_state_observations.upstream_response_id end",
    )
    .bind(request_id)
    .bind(attempt_index)
    .bind(&observation.id)
    .bind(&observation.value)
    .bind(observation.observed_at)
    .bind(&observation.transport)
    .bind(observation.upstream_response_id.as_deref())
    .execute(pool)
    .await
    .map_err(unavailable)?;
    Ok(())
}

type Result<T> = std::result::Result<T, TurnStateStoreError>;

fn startup_unavailable(error: sqlx::Error) -> StoreError {
    StoreError::Unavailable {
        backend: StoreBackend::PostgreSql,
        message: format!("hydrate OpenAI turn state overrides: {error}"),
    }
}

fn unavailable(_: sqlx::Error) -> TurnStateStoreError {
    TurnStateStoreError::Unavailable
}

fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

async fn ensure_openai(tx: &mut Transaction<'_, Postgres>, account_id: &str) -> Result<u64> {
    let account: Option<(String, i64)> = sqlx::query_as(
        "select provider_kind, identity_revision from provider_accounts where id = $1",
    )
    .bind(account_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(unavailable)?;
    match account {
        Some((kind, revision)) if kind == "openai" => {
            u64::try_from(revision).map_err(|_| TurnStateStoreError::Unavailable)
        }
        _ => Err(TurnStateStoreError::NotFound),
    }
}

async fn ensure_row(tx: &mut Transaction<'_, Postgres>, account_id: &str) -> Result<()> {
    sqlx::query("insert into openai_turn_states(account_id) values ($1) on conflict do nothing")
        .bind(account_id)
        .execute(&mut **tx)
        .await
        .map_err(unavailable)?;
    Ok(())
}

async fn view_in(tx: &mut Transaction<'_, Postgres>, account_id: &str) -> Result<TurnStateView> {
    let row = sqlx::query("select enabled, override_value, override_updated_at, config_revision, observed_id, observed_value, observed_at, observed_transport, observed_upstream_response_id, observed_client_turn_id from openai_turn_states where account_id = $1 for update")
        .bind(account_id).fetch_one(&mut **tx).await.map_err(unavailable)?;
    let override_value: Option<String> = row.get("override_value");
    let observed_value: Option<String> = row.get("observed_value");
    let observed = observed_value.map(|value| TurnStateObserved {
        id: row.get::<String, _>("observed_id"),
        bytes: value.len(),
        sha256: digest(&value),
        value,
        observed_at: row.get::<DateTime<Utc>, _>("observed_at"),
        transport: row.get::<String, _>("observed_transport"),
        upstream_response_id: row.get("observed_upstream_response_id"),
        client_turn_id: row.get("observed_client_turn_id"),
    });
    Ok(TurnStateView {
        account_id: account_id.to_owned(),
        observed,
        override_state: TurnStateOverride {
            enabled: row.get("enabled"),
            bytes: override_value.as_ref().map_or(0, String::len),
            sha256: override_value.as_deref().map(digest),
            value: override_value,
            updated_at: row.get("override_updated_at"),
        },
        config_revision: u64::try_from(row.get::<i64, _>("config_revision"))
            .map_err(|_| TurnStateStoreError::Unavailable)?,
    })
}

async fn current_model_scope(
    tx: &mut Transaction<'_, Postgres>,
    account_id: &str,
    requested_model: &str,
) -> Result<(u64, String)> {
    if requested_model.trim().is_empty()
        || requested_model.len() > 512
        || requested_model.chars().any(char::is_control)
    {
        return Err(TurnStateStoreError::Invalid);
    }
    let revision: Option<i64> = sqlx::query_scalar(
        "select identity_revision from provider_accounts
          where id = $1 and provider_kind = 'openai' for share",
    )
    .bind(account_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(unavailable)?;
    let revision = revision.ok_or(TurnStateStoreError::NotFound)?;
    let mappings: sqlx::types::Json<BTreeMap<String, String>> =
        sqlx::query_scalar("select model_mappings_json from runtime_settings where id = 1")
            .fetch_one(&mut **tx)
            .await
            .map_err(unavailable)?;
    Ok((
        u64::try_from(revision).map_err(|_| TurnStateStoreError::Unavailable)?,
        resolve_model_mapping(&mappings.0, requested_model),
    ))
}

async fn ensure_model_row(
    tx: &mut Transaction<'_, Postgres>,
    account_id: &str,
    identity_revision: u64,
    effective_model: &str,
) -> Result<()> {
    sqlx::query(
        "insert into openai_model_turn_states(account_id, identity_revision, effective_model)
         values ($1, $2, $3) on conflict do nothing",
    )
    .bind(account_id)
    .bind(i64::try_from(identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
    .bind(effective_model)
    .execute(&mut **tx)
    .await
    .map_err(unavailable)?;
    Ok(())
}

async fn ensure_account_policy_row(
    tx: &mut Transaction<'_, Postgres>,
    account_id: &str,
    identity_revision: u64,
) -> Result<()> {
    sqlx::query(
        "insert into openai_account_turn_state_policies(account_id, identity_revision)
         values ($1, $2) on conflict do nothing",
    )
    .bind(account_id)
    .bind(i64::try_from(identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
    .execute(&mut **tx)
    .await
    .map_err(unavailable)?;
    Ok(())
}

async fn promote_due_candidate(
    tx: &mut Transaction<'_, Postgres>,
    account_id: &str,
    identity_revision: u64,
    effective_model: &str,
) -> Result<()> {
    sqlx::query(
        "update openai_model_turn_states s
            set pin_value = s.candidate_value,
                pin_token_version = s.candidate_token_version,
                pin_issued_at = s.candidate_issued_at,
                pin_raw_bytes = s.candidate_raw_bytes,
                pin_source = s.candidate_source, pin_compatible_transport = 'http',
                pin_captured_at = s.candidate_captured_at,
                pin_reuse_deadline = s.candidate_reuse_deadline,
                pin_invalidated_at = null, active_activated_at = now(),
                active_candidate_id = s.candidate_id,
                active_generation = s.active_generation + 1,
                candidate_id = null, candidate_value = null,
                candidate_token_version = null, candidate_issued_at = null,
                candidate_raw_bytes = null, candidate_source = null,
                candidate_captured_at = null, candidate_reuse_deadline = null,
                config_revision = s.config_revision + 1, updated_at = now()
           from openai_account_turn_state_policies p
          where s.account_id = $1 and s.identity_revision = $2 and s.effective_model = $3
            and p.account_id = s.account_id and p.identity_revision = s.identity_revision
            and s.candidate_value is not null
            and s.candidate_reuse_deadline > now()
            and (s.pin_invalidated_at is not null or s.pin_value is null
              or least(s.pin_reuse_deadline,
                    s.pin_captured_at + p.reuse_window_seconds * interval '1 second') <= now())
            ",
    )
    .bind(account_id)
    .bind(i64::try_from(identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
    .bind(effective_model)
    .execute(&mut **tx)
    .await
    .map_err(unavailable)?;
    Ok(())
}

async fn account_policy_view_in(
    tx: &mut Transaction<'_, Postgres>,
    account_id: &str,
    identity_revision: u64,
) -> Result<AccountTurnStatePolicyView> {
    let row = sqlx::query(
        "select lock_enabled, capture_enabled, reuse_window_seconds, refresh_lead_seconds,
                capture_proxy_id,
                max_attempts, attempt_timeout_seconds, job_timeout_seconds, backoff_seconds,
                max_backoff_seconds, cooldown_seconds, config_revision
           from openai_account_turn_state_policies
          where account_id = $1 and identity_revision = $2
          for update",
    )
    .bind(account_id)
    .bind(i64::try_from(identity_revision).map_err(|_| TurnStateStoreError::Unavailable)?)
    .fetch_one(&mut **tx)
    .await
    .map_err(unavailable)?;
    Ok(AccountTurnStatePolicyView {
        account_id: account_id.to_owned(),
        identity_revision,
        config_revision: u64::try_from(row.get::<i64, _>("config_revision"))
            .map_err(|_| TurnStateStoreError::Unavailable)?,
        lock_enabled: row.get("lock_enabled"),
        capture_enabled: row.get("capture_enabled"),
        reuse_window_seconds: u32::try_from(row.get::<i32, _>("reuse_window_seconds"))
            .map_err(|_| TurnStateStoreError::Unavailable)?,
        refresh_lead_seconds: u32::try_from(row.get::<i32, _>("refresh_lead_seconds"))
            .map_err(|_| TurnStateStoreError::Unavailable)?,
        capture_proxy_id: row.get("capture_proxy_id"),
        capture_proxy: None,
        capture_policy: ModelTurnStateCapturePolicy {
            max_attempts: u8::try_from(row.get::<i16, _>("max_attempts"))
                .map_err(|_| TurnStateStoreError::Unavailable)?,
            attempt_timeout_seconds: u16::try_from(row.get::<i16, _>("attempt_timeout_seconds"))
                .map_err(|_| TurnStateStoreError::Unavailable)?,
            job_timeout_seconds: u16::try_from(row.get::<i16, _>("job_timeout_seconds"))
                .map_err(|_| TurnStateStoreError::Unavailable)?,
            backoff_seconds: u8::try_from(row.get::<i16, _>("backoff_seconds"))
                .map_err(|_| TurnStateStoreError::Unavailable)?,
            max_backoff_seconds: u8::try_from(row.get::<i16, _>("max_backoff_seconds"))
                .map_err(|_| TurnStateStoreError::Unavailable)?,
            cooldown_seconds: u32::try_from(row.get::<i32, _>("cooldown_seconds"))
                .map_err(|_| TurnStateStoreError::Unavailable)?,
        },
    })
}

async fn model_view_in(
    tx: &mut Transaction<'_, Postgres>,
    account_id: &str,
    requested_model: &str,
    identity_revision: u64,
    effective_model: &str,
) -> Result<ModelTurnStateView> {
    let row = sqlx::query(
        "select p.lock_enabled, p.capture_enabled, p.reuse_window_seconds,
                p.refresh_lead_seconds,
                p.capture_proxy_id, p.max_attempts, p.attempt_timeout_seconds,
                p.job_timeout_seconds, p.backoff_seconds, p.max_backoff_seconds,
                p.cooldown_seconds, p.config_revision as policy_revision,
                s.pin_value, s.pin_token_version, s.pin_issued_at, s.pin_raw_bytes,
                s.pin_source, s.pin_compatible_transport, s.pin_captured_at,
                s.pin_reuse_deadline, s.pin_invalidated_at, s.active_generation,
                s.active_candidate_id,
                s.candidate_id, s.candidate_value, s.candidate_token_version,
                s.candidate_issued_at, s.candidate_raw_bytes, s.candidate_source,
                s.candidate_captured_at, s.candidate_reuse_deadline,
                s.capture_requested_at, s.capture_not_before,
                s.config_revision as model_revision
           from openai_model_turn_states s
           join openai_account_turn_state_policies p
             on p.account_id = s.account_id and p.identity_revision = s.identity_revision
          where s.account_id = $1 and s.identity_revision = $2 and s.effective_model = $3
          for update",
    )
    .bind(account_id)
    .bind(i64::try_from(identity_revision).map_err(|_| TurnStateStoreError::Unavailable)?)
    .bind(effective_model)
    .fetch_one(&mut **tx)
    .await
    .map_err(unavailable)?;
    let value: Option<String> = row.get("pin_value");
    let reuse_window_seconds = u32::try_from(row.get::<i32, _>("reuse_window_seconds"))
        .map_err(|_| TurnStateStoreError::Unavailable)?;
    let pin = value.map(|value| {
        let raw_bytes = row
            .get::<Option<i32>, _>("pin_raw_bytes")
            .and_then(|value| usize::try_from(value).ok());
        let token_version = row
            .get::<Option<i16>, _>("pin_token_version")
            .and_then(|value| u8::try_from(value).ok());
        let captured_at: DateTime<Utc> = row.get("pin_captured_at");
        let stored_deadline: DateTime<Utc> = row.get("pin_reuse_deadline");
        ModelTurnStatePin {
            encoded_bytes: value.len(),
            raw_bytes,
            ciphertext_bytes: raw_bytes.and_then(|bytes| bytes.checked_sub(57)),
            envelope_format: (token_version == Some(0x80)
                && raw_bytes.is_some_and(|bytes| bytes >= 73 && (bytes - 57).is_multiple_of(16)))
            .then_some("fernet_v0x80_candidate"),
            token_version,
            issued_at: row.get("pin_issued_at"),
            timestamp_verified: false,
            sha256: digest(&value),
            value,
            captured_at,
            reuse_deadline: stored_deadline
                .min(model_reuse_deadline(captured_at, reuse_window_seconds)),
            source: row.get("pin_source"),
            compatible_transport: row.get("pin_compatible_transport"),
            invalidated: row
                .get::<Option<DateTime<Utc>>, _>("pin_invalidated_at")
                .is_some(),
            generation: u64::try_from(row.get::<i64, _>("active_generation")).unwrap_or(1),
            id: row.get("active_candidate_id"),
        }
    });
    let candidate_value: Option<String> = row.get("candidate_value");
    let candidate = candidate_value.map(|value| {
        let raw_bytes = row
            .get::<Option<i32>, _>("candidate_raw_bytes")
            .and_then(|value| usize::try_from(value).ok());
        let token_version = row
            .get::<Option<i16>, _>("candidate_token_version")
            .and_then(|value| u8::try_from(value).ok());
        ModelTurnStatePin {
            encoded_bytes: value.len(),
            raw_bytes,
            ciphertext_bytes: raw_bytes.and_then(|bytes| bytes.checked_sub(57)),
            envelope_format: (token_version == Some(0x80)
                && raw_bytes.is_some_and(|bytes| bytes >= 73 && (bytes - 57).is_multiple_of(16)))
            .then_some("fernet_v0x80_candidate"),
            token_version,
            issued_at: row.get("candidate_issued_at"),
            timestamp_verified: false,
            sha256: digest(&value),
            value,
            captured_at: row.get("candidate_captured_at"),
            reuse_deadline: row.get("candidate_reuse_deadline"),
            source: row.get("candidate_source"),
            compatible_transport: "http".to_owned(),
            invalidated: false,
            generation: u64::try_from(row.get::<i64, _>("active_generation"))
                .unwrap_or(1)
                .saturating_add(1),
            id: row.get("candidate_id"),
        }
    });
    let legacy: (bool, Option<String>) = sqlx::query_as(
        "select coalesce((select enabled from openai_turn_states where account_id = $1), false),
                (select override_value from openai_turn_states where account_id = $1)",
    )
    .bind(account_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(unavailable)?;
    let refresh_lead_seconds = u32::try_from(row.get::<i32, _>("refresh_lead_seconds"))
        .map_err(|_| TurnStateStoreError::Unavailable)?;
    let capture_enabled: bool = row.get("capture_enabled");
    let capture_proxy_id: Option<String> = row.get("capture_proxy_id");
    let capture_requested_at: Option<DateTime<Utc>> = row.get("capture_requested_at");
    let capture_not_before: Option<DateTime<Utc>> = row.get("capture_not_before");
    let next_capture_at = (capture_enabled && candidate.is_none())
        .then(|| {
            pin.as_ref().and_then(|pin| {
                pin.reuse_deadline
                    .checked_sub_signed(chrono::Duration::seconds(i64::from(
                        refresh_lead_seconds.min(reuse_window_seconds.saturating_sub(1)),
                    )))
            })
        })
        .flatten();
    let next_activation_at = candidate
        .as_ref()
        .and_then(|_| pin.as_ref().map(|pin| pin.reuse_deadline));
    let waiting_reason = if !capture_enabled {
        Some("disabled".to_owned())
    } else if candidate.is_some() {
        Some("candidate_ready".to_owned())
    } else if capture_proxy_id.is_none() {
        Some("waiting_proxy".to_owned())
    } else if capture_not_before.is_some_and(|not_before| not_before > Utc::now()) {
        Some("cooldown".to_owned())
    } else if capture_requested_at.is_some() {
        Some("queued".to_owned())
    } else if pin.is_some() {
        Some("scheduled".to_owned())
    } else {
        Some("waiting_normal_observation".to_owned())
    };
    Ok(ModelTurnStateView {
        account_id: account_id.to_owned(),
        requested_model: requested_model.to_owned(),
        effective_model: effective_model.to_owned(),
        identity_revision,
        config_revision: u64::try_from(row.get::<i64, _>("model_revision"))
            .map_err(|_| TurnStateStoreError::Unavailable)?,
        policy_revision: u64::try_from(row.get::<i64, _>("policy_revision"))
            .map_err(|_| TurnStateStoreError::Unavailable)?,
        lock_enabled: row.get("lock_enabled"),
        capture_enabled,
        reuse_window_seconds: u32::try_from(row.get::<i32, _>("reuse_window_seconds"))
            .map_err(|_| TurnStateStoreError::Unavailable)?,
        refresh_lead_seconds,
        capture_proxy_id,
        capture_proxy: None,
        capture_policy: ModelTurnStateCapturePolicy {
            max_attempts: u8::try_from(row.get::<i16, _>("max_attempts"))
                .map_err(|_| TurnStateStoreError::Unavailable)?,
            attempt_timeout_seconds: u16::try_from(row.get::<i16, _>("attempt_timeout_seconds"))
                .map_err(|_| TurnStateStoreError::Unavailable)?,
            job_timeout_seconds: u16::try_from(row.get::<i16, _>("job_timeout_seconds"))
                .map_err(|_| TurnStateStoreError::Unavailable)?,
            backoff_seconds: u8::try_from(row.get::<i16, _>("backoff_seconds"))
                .map_err(|_| TurnStateStoreError::Unavailable)?,
            max_backoff_seconds: u8::try_from(row.get::<i16, _>("max_backoff_seconds"))
                .map_err(|_| TurnStateStoreError::Unavailable)?,
            cooldown_seconds: u32::try_from(row.get::<i32, _>("cooldown_seconds"))
                .map_err(|_| TurnStateStoreError::Unavailable)?,
        },
        pin,
        next_capture_at,
        next_activation_at,
        capture_not_before,
        waiting_reason,
        candidate,
        legacy_override_enabled: legacy.0,
        legacy_override_configured: legacy.1.is_some(),
        legacy_override_value: legacy.1,
    })
}

fn model_reuse_deadline(captured_at: DateTime<Utc>, reuse_window_seconds: u32) -> DateTime<Utc> {
    captured_at + chrono::Duration::seconds(i64::from(reuse_window_seconds))
}

#[async_trait]
impl TurnStateStore for PgTurnStateStore {
    async fn load(&self, account_id: &str) -> Result<TurnStateView> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        ensure_openai(&mut tx, account_id).await?;
        ensure_row(&mut tx, account_id).await?;
        let view = view_in(&mut tx, account_id).await?;
        tx.commit().await.map_err(unavailable)?;
        Ok(view)
    }

    async fn update(
        &self,
        account_id: &str,
        enabled: bool,
        value: Option<Option<String>>,
        expected_revision: u64,
    ) -> Result<TurnStateView> {
        let clear = matches!(value, Some(None));
        if value
            .as_ref()
            .and_then(Option::as_ref)
            .is_some_and(|value| !valid_turn_state_override(value))
            || expected_revision == 0
        {
            return Err(TurnStateStoreError::Invalid);
        }
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        ensure_openai(&mut tx, account_id).await?;
        ensure_row(&mut tx, account_id).await?;
        let old = view_in(&mut tx, account_id).await?;
        if old.config_revision != expected_revision {
            return Err(TurnStateStoreError::Conflict);
        }
        let next = value.unwrap_or(old.override_state.value);
        let enabled = !clear && enabled;
        if enabled
            && next
                .as_deref()
                .is_none_or(|value| !valid_turn_state_override(value))
        {
            return Err(TurnStateStoreError::Invalid);
        }
        sqlx::query("update openai_turn_states set enabled = $2, override_value = $3, override_updated_at = now(), config_revision = config_revision + 1 where account_id = $1")
            .bind(account_id).bind(enabled).bind(next).execute(&mut *tx).await.map_err(unavailable)?;
        let view = view_in(&mut tx, account_id).await?;
        tx.commit().await.map_err(unavailable)?;
        self.publish_cached_override(&view)?;
        Ok(view)
    }

    async fn use_observed(
        &self,
        account_id: &str,
        observation_id: &str,
        enabled: bool,
        expected_revision: u64,
    ) -> Result<TurnStateView> {
        if observation_id.is_empty() || expected_revision == 0 {
            return Err(TurnStateStoreError::Invalid);
        }
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        ensure_openai(&mut tx, account_id).await?;
        ensure_row(&mut tx, account_id).await?;
        let row = sqlx::query("select observed_id, observed_value, config_revision from openai_turn_states where account_id = $1 for update")
            .bind(account_id).fetch_one(&mut *tx).await.map_err(unavailable)?;
        let revision = u64::try_from(row.get::<i64, _>("config_revision"))
            .map_err(|_| TurnStateStoreError::Unavailable)?;
        let current_id: Option<String> = row.get("observed_id");
        let value: Option<String> = row.get("observed_value");
        if revision != expected_revision || current_id.as_deref() != Some(observation_id) {
            return Err(TurnStateStoreError::Conflict);
        }
        let value = value.ok_or(TurnStateStoreError::Conflict)?;
        if !valid_turn_state_override(&value) {
            return Err(TurnStateStoreError::Invalid);
        }
        sqlx::query("update openai_turn_states set enabled = $2, override_value = $3, override_updated_at = now(), config_revision = config_revision + 1 where account_id = $1")
            .bind(account_id).bind(enabled).bind(value).execute(&mut *tx).await.map_err(unavailable)?;
        let view = view_in(&mut tx, account_id).await?;
        tx.commit().await.map_err(unavailable)?;
        self.publish_cached_override(&view)?;
        Ok(view)
    }

    fn enqueue_observation(&self, observation: TurnStateObservation) {
        let account_id = observation.account_id.clone();
        let observation_id = observation.id.clone();
        if let Err(error) = self
            .observations
            .try_send(TurnStateWrite::Returned(observation))
        {
            let reason = match error {
                mpsc::error::TrySendError::Full(_) => "full",
                mpsc::error::TrySendError::Closed(_) => "closed",
            };
            tracing::warn!(
                account_id,
                observation_id,
                reason,
                "OpenAI turn state observation queue dropped an item"
            );
        }
    }

    fn enqueue_sent(&self, sent: TurnStateSent) {
        let request_id = sent.request_id.clone();
        let attempt_index = sent.attempt_index;
        if let Err(error) = self.observations.try_send(TurnStateWrite::Sent(sent)) {
            let reason = match error {
                mpsc::error::TrySendError::Full(_) => "full",
                mpsc::error::TrySendError::Closed(_) => "closed",
            };
            tracing::warn!(
                request_id,
                attempt_index,
                reason,
                "OpenAI sent turn state receipt queue dropped an item"
            );
        }
    }

    fn active_override(&self, account_id: &str) -> Option<String> {
        self.overrides
            .read()
            .ok()
            .and_then(|overrides| overrides.get(account_id)?.value.clone())
    }

    async fn load_account_policy(&self, account_id: &str) -> Result<AccountTurnStatePolicyView> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let identity_revision = ensure_openai(&mut tx, account_id).await?;
        ensure_account_policy_row(&mut tx, account_id, identity_revision).await?;
        let view = account_policy_view_in(&mut tx, account_id, identity_revision).await?;
        tx.commit().await.map_err(unavailable)?;
        self.publish_cached_policy(&view)?;
        Ok(view)
    }

    async fn update_account_policy(
        &self,
        account_id: &str,
        update: AccountTurnStatePolicyUpdate,
    ) -> Result<AccountTurnStatePolicyView> {
        if update.expected_revision == 0
            || !(1..=86_400).contains(&update.reuse_window_seconds)
            || update.refresh_lead_seconds > 86_400
            || !(1..=10).contains(&update.max_attempts)
            || !(1..=60).contains(&update.attempt_timeout_seconds)
            || !(1..=300).contains(&update.job_timeout_seconds)
            || update.max_backoff_seconds > 60
            || update.cooldown_seconds > 86_400
        {
            return Err(TurnStateStoreError::Invalid);
        }
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let identity_revision = ensure_openai(&mut tx, account_id).await?;
        if identity_revision != update.expected_identity_revision {
            return Err(TurnStateStoreError::Conflict);
        }
        ensure_account_policy_row(&mut tx, account_id, identity_revision).await?;
        let old = account_policy_view_in(&mut tx, account_id, identity_revision).await?;
        if old.config_revision != update.expected_revision {
            return Err(TurnStateStoreError::Conflict);
        }
        sqlx::query(
            "update openai_account_turn_state_policies
                set lock_enabled = $3, capture_enabled = $4, reuse_window_seconds = $5,
                    refresh_lead_seconds = $6, capture_proxy_id = $7, max_attempts = $8,
                    attempt_timeout_seconds = $9, job_timeout_seconds = $10,
                    backoff_seconds = $11, max_backoff_seconds = $12,
                    cooldown_seconds = $13, config_revision = config_revision + 1,
                    updated_at = now()
              where account_id = $1 and identity_revision = $2",
        )
        .bind(account_id)
        .bind(i64::try_from(identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
        .bind(update.lock_enabled)
        .bind(update.capture_enabled)
        .bind(i32::try_from(update.reuse_window_seconds).map_err(|_| TurnStateStoreError::Invalid)?)
        .bind(i32::try_from(update.refresh_lead_seconds).map_err(|_| TurnStateStoreError::Invalid)?)
        .bind(update.capture_proxy_id.as_deref())
        .bind(i16::from(update.max_attempts))
        .bind(
            i16::try_from(update.attempt_timeout_seconds)
                .map_err(|_| TurnStateStoreError::Invalid)?,
        )
        .bind(i16::try_from(update.job_timeout_seconds).map_err(|_| TurnStateStoreError::Invalid)?)
        .bind(i16::from(update.backoff_seconds))
        .bind(i16::from(update.max_backoff_seconds))
        .bind(i32::try_from(update.cooldown_seconds).map_err(|_| TurnStateStoreError::Invalid)?)
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        let view = account_policy_view_in(&mut tx, account_id, identity_revision).await?;
        tx.commit().await.map_err(unavailable)?;
        self.publish_cached_policy(&view)?;
        Ok(view)
    }

    async fn load_model_state(
        &self,
        account_id: &str,
        requested_model: &str,
    ) -> Result<ModelTurnStateView> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let (identity_revision, effective_model) =
            current_model_scope(&mut tx, account_id, requested_model).await?;
        ensure_account_policy_row(&mut tx, account_id, identity_revision).await?;
        ensure_model_row(&mut tx, account_id, identity_revision, &effective_model).await?;
        promote_due_candidate(&mut tx, account_id, identity_revision, &effective_model).await?;
        let view = model_view_in(
            &mut tx,
            account_id,
            requested_model,
            identity_revision,
            &effective_model,
        )
        .await?;
        tx.commit().await.map_err(unavailable)?;
        self.publish_cached_policy_from_model(&view)?;
        self.publish_cached_model(&view)?;
        Ok(view)
    }

    async fn update_model_state(
        &self,
        account_id: &str,
        requested_model: &str,
        update: ModelTurnStateUpdate,
    ) -> Result<ModelTurnStateView> {
        if update.expected_revision == 0
            || update
                .value
                .as_deref()
                .is_some_and(|value| !valid_turn_state_override(value))
            || matches!(update.pin_action, ModelTurnStatePinAction::Replace)
                && update.value.is_none()
            || !matches!(update.pin_action, ModelTurnStatePinAction::Replace)
                && update.value.is_some()
        {
            return Err(TurnStateStoreError::Invalid);
        }
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let (identity_revision, effective_model) =
            current_model_scope(&mut tx, account_id, requested_model).await?;
        if identity_revision != update.expected_identity_revision
            || effective_model != update.expected_effective_model
        {
            return Err(TurnStateStoreError::Conflict);
        }
        ensure_account_policy_row(&mut tx, account_id, identity_revision).await?;
        ensure_model_row(&mut tx, account_id, identity_revision, &effective_model).await?;
        let old = model_view_in(
            &mut tx,
            account_id,
            requested_model,
            identity_revision,
            &effective_model,
        )
        .await?;
        if old.config_revision != update.expected_revision {
            return Err(TurnStateStoreError::Conflict);
        }
        if matches!(update.pin_action, ModelTurnStatePinAction::Keep) {
            tx.commit().await.map_err(unavailable)?;
            return Ok(old);
        }
        let now = Utc::now();
        let manual_value = match update.pin_action {
            ModelTurnStatePinAction::Replace => update.value.clone(),
            ModelTurnStatePinAction::ImportLegacy => sqlx::query_scalar::<_, Option<String>>(
                "select override_value from openai_turn_states where account_id = $1",
            )
            .bind(account_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(unavailable)?
            .flatten(),
            _ => None,
        };
        if matches!(update.pin_action, ModelTurnStatePinAction::ImportLegacy)
            && manual_value
                .as_deref()
                .is_none_or(|value| !valid_turn_state_override(value))
        {
            return Err(TurnStateStoreError::Invalid);
        }
        match update.pin_action {
            ModelTurnStatePinAction::Replace | ModelTurnStatePinAction::ImportLegacy => {
                let value = manual_value
                    .as_deref()
                    .ok_or(TurnStateStoreError::Invalid)?;
                let metadata = model_turn_state_token_metadata(value);
                sqlx::query(
                    "update openai_model_turn_states
                        set pin_value = $4, pin_token_version = $5, pin_issued_at = $6,
                            pin_raw_bytes = $7, pin_source = 'manual',
                            pin_compatible_transport = 'http', pin_captured_at = $8,
                            pin_reuse_deadline = $9, pin_invalidated_at = null,
                            active_activated_at = $8, active_candidate_id = null,
                            active_generation = active_generation + 1,
                            candidate_id = null, candidate_value = null,
                            candidate_token_version = null, candidate_issued_at = null,
                            candidate_raw_bytes = null, candidate_source = null,
                            candidate_captured_at = null, candidate_reuse_deadline = null,
                            capture_requested_at = null, capture_not_before = null,
                            rejected_value_sha256 = null,
                            config_revision = config_revision + 1, updated_at = now()
                      where account_id = $1 and identity_revision = $2 and effective_model = $3",
                )
                .bind(account_id)
                .bind(i64::try_from(identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
                .bind(&effective_model)
                .bind(value)
                .bind(metadata.token_version.map(i16::from))
                .bind(metadata.issued_at)
                .bind(
                    metadata
                        .raw_bytes
                        .and_then(|value| i32::try_from(value).ok()),
                )
                .bind(now)
                .bind(model_reuse_deadline(now, old.reuse_window_seconds))
                .execute(&mut *tx)
                .await
                .map_err(unavailable)?;
            }
            ModelTurnStatePinAction::Clear => {
                sqlx::query(
                    "update openai_model_turn_states
                        set pin_value = null, pin_token_version = null, pin_issued_at = null,
                            pin_raw_bytes = null, pin_source = null, pin_compatible_transport = null,
                            pin_captured_at = null, pin_reuse_deadline = null,
                            pin_invalidated_at = null, active_activated_at = null,
                            active_candidate_id = null,
                            active_generation = active_generation + 1,
                            candidate_id = null, candidate_value = null,
                            candidate_token_version = null, candidate_issued_at = null,
                            candidate_raw_bytes = null, candidate_source = null,
                            candidate_captured_at = null, candidate_reuse_deadline = null,
                            capture_requested_at = null, capture_not_before = null,
                            rejected_value_sha256 = null,
                            config_revision = config_revision + 1, updated_at = now()
                      where account_id = $1 and identity_revision = $2 and effective_model = $3",
                )
                .bind(account_id)
                .bind(i64::try_from(identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
                .bind(&effective_model)
                .execute(&mut *tx)
                .await
                .map_err(unavailable)?;
            }
            ModelTurnStatePinAction::Invalidate => {
                let rejected = old.pin.as_ref().map(|pin| pin.sha256.clone());
                let promote = old.candidate.as_ref().is_some_and(|candidate| {
                    candidate.reuse_deadline > now
                        && old
                            .pin
                            .as_ref()
                            .is_none_or(|pin| pin.value != candidate.value)
                        && rejected.as_deref() != Some(candidate.sha256.as_str())
                });
                if promote {
                    sqlx::query(
                        "update openai_model_turn_states
                            set pin_value = candidate_value,
                                pin_token_version = candidate_token_version,
                                pin_issued_at = candidate_issued_at,
                                pin_raw_bytes = candidate_raw_bytes,
                                pin_source = candidate_source,
                                pin_compatible_transport = 'http',
                                pin_captured_at = candidate_captured_at,
                                pin_reuse_deadline = candidate_reuse_deadline,
                                pin_invalidated_at = null, active_activated_at = now(),
                                active_candidate_id = candidate_id,
                                active_generation = active_generation + 1,
                                candidate_id = null, candidate_value = null,
                                candidate_token_version = null, candidate_issued_at = null,
                                candidate_raw_bytes = null, candidate_source = null,
                                candidate_captured_at = null, candidate_reuse_deadline = null,
                                capture_requested_at = null, capture_not_before = null,
                                rejected_value_sha256 = $4,
                                config_revision = config_revision + 1, updated_at = now()
                          where account_id = $1 and identity_revision = $2 and effective_model = $3",
                    )
                    .bind(account_id)
                    .bind(i64::try_from(identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
                    .bind(&effective_model)
                    .bind(rejected.as_deref())
                    .execute(&mut *tx)
                    .await
                    .map_err(unavailable)?;
                } else {
                    sqlx::query(
                        "update openai_model_turn_states
                            set pin_invalidated_at = now(), active_generation = active_generation + 1,
                                candidate_id = null, candidate_value = null,
                                candidate_token_version = null, candidate_issued_at = null,
                                candidate_raw_bytes = null, candidate_source = null,
                                candidate_captured_at = null, candidate_reuse_deadline = null,
                                capture_requested_at = null, capture_not_before = null,
                                rejected_value_sha256 = $4,
                                config_revision = config_revision + 1, updated_at = now()
                          where account_id = $1 and identity_revision = $2 and effective_model = $3",
                    )
                    .bind(account_id)
                    .bind(i64::try_from(identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
                    .bind(&effective_model)
                    .bind(rejected.as_deref())
                    .execute(&mut *tx)
                    .await
                    .map_err(unavailable)?;
                }
            }
            ModelTurnStatePinAction::Keep => unreachable!(),
        }
        let view = model_view_in(
            &mut tx,
            account_id,
            requested_model,
            identity_revision,
            &effective_model,
        )
        .await?;
        tx.commit().await.map_err(unavailable)?;
        self.publish_cached_policy_from_model(&view)?;
        self.publish_cached_model(&view)?;
        Ok(view)
    }

    fn active_model_pin(
        &self,
        account_id: &str,
        identity_revision: u64,
        effective_model: &str,
    ) -> Option<ActiveModelTurnStatePin> {
        let policies = self.account_policies.read().ok()?;
        let policy = policies.get(&AccountPolicyKey {
            account_id: account_id.to_owned(),
            identity_revision,
        })?;
        let pins = self.model_pins.read().ok()?;
        let pin = pins.get(&ModelPinKey {
            account_id: account_id.to_owned(),
            identity_revision,
            effective_model: effective_model.to_owned(),
        })?;
        if !policy.lock_enabled {
            return None;
        }
        let now = Utc::now();
        let active_deadline = pin.reuse_deadline?.min(model_reuse_deadline(
            pin.captured_at?,
            policy.reuse_window_seconds,
        ));
        if !pin.invalidated && active_deadline > now {
            return Some(ActiveModelTurnStatePin {
                value: pin.value.clone()?,
                sha256: digest(pin.value.as_deref()?),
                generation: pin.active_generation,
                candidate_id: pin.active_candidate_id.clone(),
                source: pin.source.clone().unwrap_or_else(|| "manual".to_owned()),
            });
        }
        let candidate_deadline = pin.candidate_reuse_deadline?.min(model_reuse_deadline(
            pin.candidate_captured_at?,
            policy.reuse_window_seconds,
        ));
        if candidate_deadline <= now {
            return None;
        }
        Some(ActiveModelTurnStatePin {
            value: pin.candidate_value.clone()?,
            sha256: digest(pin.candidate_value.as_deref()?),
            generation: pin.active_generation.saturating_add(1),
            candidate_id: pin.candidate_id.clone(),
            source: pin
                .candidate_source
                .clone()
                .unwrap_or_else(|| "capture".to_owned()),
        })
    }

    fn model_observation_scope(
        &self,
        account_id: &str,
        identity_revision: u64,
        effective_model: &str,
    ) -> Option<ModelTurnStateObservationScope> {
        let policies = self.account_policies.read().ok()?;
        let policy = policies.get(&AccountPolicyKey {
            account_id: account_id.to_owned(),
            identity_revision,
        })?;
        let pins = self.model_pins.read().ok()?;
        let pin = pins.get(&ModelPinKey {
            account_id: account_id.to_owned(),
            identity_revision,
            effective_model: effective_model.to_owned(),
        });
        let active = pin.is_some_and(|pin| {
            let active = pin.value.is_some()
                && !pin.invalidated
                && pin
                    .reuse_deadline
                    .zip(pin.captured_at)
                    .is_some_and(|(stored, captured)| {
                        stored.min(model_reuse_deadline(captured, policy.reuse_window_seconds))
                            > Utc::now()
                    });
            let candidate = pin.candidate_value.is_some()
                && pin
                    .candidate_reuse_deadline
                    .zip(pin.candidate_captured_at)
                    .is_some_and(|(stored, captured)| {
                        stored.min(model_reuse_deadline(captured, policy.reuse_window_seconds))
                            > Utc::now()
                    });
            active || candidate
        });
        (!active && (policy.lock_enabled || policy.capture_enabled)).then(|| {
            ModelTurnStateObservationScope {
                account_id: account_id.to_owned(),
                identity_revision,
                effective_model: effective_model.to_owned(),
                // 0 表示快照中尚无这个模型；writer 只能在自己成功插入首行时接受。
                config_revision: pin.map_or(0, |pin| pin.config_revision),
                policy_revision: policy.config_revision,
            }
        })
    }

    async fn maintain_model_turn_state_candidates(&self, limit: u16) -> Result<()> {
        if limit == 0 || limit > 256 {
            return Err(TurnStateStoreError::Invalid);
        }
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let rows = sqlx::query(
            "select s.account_id, s.identity_revision, s.effective_model,
                    s.candidate_reuse_deadline
               from openai_model_turn_states s
               join provider_accounts a on a.id = s.account_id
                 and a.provider_kind = 'openai'
                 and a.identity_revision = s.identity_revision
               join openai_account_turn_state_policies p
                 on p.account_id = s.account_id and p.identity_revision = s.identity_revision
              where s.candidate_value is not null
                and (
                  s.candidate_reuse_deadline <= now()
                  or s.pin_value is null or s.pin_invalidated_at is not null
                  or least(s.pin_reuse_deadline,
                       s.pin_captured_at + p.reuse_window_seconds * interval '1 second') <= now()
                )
              order by s.account_id, s.identity_revision, s.effective_model
              limit $1
              for update of s skip locked",
        )
        .bind(i64::from(limit))
        .fetch_all(&mut *tx)
        .await
        .map_err(unavailable)?;
        let mut views = Vec::with_capacity(rows.len());
        for row in rows {
            let account_id: String = row.get("account_id");
            let identity_revision = u64::try_from(row.get::<i64, _>("identity_revision"))
                .map_err(|_| TurnStateStoreError::Unavailable)?;
            let effective_model: String = row.get("effective_model");
            let candidate_deadline: DateTime<Utc> = row.get("candidate_reuse_deadline");
            if candidate_deadline <= Utc::now() {
                sqlx::query(
                    "update openai_model_turn_states
                        set candidate_id = null, candidate_value = null,
                            candidate_token_version = null, candidate_issued_at = null,
                            candidate_raw_bytes = null, candidate_source = null,
                            candidate_captured_at = null, candidate_reuse_deadline = null,
                            config_revision = config_revision + 1, updated_at = now()
                      where account_id = $1 and identity_revision = $2 and effective_model = $3",
                )
                .bind(&account_id)
                .bind(i64::try_from(identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
                .bind(&effective_model)
                .execute(&mut *tx)
                .await
                .map_err(unavailable)?;
            } else {
                promote_due_candidate(&mut tx, &account_id, identity_revision, &effective_model)
                    .await?;
            }
            views.push(
                model_view_in(
                    &mut tx,
                    &account_id,
                    &effective_model,
                    identity_revision,
                    &effective_model,
                )
                .await?,
            );
        }
        tx.commit().await.map_err(unavailable)?;
        for view in views {
            self.publish_cached_model(&view)?;
        }
        Ok(())
    }

    async fn model_capture_candidates(
        &self,
        after: Option<&ModelTurnStateCaptureCursor>,
        limit: u16,
    ) -> Result<Vec<ModelTurnStateCaptureScope>> {
        if limit == 0 || limit > 256 {
            return Err(TurnStateStoreError::Invalid);
        }
        let rows = sqlx::query(
            "select s.account_id, s.identity_revision, s.effective_model,
                    s.config_revision, p.config_revision as policy_revision,
                    p.capture_enabled, p.capture_proxy_id, p.max_attempts,
                    p.attempt_timeout_seconds, p.job_timeout_seconds,
                    p.backoff_seconds, p.max_backoff_seconds, p.cooldown_seconds
               from openai_model_turn_states s
               join provider_accounts a on a.id = s.account_id
                 and a.provider_kind = 'openai'
                 and a.identity_revision = s.identity_revision
               join openai_account_turn_state_policies p
                 on p.account_id = s.account_id and p.identity_revision = s.identity_revision
               join outbound_proxies op on op.id = p.capture_proxy_id
                 and op.last_test_success = true
                 and op.last_test_at >= now() - interval '24 hours'
               where p.capture_enabled = true
                 and s.candidate_value is null
                 and (s.capture_not_before is null or s.capture_not_before <= now())
                 and (
                   s.capture_requested_at is not null
                   or (
                     s.pin_value is not null and s.pin_invalidated_at is null
                     and least(s.pin_reuse_deadline,
                         s.pin_captured_at + p.reuse_window_seconds * interval '1 second')
                         - least(p.refresh_lead_seconds, p.reuse_window_seconds - 1)
                           * interval '1 second' <= now()
                   )
                 )
                and ($1::text is null or
                     (s.account_id, s.identity_revision, s.effective_model) >
                     ($1, $2::bigint, $3))
              order by s.account_id, s.identity_revision, s.effective_model
              limit $4",
        )
        .bind(after.map(|cursor| cursor.account_id.as_str()))
        .bind(
            after
                .map(|cursor| i64::try_from(cursor.identity_revision))
                .transpose()
                .map_err(|_| TurnStateStoreError::Invalid)?,
        )
        .bind(after.map(|cursor| cursor.effective_model.as_str()))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(unavailable)?;
        rows.into_iter()
            .map(|row| {
                let effective_model: String = row.get("effective_model");
                Ok(ModelTurnStateCaptureScope {
                    account_id: row.get("account_id"),
                    requested_model: effective_model.clone(),
                    effective_model,
                    identity_revision: u64::try_from(row.get::<i64, _>("identity_revision"))
                        .map_err(|_| TurnStateStoreError::Unavailable)?,
                    config_revision: u64::try_from(row.get::<i64, _>("config_revision"))
                        .map_err(|_| TurnStateStoreError::Unavailable)?,
                    policy_revision: u64::try_from(row.get::<i64, _>("policy_revision"))
                        .map_err(|_| TurnStateStoreError::Unavailable)?,
                    capture_enabled: row.get("capture_enabled"),
                    capture_proxy_id: row.get("capture_proxy_id"),
                    capture_policy: ModelTurnStateCapturePolicy {
                        max_attempts: u8::try_from(row.get::<i16, _>("max_attempts"))
                            .map_err(|_| TurnStateStoreError::Unavailable)?,
                        attempt_timeout_seconds: u16::try_from(
                            row.get::<i16, _>("attempt_timeout_seconds"),
                        )
                        .map_err(|_| TurnStateStoreError::Unavailable)?,
                        job_timeout_seconds: u16::try_from(
                            row.get::<i16, _>("job_timeout_seconds"),
                        )
                        .map_err(|_| TurnStateStoreError::Unavailable)?,
                        backoff_seconds: u8::try_from(row.get::<i16, _>("backoff_seconds"))
                            .map_err(|_| TurnStateStoreError::Unavailable)?,
                        max_backoff_seconds: u8::try_from(row.get::<i16, _>("max_backoff_seconds"))
                            .map_err(|_| TurnStateStoreError::Unavailable)?,
                        cooldown_seconds: u32::try_from(row.get::<i32, _>("cooldown_seconds"))
                            .map_err(|_| TurnStateStoreError::Unavailable)?,
                    },
                })
            })
            .collect()
    }

    async fn commit_model_capture(
        &self,
        scope: &ModelTurnStateCaptureScope,
        value: &str,
    ) -> Result<ModelTurnStateView> {
        if !valid_model_turn_state(value) {
            return Err(TurnStateStoreError::Invalid);
        }
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let (current_revision, current_model) =
            current_model_scope(&mut tx, &scope.account_id, &scope.requested_model).await?;
        if current_revision != scope.identity_revision || current_model != scope.effective_model {
            return Err(TurnStateStoreError::Conflict);
        }
        let old = model_view_in(
            &mut tx,
            &scope.account_id,
            &scope.requested_model,
            scope.identity_revision,
            &scope.effective_model,
        )
        .await?;
        if old.config_revision != scope.config_revision
            || old.policy_revision != scope.policy_revision
            || old.capture_proxy_id != scope.capture_proxy_id
        {
            return Err(TurnStateStoreError::Conflict);
        }
        let now = Utc::now();
        let value_sha = digest(value);
        let same_active = old.pin.as_ref().is_some_and(|pin| pin.value == value);
        let same_candidate = old
            .candidate
            .as_ref()
            .is_some_and(|candidate| candidate.value == value);
        if same_active
            && old
                .pin
                .as_ref()
                .is_some_and(|pin| pin.invalidated || pin.reuse_deadline <= now)
            || same_candidate
                && old
                    .candidate
                    .as_ref()
                    .is_some_and(|candidate| candidate.reuse_deadline <= now)
        {
            return Err(TurnStateStoreError::Conflict);
        }
        if same_active || same_candidate {
            sqlx::query(
                "update openai_model_turn_states
                    set capture_requested_at = null,
                        capture_not_before = now() + $4 * interval '1 second',
                        capture_last_result = 'same_value', capture_last_finished_at = now(),
                        updated_at = now()
                  where account_id = $1 and identity_revision = $2 and effective_model = $3",
            )
            .bind(&scope.account_id)
            .bind(i64::try_from(scope.identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
            .bind(&scope.effective_model)
            .bind(
                i32::try_from(scope.capture_policy.cooldown_seconds)
                    .map_err(|_| TurnStateStoreError::Invalid)?,
            )
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
            let view = model_view_in(
                &mut tx,
                &scope.account_id,
                &scope.requested_model,
                scope.identity_revision,
                &scope.effective_model,
            )
            .await?;
            tx.commit().await.map_err(unavailable)?;
            self.publish_cached_model(&view)?;
            return Ok(view);
        }
        let rejected: Option<String> = sqlx::query_scalar(
            "select rejected_value_sha256 from openai_model_turn_states
              where account_id = $1 and identity_revision = $2 and effective_model = $3",
        )
        .bind(&scope.account_id)
        .bind(i64::try_from(scope.identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
        .bind(&scope.effective_model)
        .fetch_one(&mut *tx)
        .await
        .map_err(unavailable)?;
        if rejected.as_deref() == Some(value_sha.as_str()) {
            return Err(TurnStateStoreError::Conflict);
        }
        let active_valid = old
            .pin
            .as_ref()
            .is_some_and(|pin| !pin.invalidated && pin.reuse_deadline > now);
        let metadata = model_turn_state_token_metadata(value);
        let candidate_id = uuid::Uuid::now_v7().to_string();
        let deadline = model_reuse_deadline(now, old.reuse_window_seconds);
        let query = if active_valid {
            "update openai_model_turn_states
                set candidate_id = $4, candidate_value = $5,
                    candidate_token_version = $6, candidate_issued_at = $7,
                    candidate_raw_bytes = $8, candidate_source = 'capture',
                    candidate_captured_at = $9, candidate_reuse_deadline = $10,
                    capture_requested_at = null, capture_not_before = null,
                    capture_last_result = 'candidate', capture_last_finished_at = now(),
                    config_revision = config_revision + 1, updated_at = now()
              where account_id = $1 and identity_revision = $2 and effective_model = $3
                and config_revision = $11"
        } else {
            "update openai_model_turn_states
                set pin_value = $5, pin_source = 'capture', pin_compatible_transport = 'http',
                    pin_token_version = $6, pin_issued_at = $7, pin_raw_bytes = $8,
                    pin_captured_at = $9, pin_reuse_deadline = $10, pin_invalidated_at = null,
                    active_activated_at = $9, active_candidate_id = nullif($4, ''),
                    active_generation = active_generation + 1,
                    candidate_id = nullif($4, $4), candidate_value = null,
                    candidate_token_version = null, candidate_issued_at = null,
                    candidate_raw_bytes = null, candidate_source = null,
                    candidate_captured_at = null, candidate_reuse_deadline = null,
                    capture_requested_at = null, capture_not_before = null,
                    capture_last_result = 'active', capture_last_finished_at = now(),
                    config_revision = config_revision + 1, updated_at = now()
              where account_id = $1 and identity_revision = $2 and effective_model = $3
                and config_revision = $11"
        };
        sqlx::query(query)
            .bind(&scope.account_id)
            .bind(i64::try_from(scope.identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
            .bind(&scope.effective_model)
            .bind(&candidate_id)
            .bind(value)
            .bind(metadata.token_version.map(i16::from))
            .bind(metadata.issued_at)
            .bind(
                metadata
                    .raw_bytes
                    .and_then(|value| i32::try_from(value).ok()),
            )
            .bind(now)
            .bind(deadline)
            .bind(i64::try_from(scope.config_revision).map_err(|_| TurnStateStoreError::Invalid)?)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        let view = model_view_in(
            &mut tx,
            &scope.account_id,
            &scope.requested_model,
            scope.identity_revision,
            &scope.effective_model,
        )
        .await?;
        tx.commit().await.map_err(unavailable)?;
        self.publish_cached_model(&view)?;
        Ok(view)
    }

    async fn record_model_capture_failure(
        &self,
        scope: &ModelTurnStateCaptureScope,
        reason: &str,
        finished_at: DateTime<Utc>,
    ) -> Result<()> {
        if reason.is_empty() || reason.len() > 128 || reason.chars().any(char::is_control) {
            return Err(TurnStateStoreError::Invalid);
        }
        sqlx::query(
            "update openai_model_turn_states s
                set capture_requested_at = coalesce(s.capture_requested_at, $6),
                    capture_not_before = $6 + $7 * interval '1 second',
                    capture_last_result = $5, capture_last_finished_at = $6,
                    updated_at = now()
               from openai_account_turn_state_policies p
              where s.account_id = $1 and s.identity_revision = $2 and s.effective_model = $3
                and s.config_revision = $4
                and p.account_id = s.account_id and p.identity_revision = s.identity_revision
                and p.config_revision = $8",
        )
        .bind(&scope.account_id)
        .bind(i64::try_from(scope.identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
        .bind(&scope.effective_model)
        .bind(i64::try_from(scope.config_revision).map_err(|_| TurnStateStoreError::Invalid)?)
        .bind(reason)
        .bind(finished_at)
        .bind(
            i32::try_from(scope.capture_policy.cooldown_seconds)
                .map_err(|_| TurnStateStoreError::Invalid)?,
        )
        .bind(i64::try_from(scope.policy_revision).map_err(|_| TurnStateStoreError::Invalid)?)
        .execute(&self.pool)
        .await
        .map_err(unavailable)?;
        Ok(())
    }

    async fn invalidate_active_model_pin(
        &self,
        account_id: &str,
        identity_revision: u64,
        effective_model: &str,
        expected_generation: u64,
        expected_candidate_id: Option<&str>,
        expected_sha256: &str,
    ) -> Result<bool> {
        if expected_generation == 0 || expected_sha256.len() != 64 {
            return Err(TurnStateStoreError::Invalid);
        }
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let current_identity: Option<i64> = sqlx::query_scalar(
            "select identity_revision from provider_accounts
              where id = $1 and provider_kind = 'openai' for share",
        )
        .bind(account_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(unavailable)?;
        if current_identity.and_then(|value| u64::try_from(value).ok()) != Some(identity_revision) {
            tx.rollback().await.map_err(unavailable)?;
            return Ok(false);
        }
        let view = model_view_in(
            &mut tx,
            account_id,
            effective_model,
            identity_revision,
            effective_model,
        )
        .await?;
        let now = Utc::now();
        let active_matches = view.lock_enabled
            && view.pin.as_ref().is_some_and(|pin| {
                !pin.invalidated
                    && pin.generation == expected_generation
                    && pin.id.as_deref() == expected_candidate_id
                    && pin.sha256 == expected_sha256
                    && pin.reuse_deadline > now
            });
        let candidate_matches = expected_candidate_id.is_some()
            && view.lock_enabled
            && view.candidate.as_ref().is_some_and(|candidate| {
                candidate.id.as_deref() == expected_candidate_id
                    && candidate.generation == expected_generation
                    && candidate.sha256 == expected_sha256
                    && candidate.reuse_deadline > now
            });
        if !active_matches && !candidate_matches {
            tx.rollback().await.map_err(unavailable)?;
            return Ok(false);
        }
        let candidate_can_promote = active_matches
            && view.candidate.as_ref().is_some_and(|candidate| {
                candidate.reuse_deadline > now && candidate.sha256 != expected_sha256
            });
        let query = if candidate_matches {
            "update openai_model_turn_states
                set candidate_id = null, candidate_value = null,
                    candidate_token_version = null, candidate_issued_at = null,
                    candidate_raw_bytes = null, candidate_source = null,
                    candidate_captured_at = null, candidate_reuse_deadline = null,
                    rejected_value_sha256 = $4, capture_requested_at = null,
                    capture_not_before = null, config_revision = config_revision + 1,
                    updated_at = now()
              where account_id = $1 and identity_revision = $2 and effective_model = $3"
        } else if candidate_can_promote {
            "update openai_model_turn_states
                set pin_value = candidate_value, pin_token_version = candidate_token_version,
                    pin_issued_at = candidate_issued_at, pin_raw_bytes = candidate_raw_bytes,
                    pin_source = candidate_source, pin_compatible_transport = 'http',
                    pin_captured_at = candidate_captured_at,
                    pin_reuse_deadline = candidate_reuse_deadline,
                    pin_invalidated_at = null, active_activated_at = now(),
                    active_candidate_id = candidate_id,
                    active_generation = active_generation + 1,
                    candidate_id = null, candidate_value = null,
                    candidate_token_version = null, candidate_issued_at = null,
                    candidate_raw_bytes = null, candidate_source = null,
                    candidate_captured_at = null, candidate_reuse_deadline = null,
                    rejected_value_sha256 = $4, capture_requested_at = null,
                    capture_not_before = null, config_revision = config_revision + 1,
                    updated_at = now()
              where account_id = $1 and identity_revision = $2 and effective_model = $3"
        } else {
            "update openai_model_turn_states
                set pin_invalidated_at = now(), active_generation = active_generation + 1,
                    rejected_value_sha256 = $4, capture_requested_at = null,
                    capture_not_before = null, config_revision = config_revision + 1,
                    updated_at = now()
              where account_id = $1 and identity_revision = $2 and effective_model = $3"
        };
        sqlx::query(query)
            .bind(account_id)
            .bind(i64::try_from(identity_revision).map_err(|_| TurnStateStoreError::Invalid)?)
            .bind(effective_model)
            .bind(expected_sha256)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        let view = model_view_in(
            &mut tx,
            account_id,
            effective_model,
            identity_revision,
            effective_model,
        )
        .await?;
        tx.commit().await.map_err(unavailable)?;
        self.publish_cached_model(&view)?;
        Ok(true)
    }
}
