//! Node 与 OpenAI harness 归档安全、安装指纹和同组回滚测试

use super::*;

fn runtime_path(fixture: &Fixture, name: &str) -> PathBuf {
    fixture.root.path().join(name)
}

#[tokio::test]
async fn runtime_bundle_installs_with_binary_and_rolls_back_to_legacy() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    fixture
        .mount_release(
            &server,
            TARGET_VERSION,
            ArchiveKind::Runtime,
            ChecksumKind::Valid,
        )
        .await;
    let service = fixture.service(&server);
    let installed = complete_update(&service, TARGET_VERSION).await;
    assert_eq!(
        installed.operation.status,
        SystemOperationStatus::Succeeded,
        "{installed:?}"
    );
    assert!(installed.need_restart);
    assert_eq!(
        fs::read(runtime_path(&fixture, "node")).unwrap(),
        b"node-v2"
    );
    assert_eq!(
        fs::read(runtime_path(&fixture, "openai-harness/worker/runner.mjs")).unwrap(),
        b"export default 'worker-v2'"
    );
    assert_eq!(
        fs::read(fixture.web().join("index.html")).unwrap(),
        b"new-web",
        "node_modules/dist must not be web"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_ne!(
            fs::metadata(runtime_path(&fixture, "node"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        assert_eq!(
            fs::metadata(runtime_path(&fixture, "openai-harness/worker/runner.mjs"))
                .unwrap()
                .permissions()
                .mode()
                & 0o222,
            0
        );
    }
    #[cfg(windows)]
    assert!(
        fs::metadata(runtime_path(&fixture, "openai-harness/worker/runner.mjs"))
            .unwrap()
            .permissions()
            .readonly()
    );
    let mut restarted_config = fixture.config(&format!("{}/repos", server.uri()));
    restarted_config.version = TARGET_VERSION.into();
    let restarted = ProcessSystemOperations::new(CancellationToken::new(), restarted_config);
    let resumed = restarted
        .update_status()
        .await
        .expect("current runtime identified");
    assert!(!resumed.need_restart);
    assert_eq!(resumed.current_version.as_deref(), Some(TARGET_VERSION));
    restarted
        .rollback(Arc::new(AllowingUpdatePreflight))
        .await
        .expect("rollback to legacy");
    assert_eq!(fs::read(fixture.executable()).unwrap(), b"old-binary");
    assert!(!runtime_path(&fixture, "node").exists());
    assert!(!runtime_path(&fixture, "openai-harness").exists());
    assert_eq!(
        fs::read(runtime_path(&fixture, "node.backup")).unwrap(),
        b"node-v2"
    );
    assert!(restarted.update_status().await.unwrap().need_restart);
}

#[tokio::test]
async fn incomplete_or_untrusted_runtime_archive_never_replaces_binary_or_web() {
    for kind in [
        ArchiveKind::RuntimeMissingLicense,
        ArchiveKind::RuntimeBadDigest,
        ArchiveKind::RuntimeSymlink,
        ArchiveKind::RuntimeDuplicate,
        ArchiveKind::RuntimeTraversal,
        ArchiveKind::RuntimeExtra,
        ArchiveKind::RuntimeCaseDuplicate,
    ] {
        let server = MockServer::start().await;
        let fixture = Fixture::new();
        fixture
            .mount_release(&server, TARGET_VERSION, kind, ChecksumKind::Valid)
            .await;
        assert_eq!(
            complete_update(&fixture.service(&server), TARGET_VERSION)
                .await
                .operation
                .status,
            SystemOperationStatus::Failed
        );
        assert_eq!(fs::read(fixture.executable()).unwrap(), b"old-binary");
        assert_eq!(
            fs::read(fixture.web().join("index.html")).unwrap(),
            b"old-web"
        );
        assert!(!runtime_path(&fixture, "node").exists());
        assert_eq!(
            fs::read_dir(runtime_path(&fixture, "tmp")).unwrap().count(),
            0,
            "failed extraction must clean read-only temporary files"
        );
    }
}

#[tokio::test]
async fn legacy_archive_cannot_leave_new_binary_with_old_runtime() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    fixture
        .mount_release(
            &server,
            TARGET_VERSION,
            ArchiveKind::Runtime,
            ChecksumKind::Valid,
        )
        .await;
    let service = fixture.service(&server);
    assert_eq!(
        complete_update(&service, TARGET_VERSION)
            .await
            .operation
            .status,
        SystemOperationStatus::Succeeded
    );
    let previous = fs::read(fixture.executable()).unwrap();
    let legacy_server = MockServer::start().await;
    fixture
        .mount_release(
            &legacy_server,
            "1.10.0",
            ArchiveKind::Safe,
            ChecksumKind::Valid,
        )
        .await;
    let mut config = fixture.config(&format!("{}/repos", legacy_server.uri()));
    config.version = TARGET_VERSION.into();
    let new_service = ProcessSystemOperations::new(CancellationToken::new(), config);
    assert_eq!(
        complete_update(&new_service, "1.10.0")
            .await
            .operation
            .status,
        SystemOperationStatus::Failed
    );
    assert_eq!(fs::read(fixture.executable()).unwrap(), previous);
    assert_eq!(
        fs::read(runtime_path(&fixture, "node")).unwrap(),
        b"node-v2"
    );
}

#[tokio::test]
async fn binary_backup_failure_restores_runtime_and_fingerprint_detects_worker_change() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    fixture
        .mount_release(
            &server,
            TARGET_VERSION,
            ArchiveKind::Runtime,
            ChecksumKind::Valid,
        )
        .await;
    fs::create_dir(fixture.executable().with_extension("backup")).expect("block binary backup");
    let service = fixture.service(&server);
    assert_eq!(
        complete_update(&service, TARGET_VERSION)
            .await
            .operation
            .status,
        SystemOperationStatus::Failed
    );
    assert_eq!(fs::read(fixture.executable()).unwrap(), b"old-binary");
    assert!(!runtime_path(&fixture, "node").exists());
    fs::remove_dir(fixture.executable().with_extension("backup")).unwrap();
    assert_eq!(
        complete_update(&service, TARGET_VERSION)
            .await
            .operation
            .status,
        SystemOperationStatus::Succeeded
    );
    let worker = runtime_path(&fixture, "openai-harness/worker/runner.mjs");
    #[cfg(windows)]
    {
        let mut permissions = fs::metadata(&worker).unwrap().permissions();
        permissions.set_readonly(false);
        fs::set_permissions(&worker, permissions).unwrap();
    }
    fs::write(&worker, "tampered-worker").unwrap();
    assert_eq!(
        service
            .update_status()
            .await
            .expect_err("running candidate changed")
            .kind(),
        SystemOperationErrorKind::Conflict
    );
}

