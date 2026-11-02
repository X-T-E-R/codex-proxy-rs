//! 验证导出进程的成功、错误、取消与暂存清理生命周期

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::postgres::{TestDatabase, drain_analytics};
use gateway_admin::model::backup::code;
use gateway_admin::ports::backup::{DatabaseDumpPort, DumpRequest};
use gateway_core::lifecycle::CancellationToken;
use gateway_store::backup::{pg_dump::PgDumpAdapter, staging::StagingArea};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt as _;

#[tokio::test]
async fn dump_process_lifecycle() {
    const PROBE_DIR: &str = "CPR_TEST_PG_DUMP_PROBE_DIR";
    let Ok(directory) = std::env::var(PROBE_DIR) else {
        // PATH 只在独立测试进程中改变，不污染同一 harness 的其他测试
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("pg_dump");
        std::fs::write(
            &script,
            r#"#!/bin/sh
printf '%s' $$ > "$CPR_TEST_PG_DUMP_PROBE_DIR/pid"
ready() {
  marker="$CPR_TEST_PG_DUMP_PROBE_DIR/ready-$1"
  printf '%s' $$ > "$marker.tmp"
  mv "$marker.tmp" "$marker"
}
case "$5" in
  wait) exec 1>&-; ready wait; exec sleep 30 ;;
  stream) printf archive; ready stream; exec sleep 30 ;;
  fail) printf partial; exit 1 ;;
  *) printf archive ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut paths = vec![directory.path().to_path_buf()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "backup::pg_dump::dump_process_lifecycle",
                "--nocapture",
            ])
            .env(PROBE_DIR, directory.path())
            .env("PATH", std::env::join_paths(paths).unwrap())
            .status()
            .unwrap();
        assert!(status.success());
        return;
    };
    let Some(database) = TestDatabase::create("dump_lifecycle").await else {
        return;
    };
    drain_analytics(&database.pool).await;
    let directory = Path::new(&directory);
    let staging = Arc::new(StagingArea::open(directory.join("staging"), 1024 * 1024).unwrap());
    let dump = |mode: &str| {
        PgDumpAdapter::new(
            staging.clone(),
            mode,
            "test-password",
            database.pool.clone(),
        )
    };

    let artifact = dump("success").dump(request("success")).await.unwrap();
    let compressed = std::fs::read(&artifact.path).unwrap();
    let mut decoded = String::new();
    async_compression::tokio::bufread::GzipDecoder::new(compressed.as_slice())
        .read_to_string(&mut decoded)
        .await
        .unwrap();
    assert!(decoded.contains("\narchive\n"));
    assert_eq!(artifact.size_bytes, compressed.len() as u64);
    assert_eq!(artifact.sha256, hex::encode(Sha256::digest(&compressed)));
    assert!(!staging.partial_path("success").exists());
    assert_eq!(
        dump("success")
            .inspect_staging("success")
            .await
            .unwrap()
            .unwrap()
            .sha256,
        artifact.sha256
    );

    let error = dump("fail").dump(request("fail")).await.unwrap_err();
    assert_eq!(error.code(), code::PG_DUMP_FAILED);
    assert!(!staging.partial_path("fail").exists());

    let small = Arc::new(StagingArea::open(directory.join("small"), 1).unwrap());
    let error = PgDumpAdapter::new(
        small.clone(),
        "success",
        "test-password",
        database.pool.clone(),
    )
    .dump(request("oversized"))
    .await
    .unwrap_err();
    assert_eq!(error.code(), code::STAGING_SPACE_EXHAUSTED);
    assert!(!small.partial_path("oversized").exists());

    std::fs::create_dir(staging.final_path("rename")).unwrap();
    assert!(dump("success").dump(request("rename")).await.is_err());
    assert!(!staging.partial_path("rename").exists());

    std::fs::remove_file(directory.join("pid")).unwrap();
    std::fs::create_dir(staging.partial_path("open")).unwrap();
    assert!(dump("success").dump(request("open")).await.is_err());
    assert!(
        !directory.join("pid").exists(),
        "open failure must not spawn a process"
    );

    for (mode, abort) in [("wait", false), ("stream", true)] {
        let adapter = dump(mode);
        let request = request(mode);
        let cancellation = request.cancellation.clone();
        // 占满独享的两条连接，确定性覆盖超过三秒的快照准备，再检验独立的取消时限
        let setup_delay = if mode == "wait" {
            let connections = [
                database.pool.acquire().await.unwrap(),
                database.pool.acquire().await.unwrap(),
            ];
            Some(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(3_500)).await;
                drop(connections);
            }))
        } else {
            None
        };
        eprintln!("pg_dump lifecycle {mode}: starting snapshot/process setup");
        let mut task = tokio::spawn(async move { adapter.dump(request).await });
        // 一致性快照与进程启动属于 setup，不消耗取消后的三秒退出合同
        let ready = wait_for_ready(directory, mode);
        let pid = tokio::select! {
            result = &mut task => panic!("{mode}: dump ended before child readiness: {result:?}"),
            pid = ready => pid,
        };
        if let Some(setup_delay) = setup_delay {
            setup_delay.await.unwrap();
        }
        assert!(
            Path::new(&format!("/proc/{pid}")).exists(),
            "{mode}: ready child must be alive"
        );
        assert_eq!(
            std::fs::read_link(format!("/proc/{pid}/fd/1")).is_err(),
            mode == "wait",
            "{mode}: ready signal must describe the intended stdout phase",
        );
        eprintln!("pg_dump lifecycle {mode}: child {pid} ready; requesting cancellation");
        // 返回、真实进程退出和回收共用截止时间，不能各自再获得三秒
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        if abort {
            task.abort();
        } else {
            cancellation.cancel();
        }
        let result = tokio::time::timeout_at(deadline, task)
            .await
            .unwrap_or_else(|_| panic!("{mode}: cancellation task return exceeded 3s"));
        if abort {
            assert!(result.unwrap_err().is_cancelled());
        } else {
            assert_eq!(result.unwrap().unwrap_err().code(), code::CANCELLED);
        }
        eprintln!("pg_dump lifecycle {mode}: cancellation returned; waiting for child reap");
        wait_for_exit(&pid, mode, deadline).await;
        assert!(
            !staging.partial_path(mode).exists(),
            "{mode}: partial archive survived cancellation"
        );
        assert!(
            !staging.final_path(mode).exists(),
            "{mode}: cancelled dump published an archive"
        );
        std::fs::remove_file(directory.join("pid")).unwrap();
    }
    assert!(
        artifact.path.exists(),
        "completed archives remain available for recovery"
    );
    database.close().await;
}

