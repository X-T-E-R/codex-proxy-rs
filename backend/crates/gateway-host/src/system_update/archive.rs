//! Release tar.gz 安全解包与制品归一化

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use super::{OperationError, internal, invalid};

const APP_BINARY_NAME: &str = "codex-proxy-rs";
const MAX_EXTRACTED_SIZE: u64 = 1024 * 1024 * 1024;
const MAX_ARCHIVE_FILES: usize = 20_000;
const MAX_MANIFEST_SIZE: u64 = 4 * 1024 * 1024;
const RUNTIME_MANIFEST: &str = "runtime-manifest.json";
const BUNDLE_MANIFEST: &str = "bundle-manifest.json";

#[derive(Debug)]
pub(crate) struct ExtractedRelease {
    pub(crate) binary_path: PathBuf,
    pub(crate) web_dist_dir: Option<PathBuf>,
    pub(crate) official_plugins_dir: PathBuf,
    pub(crate) runtime: Option<ExtractedRuntime>,
}

#[derive(Debug)]
pub(crate) struct ExtractedRuntime {
    pub(crate) node: PathBuf,
    pub(crate) node_license: PathBuf,
    pub(crate) harness: PathBuf,
    pub(crate) manifest: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeManifest {
    schema_version: u8,
    node_version: String,
    node_sha256: String,
    node_license_sha256: String,
    harness_manifest_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleManifest {
    schema_version: u8,
    opencode_commit: String,
    pi_commit: String,
    node_version: String,
    files: Vec<BundleFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleFile {
    path: String,
    sha256: String,
}

pub(crate) fn extract_release(
    archive_path: &Path,
    temp_dir: &Path,
) -> Result<ExtractedRelease, OperationError> {
    let file = fs::File::open(archive_path)
        .map_err(|error| internal(format!("failed to open release archive: {error}")))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let binary_path = temp_dir.join(APP_BINARY_NAME);
    let web_dist_dir = temp_dir.join("web-dist");
    let official_plugins_dir = temp_dir.join("official-plugins");
    let mut found_binary = false;
    let mut found_web = false;
    let mut found_official_manifest = false;
    let mut official_files = BTreeSet::new();
    let mut harness_files = BTreeSet::new();
    let mut harness_names = BTreeSet::new();
    let mut runtime_files = BTreeSet::new();
    let harness = temp_dir.join("openai-harness");
    let mut extracted_size = 0_u64;
    let mut file_count = 0_usize;

    for entry in archive
        .entries()
        .map_err(|error| internal(format!("failed to read release archive: {error}")))?
    {
        let mut entry =
            entry.map_err(|error| internal(format!("invalid archive entry: {error}")))?;
        let path = entry
            .path()
            .map_err(|error| internal(format!("invalid archive path: {error}")))?
            .into_owned();
        if unsafe_archive_path(&path) {
            return Err(invalid("release archive contains an unsafe path"));
        }
        let harness_relative = path
            .strip_prefix(Path::new(APP_BINARY_NAME).join("openai-harness"))
            .ok();
        let runtime_name = path.strip_prefix(APP_BINARY_NAME).ok().filter(|name| {
            matches!(
                name.to_str(),
                Some("node" | "node-LICENSE" | RUNTIME_MANIFEST)
            )
        });
        let official_relative = if harness_relative.is_some() || runtime_name.is_some() {
            None
        } else {
            official_plugin_relative_path(&path)?
        };
        if !entry.header().entry_type().is_file() {
            if official_relative.is_some()
                || runtime_name.is_some()
                || harness_relative.is_some() && !entry.header().entry_type().is_dir()
            {
                return Err(invalid("release archive contains a non-file managed asset"));
            }
            continue;
        }
        file_count = file_count.saturating_add(1);
        extracted_size = extracted_size.saturating_add(entry.header().size().unwrap_or(u64::MAX));
        if file_count > MAX_ARCHIVE_FILES || extracted_size > MAX_EXTRACTED_SIZE {
            return Err(invalid("release archive expands beyond safety limits"));
        }

        if let Some(name) = runtime_name {
            if !runtime_files.insert(name.to_path_buf()) {
                return Err(invalid(
                    "release archive contains a duplicate runtime asset",
                ));
            }
            if entry.header().size().unwrap_or(u64::MAX) == 0
                || name == Path::new(RUNTIME_MANIFEST)
                    && entry.header().size().unwrap_or(u64::MAX) > MAX_MANIFEST_SIZE
            {
                return Err(invalid("release runtime asset has an invalid size"));
            }
            let target = temp_dir.join(name);
            entry
                .unpack(&target)
                .map_err(|error| internal(format!("failed to extract runtime asset: {error}")))?;
            continue;
        }
        if let Some(relative) = harness_relative {
            if relative.as_os_str().is_empty()
                || !harness_names.insert(relative.to_string_lossy().to_lowercase())
                || !harness_files.insert(relative.to_path_buf())
            {
                return Err(invalid(
                    "release archive contains a duplicate harness asset",
                ));
            }
            if relative == Path::new(BUNDLE_MANIFEST)
                && entry.header().size().unwrap_or(u64::MAX) > MAX_MANIFEST_SIZE
            {
                return Err(invalid("release bundle manifest exceeds safety limits"));
            }
            let target = harness.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| internal(format!("failed to create harness dir: {error}")))?;
            }
            entry
                .unpack(&target)
                .map_err(|error| internal(format!("failed to extract harness asset: {error}")))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(&target, fs::Permissions::from_mode(0o444)).map_err(
                    |error| internal(format!("failed to protect harness asset: {error}")),
                )?;
            }
            #[cfg(windows)]
            protect_windows_file(&target)?;
            continue;
        }
        if path == Path::new(APP_BINARY_NAME)
            || path == Path::new(APP_BINARY_NAME).join(APP_BINARY_NAME)
        {
            if found_binary {
                return Err(invalid("release archive contains duplicate binaries"));
            }
            entry
                .unpack(&binary_path)
                .map_err(|error| internal(format!("failed to extract binary: {error}")))?;
            found_binary = true;
            continue;
        }
        if let Some(relative) = official_relative {
            if !official_files.insert(relative.clone()) {
                return Err(invalid(
                    "release archive contains a duplicate official plugin asset",
                ));
            }
            fs::create_dir_all(&official_plugins_dir).map_err(|error| {
                internal(format!("failed to create official plugin dir: {error}"))
            })?;
            let target = official_plugins_dir.join(&relative);
            entry.unpack(&target).map_err(|error| {
                internal(format!("failed to extract official plugin asset: {error}"))
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(&target, fs::Permissions::from_mode(0o444)).map_err(
                    |error| internal(format!("failed to protect official plugin asset: {error}")),
                )?;
            }
            found_official_manifest |= relative == Path::new("plugin-release-manifest.json");
            continue;
        }
        if let Some(relative) = web_dist_relative_path(&path) {
            if relative.as_os_str().is_empty() {
                continue;
            }
            let target = web_dist_dir.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    internal(format!("failed to create web asset dir: {error}"))
                })?;
            }
            entry
                .unpack(&target)
                .map_err(|error| internal(format!("failed to extract web asset: {error}")))?;
            found_web = true;
        }
    }
    if !found_binary {
        return Err(invalid("release archive does not contain codex-proxy-rs"));
    }
    if !found_official_manifest {
        return Err(invalid(
            "release archive does not contain the official plugin manifest",
        ));
    }
    let runtime = if runtime_files.is_empty() && harness_files.is_empty() {
        None
    } else {
        if runtime_files.len() != 3
            || !runtime_files.contains(Path::new("node"))
            || !runtime_files.contains(Path::new("node-LICENSE"))
            || !runtime_files.contains(Path::new(RUNTIME_MANIFEST))
            || !harness_files.contains(Path::new(BUNDLE_MANIFEST))
        {
            return Err(invalid("release runtime bundle is incomplete"));
        }
        verify_runtime_bundle(temp_dir, &harness, &harness_files)?;
        #[cfg(unix)]
        protect_harness_directories(&harness)?;
        Some(ExtractedRuntime {
            node: temp_dir.join("node"),
            node_license: temp_dir.join("node-LICENSE"),
            harness,
            manifest: temp_dir.join(RUNTIME_MANIFEST),
        })
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&binary_path, fs::Permissions::from_mode(0o755))
            .map_err(|error| internal(format!("failed to chmod binary: {error}")))?;
        if let Some(runtime) = &runtime {
            fs::set_permissions(&runtime.node, fs::Permissions::from_mode(0o755))
                .map_err(|error| internal(format!("failed to chmod node: {error}")))?;
            fs::set_permissions(&runtime.node_license, fs::Permissions::from_mode(0o444))
                .map_err(|error| internal(format!("failed to protect node license: {error}")))?;
            fs::set_permissions(&runtime.manifest, fs::Permissions::from_mode(0o444)).map_err(
                |error| internal(format!("failed to protect runtime manifest: {error}")),
            )?;
        }
    }
    #[cfg(windows)]
    if let Some(runtime) = &runtime {
        protect_windows_file(&runtime.node_license)?;
        protect_windows_file(&runtime.manifest)?;
    }
    Ok(ExtractedRelease {
        binary_path,
        web_dist_dir: found_web.then_some(web_dist_dir),
        official_plugins_dir,
        runtime,
    })
}

