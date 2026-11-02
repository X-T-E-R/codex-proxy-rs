//! 验证历史删除与诊断清除的轮转、批次预算、时长限制与取消行为

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gateway_admin::{
    model::retention::{RetentionPolicy, RetentionTarget},
    ports::{
        retention::RetentionStore,
        store::{AdminStoreError, AdminStoreErrorKind, AdminStoreResult},
    },
};
use gateway_core::{
    lifecycle::CancellationToken,
    task::{ScheduledTask, WorkerCycleContext, WorkerId, WorkerKind},
};
use gateway_host::retention::{RetentionCycleBudget, RetentionTask};
use std::{
    num::NonZeroU32,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Store {
    calls: Mutex<Vec<RetentionTarget>>,
    limits: Mutex<Vec<u32>>,
    fail: bool,
    fail_load: bool,
    full: bool,
    full_target: Option<RetentionTarget>,
    block_load: bool,
    block_target: Option<RetentionTarget>,
}

#[async_trait]
impl RetentionStore for Store {
    async fn load_policy(&self) -> AdminStoreResult<RetentionPolicy> {
        if self.block_load {
            std::future::pending::<()>().await;
        }
        if self.fail_load {
            return Err(AdminStoreError::new(
                AdminStoreErrorKind::Unavailable,
                "retention settings",
                "fixture settings failure",
            ));
        }
        Ok(RetentionPolicy::try_new(31, 7, 90).unwrap())
    }
    async fn purge_batch(
        &self,
        target: RetentionTarget,
        _: DateTime<Utc>,
        _: RetentionPolicy,
        limit: NonZeroU32,
    ) -> AdminStoreResult<u64> {
        self.calls.lock().unwrap().push(target);
        self.limits.lock().unwrap().push(limit.get());
        if self.block_target == Some(target) {
            std::future::pending::<()>().await;
        }
        if self.fail {
            return Err(AdminStoreError::new(
                AdminStoreErrorKind::Unavailable,
                "retention",
                "fixture",
            ));
        }
        Ok(if self.full || self.full_target == Some(target) {
            u64::from(limit.get())
        } else {
            0
        })
    }
}

fn context(cancellation: CancellationToken) -> WorkerCycleContext {
    WorkerCycleContext::new(
        WorkerId::try_new(WorkerKind::Retention, "postgres").unwrap(),
        None,
        cancellation,
    )
}

fn budget() -> RetentionCycleBudget {
    RetentionCycleBudget {
        batch_rows: NonZeroU32::new(2).unwrap(),
        max_batches: NonZeroU32::new(6).unwrap(),
        max_duration: Duration::from_secs(1),
        batch_pause: Duration::ZERO,
    }
}

#[tokio::test]
async fn full_tables_rotate_until_batch_budget_is_exhausted() {
    let store = Arc::new(Store {
        full: true,
        ..Store::default()
    });
    RetentionTask::new(store.clone(), budget())
        .run_cycle(context(CancellationToken::new()))
        .await
        .unwrap();
    assert_eq!(
        *store.calls.lock().unwrap(),
        [
            RetentionTarget::ModelRequests,
            RetentionTarget::OpsEvents,
            RetentionTarget::AdminAuditEvents,
            RetentionTarget::SuccessfulDiagnosticTraces,
            RetentionTarget::ModelRequests,
            RetentionTarget::OpsEvents,
        ]
    );
}

#[tokio::test]
async fn successful_trace_backlog_uses_target_limit_and_remaining_cycle_budget() {
    for (budget, expected_limits) in [
        (budget(), vec![2; 6]),
        (
            RetentionCycleBudget::default(),
            [vec![5_000; 3], vec![100; 9]].concat(),
        ),
    ] {
        let store = Arc::new(Store {
            full_target: Some(RetentionTarget::SuccessfulDiagnosticTraces),
            ..Store::default()
        });
        RetentionTask::new(store.clone(), budget)
            .run_cycle(context(CancellationToken::new()))
            .await
            .unwrap();
        let mut expected_targets = vec![
            RetentionTarget::ModelRequests,
            RetentionTarget::OpsEvents,
            RetentionTarget::AdminAuditEvents,
        ];
        expected_targets.resize(
            expected_limits.len(),
            RetentionTarget::SuccessfulDiagnosticTraces,
        );
        assert_eq!(*store.calls.lock().unwrap(), expected_targets);
        assert_eq!(*store.limits.lock().unwrap(), expected_limits);
    }
}

#[tokio::test]
async fn store_failures_keep_safe_message_and_internal_error_details() {
    for (fail, fail_load, message, detail) in [
        (true, false, "retention cleanup failed", "fixture"),
        (
            false,
            true,
            "retention settings read failed",
            "fixture settings failure",
        ),
    ] {
        let store = Arc::new(Store {
            fail,
            fail_load,
            ..Store::default()
        });
        let error = RetentionTask::new(store, budget())
            .run_cycle(context(CancellationToken::new()))
            .await
            .unwrap_err();
        assert_eq!(error.as_safe_str(), message);
        assert!(error.error_details().unwrap().as_str().contains(detail));
    }
}

#[tokio::test]
async fn empty_tables_are_not_revisited_and_errors_stop_the_cycle() {
    let store = Arc::new(Store::default());
    RetentionTask::new(store.clone(), budget())
        .run_cycle(context(CancellationToken::new()))
        .await
        .unwrap();
    assert_eq!(store.calls.lock().unwrap().len(), 4);
    let store = Arc::new(Store {
        fail: true,
        ..Store::default()
    });
    assert!(
        RetentionTask::new(store.clone(), budget())
            .run_cycle(context(CancellationToken::new()))
            .await
            .is_err()
    );
    assert_eq!(store.calls.lock().unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn duration_budget_stops_cleanup_during_batch_pause() {
    let store = Arc::new(Store {
        full: true,
        ..Store::default()
    });
    let mut budget = budget();
    budget.batch_pause = Duration::from_secs(2);
    RetentionTask::new(store.clone(), budget)
        .run_cycle(context(CancellationToken::new()))
        .await
        .unwrap();
    assert_eq!(store.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cancellation_interrupts_settings_read_before_any_delete() {
    let store = Arc::new(Store {
        block_load: true,
        ..Store::default()
    });
    let token = CancellationToken::new();
    let task = RetentionTask::new(store.clone(), budget());
    let run = task.run_cycle(context(token.clone()));
    tokio::pin!(run);
    tokio::select! {
        result = &mut run => panic!("unexpected completion: {result:?}"),
        () = tokio::task::yield_now() => token.cancel(),
    }
    run.await.unwrap();
    assert!(store.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancellation_interrupts_successful_trace_cleanup() {
    let store = Arc::new(Store {
        block_target: Some(RetentionTarget::SuccessfulDiagnosticTraces),
        ..Store::default()
    });
    let token = CancellationToken::new();
    let task = RetentionTask::new(store.clone(), budget());
    let run = task.run_cycle(context(token.clone()));
    tokio::pin!(run);
    tokio::select! {
        result = &mut run => panic!("unexpected completion: {result:?}"),
        () = tokio::task::yield_now() => token.cancel(),
    }
    run.await.unwrap();
    assert_eq!(
        *store.calls.lock().unwrap(),
        [
            RetentionTarget::ModelRequests,
            RetentionTarget::OpsEvents,
            RetentionTarget::AdminAuditEvents,
            RetentionTarget::SuccessfulDiagnosticTraces,
        ]
    );
}
