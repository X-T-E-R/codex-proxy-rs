//! 验证并发等待队列的容量、顺序、优先级、超时与取消回收

use std::time::{Duration, Instant, SystemTime};

use futures::{FutureExt, executor::block_on};
use gateway_core::concurrency::{
    CapacityWait, ConcurrencyQueuePolicy, ConcurrencyWaitBudget, ConcurrencyWaitQueue,
    QueueRejection, WaitPriority,
};

#[test]
fn waiting_is_bounded_per_owner_and_cancelled_head_wakes_the_next_request() {
    block_on(async {
        let queue = ConcurrencyWaitQueue::default();
        let deadline = Instant::now() + Duration::from_secs(2);
        let first = queue.enqueue(&["key_a"], 2, deadline).unwrap();
        let second = queue.enqueue(&["key_a"], 2, deadline).unwrap();
        assert!(matches!(
            queue.enqueue(&["key_a"], 2, deadline),
            Err(QueueRejection::Full)
        ));
        let independent = queue.enqueue(&["key_b"], 2, deadline).unwrap();
        assert!(second.turn().now_or_never().is_none());
        independent.turn().await.unwrap();
        first.turn().await.unwrap();
        drop(first);
        second.turn().await.unwrap();
        drop(second);
        assert!(!queue.has_waiters(&"key_a"));
    });
}

#[test]
fn cancelling_a_middle_waiter_preserves_fifo_and_reclaims_capacity() {
    block_on(async {
        let queue = ConcurrencyWaitQueue::default();
        let deadline = Instant::now() + Duration::from_secs(2);
        let head = queue.enqueue(&["account"], 3, deadline).unwrap();
        let cancelled = queue.enqueue(&["account"], 3, deadline).unwrap();
        let next = queue.enqueue(&["account"], 3, deadline).unwrap();
        drop(cancelled);
        let tail = queue.enqueue(&["account"], 3, deadline).unwrap();
        drop(head);
        next.turn().await.unwrap();
        assert!(tail.turn().now_or_never().is_none());
        drop(next);
        tail.turn().await.unwrap();
    });
}

#[test]
fn wait_deadline_applies_before_and_after_becoming_head() {
    block_on(async {
        let queue = ConcurrencyWaitQueue::default();
        let deadline = Instant::now() + Duration::from_millis(20);
        let head = queue.enqueue(&["account"], 2, deadline).unwrap();
        let tail = queue.enqueue(&["account"], 2, deadline).unwrap();
        assert_eq!(tail.turn().await, Err(QueueRejection::Timeout));
        assert_eq!(head.turn().await, Err(QueueRejection::Timeout));
        drop((head, tail));
        assert!(!queue.has_waiters(&"account"));
    });
}

#[test]
fn a_full_account_queue_does_not_prevent_waiting_on_another_eligible_account() {
    let queue = ConcurrencyWaitQueue::default();
    let deadline = Instant::now() + Duration::from_secs(2);
    let _first = queue.enqueue(&["a"], 1, deadline).unwrap();
    let second = queue.enqueue(&["a", "b"], 1, deadline).unwrap();
    assert_eq!(*second.key(), "b");
    assert!(matches!(
        queue.enqueue(&["a", "b"], 1, deadline),
        Err(QueueRejection::Full)
    ));
}

#[test]
fn request_deadline_bounds_waiting_and_dropped_future_releases_its_ticket() {
    block_on(async {
        let queue = ConcurrencyWaitQueue::default();
        let policy = ConcurrencyQueuePolicy {
            max_waiting: 1,
            timeout: Duration::from_secs(30),
        };
        let budget = ConcurrencyWaitBudget::default();
        let mut waiting = CapacityWait::new(
            &queue,
            policy,
            SystemTime::now() + Duration::from_millis(20),
            &budget,
        );
        assert_eq!(waiting.wait(&["a"]).await, Err(QueueRejection::Timeout));
        drop(waiting);
        assert!(!queue.has_waiters(&"a"));

        let budget = ConcurrencyWaitBudget::default();
        let mut waiting = CapacityWait::new(
            &queue,
            policy,
            SystemTime::now() + Duration::from_secs(2),
            &budget,
        );
        assert!(waiting.wait(&["a"]).now_or_never().is_none());
        assert!(queue.has_waiters(&"a"));
        drop(waiting);
        assert!(!queue.has_waiters(&"a"));
    });
}

