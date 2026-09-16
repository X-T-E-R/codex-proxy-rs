//! OpenAI 账号 turn state 的权威配置、进程内快照与有界观测写入。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gateway_core::lifecycle::CancellationToken;
use gateway_core::provider_ports::turn_state::{
    TurnStateObservation, TurnStateObserved, TurnStateOverride, TurnStateStore,
    TurnStateStoreError, TurnStateView, valid_turn_state_override,
};
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
    observations: mpsc::Sender<TurnStateObservation>,
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
        let (sender, receiver) = mpsc::channel(OBSERVATION_QUEUE_CAPACITY);
        Ok((
            Self {
                pool: pool.clone(),
                overrides,
                observations: sender,
            },
            TurnStateObservationWriter {
                pool,
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
}

pub struct TurnStateObservationWriter {
    pool: PgPool,
    receiver: Mutex<mpsc::Receiver<TurnStateObservation>>,
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
                        drain_observations(&self.pool, &mut receiver).await;
                        return Ok(());
                    }
                    observation = receiver.recv() => observation,
                };
                let Some(observation) = observation else {
                    return Err(WorkerTaskError::safe("turn state observation queue closed"));
                };
                persist_observation(&self.pool, observation).await;
            }
        })
    }
}

async fn drain_observations(pool: &PgPool, receiver: &mut mpsc::Receiver<TurnStateObservation>) {
    receiver.close();
    let started = Instant::now();
    while let Some(observation) = receiver.recv().await {
        let remaining = SHUTDOWN_DRAIN_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero()
            || tokio::time::timeout(remaining, persist_observation(pool, observation))
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

async fn persist_observation(pool: &PgPool, observation: TurnStateObservation) {
    let account_id = observation.account_id.clone();
    let observation_id = observation.id.clone();
    if let Err(error) = persist_observation_inner(pool, observation).await {
        tracing::warn!(
            account_id,
            observation_id,
            error_kind = ?error,
            "OpenAI turn state observation write failed"
        );
    }
}

async fn persist_observation_inner(pool: &PgPool, observation: TurnStateObservation) -> Result<()> {
    if observation.value.is_empty() || observation.value.len() > 16 * 1024 {
        return Err(TurnStateStoreError::Invalid);
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

async fn ensure_openai(tx: &mut Transaction<'_, Postgres>, account_id: &str) -> Result<()> {
    let kind: Option<String> =
        sqlx::query_scalar("select provider_kind from provider_accounts where id = $1")
            .bind(account_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(unavailable)?;
    match kind.as_deref() {
        Some("openai") => Ok(()),
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
        if let Err(error) = self.observations.try_send(observation) {
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

    fn active_override(&self, account_id: &str) -> Option<String> {
        self.overrides
            .read()
            .ok()
            .and_then(|overrides| overrides.get(account_id)?.value.clone())
    }
}