#[tokio::test]
async fn cleanup_reports_failure_and_still_removes_the_other_archive() {
    let directory = tempfile::tempdir().unwrap();
    let staging = Arc::new(StagingArea::open(directory.path().to_path_buf(), 64).unwrap());
    let adapter = PgDumpAdapter::new(
        staging.clone(),
        "unused",
        "unused",
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap(),
    );
    adapter.cleanup_staging("missing").await.unwrap();
    std::fs::create_dir(staging.partial_path("blocked")).unwrap();
    std::fs::write(staging.final_path("blocked"), b"archive").unwrap();
    assert!(adapter.cleanup_staging("blocked").await.is_err());
    assert!(!staging.final_path("blocked").exists());
    assert!(staging.partial_path("blocked").is_dir());
}

fn request(backup_id: &str) -> DumpRequest {
    DumpRequest {
        backup_id: backup_id.to_owned(),
        cancellation: CancellationToken::new(),
    }
}

async fn wait_for_ready(directory: &Path, mode: &str) -> String {
    // setup 包含数据库快照查询，沿用该查询的 30 秒期限，不能套用取消时限
    tokio::time::timeout(Duration::from_secs(30), async {
        let path = directory.join(format!("ready-{mode}"));
        loop {
            match std::fs::read_to_string(&path) {
                Ok(pid) => {
                    pid.parse::<u32>().expect("ready marker must contain a PID");
                    return pid;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("{mode}: cannot read child readiness: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("{mode}: snapshot/process setup did not reach child readiness in 30s")
    })
}

async fn wait_for_exit(pid: &str, mode: &str, deadline: tokio::time::Instant) {
    tokio::time::timeout_at(deadline, async {
        while Path::new(&format!("/proc/{pid}")).exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{mode}: cancelled child {pid} was not reaped within 3s"));
}