#[cfg(windows)]
fn protect_windows_file(path: &Path) -> Result<(), OperationError> {
    let mut permissions = fs::metadata(path)
        .map_err(|error| internal(format!("failed to inspect runtime asset: {error}")))?
        .permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)
        .map_err(|error| internal(format!("failed to protect runtime asset: {error}")))
}

fn unsafe_archive_path(path: &Path) -> bool {
    path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
}

#[cfg(unix)]
fn protect_harness_directories(root: &Path) -> Result<(), OperationError> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut pending = vec![root.to_path_buf()];
    let mut directories = Vec::new();
    while let Some(directory) = pending.pop() {
        let metadata = fs::symlink_metadata(&directory)
            .map_err(|error| internal(format!("failed to inspect harness directory: {error}")))?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(invalid("harness contains a non-directory path"));
        }
        for entry in fs::read_dir(&directory)
            .map_err(|error| internal(format!("failed to read harness directory: {error}")))?
        {
            let entry = entry
                .map_err(|error| internal(format!("failed to inspect harness asset: {error}")))?;
            let kind = entry
                .file_type()
                .map_err(|error| internal(format!("failed to inspect harness asset: {error}")))?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if !kind.is_file() {
                return Err(invalid("harness contains a non-file asset"));
            }
        }
        directories.push(directory);
    }
    for directory in directories.into_iter().rev() {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o555))
            .map_err(|error| internal(format!("failed to protect harness directory: {error}")))?;
    }
    Ok(())
}