#[test]
fn new_layers_and_retries_cannot_restart_a_requests_wait_budget() {
    block_on(async {
        let keys = ConcurrencyWaitQueue::default();
        let accounts = ConcurrencyWaitQueue::default();
        let policy = ConcurrencyQueuePolicy {
            max_waiting: 1,
            timeout: Duration::from_millis(300),
        };
        let request_deadline = SystemTime::now() + Duration::from_secs(5);
        let budget = ConcurrencyWaitBudget::default();
        let mut key_wait = CapacityWait::new(&keys, policy, request_deadline, &budget);
        key_wait.wait(&["key"]).await.unwrap();
        drop(key_wait);

        // 第一层已取得重试机会，后续准备跨过总时限后不能在账号层重新排队
        futures_timer::Delay::new(policy.timeout).await;
        let next_attempt_budget = budget.clone();
        let mut account_wait =
            CapacityWait::new(&accounts, policy, request_deadline, &next_attempt_budget);
        assert_eq!(
            account_wait.wait(&["account"]).now_or_never(),
            Some(Err(QueueRejection::Timeout))
        );
        assert!(!accounts.has_waiters(&"account"));

        // 另一个请求仍有完整预算，不能受上一请求超时影响
        let independent_budget = ConcurrencyWaitBudget::default();
        let mut independent =
            CapacityWait::new(&accounts, policy, request_deadline, &independent_budget);
        independent.wait(&["account"]).await.unwrap();
    });
}

#[test]
fn creating_a_waiter_does_not_start_the_budget_before_queueing() {
    block_on(async {
        let queue = ConcurrencyWaitQueue::default();
        let policy = ConcurrencyQueuePolicy {
            max_waiting: 1,
            timeout: Duration::from_millis(300),
        };
        let budget = ConcurrencyWaitBudget::default();
        let mut waiting = CapacityWait::new(
            &queue,
            policy,
            SystemTime::now() + Duration::from_secs(5),
            &budget,
        );
        futures_timer::Delay::new(policy.timeout).await;
        waiting.wait(&["account"]).await.unwrap();
    });
}

#[test]
fn custom_priority_reads_live_counts_and_preserves_limits_and_fifo() {
    block_on(async {
        let queue = ConcurrencyWaitQueue::default();
        let deadline = Instant::now() + Duration::from_secs(2);
        let head = queue.enqueue(&["a"], 2, deadline).unwrap();
        let mut observed = Vec::new();
        let tail = queue
            .enqueue_with_priority(&["a", "b"], 2, deadline, |key, count| {
                observed.push((*key, count));
                if *key == "a" { 10.0 } else { 0.0 }
            })
            .unwrap();
        assert_eq!(observed, [("a", 1), ("b", 0)]);
        assert_eq!(*tail.key(), "a");
        assert!(tail.turn().now_or_never().is_none());
        // 满队列不参与评分，不能因偏好而突破上限
        let fallback = queue
            .enqueue_with_priority(&["a", "b"], 2, deadline, |key, count| {
                assert_eq!((*key, count), ("b", 0));
                0.0
            })
            .unwrap();
        assert_eq!(*fallback.key(), "b");
        drop(head);
        tail.turn().await.unwrap();
        drop((tail, fallback));
        let tie = queue
            .enqueue_with_priority(&["b", "a"], 2, deadline, |_, _| 1.0)
            .unwrap();
        assert_eq!(*tie.key(), "b");
    });
}

