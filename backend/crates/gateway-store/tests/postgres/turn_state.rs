use std::sync::Arc;

use chrono::{Duration, Utc};
use gateway_core::lifecycle::CancellationToken;
use gateway_core::provider_ports::turn_state::{
    TurnStateObservation, TurnStateStore, TurnStateStoreError,
};
use gateway_core::task::DaemonTask;
use gateway_store::postgres::{PgTurnStateStore, TurnStateObservationWriter};

use super::TestDatabase;

#[tokio::test]
async fn turn_state_override_is_versioned_validated_clearable_and_account_scoped() {
    let Some(database) = TestDatabase::create("turn_state_override").await else {
        return;
    };
    seed_account(&database.pool, "acct_turn_a", "openai").await;
    seed_account(&database.pool, "acct_turn_b", "openai").await;
    let (store, _writer) = PgTurnStateStore::initialize(database.pool.clone())
        .await
        .expect("initialize store");

    let initial = store.load("acct_turn_a").await.expect("initial state");
    assert_eq!(initial.config_revision, 1);
    assert!(!initial.override_state.enabled);
    assert_eq!(store.active_override("acct_turn_a"), None);
    assert!(matches!(
        store.update("acct_turn_a", true, None, 1).await,
        Err(TurnStateStoreError::Invalid)
    ));
    for invalid in ["line\r\nbreak", "control\u{1f}", "非 ASCII"] {
        assert!(matches!(
            store
                .update("acct_turn_a", false, Some(Some(invalid.to_owned())), 1)
                .await,
            Err(TurnStateStoreError::Invalid)
        ));
    }

    let enabled = store
        .update(
            "acct_turn_a",
            true,
            Some(Some("manual-header-value".into())),
            1,
        )
        .await
        .expect("enable override");
    assert_eq!(enabled.config_revision, 2);
    assert_eq!(
        store.active_override("acct_turn_a").as_deref(),
        Some("manual-header-value")
    );
    assert_eq!(store.active_override("acct_turn_b"), None);
    let (restarted, _restarted_writer) = PgTurnStateStore::initialize(database.pool.clone())
        .await
        .expect("hydrate committed override after restart");
    assert_eq!(
        restarted.active_override("acct_turn_a").as_deref(),
        Some("manual-header-value")
    );
    assert!(matches!(
        store.update("acct_turn_a", false, None, 1).await,
        Err(TurnStateStoreError::Conflict)
    ));

    let cleared = store
        .update("acct_turn_a", true, Some(None), 2)
        .await
        .expect("null clears and disables");
    assert!(!cleared.override_state.enabled);
    assert_eq!(cleared.override_state.value, None);
    assert_eq!(cleared.config_revision, 3);
    assert_eq!(store.active_override("acct_turn_a"), None);
    store
        .publish_cached_override(&enabled)
        .expect("late rev2 update publish");
    assert_eq!(
        store.active_override("acct_turn_a"),
        None,
        "late enabled update must not replace the rev3 tombstone"
    );
    database.close().await;
}