#[tokio::test]
async fn failed_rollback_confirmation_restores_new_binary_and_runtime() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    fixture
        .mount_release(
            &server,
            TARGET_VERSION,
            ArchiveKind::Runtime,
            ChecksumKind::Valid,
        )
        .await;
    let service = fixture.service(&server);
    assert_eq!(
        complete_update(&service, TARGET_VERSION)
            .await
            .operation
            .status,
        SystemOperationStatus::Succeeded
    );
    let mut config = fixture.config(&format!("{}/repos", server.uri()));
    config.version = TARGET_VERSION.into();
    let restarted = ProcessSystemOperations::new(CancellationToken::new(), config);
    let error = restarted
        .rollback(Arc::new(ChangingRollbackRevisionPreflight {
            confirmations: AtomicUsize::new(0),
        }))
        .await
        .expect_err("post-swap revision changed");
    assert_eq!(error.kind(), SystemOperationErrorKind::Conflict);
    assert_eq!(fs::read(fixture.executable()).unwrap(), b"new-binary");
    assert_eq!(
        fs::read(runtime_path(&fixture, "node")).unwrap(),
        b"node-v2"
    );
    assert_eq!(
        fs::read(runtime_path(&fixture, "openai-harness/worker/runner.mjs")).unwrap(),
        b"export default 'worker-v2'"
    );
}

#[tokio::test]
async fn cancelled_rollback_restores_runtime_before_releasing_operation() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    fixture
        .mount_release(
            &server,
            TARGET_VERSION,
            ArchiveKind::Runtime,
            ChecksumKind::Valid,
        )
        .await;
    assert_eq!(
        complete_update(&fixture.service(&server), TARGET_VERSION)
            .await
            .operation
            .status,
        SystemOperationStatus::Succeeded
    );
    let mut config = fixture.config(&format!("{}/repos", server.uri()));
    config.version = TARGET_VERSION.into();
    let restarted = ProcessSystemOperations::new(CancellationToken::new(), config);
    let reached = Arc::new(tokio::sync::Notify::new());
    let service = restarted.clone();
    let reached_task = Arc::clone(&reached);
    let task = tokio::spawn(async move {
        service
            .rollback(Arc::new(BlockingRollbackPreflight {
                confirmations: AtomicUsize::new(0),
                reached_after_swap: reached_task,
            }))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), reached.notified())
        .await
        .expect("swapped before cancellation");
    task.abort();
    assert!(task.await.expect_err("cancelled").is_cancelled());
    assert_eq!(fs::read(fixture.executable()).unwrap(), b"new-binary");
    assert_eq!(
        fs::read(runtime_path(&fixture, "node")).unwrap(),
        b"node-v2"
    );
    assert_eq!(
        fs::read(runtime_path(&fixture, "openai-harness/worker/runner.mjs")).unwrap(),
        b"export default 'worker-v2'"
    );
}

#[tokio::test]
async fn legacy_update_after_bundle_rollback_does_not_resurrect_stale_runtime_backup() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    fixture
        .mount_release(
            &server,
            TARGET_VERSION,
            ArchiveKind::Runtime,
            ChecksumKind::Valid,
        )
        .await;
    let initial = fixture.service(&server);
    assert_eq!(
        complete_update(&initial, TARGET_VERSION)
            .await
            .operation
            .status,
        SystemOperationStatus::Succeeded
    );
    let mut updated_config = fixture.config(&format!("{}/repos", server.uri()));
    updated_config.version = TARGET_VERSION.into();
    ProcessSystemOperations::new(CancellationToken::new(), updated_config)
        .rollback(Arc::new(AllowingUpdatePreflight))
        .await
        .expect("restore legacy");
    assert!(runtime_path(&fixture, "node.backup").exists());

    let legacy_server = MockServer::start().await;
    fixture
        .mount_release(
            &legacy_server,
            "1.10.0",
            ArchiveKind::Safe,
            ChecksumKind::Valid,
        )
        .await;
    let legacy = fixture.service(&legacy_server);
    assert_eq!(
        complete_update(&legacy, "1.10.0").await.operation.status,
        SystemOperationStatus::Succeeded
    );
    assert!(!runtime_path(&fixture, "node").exists());
    assert!(!runtime_path(&fixture, "node.backup").exists());
    legacy
        .rollback(Arc::new(AllowingUpdatePreflight))
        .await
        .expect("legacy rollback");
    assert_eq!(fs::read(fixture.executable()).unwrap(), b"old-binary");
    assert!(!runtime_path(&fixture, "openai-harness").exists());
}