#[test]
fn changed_scores_keep_existing_position_until_account_becomes_ineligible() {
    block_on(async {
        let queue = ConcurrencyWaitQueue::default();
        let budget = ConcurrencyWaitBudget::default();
        let policy = ConcurrencyQueuePolicy {
            max_waiting: 1,
            timeout: Duration::from_secs(2),
        };
        let mut waiting = CapacityWait::new(
            &queue,
            policy,
            SystemTime::now() + Duration::from_secs(2),
            &budget,
        );
        waiting
            .wait_with_priority(&["a", "b"], |key, _| if *key == "a" { 1.0 } else { 0.0 })
            .await
            .unwrap();
        waiting
            .wait_with_priority(&["a", "b"], |_, _| {
                panic!("existing ticket must not be rescored")
            })
            .await
            .unwrap();
        assert!(queue.has_waiters(&"a"));
        assert!(!queue.has_waiters(&"b"));
        waiting
            .wait_with_priority(&["b"], |_, _| 1.0)
            .await
            .unwrap();
        assert!(!queue.has_waiters(&"a"));
        assert!(queue.has_waiters(&"b"));
        drop(waiting);
        assert!(!queue.has_waiters(&"b"));
    });
}

#[test]
fn high_priority_waiters_go_ahead_of_normal_waiters_and_keep_fifo_among_themselves() {
    block_on(async {
        let queue = ConcurrencyWaitQueue::default();
        let deadline = Instant::now() + Duration::from_secs(2);
        let policy = ConcurrencyQueuePolicy {
            max_waiting: 2,
            timeout: Duration::from_secs(2),
        };
        let request_deadline = SystemTime::now() + Duration::from_secs(2);
        let first_normal = queue.enqueue(&["a"], 2, deadline).unwrap();
        let second_normal = queue.enqueue(&["a"], 2, deadline).unwrap();

        let normal_budget = ConcurrencyWaitBudget::default();
        let normal = CapacityWait::new(&queue, policy, request_deadline, &normal_budget);
        let first_budget = ConcurrencyWaitBudget::default();
        let mut first_high = CapacityWait::new(&queue, policy, request_deadline, &first_budget)
            .with_priority(WaitPriority::High);
        // 只有普通等待者时，高优先级请求可以直接尝试租约，普通请求仍须排队
        assert!(first_high.can_try(&"a"));
        assert!(!normal.can_try(&"a"));

        // 普通等待者已占满单队列上限，高优先级仍可入队并直接成为队首
        first_high.wait(&["a"]).await.unwrap();
        let second_budget = ConcurrencyWaitBudget::default();
        let mut second_high = CapacityWait::new(&queue, policy, request_deadline, &second_budget)
            .with_priority(WaitPriority::High);
        assert!(!second_high.can_try(&"a"));
        assert!(second_high.wait(&["a"]).now_or_never().is_none());
        assert!(first_normal.turn().now_or_never().is_none());

        drop(first_high);
        second_high.wait(&["a"]).await.unwrap();
        assert!(first_normal.turn().now_or_never().is_none());
        drop(second_high);
        first_normal.turn().await.unwrap();
        drop(first_normal);
        second_normal.turn().await.unwrap();
    });
}

#[test]
fn displaced_normal_head_yields_its_lease_attempt_until_high_priority_waiters_leave() {
    block_on(async {
        let queue = ConcurrencyWaitQueue::default();
        let policy = ConcurrencyQueuePolicy {
            max_waiting: 2,
            timeout: Duration::from_secs(2),
        };
        let deadline = SystemTime::now() + Duration::from_secs(2);
        let normal_budget = ConcurrencyWaitBudget::default();
        let high_budget = ConcurrencyWaitBudget::default();
        let mut normal = CapacityWait::new(&queue, policy, deadline, &normal_budget);
        normal.wait(&["account"]).await.unwrap();
        assert!(normal.can_try(&"account"));

        // 普通队首已经开始重读容量，尚未取得租约时被高优先级请求插队
        let mut high = CapacityWait::new(&queue, policy, deadline, &high_budget)
            .with_priority(WaitPriority::High);
        high.wait(&["account"]).await.unwrap();
        assert!(!normal.can_try(&"account"));
        assert!(high.can_try(&"account"));
        assert!(normal.wait(&["account"]).now_or_never().is_none());

        // 审批取消也必须恢复原队首的资格，不能丢失其等待位置
        drop(high);
        normal.wait(&["account"]).await.unwrap();
        assert!(normal.can_try(&"account"));
        drop(normal);
        assert!(!queue.has_waiters(&"account"));
    });
}

