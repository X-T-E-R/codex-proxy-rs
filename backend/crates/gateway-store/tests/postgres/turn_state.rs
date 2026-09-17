use std::sync::Arc;

use chrono::{Duration, Utc};
use gateway_core::lifecycle::CancellationToken;
use gateway_core::provider_ports::turn_state::{
    TurnStateObservation, TurnStateStore, TurnStateStoreError,
};
use gateway_core::task::DaemonTask;
use gateway_store::postgres::{PgTurnStateStore, TurnStateObservationWriter};

use super::{TestDatabase, observability_repository};
use gateway_store::postgres::{
    ObservabilityRange, ObservabilityRepository as _, PgRetentionRepository,
    RetentionRepository as _, RuntimeRetentionSettings, UsageRecordFilter,
};

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

#[tokio::test]
async fn request_turn_state_uses_only_final_attempt_and_counts_terminal_requests() {
    let Some(database) = TestDatabase::create("request_turn_state").await else {
        return;
    };
    seed_account(&database.pool, "acct_turn_observed", "openai").await;
    let now = Utc::now();
    for (id, outcome, attempt, collected) in [
        ("req_state_292", "succeeded", 2, true),
        ("req_state_other", "failed", 1, true),
        ("req_state_cancel_observed", "cancelled", 1, true),
        ("req_state_missing", "cancelled", 2, true),
        ("req_state_historical", "succeeded", 1, false),
        ("req_state_pending", "running", 1, true),
        ("req_state_image", "succeeded", 1, true),
    ] {
        seed_request(&database.pool, id, outcome, attempt, collected, now).await;
    }
    sqlx::query(
        "update model_requests set operation = 'generate_image' where id = 'req_state_image'",
    )
    .execute(&database.pool)
    .await
    .expect("mark non-Responses request");
    let (store, writer) = PgTurnStateStore::initialize(database.pool.clone())
        .await
        .expect("initialize request observations");
    let (cancellation, writer_task) = start_writer(writer);
    for (id, request_id, attempt, value, observed_at, response_id) in [
        (
            "old_attempt",
            "req_state_missing",
            1,
            "x".repeat(292),
            now,
            None,
        ),
        (
            "first_final",
            "req_state_292",
            2,
            "old".to_owned(),
            now,
            None,
        ),
        (
            "final_292",
            "req_state_292",
            2,
            "é".repeat(146),
            now + Duration::milliseconds(2),
            None,
        ),
        (
            "late_old",
            "req_state_292",
            2,
            "late".to_owned(),
            now - Duration::milliseconds(2),
            None,
        ),
        (
            "final_292",
            "req_state_292",
            2,
            "é".repeat(146),
            now + Duration::milliseconds(2),
            Some("response_292"),
        ),
        (
            "failed_state",
            "req_state_other",
            1,
            "other".to_owned(),
            now,
            None,
        ),
        (
            "cancelled_state",
            "req_state_cancel_observed",
            1,
            "cancel".to_owned(),
            now,
            None,
        ),
    ] {
        let mut receipt = observation(id, &value, observed_at, response_id);
        if request_id == "req_state_292" {
            receipt.transport = "websocket".to_owned();
        }
        receipt.request_id = Some(request_id.to_owned());
        receipt.attempt_index = Some(attempt);
        store.enqueue_observation(receipt);
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let count: i64 = sqlx::query_scalar(
                "select count(*) from request_turn_state_observations
                  where request_id in ('req_state_292','req_state_other','req_state_missing',
                                       'req_state_cancel_observed')",
            )
            .fetch_one(&database.pool)
            .await
            .expect("count request observations");
            let enriched: Option<String> = sqlx::query_scalar(
                "select upstream_response_id from request_turn_state_observations
                  where request_id = 'req_state_292' and attempt_index = 2",
            )
            .fetch_optional(&database.pool)
            .await
            .expect("query enriched response")
            .flatten();
            if count == 4 && enriched.as_deref() == Some("response_292") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("request observations persisted");

    let repository = observability_repository(&database.pool);
    let detail = repository
        .usage_record_detail("req_state_292")
        .await
        .expect("observed detail");
    assert!(!format!("{detail:?}").contains(&"é".repeat(146)));
    assert!(format!("{detail:?}").contains("[REDACTED]"));
    let state = detail.turn_state;
    assert!(!format!("{state:?}").contains(&"é".repeat(146)));
    assert_eq!(state.summary.classification, "observed292");
    assert_eq!(state.summary.bytes, Some(292));
    assert_eq!(state.source.as_deref(), Some("websocket"));
    assert_eq!(state.value.as_deref(), Some("é".repeat(146).as_str()));
    assert_eq!(state.upstream_response_id.as_deref(), Some("response_292"));
    assert_eq!(state.attempt_index, Some(2));
    assert!(state.changed);
    assert_eq!(
        state.sha256.as_deref(),
        Some("e159d4fe78fd499d6a76fe377e361a84978cc7ad147cc85bd939c5a034a7a9b4")
    );
    assert_eq!(
        repository
            .usage_record_detail("req_state_missing")
            .await
            .unwrap()
            .turn_state
            .summary
            .classification,
        "unobserved"
    );
    assert_eq!(
        repository
            .usage_record_detail("req_state_historical")
            .await
            .unwrap()
            .turn_state
            .summary
            .classification,
        "notCollected"
    );
    assert_eq!(
        repository
            .usage_record_detail("req_state_cancel_observed")
            .await
            .unwrap()
            .turn_state
            .summary
            .classification,
        "observedOther"
    );
    assert_eq!(
        repository
            .usage_record_detail("req_state_pending")
            .await
            .unwrap()
            .turn_state
            .summary
            .classification,
        "pending"
    );
    assert_eq!(
        repository
            .usage_record_detail("req_state_image")
            .await
            .unwrap()
            .turn_state
            .summary
            .classification,
        "notApplicable"
    );

    let range = ObservabilityRange::new(now - Duration::seconds(1), now + Duration::seconds(1))
        .expect("valid range");
    let totals = repository
        .usage_summary(range, UsageRecordFilter::default())
        .await
        .unwrap();
    assert_eq!(
        (
            totals.turn_state.observed_292,
            totals.turn_state.observed_other,
            totals.turn_state.unobserved,
            totals.turn_state.not_collected
        ),
        (1, 2, 1, 1)
    );
    let filtered = repository
        .usage_summary(
            range,
            UsageRecordFilter {
                outcome: Some("failed".to_owned()),
                ..UsageRecordFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(filtered.turn_state.observed_other, 1);
    assert_eq!(filtered.turn_state.observed_292, 0);

    cancellation.cancel();
    writer_task
        .await
        .expect("writer task")
        .expect("writer shutdown");
    database.close().await;
}

#[tokio::test]
async fn request_turn_state_retention_deletes_expired_and_orphaned_raw() {
    let Some(database) = TestDatabase::create("request_state_retention").await else {
        return;
    };
    seed_account(&database.pool, "acct_turn_observed", "openai").await;
    let old = Utc::now() - Duration::days(32);
    seed_request(&database.pool, "req_expired_state", "failed", 1, true, old).await;
    seed_request(
        &database.pool,
        "req_recent_completion",
        "failed",
        1,
        true,
        old,
    )
    .await;
    sqlx::query(
        "update model_requests set completed_at = now(), downstream_committed_at = now()
          where id = 'req_recent_completion'",
    )
    .execute(&database.pool)
    .await
    .expect("make long-running request recently completed");
    sqlx::query(
        "insert into request_turn_state_observations
           (request_id, attempt_index, observation_id, value, observed_at, source)
         values ('req_expired_state', 1, 'expired', 'raw-old', $1, 'http'),
                ('req_orphan_state', 1, 'orphan', 'raw-orphan', $1, 'websocket'),
                ('req_recent_completion', 1, 'recent', 'raw-retained', $1, 'http')",
    )
    .bind(old)
    .execute(&database.pool)
    .await
    .expect("seed old raw");
    let report = PgRetentionRepository::new(database.pool.clone())
        .apply_retention(
            Utc::now(),
            RuntimeRetentionSettings {
                usage_retention_days: 31,
                ops_event_retention_days: 30,
                audit_retention_days: 90,
            },
        )
        .await
        .expect("retention");
    assert_eq!(report.model_requests, 1);
    let remaining: i64 = sqlx::query_scalar("select count(*) from request_turn_state_observations")
        .fetch_one(&database.pool)
        .await
        .expect("remaining raw");
    assert_eq!(remaining, 1);
    let retained: String = sqlx::query_scalar(
        "select value from request_turn_state_observations
          where request_id = 'req_recent_completion'",
    )
    .fetch_one(&database.pool)
    .await
    .expect("recent request retains raw");
    assert_eq!(retained, "raw-retained");
    let (store, writer) = PgTurnStateStore::initialize(database.pool.clone())
        .await
        .expect("initialize late writer");
    let (cancellation, writer_task) = start_writer(writer);
    let mut late = observation("late-after-retention", "must-not-return", old, None);
    late.request_id = Some("req_expired_state".to_owned());
    late.attempt_index = Some(1);
    store.enqueue_observation(late);
    wait_for_observation(&store, "acct_turn_observed", "late-after-retention").await;
    let expired_raw: i64 = sqlx::query_scalar(
        "select count(*) from request_turn_state_observations
          where request_id = 'req_expired_state'",
    )
    .fetch_one(&database.pool)
    .await
    .expect("count late raw");
    assert_eq!(expired_raw, 0);
    cancellation.cancel();
    writer_task.await.unwrap().unwrap();
    database.close().await;
}

#[tokio::test]
async fn request_turn_state_observation_can_arrive_before_request_row_and_preserve_change() {
    let Some(database) = TestDatabase::create("request_state_write_race").await else {
        return;
    };
    seed_account(&database.pool, "acct_turn_observed", "openai").await;
    let (store, writer) = PgTurnStateStore::initialize(database.pool.clone())
        .await
        .expect("initialize race writer");
    let (cancellation, writer_task) = start_writer(writer);
    let now = Utc::now();
    for (index, value) in std::iter::once("state-a")
        .chain(std::iter::repeat_n("state-b", 32))
        .enumerate()
    {
        let mut receipt = observation(&format!("race-{index:02}"), value, now, None);
        receipt.request_id = Some("req_state_write_race".to_owned());
        receipt.attempt_index = Some(1);
        receipt.transport = "websocket".to_owned();
        store.enqueue_observation(receipt);
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let row = sqlx::query_as::<_, (String, bool)>(
                "select value, changed from request_turn_state_observations
                  where request_id = 'req_state_write_race' and attempt_index = 1",
            )
            .fetch_optional(&database.pool)
            .await
            .expect("load race observation");
            if row
                .as_ref()
                .is_some_and(|row| row == &("state-b".to_owned(), true))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("observation persisted before request row");
    seed_request(
        &database.pool,
        "req_state_write_race",
        "failed",
        1,
        true,
        now,
    )
    .await;
    let state = observability_repository(&database.pool)
        .usage_record_detail("req_state_write_race")
        .await
        .expect("detail after request insert")
        .turn_state;
    assert_eq!(state.value.as_deref(), Some("state-b"));
    assert_eq!(state.source.as_deref(), Some("websocket"));
    assert!(state.changed);
    cancellation.cancel();
    writer_task.await.unwrap().unwrap();
    database.close().await;
}

async fn seed_request(
    pool: &sqlx::PgPool,
    id: &str,
    outcome: &str,
    attempts: i32,
    collected: bool,
    started_at: chrono::DateTime<Utc>,
) {
    let completed_at = (outcome != "running").then_some(started_at + Duration::milliseconds(5));
    sqlx::query(
        "insert into model_requests
           (id, client_api_key_ref, config_revision, protocol, operation, endpoint,
            client_transport, requested_model_id, provider_kind, provider_account_ref,
            upstream_transport, attempt_count, outcome, started_at, deadline_at,
            completed_at, routing_scope, routing_group_refs, routing_group_names_snapshot,
            turn_state_collection_enabled, downstream_committed_at, client_status_code)
         values ($1, 'key-state', 1, 'openai', 'generate', '/v1/responses',
                 'http_sse', 'coding', 'openai', 'acct_turn_observed', 'http_sse',
                 $2, $3, $4, $4 + interval '30 seconds', $5,
                 'all', '{}'::text[], '[]'::jsonb, $6, $5, case when $3 = 'succeeded' then 200 else null end)",
    )
    .bind(id).bind(attempts).bind(outcome).bind(started_at).bind(completed_at).bind(collected)
    .execute(pool).await.expect("seed request");
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
        request_id: None,
        attempt_index: None,
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