fn web_dist_relative_path(path: &Path) -> Option<PathBuf> {
    path.strip_prefix(Path::new(APP_BINARY_NAME).join("web/dist"))
        .or_else(|_| path.strip_prefix("web/dist"))
        .ok()
        .map(Path::to_path_buf)
}

fn verify_runtime_bundle(
    root: &Path,
    harness: &Path,
    extracted: &BTreeSet<PathBuf>,
) -> Result<(), OperationError> {
    let runtime: RuntimeManifest = serde_json::from_slice(
        &fs::read(root.join(RUNTIME_MANIFEST))
            .map_err(|error| invalid(format!("cannot read runtime manifest: {error}")))?,
    )
    .map_err(|error| invalid(format!("invalid runtime manifest: {error}")))?;
    let bundle_path = harness.join(BUNDLE_MANIFEST);
    let bundle: BundleManifest = serde_json::from_slice(
        &fs::read(&bundle_path)
            .map_err(|error| invalid(format!("cannot read bundle manifest: {error}")))?,
    )
    .map_err(|error| invalid(format!("invalid bundle manifest: {error}")))?;
    if runtime.schema_version != 1
        || bundle.schema_version != 1
        || runtime.node_version != bundle.node_version
        || runtime.node_version.trim().is_empty()
        || bundle.opencode_commit.trim().is_empty()
        || bundle.pi_commit.trim().is_empty()
    {
        return Err(invalid("release runtime manifest versions are invalid"));
    }
    for (path, expected) in [
        (root.join("node"), runtime.node_sha256.as_str()),
        (
            root.join("node-LICENSE"),
            runtime.node_license_sha256.as_str(),
        ),
        (bundle_path, runtime.harness_manifest_sha256.as_str()),
    ] {
        verify_digest(&path, expected)?;
    }
    let mut listed = BTreeSet::new();
    for file in &bundle.files {
        if !valid_bundle_path(&file.path) || !listed.insert(PathBuf::from(&file.path)) {
            return Err(invalid(
                "release bundle manifest contains an unsafe or duplicate path",
            ));
        }
        verify_digest(&harness.join(&file.path), &file.sha256)?;
    }
    let mut actual = extracted.clone();
    actual.remove(Path::new(BUNDLE_MANIFEST));
    if listed != actual
        || ![
            "worker/package.json",
            "worker/package-lock.json",
            "worker/NOTICE.md",
            "worker/source-manifest.json",
        ]
        .into_iter()
        .all(|path| listed.contains(Path::new(path)))
        || !listed
            .iter()
            .any(|path| path.starts_with("worker/node_modules"))
        || !listed
            .iter()
            .any(|path| path.starts_with("sources/opencode"))
        || !listed.iter().any(|path| path.starts_with("sources/pi"))
        || !listed.iter().any(|path| {
            path.starts_with("worker") && path.extension().is_some_and(|ext| ext == "mjs")
        })
    {
        return Err(invalid(
            "release bundle manifest does not cover the required files",
        ));
    }
    Ok(())
}

