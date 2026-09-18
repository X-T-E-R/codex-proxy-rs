use std::sync::Arc;

use chrono::{Duration, Utc};
use gateway_core::lifecycle::CancellationToken;
use gateway_core::provider_ports::turn_state::{
    AccountTurnStatePolicyUpdate, ModelTurnStateCaptureTriggerMode, ModelTurnStatePinAction,
    ModelTurnStateUpdate, TurnStateObservation, TurnStateStore, TurnStateStoreError,
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
async fn model_pin_is_scoped_aged_fenced_and_capture_does_not_pollute_requests() {
    let Some(database) = TestDatabase::create("model_turn_state").await else {
        return;
    };
    seed_account(&database.pool, "acct_model_state", "openai").await;
    sqlx::query(
        "insert into outbound_proxies
           (id, name, proxy_url, last_test_at, last_test_success, last_test_latency_ms,
            last_test_message)
         values ('proxy_capture', 'Capture', 'socks5://127.0.0.1:823', now(), true, 1, 'ok')",
    )
    .execute(&database.pool)
    .await
    .expect("seed capture proxy");
    sqlx::query(
        "update runtime_settings
            set model_mappings_json = '{\"public-codex\":\"upstream-codex\"}'::jsonb
          where id = 1",
    )
    .execute(&database.pool)
    .await
    .expect("seed model mapping");
    let (store, _writer) = PgTurnStateStore::initialize(database.pool.clone())
        .await
        .expect("initialize model store");
    let initial = store
        .load_model_state("acct_model_state", "public-codex")
        .await
        .expect("load model state");
    assert_eq!(initial.effective_model, "upstream-codex");
    assert_eq!(initial.reuse_window_seconds, 7_200);
    assert_eq!(initial.capture_policy.max_attempts, 3);
    assert_eq!(initial.capture_policy.attempt_timeout_seconds, 8);
    assert_eq!(initial.capture_policy.job_timeout_seconds, 30);
    assert_eq!(initial.capture_policy.backoff_seconds, 1);
    assert_eq!(initial.capture_policy.max_backoff_seconds, 4);
    assert_eq!(initial.capture_policy.cooldown_seconds, 900);

    let initial_policy = store
        .load_account_policy("acct_model_state")
        .await
        .expect("load account policy");
    let lock_only = store
        .update_account_policy(
            "acct_model_state",
            AccountTurnStatePolicyUpdate {
                lock_enabled: true,
                capture_proxy_id: None,
                ..account_policy_update(&initial_policy)
            },
        )
        .await
        .expect("save pending lock intent before a pin exists");
    assert!(
        store
            .model_observation_scope("acct_model_state", 1, "upstream-codex")
            .is_some(),
        "lock-only policy must adopt a valid state from ordinary traffic"
    );
    let waiting = store
        .update_account_policy(
            "acct_model_state",
            AccountTurnStatePolicyUpdate {
                capture_enabled: true,
                capture_proxy_id: None,
                ..account_policy_update(&lock_only)
            },
        )
        .await
        .expect("save permissive account policy while capture proxy is missing");
    assert!(waiting.lock_enabled);
    assert!(waiting.capture_enabled);
    assert!(waiting.capture_proxy_id.is_none());
    let reloaded_waiting = store
        .load_account_policy("acct_model_state")
        .await
        .expect("reload account policy after saving switches");
    assert!(
        reloaded_waiting.lock_enabled,
        "the account lock switch must survive a fresh policy read without a model pin"
    );
    assert!(
        reloaded_waiting.capture_enabled,
        "the account capture switch must survive the same policy round trip"
    );
    assert_eq!(reloaded_waiting.config_revision, waiting.config_revision);
    let configured_policy = store
        .update_account_policy(
            "acct_model_state",
            AccountTurnStatePolicyUpdate {
                capture_proxy_id: Some("proxy_capture".to_owned()),
                ..account_policy_update(&waiting)
            },
        )
        .await
        .expect("attach tested capture proxy");
    let pending = store
        .load_model_state("acct_model_state", "public-codex")
        .await
        .expect("load model under account policy");
    assert!(pending.lock_enabled);
    assert!(pending.capture_enabled);
    assert!(pending.pin.is_none());
    assert_eq!(
        store.active_model_pin("acct_model_state", 1, "upstream-codex"),
        None,
        "a pending lock must not synthesize or inject a value"
    );
    assert!(
        store
            .model_observation_scope("acct_model_state", 1, "upstream-codex")
            .is_some(),
        "normal HTTP traffic must observe the first state before proxy capture"
    );

    let value = synthetic_fernet_candidate();
    let locked = store
        .update_model_state(
            "acct_model_state",
            "public-codex",
            ModelTurnStateUpdate {
                expected_identity_revision: pending.identity_revision,
                expected_effective_model: pending.effective_model.clone(),
                lock_enabled: configured_policy.lock_enabled,
                capture_enabled: configured_policy.capture_enabled,
                reuse_window_seconds: configured_policy.reuse_window_seconds,
                capture_proxy_id: configured_policy.capture_proxy_id.clone(),
                max_attempts: configured_policy.capture_policy.max_attempts,
                attempt_timeout_seconds: configured_policy.capture_policy.attempt_timeout_seconds,
                job_timeout_seconds: configured_policy.capture_policy.job_timeout_seconds,
                backoff_seconds: configured_policy.capture_policy.backoff_seconds,
                max_backoff_seconds: configured_policy.capture_policy.max_backoff_seconds,
                cooldown_seconds: configured_policy.capture_policy.cooldown_seconds,
                pin_action: ModelTurnStatePinAction::Replace,
                value: Some(value.clone()),
                expected_revision: pending.config_revision,
            },
        )
        .await
        .expect("lock model pin");
    assert_eq!(locked.capture_policy.attempt_timeout_seconds, 8);
    assert_eq!(locked.capture_policy.job_timeout_seconds, 30);
    assert_eq!(
        store
            .active_model_pin("acct_model_state", 1, "upstream-codex")
            .map(|pin| pin.value),
        Some(value.clone())
    );
    let original = locked.pin.expect("manual pin");
    assert_eq!(original.encoded_bytes, 292);
    assert_eq!(original.raw_bytes, Some(217));
    assert_eq!(original.ciphertext_bytes, Some(160));
    assert_eq!(original.token_version, Some(0x80));
    assert_eq!(
        original.issued_at.map(|value| value.timestamp()),
        Some(1_789_650_773)
    );
    assert!(!original.timestamp_verified);
    let shortened_policy = store
        .update_account_policy(
            "acct_model_state",
            AccountTurnStatePolicyUpdate {
                reuse_window_seconds: 60,
                ..account_policy_update(&configured_policy)
            },
        )
        .await
        .expect("shorten account reuse window");
    let shortened = store
        .load_model_state("acct_model_state", "public-codex")
        .await
        .expect("reload model with shorter account policy");
    let shortened_pin = shortened.pin.as_ref().expect("shortened pin");
    let expected_short_deadline = original
        .reuse_deadline
        .min(original.captured_at + Duration::seconds(60));
    assert_eq!(shortened_pin.captured_at, original.captured_at);
    assert_eq!(shortened_pin.reuse_deadline, expected_short_deadline);
    let mut invalid_attempt_timeout = account_policy_update(&shortened_policy);
    invalid_attempt_timeout.attempt_timeout_seconds = 61;
    assert!(matches!(
        store
            .update_account_policy("acct_model_state", invalid_attempt_timeout)
            .await,
        Err(TurnStateStoreError::Invalid)
    ));
    let mut invalid_job_timeout = account_policy_update(&shortened_policy);
    invalid_job_timeout.job_timeout_seconds = 301;
    assert!(matches!(
        store
            .update_account_policy("acct_model_state", invalid_job_timeout)
            .await,
        Err(TurnStateStoreError::Invalid)
    ));
    sqlx::query(
        "update openai_model_turn_states
            set pin_reuse_deadline = now() - interval '1 second'
          where account_id = 'acct_model_state'",
    )
    .execute(&database.pool)
    .await
    .expect("age pin");
    let observation_only_policy = store
        .update_account_policy(
            "acct_model_state",
            AccountTurnStatePolicyUpdate {
                capture_enabled: false,
                ..account_policy_update(&shortened_policy)
            },
        )
        .await
        .expect("pause capture while checking ordinary observation");
    let (aged_store, writer) = PgTurnStateStore::initialize(database.pool.clone())
        .await
        .expect("hydrate aged model store");
    let (writer_cancellation, writer_task) = start_writer(writer);
    assert_eq!(
        aged_store.active_model_pin("acct_model_state", 1, "upstream-codex"),
        None,
        "AGED pins stop HTTP injection and fall back to legacy/normal continuation"
    );
    let candidates = aged_store
        .model_capture_candidates(None, 8)
        .await
        .expect("load automatic candidates");
    assert!(
        candidates.is_empty(),
        "TTL expiry must wait for an ordinary HTTP observation before residential capture"
    );
    let scope = aged_store
        .model_observation_scope("acct_model_state", 1, "upstream-codex")
        .expect("aged pin accepts an ordinary HTTP observation");
    let suspect = format!("{}\u{1}", "A".repeat(291));
    assert_eq!(suspect.len(), 292);
    aged_store.enqueue_observation(TurnStateObservation {
        id: "obs_non_printable_292".to_owned(),
        account_id: "acct_model_state".to_owned(),
        request_id: None,
        attempt_index: None,
        value: suspect,
        observed_at: Utc::now(),
        transport: "http".to_owned(),
        upstream_response_id: None,
        client_turn_id: None,
        model_scope: Some(scope.clone()),
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let observed = aged_store
                .load("acct_model_state")
                .await
                .expect("load suspect observation");
            if observed
                .observed
                .as_ref()
                .is_some_and(|value| value.id == "obs_non_printable_292")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("suspect observation is persisted without advancing model state");
    assert!(
        aged_store
            .model_capture_candidates(None, 8)
            .await
            .expect("suspect value must not schedule residential capture")
            .is_empty()
    );
    let capture_policy = aged_store
        .update_account_policy(
            "acct_model_state",
            AccountTurnStatePolicyUpdate {
                capture_enabled: true,
                ..account_policy_update(&observation_only_policy)
            },
        )
        .await
        .expect("resume capture after suspect observation");
    assert!(capture_policy.capture_enabled);
    let first_request_policy = aged_store
        .update_account_policy(
            "acct_model_state",
            AccountTurnStatePolicyUpdate {
                capture_trigger_mode: ModelTurnStateCaptureTriggerMode::FirstRequestAfterExpiry,
                ..account_policy_update(&capture_policy)
            },
        )
        .await
        .expect("select first-request-after-expiry mode");
    let requested_at = Utc::now();
    for _ in 0..2 {
        aged_store.enqueue_capture_after_expiry(
            "acct_model_state",
            1,
            "upstream-codex",
            requested_at,
        );
    }
    let first_request_candidates = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let candidates = aged_store
                .model_capture_candidates(None, 8)
                .await
                .expect("load first-request candidates");
            if !candidates.is_empty() {
                break candidates;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("expired first request queues capture");
    assert_eq!(
        first_request_candidates.len(),
        1,
        "duplicate signals coalesce"
    );
    let failure_policy = aged_store
        .update_account_policy(
            "acct_model_state",
            AccountTurnStatePolicyUpdate {
                capture_trigger_mode: ModelTurnStateCaptureTriggerMode::OnAttributedFailure,
                ..account_policy_update(&first_request_policy)
            },
        )
        .await
        .expect("restore attributed-failure mode and clear the old signal");
    assert_eq!(
        failure_policy.capture_trigger_mode,
        ModelTurnStateCaptureTriggerMode::OnAttributedFailure
    );
    let scope = aged_store
        .model_observation_scope("acct_model_state", 1, "upstream-codex")
        .expect("capture-enabled aged pin accepts another ordinary observation");
    aged_store.enqueue_observation(TurnStateObservation {
        id: "obs_non_292".to_owned(),
        account_id: "acct_model_state".to_owned(),
        request_id: None,
        attempt_index: None,
        value: "short".to_owned(),
        observed_at: Utc::now(),
        transport: "http".to_owned(),
        upstream_response_id: None,
        client_turn_id: None,
        model_scope: Some(scope.clone()),
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let observed = aged_store
                .load("acct_model_state")
                .await
                .expect("load expired non-292 observation");
            if observed
                .observed
                .as_ref()
                .is_some_and(|value| value.id == "obs_non_292")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("expired non-292 observation is persisted");
    assert!(
        aged_store
            .model_capture_candidates(None, 8)
            .await
            .expect("expired value does not re-enter bootstrap capture")
            .is_empty()
    );

    sqlx::query(
        "update provider_accounts set credential_revision = credential_revision + 1
          where id = 'acct_model_state'",
    )
    .execute(&database.pool)
    .await
    .expect("refresh access token revision");
    let observed_at = Utc::now();
    let observed_value = "Z".repeat(292);
    aged_store.enqueue_observation(TurnStateObservation {
        id: "obs_292".to_owned(),
        account_id: "acct_model_state".to_owned(),
        request_id: None,
        attempt_index: None,
        value: observed_value.clone(),
        observed_at,
        transport: "http".to_owned(),
        upstream_response_id: None,
        client_turn_id: None,
        model_scope: Some(scope),
    });
    let captured = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let captured = aged_store
                .load_model_state("acct_model_state", "public-codex")
                .await
                .expect("load normally observed model pin");
            if captured
                .pin
                .as_ref()
                .is_some_and(|pin| pin.source == "observation" && !pin.invalidated)
            {
                break captured;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("292-byte ordinary observation publishes a pin");
    let captured_pin = captured.pin.expect("ordinary observation pin");
    assert!(
        (captured_pin.captured_at - observed_at)
            .num_milliseconds()
            .abs()
            <= 1,
        "PostgreSQL stores the observation timestamp at microsecond precision"
    );
    assert!(captured_pin.reuse_deadline > Utc::now());
    assert_eq!(captured_pin.source, "observation");
    assert_eq!(captured_pin.value, observed_value);
    assert_eq!(captured_pin.envelope_format, None);
    let request_count: i64 = sqlx::query_scalar("select count(*) from model_requests")
        .fetch_one(&database.pool)
        .await
        .expect("count ordinary requests");
    assert_eq!(
        request_count, 0,
        "capture store path does not create requests"
    );

    let next_candidates = aged_store
        .model_capture_candidates(None, 8)
        .await
        .expect("valid ordinary observation clears capture request");
    assert!(next_candidates.is_empty());
    assert!(
        aged_store
            .invalidate_active_model_pin(
                "acct_model_state",
                1,
                "upstream-codex",
                captured_pin.generation,
                captured_pin.id.as_deref(),
                &captured_pin.sha256,
            )
            .await
            .expect("invalidate attributable pin")
    );
    assert!(
        !aged_store
            .invalidate_active_model_pin(
                "acct_model_state",
                1,
                "upstream-codex",
                captured_pin.generation,
                captured_pin.id.as_deref(),
                &captured_pin.sha256,
            )
            .await
            .expect("stale invalidation is fenced")
    );
    let invalid_candidates = aged_store
        .model_capture_candidates(None, 8)
        .await
        .expect("attributable invalidation schedules capture in the default mode");
    assert_eq!(invalid_candidates.len(), 1);
    assert!(matches!(
        aged_store
            .commit_model_capture(&invalid_candidates[0], &observed_value)
            .await,
        Err(TurnStateStoreError::Conflict)
    ));
    let replacement = "B".repeat(292);
    let recaptured = aged_store
        .commit_model_capture(&invalid_candidates[0], &replacement)
        .await
        .expect("residential capture accepts a different value after rejection");
    let recaptured_pin = recaptured.pin.expect("renewed residential pin");
    assert_eq!(recaptured_pin.source, "capture");
    assert!(recaptured_pin.reuse_deadline > Utc::now());
    assert!(
        aged_store
            .active_model_pin("acct_model_state", 1, "upstream-codex")
            .is_some()
    );
    sqlx::query(
        "update provider_accounts set identity_revision = identity_revision + 1
          where id = 'acct_model_state'",
    )
    .execute(&database.pool)
    .await
    .expect("rotate identity");
    assert!(matches!(
        aged_store
            .commit_model_capture(&invalid_candidates[0], &"B".repeat(292))
            .await,
        Err(TurnStateStoreError::Conflict)
    ));
    writer_cancellation.cancel();
    writer_task
        .await
        .expect("join observation writer")
        .expect("stop observation writer");
    database.close().await;
}

#[tokio::test]
async fn shortened_reuse_window_queues_first_expired_request_and_expires_candidate() {
    let Some(database) = TestDatabase::create("model_turn_state_shortened_window").await else {
        return;
    };
    const ACCOUNT_ID: &str = "acct_shortened_window";
    const MODEL: &str = "gpt-5.4";
    seed_account(&database.pool, ACCOUNT_ID, "openai").await;
    sqlx::query(
        "insert into outbound_proxies
           (id, name, proxy_url, last_test_at, last_test_success, last_test_latency_ms,
            last_test_message)
         values ('proxy_shortened_window', 'Shortened window',
                 'socks5://127.0.0.1:824', now(), true, 1, 'ok')",
    )
    .execute(&database.pool)
    .await
    .expect("seed capture proxy");
    let (store, writer) = PgTurnStateStore::initialize(database.pool.clone())
        .await
        .expect("initialize shortened-window store");
    let (writer_cancellation, writer_task) = start_writer(writer);
    let policy = store
        .load_account_policy(ACCOUNT_ID)
        .await
        .expect("load account policy");
    let policy = store
        .update_account_policy(
            ACCOUNT_ID,
            AccountTurnStatePolicyUpdate {
                lock_enabled: true,
                capture_enabled: true,
                capture_trigger_mode: ModelTurnStateCaptureTriggerMode::FirstRequestAfterExpiry,
                capture_proxy_id: Some("proxy_shortened_window".to_owned()),
                reuse_window_seconds: 7_200,
                ..account_policy_update(&policy)
            },
        )
        .await
        .expect("enable first-request capture");
    let empty = store
        .load_model_state(ACCOUNT_ID, MODEL)
        .await
        .expect("load empty model state");
    let value = synthetic_fernet_candidate();
    let pinned = store
        .update_model_state(
            ACCOUNT_ID,
            MODEL,
            ModelTurnStateUpdate {
                expected_identity_revision: empty.identity_revision,
                expected_effective_model: empty.effective_model.clone(),
                lock_enabled: policy.lock_enabled,
                capture_enabled: policy.capture_enabled,
                reuse_window_seconds: policy.reuse_window_seconds,
                capture_proxy_id: policy.capture_proxy_id.clone(),
                max_attempts: policy.capture_policy.max_attempts,
                attempt_timeout_seconds: policy.capture_policy.attempt_timeout_seconds,
                job_timeout_seconds: policy.capture_policy.job_timeout_seconds,
                backoff_seconds: policy.capture_policy.backoff_seconds,
                max_backoff_seconds: policy.capture_policy.max_backoff_seconds,
                cooldown_seconds: policy.capture_policy.cooldown_seconds,
                pin_action: ModelTurnStatePinAction::Replace,
                value: Some(value.clone()),
                expected_revision: empty.config_revision,
            },
        )
        .await
        .expect("seed active pin");
    sqlx::query(
        "update openai_model_turn_states
            set pin_captured_at = now() - interval '10 minutes',
                pin_reuse_deadline = now() + interval '100 minutes',
                candidate_id = 'candidate-shortened-window',
                candidate_value = $2, candidate_source = 'capture',
                candidate_captured_at = now() - interval '10 minutes',
                candidate_reuse_deadline = now() + interval '100 minutes',
                config_revision = config_revision + 1, updated_at = now()
          where account_id = $1 and effective_model = $3",
    )
    .bind(ACCOUNT_ID)
    .bind("B".repeat(292))
    .bind(MODEL)
    .execute(&database.pool)
    .await
    .expect("age active and candidate while keeping stored deadlines in the future");
    let hydrated = store
        .load_model_state(ACCOUNT_ID, MODEL)
        .await
        .expect("hydrate aged values under the original window");
    assert!(hydrated.pin.is_some());
    assert!(hydrated.candidate.is_some());
    assert!(store.active_model_pin(ACCOUNT_ID, 1, MODEL).is_some());

    let shortened = store
        .update_account_policy(
            ACCOUNT_ID,
            AccountTurnStatePolicyUpdate {
                reuse_window_seconds: 60,
                ..account_policy_update(&policy)
            },
        )
        .await
        .expect("shorten the current account window");
    assert_eq!(shortened.reuse_window_seconds, 60);
    assert_eq!(
        store.active_model_pin(ACCOUNT_ID, 1, MODEL),
        None,
        "the active and candidate values are expired under the current policy"
    );

    let requested_at = Utc::now();
    for _ in 0..2 {
        store.enqueue_capture_after_expiry(ACCOUNT_ID, 1, MODEL, requested_at);
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let requested: bool = sqlx::query_scalar(
                "select capture_requested_at is not null
                   from openai_model_turn_states
                  where account_id = $1 and effective_model = $2",
            )
            .bind(ACCOUNT_ID)
            .bind(MODEL)
            .fetch_one(&database.pool)
            .await
            .expect("load expired-request signal");
            if requested {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("shortened effective deadline queues the first request");

    store
        .maintain_model_turn_state_candidates(8)
        .await
        .expect("clear candidate expired by the shortened window");
    let candidates = store
        .model_capture_candidates(None, 8)
        .await
        .expect("load queued capture after candidate cleanup");
    assert_eq!(candidates.len(), 1, "duplicate request signals coalesce");
    let state = store
        .load_model_state(ACCOUNT_ID, MODEL)
        .await
        .expect("load state after effective expiry maintenance");
    assert!(state.candidate.is_none());
    assert_eq!(state.capture_trigger_mode, shortened.capture_trigger_mode);
    assert!(pinned.pin.is_some());

    writer_cancellation.cancel();
    writer_task
        .await
        .expect("writer task")
        .expect("writer shutdown");
    database.close().await;
}

#[tokio::test]
async fn proactive_capture_stages_candidate_and_promotes_without_extending_candidate_ttl() {
    let Some(database) = TestDatabase::create("model_turn_state_candidate").await else {
        return;
    };
    seed_account(&database.pool, "acct_candidate", "openai").await;
    sqlx::query(
        "insert into outbound_proxies
           (id, name, proxy_url, last_test_at, last_test_success, last_test_latency_ms,
            last_test_message)
         values ('proxy_candidate', 'Candidate', 'socks5://127.0.0.1:823',
                 now(), true, 1, 'ok')",
    )
    .execute(&database.pool)
    .await
    .expect("seed capture proxy");
    let (store, _writer) = PgTurnStateStore::initialize(database.pool.clone())
        .await
        .expect("initialize candidate store");
    let initial_policy = store
        .load_account_policy("acct_candidate")
        .await
        .expect("load candidate policy");
    let policy = store
        .update_account_policy(
            "acct_candidate",
            AccountTurnStatePolicyUpdate {
                lock_enabled: true,
                capture_enabled: true,
                reuse_window_seconds: 3_600,
                refresh_lead_seconds: 900,
                capture_trigger_mode: ModelTurnStateCaptureTriggerMode::BeforeExpiryIfUsed,
                capture_proxy_id: Some("proxy_candidate".to_owned()),
                ..account_policy_update(&initial_policy)
            },
        )
        .await
        .expect("configure proactive capture");
    let empty = store
        .load_model_state("acct_candidate", "gpt-5.6-sol")
        .await
        .expect("load candidate scope");
    let active_value = "A".repeat(292);
    store
        .update_model_state(
            "acct_candidate",
            "gpt-5.6-sol",
            ModelTurnStateUpdate {
                expected_identity_revision: empty.identity_revision,
                expected_effective_model: empty.effective_model.clone(),
                lock_enabled: true,
                capture_enabled: true,
                reuse_window_seconds: 3_600,
                capture_proxy_id: policy.capture_proxy_id.clone(),
                max_attempts: policy.capture_policy.max_attempts,
                attempt_timeout_seconds: policy.capture_policy.attempt_timeout_seconds,
                job_timeout_seconds: policy.capture_policy.job_timeout_seconds,
                backoff_seconds: policy.capture_policy.backoff_seconds,
                max_backoff_seconds: policy.capture_policy.max_backoff_seconds,
                cooldown_seconds: policy.capture_policy.cooldown_seconds,
                pin_action: ModelTurnStatePinAction::Replace,
                value: Some(active_value.clone()),
                expected_revision: empty.config_revision,
            },
        )
        .await
        .expect("seed active value");
    sqlx::query(
        "update openai_model_turn_states
            set pin_captured_at = now() - interval '45 minutes',
                pin_reuse_deadline = now() + interval '15 minutes',
                active_activated_at = now() - interval '45 minutes'
          where account_id = 'acct_candidate' and effective_model = 'gpt-5.6-sol'",
    )
    .execute(&database.pool)
    .await
    .expect("move active into refresh lead");
    let (store, _writer) = PgTurnStateStore::initialize(database.pool.clone())
        .await
        .expect("restart candidate store");
    assert!(
        store
            .model_capture_candidates(None, 8)
            .await
            .expect("unused active must not schedule proactive capture")
            .is_empty()
    );
    sqlx::query(
        "update openai_model_turn_states
            set active_sent_count = 1,
                active_last_sent_at = now() - interval '30 minutes'
          where account_id = 'acct_candidate' and effective_model = 'gpt-5.6-sol'",
    )
    .execute(&database.pool)
    .await
    .expect("mark active as actually sent");
    let scopes = store
        .model_capture_candidates(None, 8)
        .await
        .expect("scan proactive candidate");
    assert_eq!(scopes.len(), 1, "45 minutes enters a 15 minute lead");
    let candidate_value = "C".repeat(292);
    let staged = store
        .commit_model_capture(&scopes[0], &candidate_value)
        .await
        .expect("stage candidate while active remains valid");
    assert_eq!(
        staged.pin.as_ref().map(|pin| pin.value.as_str()),
        Some(active_value.as_str())
    );
    let candidate = staged.candidate.as_ref().expect("candidate staged");
    assert_eq!(candidate.value, candidate_value);
    let candidate_captured_at = candidate.captured_at;
    let candidate_deadline = candidate.reuse_deadline;
    let candidate_generation = candidate.generation;
    let candidate_id = candidate.id.clone().expect("candidate ID");
    let candidate_sha256 = candidate.sha256.clone();
    assert_eq!(
        candidate_deadline,
        candidate_captured_at + Duration::hours(1)
    );
    assert_eq!(
        staged.next_activation_at,
        staged.pin.as_ref().map(|pin| pin.reuse_deadline)
    );

    sqlx::query(
        "update openai_model_turn_states
            set pin_reuse_deadline = now() - interval '1 second'
          where account_id = 'acct_candidate' and effective_model = 'gpt-5.6-sol'",
    )
    .execute(&database.pool)
    .await
    .expect("expire active value");
    store
        .maintain_model_turn_state_candidates(8)
        .await
        .expect("background maintenance promotes due candidate");
    let hot = store
        .active_model_pin("acct_candidate", 1, "gpt-5.6-sol")
        .expect("background maintenance publishes promoted cache");
    assert_eq!(hot.value, candidate_value);
    assert_eq!(hot.candidate_id.as_deref(), Some(candidate_id.as_str()));
    let promoted = store
        .load_model_state("acct_candidate", "gpt-5.6-sol")
        .await
        .expect("promote due candidate");
    assert_eq!(
        promoted.pin.as_ref().map(|pin| pin.value.as_str()),
        Some(candidate_value.as_str())
    );
    assert_eq!(
        promoted.pin.as_ref().map(|pin| pin.captured_at),
        Some(candidate_captured_at)
    );
    assert_eq!(
        promoted.pin.as_ref().map(|pin| pin.reuse_deadline),
        Some(candidate_deadline)
    );
    assert!(promoted.candidate.is_none());
    let promoted_revision = promoted.config_revision;
    store
        .maintain_model_turn_state_candidates(8)
        .await
        .expect("second maintenance round is idempotent");
    let (restarted, _writer) = PgTurnStateStore::initialize(database.pool.clone())
        .await
        .expect("restart after persisted promotion");
    let restarted_view = restarted
        .load_model_state("acct_candidate", "gpt-5.6-sol")
        .await
        .expect("load promoted state after restart");
    assert_eq!(restarted_view.config_revision, promoted_revision);
    assert_eq!(
        restarted
            .active_model_pin("acct_candidate", 1, "gpt-5.6-sol")
            .map(|pin| pin.value),
        Some(candidate_value)
    );
    assert!(
        restarted
            .invalidate_active_model_pin(
                "acct_candidate",
                1,
                "gpt-5.6-sol",
                candidate_generation,
                Some(&candidate_id),
                &candidate_sha256,
            )
            .await
            .expect("in-flight candidate rejection matches after persistent promotion")
    );
    database.close().await;
}

#[tokio::test]
async fn model_update_rejects_stale_identity_and_effective_model_even_at_same_revision() {
    let Some(database) = TestDatabase::create("model_turn_state_scope_cas").await else {
        return;
    };
    seed_account(&database.pool, "acct_scope_cas", "openai").await;
    sqlx::query(
        "update runtime_settings
            set model_mappings_json = '{\"public-codex\":\"upstream-a\"}'::jsonb
          where id = 1",
    )
    .execute(&database.pool)
    .await
    .expect("seed first mapping");
    let (store, _writer) = PgTurnStateStore::initialize(database.pool.clone())
        .await
        .expect("initialize model store");
    let read = store
        .load_model_state("acct_scope_cas", "public-codex")
        .await
        .expect("read initial scope");

    sqlx::query(
        "update runtime_settings
            set model_mappings_json = '{\"public-codex\":\"upstream-b\"}'::jsonb
          where id = 1",
    )
    .execute(&database.pool)
    .await
    .expect("switch mapping without changing model config revision");
    assert!(matches!(
        store
            .update_model_state(
                "acct_scope_cas",
                "public-codex",
                disabled_model_update(&read),
            )
            .await,
        Err(TurnStateStoreError::Conflict)
    ));

    sqlx::raw_sql(
        "update runtime_settings
            set model_mappings_json = '{\"public-codex\":\"upstream-a\"}'::jsonb;
         update provider_accounts
            set identity_revision = identity_revision + 1
          where id = 'acct_scope_cas'",
    )
    .execute(&database.pool)
    .await
    .expect("switch identity without changing old model config revision");
    assert!(matches!(
        store
            .update_model_state(
                "acct_scope_cas",
                "public-codex",
                disabled_model_update(&read),
            )
            .await,
        Err(TurnStateStoreError::Conflict)
    ));
    database.close().await;
}

#[tokio::test]
async fn ordinary_observation_and_capture_are_fenced_by_a_concurrent_manual_replace() {
    let Some(database) = TestDatabase::create("model_turn_state_observation_race").await else {
        return;
    };
    seed_account(&database.pool, "acct_observation_race", "openai").await;
    sqlx::query(
        "insert into outbound_proxies
           (id, name, proxy_url, last_test_at, last_test_success, last_test_latency_ms,
            last_test_message)
         values ('proxy_observation_race', 'Capture', 'http://127.0.0.1:823',
                 now(), true, 1, 'ok')",
    )
    .execute(&database.pool)
    .await
    .expect("seed capture proxy");
    let (store, writer) = PgTurnStateStore::initialize(database.pool.clone())
        .await
        .expect("initialize observation race store");
    let (writer_cancellation, writer_task) = start_writer(writer);
    store
        .load_model_state("acct_observation_race", "gpt-5.4")
        .await
        .expect("load initial model state");
    let initial_policy = store
        .load_account_policy("acct_observation_race")
        .await
        .expect("load initial account policy");
    store
        .update_account_policy(
            "acct_observation_race",
            AccountTurnStatePolicyUpdate {
                capture_enabled: true,
                capture_proxy_id: Some("proxy_observation_race".to_owned()),
                ..account_policy_update(&initial_policy)
            },
        )
        .await
        .expect("enable capture");
    let configured = store
        .load_model_state("acct_observation_race", "gpt-5.4")
        .await
        .expect("reload model under account capture policy");
    let observation_scope = store
        .model_observation_scope("acct_observation_race", 1, "gpt-5.4")
        .expect("empty configured scope accepts ordinary observation");
    sqlx::query(
        "update openai_model_turn_states set capture_requested_at = now()
          where account_id = 'acct_observation_race' and effective_model = 'gpt-5.4'",
    )
    .execute(&database.pool)
    .await
    .expect("seed pending automatic capture");
    let capture_scope = store
        .model_capture_candidates(None, 8)
        .await
        .expect("load pending capture")
        .pop()
        .expect("pending capture scope");

    let manual_value = synthetic_fernet_candidate();
    let manual = store
        .update_model_state(
            "acct_observation_race",
            "gpt-5.4",
            ModelTurnStateUpdate {
                expected_identity_revision: configured.identity_revision,
                expected_effective_model: configured.effective_model.clone(),
                lock_enabled: true,
                capture_enabled: true,
                reuse_window_seconds: configured.reuse_window_seconds,
                capture_proxy_id: configured.capture_proxy_id.clone(),
                max_attempts: configured.capture_policy.max_attempts,
                attempt_timeout_seconds: configured.capture_policy.attempt_timeout_seconds,
                job_timeout_seconds: configured.capture_policy.job_timeout_seconds,
                backoff_seconds: configured.capture_policy.backoff_seconds,
                max_backoff_seconds: configured.capture_policy.max_backoff_seconds,
                cooldown_seconds: configured.capture_policy.cooldown_seconds,
                pin_action: ModelTurnStatePinAction::Replace,
                value: Some(manual_value.clone()),
                expected_revision: configured.config_revision,
            },
        )
        .await
        .expect("manual replace wins race");

    store.enqueue_observation(TurnStateObservation {
        id: "obs_stale_after_manual".to_owned(),
        account_id: "acct_observation_race".to_owned(),
        request_id: None,
        attempt_index: None,
        value: manual_value.clone(),
        observed_at: Utc::now(),
        transport: "http".to_owned(),
        upstream_response_id: None,
        client_turn_id: None,
        model_scope: Some(observation_scope),
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let observed = store
                .load("acct_observation_race")
                .await
                .expect("load stale ordinary observation");
            if observed
                .observed
                .as_ref()
                .is_some_and(|value| value.id == "obs_stale_after_manual")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stale ordinary observation is persisted");
    writer_cancellation.cancel();
    writer_task
        .await
        .expect("join observation race writer")
        .expect("stop observation race writer");
    let after = store
        .load_model_state("acct_observation_race", "gpt-5.4")
        .await
        .expect("load state after stale observation");
    assert_eq!(after.config_revision, manual.config_revision);
    assert_eq!(
        after.pin.as_ref().map(|pin| pin.source.as_str()),
        Some("manual")
    );
    assert!(
        store
            .model_capture_candidates(None, 8)
            .await
            .expect("load capture signal after writer drain")
            .is_empty(),
        "manual replace clears the pending automatic capture signal"
    );
    assert!(matches!(
        store
            .commit_model_capture(&capture_scope, &manual_value)
            .await,
        Err(TurnStateStoreError::Conflict)
    ));

    database.close().await;
}

#[tokio::test]
async fn websocket_observation_bootstraps_empty_models_before_proxy_capture() {
    let Some(database) = TestDatabase::create("model_turn_state_first_observation").await else {
        return;
    };
    seed_account(&database.pool, "acct_first_observation", "openai").await;
    let (store, writer) = PgTurnStateStore::initialize(database.pool.clone())
        .await
        .expect("initialize first observation store");
    let (writer_cancellation, writer_task) = start_writer(writer);
    let policy = store
        .load_account_policy("acct_first_observation")
        .await
        .expect("load account policy");
    let policy = store
        .update_account_policy(
            "acct_first_observation",
            AccountTurnStatePolicyUpdate {
                lock_enabled: true,
                capture_enabled: true,
                ..account_policy_update(&policy)
            },
        )
        .await
        .expect("enable account policy without opening a model");
    assert_eq!(
        policy.capture_trigger_mode,
        ModelTurnStateCaptureTriggerMode::OnAttributedFailure,
        "bootstrap observations are independent of the later rotation trigger"
    );

    let first_scope = store
        .model_observation_scope("acct_first_observation", 1, "brand-new-292")
        .expect("new effective model receives account-policy scope");
    assert_eq!(first_scope.config_revision, 0);
    assert_eq!(first_scope.policy_revision, policy.config_revision);
    let valid = synthetic_fernet_candidate();
    store.enqueue_observation(TurnStateObservation {
        id: "obs_first_non292_before_292".to_owned(),
        account_id: "acct_first_observation".to_owned(),
        request_id: None,
        attempt_index: None,
        value: "short-before-valid".to_owned(),
        observed_at: Utc::now(),
        transport: "websocket".to_owned(),
        upstream_response_id: None,
        client_turn_id: None,
        model_scope: Some(first_scope.clone()),
    });
    let before_other: (i64, chrono::DateTime<Utc>, Option<String>, Option<String>) =
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let state = sqlx::query_as::<
                    _,
                    (i64, chrono::DateTime<Utc>, Option<String>, Option<String>),
                >(
                    "select config_revision, capture_requested_at,
                            pin_value, initial_observation_scope_id
                       from openai_model_turn_states
                      where account_id = 'acct_first_observation'
                        and effective_model = 'brand-new-292'",
                )
                .fetch_optional(&database.pool)
                .await
                .expect("load initial observation scope state");
                if let Some(state) = state
                    && state.0 == 1
                    && state.2.is_none()
                    && state.3.as_deref() == Some(first_scope.scope_id.as_str())
                {
                    break state;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("non-292 observation creates its model row and capture signal");

    let mut other_scope = first_scope.clone();
    other_scope.scope_id = "different-request-scope".to_owned();
    store.enqueue_observation(TurnStateObservation {
        id: "obs_other_scope_292".to_owned(),
        account_id: "acct_first_observation".to_owned(),
        request_id: None,
        attempt_index: None,
        value: valid.clone(),
        observed_at: Utc::now(),
        transport: "websocket".to_owned(),
        upstream_response_id: None,
        client_turn_id: None,
        model_scope: Some(other_scope),
    });
    store.enqueue_observation(TurnStateObservation {
        id: "obs_other_scope_barrier".to_owned(),
        account_id: "acct_first_observation".to_owned(),
        request_id: None,
        attempt_index: None,
        value: "writer-barrier".to_owned(),
        observed_at: Utc::now(),
        transport: "websocket".to_owned(),
        upstream_response_id: None,
        client_turn_id: None,
        model_scope: None,
    });
    wait_for_observation(&store, "acct_first_observation", "obs_other_scope_barrier").await;
    let after_other: (i64, chrono::DateTime<Utc>, Option<String>, Option<String>) = sqlx::query_as(
        "select config_revision, capture_requested_at,
                    pin_value, initial_observation_scope_id
               from openai_model_turn_states
              where account_id = 'acct_first_observation'
                and effective_model = 'brand-new-292'",
    )
    .fetch_one(&database.pool)
    .await
    .expect("load state after different scope was processed");
    assert_eq!(after_other, before_other);
    assert_eq!(
        store.active_model_pin("acct_first_observation", 1, "brand-new-292"),
        None,
        "a different config-revision-zero scope cannot adopt the row"
    );

    store.enqueue_observation(TurnStateObservation {
        id: "obs_first_292".to_owned(),
        account_id: "acct_first_observation".to_owned(),
        request_id: None,
        attempt_index: None,
        value: valid.clone(),
        observed_at: Utc::now(),
        transport: "websocket".to_owned(),
        upstream_response_id: None,
        client_turn_id: None,
        model_scope: Some(first_scope),
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let learned: (Option<String>, bool, bool, Option<String>) = sqlx::query_as(
                "select pin_value, capture_requested_at is null,
                        capture_not_before is null, initial_observation_scope_id
                   from openai_model_turn_states
                  where account_id = 'acct_first_observation'
                    and effective_model = 'brand-new-292'",
            )
            .fetch_one(&database.pool)
            .await
            .expect("load same-scope learned state");
            if learned.0.as_deref() == Some(valid.as_str())
                && learned.1
                && learned.2
                && learned.3.is_none()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("same scope upgrades the model and clears its capture signal");
    assert_eq!(
        store
            .active_model_pin("acct_first_observation", 1, "brand-new-292")
            .map(|pin| pin.value),
        Some(valid)
    );
    let non292_scope = store
        .model_observation_scope("acct_first_observation", 1, "brand-new-non292")
        .expect("second new model receives independent scope");
    assert_eq!(non292_scope.config_revision, 0);
    store.enqueue_observation(TurnStateObservation {
        id: "obs_first_non292".to_owned(),
        account_id: "acct_first_observation".to_owned(),
        request_id: None,
        attempt_index: None,
        value: "short".to_owned(),
        observed_at: Utc::now(),
        transport: "websocket".to_owned(),
        upstream_response_id: None,
        client_turn_id: None,
        model_scope: Some(non292_scope),
    });
    for _ in 0..100 {
        let requested: Option<bool> = sqlx::query_scalar(
            "select capture_requested_at is not null
               from openai_model_turn_states
              where account_id = 'acct_first_observation'
                and effective_model = 'brand-new-non292'",
        )
        .fetch_optional(&database.pool)
        .await
        .expect("load non-292 model row");
        if requested == Some(true) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        sqlx::query_scalar::<_, bool>(
            "select capture_requested_at is not null
               from openai_model_turn_states
              where account_id = 'acct_first_observation'
                and effective_model = 'brand-new-non292'",
        )
        .fetch_one(&database.pool)
        .await
        .expect("non-292 model row exists")
    );
    writer_cancellation.cancel();
    writer_task
        .await
        .expect("writer task")
        .expect("writer shutdown");
    database.close().await;
}

#[tokio::test]
async fn automatic_capture_candidates_page_past_the_first_sixty_four_scopes() {
    let Some(database) = TestDatabase::create("model_turn_state_candidate_cursor").await else {
        return;
    };
    seed_account(&database.pool, "acct_candidate_cursor", "openai").await;
    sqlx::query(
        "insert into outbound_proxies
           (id, name, proxy_url, last_test_at, last_test_success, last_test_latency_ms,
            last_test_message)
         values ('proxy_cursor', 'Cursor', 'http://127.0.0.1:8080', now(), true, 1, 'ok')",
    )
    .execute(&database.pool)
    .await
    .expect("seed candidate proxy");
    sqlx::query(
        "insert into openai_account_turn_state_policies
           (account_id, identity_revision, capture_enabled, capture_proxy_id)
         values ('acct_candidate_cursor', 1, true, 'proxy_cursor')",
    )
    .execute(&database.pool)
    .await
    .expect("seed account capture policy");
    for index in 0..65 {
        sqlx::query(
            "insert into openai_model_turn_states
               (account_id, identity_revision, effective_model, capture_requested_at)
             values ('acct_candidate_cursor', 1, $1, now())",
        )
        .bind(format!("model-{index:03}"))
        .execute(&database.pool)
        .await
        .expect("seed automatic capture candidate");
    }
    let (store, _writer) = PgTurnStateStore::initialize(database.pool.clone())
        .await
        .expect("initialize model store");
    let first = store
        .model_capture_candidates(None, 64)
        .await
        .expect("load first bounded candidate page");
    assert_eq!(first.len(), 64);
    let cursor = first.last().expect("first page tail").cursor();
    let second = store
        .model_capture_candidates(Some(&cursor), 64)
        .await
        .expect("load next bounded candidate page");
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].effective_model, "model-064");
    database.close().await;
}

fn disabled_model_update(
    view: &gateway_core::provider_ports::turn_state::ModelTurnStateView,
) -> ModelTurnStateUpdate {
    ModelTurnStateUpdate {
        expected_identity_revision: view.identity_revision,
        expected_effective_model: view.effective_model.clone(),
        lock_enabled: false,
        capture_enabled: false,
        reuse_window_seconds: view.reuse_window_seconds,
        capture_proxy_id: None,
        max_attempts: view.capture_policy.max_attempts,
        attempt_timeout_seconds: view.capture_policy.attempt_timeout_seconds,
        job_timeout_seconds: view.capture_policy.job_timeout_seconds,
        backoff_seconds: view.capture_policy.backoff_seconds,
        max_backoff_seconds: view.capture_policy.max_backoff_seconds,
        cooldown_seconds: view.capture_policy.cooldown_seconds,
        pin_action: ModelTurnStatePinAction::Keep,
        value: None,
        expected_revision: view.config_revision,
    }
}

fn account_policy_update(
    view: &gateway_core::provider_ports::turn_state::AccountTurnStatePolicyView,
) -> AccountTurnStatePolicyUpdate {
    AccountTurnStatePolicyUpdate {
        expected_identity_revision: view.identity_revision,
        expected_revision: view.config_revision,
        lock_enabled: view.lock_enabled,
        capture_enabled: view.capture_enabled,
        reuse_window_seconds: view.reuse_window_seconds,
        refresh_lead_seconds: view.refresh_lead_seconds,
        capture_trigger_mode: view.capture_trigger_mode,
        capture_proxy_id: view.capture_proxy_id.clone(),
        max_attempts: view.capture_policy.max_attempts,
        attempt_timeout_seconds: view.capture_policy.attempt_timeout_seconds,
        job_timeout_seconds: view.capture_policy.job_timeout_seconds,
        backoff_seconds: view.capture_policy.backoff_seconds,
        max_backoff_seconds: view.capture_policy.max_backoff_seconds,
        cooldown_seconds: view.capture_policy.cooldown_seconds,
    }
}

fn synthetic_fernet_candidate() -> String {
    let mut raw = Vec::with_capacity(217);
    raw.push(0x80);
    raw.extend_from_slice(&1_789_650_773_u64.to_be_bytes());
    raw.extend_from_slice(&[0x11; 16]);
    raw.extend_from_slice(&[0x22; 160]);
    raw.extend_from_slice(&[0x33; 32]);
    assert_eq!(raw.len(), 217);
    url_safe_base64(&raw)
}

fn url_safe_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut chunks = bytes.chunks_exact(3);
    for chunk in &mut chunks {
        encoded.push(char::from(ALPHABET[usize::from(chunk[0] >> 2)]));
        encoded.push(char::from(
            ALPHABET[usize::from((chunk[0] & 0x03) << 4 | chunk[1] >> 4)],
        ));
        encoded.push(char::from(
            ALPHABET[usize::from((chunk[1] & 0x0f) << 2 | chunk[2] >> 6)],
        ));
        encoded.push(char::from(ALPHABET[usize::from(chunk[2] & 0x3f)]));
    }
    match chunks.remainder() {
        [first] => {
            encoded.push(char::from(ALPHABET[usize::from(*first >> 2)]));
            encoded.push(char::from(ALPHABET[usize::from((*first & 0x03) << 4)]));
            encoded.push_str("==");
        }
        [first, second] => {
            encoded.push(char::from(ALPHABET[usize::from(*first >> 2)]));
            encoded.push(char::from(
                ALPHABET[usize::from((*first & 0x03) << 4 | *second >> 4)],
            ));
            encoded.push(char::from(ALPHABET[usize::from((*second & 0x0f) << 2)]));
            encoded.push('=');
        }
        [] => {}
        _ => unreachable!("chunks_exact remainder is shorter than three bytes"),
    }
    encoded
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
        model_scope: None,
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
