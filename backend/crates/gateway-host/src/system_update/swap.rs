//! 发行二进制、Web、插件与 Node harness 的交易式替换及回滚

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::archive::{ExtractedRelease, ExtractedRuntime};
use super::{OperationError, SystemUpdateConfig, conflict, internal};

// Node 与 harness 先落位、binary 最后替换；回滚按相反顺序，避免新程序配旧 worker
pub(crate) fn replace_release_files(
    executable: &Path,
    web_dist: &Path,
    extracted: ExtractedRelease,
) -> Result<(), OperationError> {
    let current = runtime_paths(executable)?;
    let present = current
        .each_ref()
        .map(|path| match fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(internal(format!(
                "failed to inspect runtime asset: {error}"
            ))),
        });
    let present = present.into_iter().collect::<Result<Vec<_>, _>>()?;
    if !present.iter().all(|found| *found) && !present.iter().all(|found| !*found) {
        return Err(conflict("安装的 Node 与 harness 文件不完整"));
    }
    if extracted.runtime.is_none() && present[0] {
        return Err(conflict("旧版更新包不含 Node harness，不能替换当前发行"));
    }
    if let Some(runtime) = &extracted.runtime {
        install_runtime(&current, runtime)?;
    } else {
        // 从已回滚的旧版再次安装旧归档时，旧的 bundle 备份不能混入新回滚候选
        for (index, path) in current.iter().enumerate() {
            let backup = backup_path_for(path);
            match fs::symlink_metadata(&backup) {
                Ok(_) => {
                    let result = if index == 2 {
                        remove_dir_all(&backup)
                    } else {
                        remove_file(&backup)
                    };
                    result.map_err(|error| {
                        internal(format!("failed to clear stale runtime backup: {error}"))
                    })?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(internal(format!(
                        "failed to inspect runtime backup: {error}"
                    )));
                }
            }
        }
    }
    let has_runtime = extracted.runtime.is_some();
    match replace_release_files_inner(executable, web_dist, extracted) {
        Ok(()) => Ok(()),
        Err(error) if has_runtime => {
            let mut failures = Vec::new();
            restore_runtime(&current, &mut failures);
            if failures.is_empty() {
                Err(error)
            } else {
                Err(internal(format!(
                    "{error}; runtime rollback failed: {}",
                    failures.join("; ")
                )))
            }
        }
        Err(error) => Err(error),
    }
}

fn replace_release_files_inner(
    executable: &Path,
    web_dist: &Path,
    extracted: ExtractedRelease,
) -> Result<(), OperationError> {
    let official_plugins = official_plugins_dir(executable)?;
    if !official_plugins.exists() {
        fs::create_dir_all(&official_plugins).map_err(|error| {
            internal(format!(
                "failed to prepare official plugin rollback directory: {error}"
            ))
        })?;
    }
    let official_backup = backup_path_for(&official_plugins);
    replace_dir(
        &official_plugins,
        &official_backup,
        &extracted.official_plugins_dir,
        "official plugin assets",
    )?;
    if let Err(error) = protect_official_plugins(&official_plugins) {
        let mut rollback = Vec::new();
        collect_rollback_error(
            &mut rollback,
            "restore official plugin assets",
            restore_dir(&official_plugins, &official_backup),
        );
        return Err(error_with_rollback(
            "failed to protect official plugin assets",
            error,
            rollback,
        ));
    }

    let web_backup = backup_path_for(web_dist);
    let web_replaced = match extracted.web_dist_dir {
        Some(new_web) => {
            if let Err(error) = replace_dir(web_dist, &web_backup, &new_web, "web assets") {
                let mut rollback = Vec::new();
                collect_rollback_error(
                    &mut rollback,
                    "restore official plugin assets",
                    restore_dir(&official_plugins, &official_backup),
                );
                return Err(error_with_rollback(
                    "failed to replace web assets",
                    error,
                    rollback,
                ));
            }
            true
        }
        None => false,
    };

    let binary_backup = backup_path_for(executable);
    if binary_backup.exists()
        && let Err(error) = remove_file(&binary_backup)
    {
        let mut rollback = Vec::new();
        if web_replaced {
            collect_rollback_error(
                &mut rollback,
                "restore web assets",
                restore_dir(web_dist, &web_backup),
            );
        }
        collect_rollback_error(
            &mut rollback,
            "restore official plugin assets",
            restore_dir(&official_plugins, &official_backup),
        );
        return Err(error_with_rollback(
            "failed to remove old binary backup",
            error,
            rollback,
        ));
    }
    if let Err(error) = move_file(executable, &binary_backup) {
        let mut rollback = Vec::new();
        if web_replaced {
            collect_rollback_error(
                &mut rollback,
                "restore web assets",
                restore_dir(web_dist, &web_backup),
            );
        }
        collect_rollback_error(
            &mut rollback,
            "restore official plugin assets",
            restore_dir(&official_plugins, &official_backup),
        );
        return Err(error_with_rollback("binary backup failed", error, rollback));
    }
    if let Err(error) = move_file(&extracted.binary_path, executable) {
        let mut rollback = Vec::new();
        if executable.exists() {
            collect_rollback_error(
                &mut rollback,
                "remove partial replacement binary",
                remove_file(executable),
            );
        }
        collect_rollback_error(
            &mut rollback,
            "restore previous binary",
            move_file(&binary_backup, executable),
        );
        if web_replaced {
            collect_rollback_error(
                &mut rollback,
                "restore web assets",
                restore_dir(web_dist, &web_backup),
            );
        }
        collect_rollback_error(
            &mut rollback,
            "restore official plugin assets",
            restore_dir(&official_plugins, &official_backup),
        );
        return Err(error_with_rollback(
            "binary replace failed",
            error,
            rollback,
        ));
    }
    Ok(())
}

