//! 活跃 worker 的在线更新隔离、归档拒绝与完整镜像恢复提示测试

use super::*;

fn runtime_path(fixture: &Fixture, name: &str) -> PathBuf {
    fixture.root.path().join(name)
}

fn seed_bundle(fixture: &Fixture) -> PathBuf {
    let worker = runtime_path(fixture, "openai-harness/worker/worker.mjs");
    fs::create_dir_all(worker.parent().unwrap()).expect("worker directory");
    fs::write(&worker, "active-worker-v1").expect("active worker");
    fs::write(
        runtime_path(fixture, "openai-harness/bundle-manifest.json"),
        "bundle-v1",
    )
    .expect("bundle manifest");
    fs::write(runtime_path(fixture, "node"), "active-node").expect("node");
    fs::write(runtime_path(fixture, "node-LICENSE"), "active-license").expect("node license");
    fs::write(runtime_path(fixture, "runtime-manifest.json"), "runtime-v1")
        .expect("runtime manifest");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(
            runtime_path(fixture, "node"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("executable node");
    }
    worker
}

#[tokio::test]
async fn bundle_archive_rejected_before_any_active_swap_or_worker_respawn() {
    for kind in [ArchiveKind::Runtime, ArchiveKind::RuntimeSymlink] {
        let server = MockServer::start().await;
        let fixture = Fixture::new();
        let active_worker = runtime_path(&fixture, "external-worker/worker.mjs");
        fs::create_dir_all(active_worker.parent().unwrap()).unwrap();
        fs::write(&active_worker, "old-live-worker").unwrap();
        let (started, request_started) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let worker_path = active_worker.clone();
        let live_request = tokio::spawn(async move {
            let first = fs::read(&worker_path).unwrap();
            started.send(()).unwrap();
            resumed.await.unwrap();
            (first, fs::read(&worker_path).unwrap())
        });
        request_started.await.unwrap();
        fixture
            .mount_release(&server, TARGET_VERSION, kind, ChecksumKind::Valid)
            .await;
        let status = complete_update(&fixture.service(&server), TARGET_VERSION).await;
        assert_eq!(status.operation.status, SystemOperationStatus::Failed);
        assert_eq!(fs::read(fixture.executable()).unwrap(), b"old-binary");
        assert_eq!(
            fs::read(fixture.web().join("index.html")).unwrap(),
            b"old-web"
        );
        assert_eq!(
            fs::read(fixture.official().join("plugin-release-manifest.json")).unwrap(),
            OLD_RELEASE_MANIFEST.as_bytes()
        );
        assert!(!runtime_path(&fixture, "node").exists());
        assert!(!runtime_path(&fixture, "codex-proxy-rs.backup").exists());
        resume.send(()).unwrap();
        let (before, after) = live_request.await.unwrap();
        assert_eq!(before, b"old-live-worker");
        assert_eq!(after, b"old-live-worker");
        // worker 崩溃重启仍从固定目录重新打开旧版本文件
        assert_eq!(fs::read(&active_worker).unwrap(), b"old-live-worker");
        assert_eq!(
            fs::read_dir(runtime_path(&fixture, "tmp")).unwrap().count(),
            0
        );
        if matches!(kind, ArchiveKind::Runtime) {
            assert!(
                status
                    .operation
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("完整镜像")
            );
        }
    }
}

#[tokio::test]
async fn installed_bundle_disables_update_detail_and_blocks_live_rollback() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    fixture
        .mount_release(
            &server,
            TARGET_VERSION,
            ArchiveKind::Safe,
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
    let worker = seed_bundle(&fixture);
    let mut config = fixture.config("http://127.0.0.1:1/repos");
    config.version = TARGET_VERSION.into();
    let service = ProcessSystemOperations::new(CancellationToken::new(), config);
    let detail = service
        .update_detail(true, None)
        .await
        .expect("update detail");
    assert!(!detail.update_supported);
    assert!(!detail.has_update);
    assert!(detail.unsupported_reason.unwrap().contains("完整镜像"));
    assert_eq!(
        service
            .perform_test_update(Some("1.10.0".into()))
            .await
            .expect_err("live update blocked")
            .kind(),
        SystemOperationErrorKind::Conflict
    );
    let error = service
        .rollback(Arc::new(AllowingUpdatePreflight))
        .await
        .expect_err("live rollback blocked");
    assert_eq!(error.kind(), SystemOperationErrorKind::Conflict);
    assert!(error.message().contains("完整镜像"));
    assert_eq!(fs::read(fixture.executable()).unwrap(), b"new-binary");
    assert_eq!(fs::read(&worker).unwrap(), b"active-worker-v1");
    assert_eq!(
        fs::read(&worker).unwrap(),
        b"active-worker-v1",
        "worker respawn must reopen the old bytes"
    );
    assert!(!runtime_path(&fixture, "node.backup").exists());
}

#[tokio::test]
async fn interrupted_partial_bundle_requires_complete_redeployment_before_retry() {
    let fixture = Fixture::new();
    fs::write(fixture.state(), r#"{"currentVersion":"1.0.0","operation":{"operationId":"interrupted","kind":"update","status":"running","targetVersion":"1.9.9"}}"#).unwrap();
    fs::write(runtime_path(&fixture, "node"), "partially-copied-node").unwrap();
    let service = fixture.service_for_url("http://127.0.0.1:1/repos");
    let error = service
        .update_status()
        .await
        .expect_err("partial deployment cannot run update");
    assert_eq!(error.kind(), SystemOperationErrorKind::Conflict);
    assert!(error.message().contains("完整镜像"));
    assert_eq!(fs::read(fixture.executable()).unwrap(), b"old-binary");
    assert!(!runtime_path(&fixture, "openai-harness").exists());

    // 离线整体部署恢复完整的旧式发行目录后，重新启动的实例可以校准状态
    fs::remove_file(runtime_path(&fixture, "node")).unwrap();
    let restarted = fixture.service_for_url("http://127.0.0.1:1/repos");
    let recovered = restarted
        .update_status()
        .await
        .expect("complete deployment reconciled");
    assert!(!recovered.need_restart);
    assert_eq!(recovered.operation.status, SystemOperationStatus::Failed);
}