#[test]
fn high_priority_waiters_still_require_queueing_to_be_enabled() {
    block_on(async {
        let queue = ConcurrencyWaitQueue::<&str>::default();
        let policy = ConcurrencyQueuePolicy {
            max_waiting: 0,
            timeout: Duration::from_secs(2),
        };
        let budget = ConcurrencyWaitBudget::default();
        let mut waiting = CapacityWait::new(
            &queue,
            policy,
            SystemTime::now() + Duration::from_secs(2),
            &budget,
        )
        .with_priority(WaitPriority::High);
        assert_eq!(waiting.wait(&["a"]).await, Err(QueueRejection::Full));
        assert!(!queue.has_waiters(&"a"));
    });
}

/// 已在队首的绑定会话仍需按 500ms 周期重查，不能立即返回形成忙循环
#[test]
fn bound_session_recheck_waits_about_the_poll_interval() {
    block_on(async {
        let queue = ConcurrencyWaitQueue::<&str>::default();
        let policy = ConcurrencyQueuePolicy {
            max_waiting: 1,
            timeout: Duration::from_secs(5),
        };
        let budget = ConcurrencyWaitBudget::default();
        let mut waiting = CapacityWait::new(
            &queue,
            policy,
            SystemTime::now() + Duration::from_secs(5),
            &budget,
        )
        .with_periodic_recheck(true);
        let started = Instant::now();
        waiting.wait(&["account"]).await.unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(500),
            "periodic recheck must not spin: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "periodic recheck must stay bounded: {elapsed:?}"
        );
        assert!(waiting.can_try(&"account"));
    });
}

/// 真实周期重查路径必须在本地交接后的下一次 poll 完成，不依赖计时精度
#[test]
fn periodic_capacity_wait_wakes_when_the_local_head_is_released() {
    use std::future::Future as _;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::task::{Context, Poll, Wake};

    #[derive(Default)]
    struct WakeFlag(AtomicBool);
    impl Wake for WakeFlag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    block_on(async {
        let queue = ConcurrencyWaitQueue::default();
        let head = queue
            .enqueue(&["account"], 2, Instant::now() + Duration::from_secs(5))
            .unwrap();
        let budget = ConcurrencyWaitBudget::default();
        let mut next = CapacityWait::new(
            &queue,
            ConcurrencyQueuePolicy {
                max_waiting: 2,
                timeout: Duration::from_secs(5),
            },
            None,
            &budget,
        )
        .with_periodic_recheck(true);
        {
            let wake = Arc::new(WakeFlag::default());
            let waker = std::task::Waker::from(Arc::clone(&wake));
            let mut context = Context::from_waker(&waker);
            let mut waiting = std::pin::pin!(next.wait(&["account"]));
            assert!(waiting.as_mut().poll(&mut context).is_pending());
            drop(head);
            assert!(wake.0.load(Ordering::SeqCst), "本地交接必须实际唤醒等待者");
            assert_eq!(waiting.as_mut().poll(&mut context), Poll::Ready(Ok(())));
        }
        assert!(next.can_try(&"account"));
        // 已在队首但仍无容量时必须等待下一个周期，不能因 turn() 就绪而空转
        assert!(next.wait(&["account"]).now_or_never().is_none());
        drop(next);
        assert!(!queue.has_waiters(&"account"), "取消释放等待位置");
    });
}

/// 排队预算短于重查周期时仍按原截止失败，不能等满 500ms 或取得后序账号资格
#[test]
fn periodic_capacity_wait_preserves_a_shorter_queue_deadline() {
    block_on(async {
        let queue = ConcurrencyWaitQueue::default();
        let head = queue
            .enqueue(&["account"], 2, Instant::now() + Duration::from_secs(5))
            .unwrap();
        let budget = ConcurrencyWaitBudget::default();
        let mut waiting = CapacityWait::new(
            &queue,
            ConcurrencyQueuePolicy {
                max_waiting: 2,
                timeout: Duration::from_millis(20),
            },
            None,
            &budget,
        )
        .with_periodic_recheck(true);
        assert_eq!(
            waiting.wait(&["account"]).await,
            Err(QueueRejection::Timeout)
        );
        assert!(!waiting.can_try(&"account"));
        drop(waiting);
        assert!(queue.has_waiters(&"account"));
        drop(head);
        assert!(!queue.has_waiters(&"account"));
    });
}