fn valid_bundle_path(value: &str) -> bool {
    !value.contains('\\')
        && !value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        && (value.starts_with("worker/")
            || value.starts_with("sources/opencode/")
            || value.starts_with("sources/pi/"))
        && value != BUNDLE_MANIFEST
}

fn verify_digest(path: &Path, expected: &str) -> Result<(), OperationError> {
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("release runtime digest is invalid"));
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| invalid(format!("runtime file is unavailable: {error}")))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(invalid("runtime asset must be a regular file"));
    }
    let mut hash = Sha256::new();
    let mut file = fs::File::open(path)
        .map_err(|error| internal(format!("cannot open runtime asset: {error}")))?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| internal(format!("cannot hash runtime asset: {error}")))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    if !hex::encode(hash.finalize()).eq_ignore_ascii_case(expected) {
        return Err(invalid("release runtime asset digest mismatch"));
    }
    Ok(())
}

fn official_plugin_relative_path(path: &Path) -> Result<Option<PathBuf>, OperationError> {
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    for index in 0..components.len() {
        if components[index] != "plugins"
            || components
                .get(index + 1)
                .is_none_or(|value| *value != "official")
        {
            continue;
        }
        let relative = &components[index + 2..];
        if relative.is_empty() {
            return Ok(None);
        }
        if relative.len() != 1 {
            return Err(invalid(
                "release archive contains a nested official plugin asset",
            ));
        }
        let file_name = relative[0];
        let is_manifest = file_name == "plugin-release-manifest.json";
        let is_archive = file_name
            .to_str()
            .is_some_and(|value| value.ends_with(".tar.gz"));
        if !is_manifest && !is_archive {
            return Err(invalid(
                "release archive contains an unexpected official plugin asset",
            ));
        }
        return Ok(Some(PathBuf::from(file_name)));
    }
    Ok(None)
}