pub(crate) fn rollback_release(config: &SystemUpdateConfig) -> Result<(), OperationError> {
    let web_dist = config.web_dist_dir()?;
    let executable = config.executable_path()?;
    let binary_backup = backup_path_for(&executable);
    if !binary_backup.exists() {
        return Err(conflict("no binary backup found for rollback"));
    }
    swap_file(&executable, &binary_backup)
        .map_err(|error| internal(format!("binary rollback failed: {error}")))?;

    let web_backup = backup_path_for(web_dist);
    if web_backup.exists()
        && let Err(error) = swap_dir(web_dist, &web_backup)
    {
        let mut rollback = Vec::new();
        collect_rollback_error(
            &mut rollback,
            "restore binary after web rollback failure",
            swap_file(&executable, &binary_backup),
        );
        return Err(error_with_rollback("web rollback failed", error, rollback));
    }
    let official_plugins = config.official_plugins_dir()?;
    let official_backup = backup_path_for(&official_plugins);
    if official_backup.exists()
        && let Err(error) = swap_dir(&official_plugins, &official_backup)
    {
        let mut rollback = Vec::new();
        if web_backup.exists() {
            collect_rollback_error(
                &mut rollback,
                "restore web assets after official plugin rollback failure",
                swap_dir(web_dist, &web_backup),
            );
        }
        collect_rollback_error(
            &mut rollback,
            "restore binary after official plugin rollback failure",
            swap_file(&executable, &binary_backup),
        );
        return Err(error_with_rollback(
            "official plugin rollback failed",
            error,
            rollback,
        ));
    }
    if let Err(error) = swap_runtime(&runtime_paths(&executable)?) {
        let mut rollback = Vec::new();
        if official_backup.exists() {
            collect_rollback_error(
                &mut rollback,
                "restore official plugins",
                swap_dir(&official_plugins, &official_backup),
            );
        }
        if web_backup.exists() {
            collect_rollback_error(
                &mut rollback,
                "restore web assets",
                swap_dir(web_dist, &web_backup),
            );
        }
        collect_rollback_error(
            &mut rollback,
            "restore binary",
            swap_file(&executable, &binary_backup),
        );
        return Err(error_with_rollback(
            "runtime rollback failed",
            error,
            rollback,
        ));
    }
    Ok(())
}

pub(crate) fn rollback_official_plugins_dir(
    config: &SystemUpdateConfig,
) -> Result<PathBuf, OperationError> {
    config
        .official_plugins_dir()
        .map(|directory| backup_path_for(&directory))
}

fn official_plugins_dir(executable: &Path) -> Result<PathBuf, OperationError> {
    executable
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| path.join("plugins").join("official"))
        .ok_or_else(|| internal("failed to resolve official plugin release directory"))
}