#[tokio::test]
async fn observations_reject_older_writes_enrich_in_place_and_fence_adoption() {
    let Some(database) = TestDatabase::create("turn_state_observed").await else {
        return;
    };
    seed_account(&database.pool, "acct_turn_observed", "openai").await;
    seed_account(&database.pool, "acct_turn_xai", "xai").await;
    let (store, writer) = PgTurnStateStore::initialize(database.pool.clone())
        .await
        .expect("initialize store");
    let (cancellation, writer_task) = start_writer(writer);
    let now = Utc::now();

    store.enqueue_observation(observation("obs_new", "new-value", now, Some("resp_new")));
    store.enqueue_observation(observation(
        "obs_old",
        "old-value",
        now - Duration::seconds(1),
        None,
    ));
    wait_for_observation(&store, "acct_turn_observed", "obs_new").await;
    let latest = store.load("acct_turn_observed").await.expect("latest");
    let latest_observed = latest.observed.as_ref().expect("observation");
    assert_eq!(latest_observed.value, "new-value");
    assert_eq!(
        latest_observed.upstream_response_id.as_deref(),
        Some("resp_new")
    );

    store.enqueue_observation(observation(
        "obs_new",
        "new-value",
        now,
        Some("resp_enriched"),
    ));
    wait_for_response_id(&store, "acct_turn_observed", "resp_enriched").await;
    assert!(matches!(
        store
            .use_observed(
                "acct_turn_observed",
                "obs_old",
                true,
                latest.config_revision
            )
            .await,
        Err(TurnStateStoreError::Conflict)
    ));
    let adopted = store
        .use_observed(
            "acct_turn_observed",
            "obs_new",
            true,
            latest.config_revision,
        )
        .await
        .expect("adopt current observation");
    assert_eq!(adopted.override_state.value.as_deref(), Some("new-value"));
    assert_eq!(
        store.active_override("acct_turn_observed").as_deref(),
        Some("new-value")
    );
    let disabled = store
        .update("acct_turn_observed", false, None, adopted.config_revision)
        .await
        .expect("disable adopted override");
    assert!(!disabled.override_state.enabled);
    store
        .publish_cached_override(&adopted)
        .expect("late use-observed publish");
    assert_eq!(
        store.active_override("acct_turn_observed"),
        None,
        "late use-observed publish must not replace the newer tombstone"
    );

    store.enqueue_observation(observation(
        "obs_invalid",
        "bad\r\nvalue",
        now + Duration::seconds(1),
        None,
    ));
    wait_for_observation(&store, "acct_turn_observed", "obs_invalid").await;
    let invalid = store
        .load("acct_turn_observed")
        .await
        .expect("invalid observed value");
    assert!(matches!(
        store
            .use_observed(
                "acct_turn_observed",
                "obs_invalid",
                true,
                invalid.config_revision,
            )
            .await,
        Err(TurnStateStoreError::Invalid)
    ));
    assert!(matches!(
        store.load("acct_turn_xai").await,
        Err(TurnStateStoreError::NotFound)
    ));
    cancellation.cancel();
    writer_task
        .await
        .expect("writer task")
        .expect("writer shutdown");
    database.close().await;
}

fn observation(
    id: &str,
    value: &str,
    observed_at: chrono::DateTime<Utc>,
    upstream_response_id: Option<&str>,
) -> TurnStateObservation {
    TurnStateObservation {
        id: id.to_owned(),
        account_id: "acct_turn_observed".to_owned(),
        value: value.to_owned(),
        observed_at,
        transport: "http".to_owned(),
        upstream_response_id: upstream_response_id.map(str::to_owned),
        client_turn_id: Some("turn_1".to_owned()),
    }
}

fn start_writer(
    writer: TurnStateObservationWriter,
) -> (
    CancellationToken,
    tokio::task::JoinHandle<Result<(), gateway_core::task::WorkerTaskError>>,
) {
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let writer = Arc::new(writer);
    let task = tokio::spawn(async move { writer.run(task_cancellation).await });
    (cancellation, task)
}

async fn wait_for_observation(store: &PgTurnStateStore, account_id: &str, id: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if store
                .load(account_id)
                .await
                .ok()
                .and_then(|view| view.observed)
                .is_some_and(|observed| observed.id == id)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("observation persisted");
}

async fn wait_for_response_id(store: &PgTurnStateStore, account_id: &str, response_id: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if store
                .load(account_id)
                .await
                .ok()
                .and_then(|view| view.observed)
                .and_then(|observed| observed.upstream_response_id)
                .as_deref()
                == Some(response_id)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("observation enrichment persisted");
}

async fn seed_account(pool: &sqlx::PgPool, id: &str, provider: &str) {
    sqlx::query(
        "insert into provider_accounts (
           id, provider_kind, name, authentication_kind, provider_credentials_json,
           credential_revision, has_refresh_token, enabled, credential_state,
           credential_observed_at, created_at, updated_at
         ) values ($1, $2, $1, 'oauth', '{}'::jsonb, 1, false, true, 'ready', now(), now(), now())",
    )
    .bind(id)
    .bind(provider)
    .execute(pool)
    .await
    .expect("seed account");
}