fn runtime_paths(executable: &Path) -> Result<[PathBuf; 4], OperationError> {
    let parent = executable
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| internal("failed to resolve runtime release directory"))?;
    Ok([
        "node",
        "node-LICENSE",
        "openai-harness",
        "runtime-manifest.json",
    ]
    .map(|name| parent.join(name)))
}

fn install_runtime(paths: &[PathBuf; 4], runtime: &ExtractedRuntime) -> Result<(), OperationError> {
    let sources = [
        &runtime.node,
        &runtime.node_license,
        &runtime.harness,
        &runtime.manifest,
    ];
    for index in 0..4 {
        let path = &paths[index];
        let backup = backup_path_for(path);
        let result = if index == 2 {
            replace_dir(path, &backup, sources[index], "harness")
        } else {
            replace_file(path, &backup, sources[index])
                .map_err(|error| internal(format!("failed to replace runtime file: {error}")))
        };
        if let Err(error) = result {
            let mut failures = Vec::new();
            restore_runtime(&paths[..index], &mut failures);
            return if failures.is_empty() {
                Err(error)
            } else {
                Err(internal(format!(
                    "{error}; runtime rollback failed: {}",
                    failures.join("; ")
                )))
            };
        }
    }
    Ok(())
}

fn replace_file(current: &Path, backup: &Path, replacement: &Path) -> io::Result<()> {
    if backup.exists() {
        remove_file(backup)?;
    }
    if current.exists() {
        move_file(current, backup)?;
    }
    if let Err(error) = move_file(replacement, current) {
        let recovery = restore_file(current, backup);
        return Err(match recovery {
            Ok(()) => error,
            Err(restore) => {
                io::Error::new(error.kind(), format!("{error}; restore failed: {restore}"))
            }
        });
    }
    Ok(())
}

fn restore_file(current: &Path, backup: &Path) -> io::Result<()> {
    if current.exists() {
        remove_file(current)?;
    }
    if backup.exists() {
        move_file(backup, current)?;
    }
    Ok(())
}

fn restore_runtime(paths: &[PathBuf], failures: &mut Vec<String>) {
    for (index, path) in paths.iter().enumerate().rev() {
        let backup = backup_path_for(path);
        let result = if index == 2 {
            restore_dir(path, &backup)
        } else {
            restore_file(path, &backup)
        };
        collect_rollback_error(failures, "restore runtime asset", result);
    }
}

fn swap_runtime(paths: &[PathBuf; 4]) -> io::Result<()> {
    for (index, path) in paths.iter().enumerate() {
        let backup = backup_path_for(path);
        let result = if index == 2 {
            swap_optional_dir(path, &backup)
        } else {
            swap_optional_file(path, &backup)
        };
        if let Err(error) = result {
            let mut recovery = Vec::new();
            for prior in paths[..index].iter().rev() {
                let previous_backup = backup_path_for(prior);
                let recovered = if prior
                    .file_name()
                    .is_some_and(|name| name == "openai-harness")
                {
                    swap_optional_dir(prior, &previous_backup)
                } else {
                    swap_optional_file(prior, &previous_backup)
                };
                collect_rollback_error(&mut recovery, "restore runtime swap", recovered);
            }
            return if recovery.is_empty() {
                Err(error)
            } else {
                Err(io::Error::new(
                    error.kind(),
                    format!("{error}; recovery failed: {}", recovery.join("; ")),
                ))
            };
        }
    }
    Ok(())
}

fn swap_optional_file(path: &Path, backup: &Path) -> io::Result<()> {
    match (path.exists(), backup.exists()) {
        (true, true) => swap_file(path, backup),
        (true, false) => move_file(path, backup),
        (false, true) => move_file(backup, path),
        (false, false) => Ok(()),
    }
}

fn swap_optional_dir(path: &Path, backup: &Path) -> io::Result<()> {
    match (path.exists(), backup.exists()) {
        (true, true) => swap_dir(path, backup),
        (true, false) => move_dir(path, backup),
        (false, true) => move_dir(backup, path),
        (false, false) => Ok(()),
    }
}

fn replace_dir(
    current: &Path,
    backup: &Path,
    replacement: &Path,
    label: &'static str,
) -> Result<(), OperationError> {
    if backup.exists() {
        remove_dir_all(backup)
            .map_err(|error| internal(format!("failed to remove old {label} backup: {error}")))?;
    }
    if current.exists() {
        move_dir(current, backup)
            .map_err(|error| internal(format!("failed to backup {label}: {error}")))?;
    }
    if let Err(error) = move_dir(replacement, current) {
        let mut rollback = Vec::new();
        if backup.exists() {
            collect_rollback_error(
                &mut rollback,
                "restore previous directory contents",
                restore_dir(current, backup),
            );
        }
        return Err(error_with_rollback(
            "failed to replace directory contents",
            error,
            rollback,
        ));
    }
    Ok(())
}

fn swap_file(current: &Path, backup: &Path) -> io::Result<()> {
    if !current.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("current binary is missing: {}", current.display()),
        ));
    }
    let swap = swap_path_for(current);
    if swap.exists() {
        remove_file(&swap)?;
    }
    super::swap_paths::swap_paths(current, backup, &swap, move_file)
}

fn swap_dir(current: &Path, backup: &Path) -> io::Result<()> {
    if !current.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("current web directory is missing: {}", current.display()),
        ));
    }
    let swap = swap_path_for(current);
    if swap.exists() {
        remove_dir_all(&swap)?;
    }
    super::swap_paths::swap_paths(current, backup, &swap, move_dir)
}

fn restore_dir(current: &Path, backup: &Path) -> io::Result<()> {
    if current.exists() {
        remove_dir_all(current)?;
    }
    if backup.exists() {
        move_dir(backup, current)?;
    }
    Ok(())
}

fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            if let Err(copy_error) = fs::copy(from, to) {
                let _ = remove_file(to);
                return Err(copy_error);
            }
            remove_file(from)
        }
        Err(error) => Err(error),
    }
}

fn move_dir(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            if let Err(copy_error) = copy_dir_all(from, to) {
                let _ = remove_dir_all(to);
                return Err(copy_error);
            }
            remove_dir_all(from)
        }
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("directory rename failed: {error}"),
        )),
    }
}

#[cfg(unix)]
fn protect_official_plugins(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o555))
}

#[cfg(not(unix))]
fn protect_official_plugins(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn remove_file(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_file() && metadata.permissions().readonly() {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions)?;
        }
    }
    fs::remove_file(path)
}

pub(super) fn remove_dir_all(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    make_directories_writable(path)?;
    #[cfg(windows)]
    make_windows_tree_writable(path)?;
    fs::remove_dir_all(path)
}

#[cfg(windows)]
fn make_windows_tree_writable(root: &Path) -> io::Result<()> {
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "release directory contains an unsupported file type",
            ));
        }
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                directories.push(entry.path());
            } else if kind.is_file() {
                let mut permissions = entry.metadata()?.permissions();
                if permissions.readonly() {
                    permissions.set_readonly(false);
                    fs::set_permissions(entry.path(), permissions)?;
                }
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "release directory contains an unsupported file type",
                ));
            }
        }
        if metadata.permissions().readonly() {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            fs::set_permissions(&directory, permissions)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn make_directories_writable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "release directory contains an unsupported file type",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            make_directories_writable(&entry.path())?;
        } else if !file_type.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "release directory contains an unsupported file type",
            ));
        }
    }
    Ok(())
}

fn copy_dir_all(from: &Path, to: &Path) -> io::Result<()> {
    let permissions = fs::metadata(from)?.permissions();
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target)?;
            fs::set_permissions(&target, entry.metadata()?.permissions())?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported file type in {}", entry.path().display()),
            ));
        }
    }
    fs::set_permissions(to, permissions)?;
    Ok(())
}

pub(crate) fn backup_path_for(path: &Path) -> PathBuf {
    let mut backup = path.as_os_str().to_os_string();
    backup.push(".backup");
    PathBuf::from(backup)
}

fn swap_path_for(path: &Path) -> PathBuf {
    let mut swap = path.as_os_str().to_os_string();
    swap.push(".rollback-swap");
    PathBuf::from(swap)
}

fn collect_rollback_error(errors: &mut Vec<String>, action: &'static str, result: io::Result<()>) {
    if let Err(error) = result {
        errors.push(format!("{action}: {error}"));
    }
}

fn error_with_rollback(
    context: &'static str,
    error: impl fmt::Display,
    rollback_errors: Vec<String>,
) -> OperationError {
    if rollback_errors.is_empty() {
        return internal(format!("{context}: {error}"));
    }
    internal(format!(
        "{context}: {error}; rollback failed: {}",
        rollback_errors.join("; ")
    ))
}
