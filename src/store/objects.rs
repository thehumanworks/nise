use std::path::{Component, Path, PathBuf};
use std::{fs, io};

use eyre::{Context, bail};
use serde::Serialize;
use walkdir::WalkDir;

use crate::store::manifests::write_object_manifest;
use crate::store::{
    HASH_ALGORITHM_SHA256, ObjectManifest, StoreRoot, canonical_tree_hash,
    validate_object_manifest_for_tree,
};
use crate::{Result, duration, file};

#[derive(Debug, Clone)]
pub struct StoreObjectPublishInput {
    pub name: String,
    pub platform: String,
    pub created_by: String,
    pub executable_paths: Vec<PathBuf>,
    pub bin_paths: Vec<PathBuf>,
    pub references: Vec<String>,
    pub realisations: Vec<String>,
    pub relocation_tokens: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublishedStoreObject {
    pub object_id: String,
    pub path: PathBuf,
    pub tree_hash: String,
    pub bytes: u64,
    pub files: u64,
    pub reused_existing: bool,
    pub manifest: ObjectManifest,
}

pub fn publish_store_object(
    store: &StoreRoot,
    build_path: impl AsRef<Path>,
    input: StoreObjectPublishInput,
) -> Result<PublishedStoreObject> {
    let build_path = build_path.as_ref();
    if !build_path.is_dir() {
        bail!(
            "store object build path is not a directory: {}",
            file::display_path(build_path)
        );
    }
    if !build_path.starts_with(store.tmp_dir()) {
        bail!(
            "store object build path must be under store tmp dir: {}",
            file::display_path(build_path)
        );
    }
    validate_manifest_paths("executable_paths", &input.executable_paths)?;
    validate_manifest_paths("bin_paths", &input.bin_paths)?;
    validate_declared_paths(build_path, &input.executable_paths, &input.bin_paths)?;
    validate_publishable_tree(build_path, &input.relocation_tokens)?;

    let tree = canonical_tree_hash(build_path)?;
    let object_id = format!("{}:{}", tree.hash_algorithm, tree.hash);
    let hash = object_hash(&object_id)?.to_string();
    let object_path = store_object_path(store, &object_id, &input.name)?;
    let manifest = ObjectManifest {
        schema_version: crate::store::SCHEMA_VERSION,
        object_id,
        tree_hash: tree.hash,
        hash_algorithm: tree.hash_algorithm,
        name: input.name,
        platform: input.platform,
        created_by: input.created_by,
        created_at: duration::process_now().to_string(),
        bytes: tree.bytes,
        files: tree.files,
        executable_paths: input.executable_paths,
        bin_paths: input.bin_paths,
        references: input.references,
        realisations: input.realisations,
    };

    if let Some(existing_path) = existing_object_path_for_hash(store, &hash, &object_path)? {
        return reuse_existing_object(build_path, existing_path);
    }

    write_object_manifest(build_path, &manifest)?;
    sync_manifest(build_path)?;
    let parent = object_path.parent().unwrap_or(store.path());
    file::create_dir_all(parent)?;
    match file::rename(build_path, &object_path) {
        Ok(()) => {}
        Err(_err) => {
            if let Some(existing_path) = existing_object_path_for_hash(store, &hash, &object_path)?
            {
                return reuse_existing_object(build_path, existing_path);
            }
            return Err(_err);
        }
    }
    file::sync_dir(parent)?;
    seal_readonly(&object_path)?;

    let manifest = validate_object_manifest_for_tree(&object_path)?;
    Ok(published_object(object_path, manifest, false))
}

pub fn store_object_path(
    store: &StoreRoot,
    object_id: &str,
    name: impl AsRef<str>,
) -> Result<PathBuf> {
    let hash = object_hash(object_id)?;
    Ok(store
        .objects_dir()
        .join(HASH_ALGORITHM_SHA256)
        .join(&hash[..2])
        .join(format!("{}-{}", hash, sanitize_object_name(name.as_ref()))))
}

fn existing_object_path_for_hash(
    store: &StoreRoot,
    hash: &str,
    preferred_path: &Path,
) -> Result<Option<PathBuf>> {
    if preferred_path.exists() {
        return Ok(Some(preferred_path.to_path_buf()));
    }
    let prefix_dir = store
        .objects_dir()
        .join(HASH_ALGORITHM_SHA256)
        .join(&hash[..2]);
    if !prefix_dir.exists() {
        return Ok(None);
    }

    let prefix = format!("{hash}-");
    for entry in fs::read_dir(prefix_dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix))
        {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn reuse_existing_object(
    build_path: &Path,
    existing_path: PathBuf,
) -> Result<PublishedStoreObject> {
    let existing = validate_object_manifest_for_tree(&existing_path)?;
    file::remove_all(build_path)?;
    Ok(published_object(existing_path, existing, true))
}

fn published_object(
    path: PathBuf,
    manifest: ObjectManifest,
    reused_existing: bool,
) -> PublishedStoreObject {
    PublishedStoreObject {
        object_id: manifest.object_id.clone(),
        path,
        tree_hash: manifest.tree_hash.clone(),
        bytes: manifest.bytes,
        files: manifest.files,
        reused_existing,
        manifest,
    }
}

fn validate_manifest_paths(kind: &str, paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        if path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            bail!(
                "store object {kind} must be relative paths inside the object: {}",
                file::display_path(path)
            );
        }
    }
    Ok(())
}

fn validate_declared_paths(
    build_path: &Path,
    executable_paths: &[PathBuf],
    bin_paths: &[PathBuf],
) -> Result<()> {
    for path in executable_paths {
        let full_path = build_path.join(path);
        let metadata = fs::symlink_metadata(&full_path).wrap_err_with(|| {
            format!(
                "declared executable path does not exist: {}",
                file::display_path(&full_path)
            )
        })?;
        if !metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
            bail!(
                "declared executable path is not a file: {}",
                file::display_path(&full_path)
            );
        }
    }
    for path in bin_paths {
        let full_path = build_path.join(path);
        if !full_path.is_dir() {
            bail!(
                "declared bin path is not a directory: {}",
                file::display_path(&full_path)
            );
        }
    }
    Ok(())
}

fn object_hash(object_id: &str) -> Result<&str> {
    let Some((algorithm, hash)) = object_id.split_once(':') else {
        bail!("store object id must include hash algorithm: {object_id}");
    };
    if algorithm != HASH_ALGORITHM_SHA256 {
        bail!(
            "unsupported store object hash algorithm {}, expected {}",
            algorithm,
            HASH_ALGORITHM_SHA256
        );
    }
    if hash.len() < 2 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid store object hash: {object_id}");
    }
    Ok(hash)
}

fn sanitize_object_name(name: &str) -> String {
    let sanitized = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if sanitized.is_empty() {
        "object".to_string()
    } else {
        sanitized
    }
}

fn validate_publishable_tree(root: &Path, tokens: &[String]) -> Result<()> {
    let tokens = tokens
        .iter()
        .filter(|token| !token.is_empty())
        .map(String::as_bytes)
        .collect::<Vec<_>>();

    for entry in WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
    {
        let entry = entry?;
        let path = entry.path();
        if path == root {
            continue;
        }
        let file_type = entry.file_type();
        if file_type.is_dir() {
            continue;
        }

        if file_type.is_symlink() {
            let target = fs::read_link(path)
                .wrap_err_with(|| format!("failed to read symlink {}", file::display_path(path)))?;
            scan_tokens(path, target.to_string_lossy().as_bytes(), &tokens)?;
            continue;
        }
        if !file_type.is_file() {
            bail!(
                "store object candidate contains unsupported filesystem entry: {}",
                file::display_path(path)
            );
        }
        reject_hardlinks(path)?;
        if !tokens.is_empty() {
            let bytes = file::read(path)?;
            scan_tokens(path, &bytes, &tokens)?;
        }
    }
    Ok(())
}

fn scan_tokens(path: &Path, bytes: &[u8], tokens: &[&[u8]]) -> Result<()> {
    for token in tokens {
        if contains_bytes(bytes, token) {
            bail!(
                "relocation token remains in store object candidate: {}",
                file::display_path(path)
            );
        }
    }
    Ok(())
}

fn reject_hardlinks(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::metadata(path)
            .wrap_err_with(|| format!("failed to stat {}", file::display_path(path)))?;
        if metadata.nlink() > 1 {
            bail!(
                "store object candidate contains hardlinked file: {}",
                file::display_path(path)
            );
        }
    }
    Ok(())
}

fn sync_manifest(build_path: &Path) -> Result<()> {
    let manifest_path = build_path.join(crate::store::OBJECT_MANIFEST_FILE);
    fs::File::open(&manifest_path)
        .wrap_err_with(|| format!("failed to open {}", file::display_path(&manifest_path)))?
        .sync_all()
        .wrap_err_with(|| format!("failed to sync {}", file::display_path(&manifest_path)))?;
    file::sync_dir(build_path)
}

fn seal_readonly(root: &Path) -> Result<()> {
    let entries = WalkDir::new(root)
        .follow_links(false)
        .contents_first(true)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(path)
            .wrap_err_with(|| format!("failed to stat {}", file::display_path(path)))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let permissions = readonly_permissions(metadata.permissions());
        fs::set_permissions(path, permissions)
            .wrap_err_with(|| format!("failed to seal {}", file::display_path(path)))?;
    }
    Ok(())
}

fn make_writable_recursive(root: &Path) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        let path = entry.path();
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(err)
                    .wrap_err_with(|| format!("failed to stat {}", file::display_path(path)));
            }
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        let permissions = writable_permissions(metadata.permissions(), metadata.is_dir());
        fs::set_permissions(path, permissions)
            .wrap_err_with(|| format!("failed to unseal {}", file::display_path(path)))?;
    }
    Ok(())
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(unix)]
fn readonly_permissions(mut permissions: fs::Permissions) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;

    permissions.set_mode(permissions.mode() & !0o222);
    permissions
}

#[cfg(not(unix))]
fn readonly_permissions(mut permissions: fs::Permissions) -> fs::Permissions {
    permissions.set_readonly(true);
    permissions
}

#[cfg(unix)]
fn writable_permissions(mut permissions: fs::Permissions, is_dir: bool) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;

    let write_bits = if is_dir { 0o700 } else { 0o600 };
    permissions.set_mode(permissions.mode() | write_bits);
    permissions
}

#[cfg(not(unix))]
fn writable_permissions(mut permissions: fs::Permissions, _is_dir: bool) -> fs::Permissions {
    permissions.set_readonly(false);
    permissions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str) -> StoreObjectPublishInput {
        StoreObjectPublishInput {
            name: name.to_string(),
            platform: "test-platform".to_string(),
            created_by: "nise-test".to_string(),
            executable_paths: vec![PathBuf::from("bin/demo")],
            bin_paths: vec![PathBuf::from("bin")],
            references: vec![],
            realisations: vec!["sha256:realisation".to_string()],
            relocation_tokens: vec![],
        }
    }

    fn write_build(store: &StoreRoot, txn: &str, contents: &str) -> Result<PathBuf> {
        let build = store.tmp_dir().join(txn).join("build");
        file::create_dir_all(build.join("bin"))?;
        file::write(build.join("bin/demo"), contents)?;
        Ok(build)
    }

    #[test]
    fn publishes_object_manifest_and_seals_build_tree() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let build = write_build(&store, "txn", "demo")?;

        let published = publish_store_object(&store, &build, input("demo tool/1.0.0"))?;

        assert!(!build.exists());
        assert!(published.path.exists());
        assert!(
            published
                .path
                .ends_with(format!("{}-demo-tool-1.0.0", published.tree_hash))
        );
        assert!(!published.reused_existing);
        assert_eq!(published.manifest.object_id, published.object_id);
        assert_eq!(published.manifest.bytes, 4);
        assert_eq!(published.manifest.files, 1);
        assert_eq!(published.manifest.bin_paths, vec![PathBuf::from("bin")]);
        assert_readonly(&published.path)?;
        assert_readonly(&published.path.join("bin/demo"))?;
        make_writable_recursive(&published.path)?;
        Ok(())
    }

    #[test]
    fn rejects_builds_that_still_contain_relocation_tokens() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let build = write_build(&store, "txn", "prefix=/tmp/staging-token")?;
        let mut input = input("demo");
        input.relocation_tokens = vec!["/tmp/staging-token".to_string()];

        let err = publish_store_object(&store, &build, input).unwrap_err();

        assert!(err.to_string().contains("relocation token remains"));
        assert!(build.exists());
        Ok(())
    }

    #[test]
    fn reuses_existing_object_with_same_hash_and_name() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let first_build = write_build(&store, "first", "same")?;
        let first = publish_store_object(&store, &first_build, input("demo"))?;
        let second_build = write_build(&store, "second", "same")?;

        let second = publish_store_object(&store, &second_build, input("demo"))?;

        assert_eq!(second.path, first.path);
        assert_eq!(second.object_id, first.object_id);
        assert!(second.reused_existing);
        assert!(!second_build.exists());
        make_writable_recursive(&first.path)?;
        Ok(())
    }

    #[test]
    fn reuses_existing_object_with_same_hash_even_when_name_differs() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let first_build = write_build(&store, "first", "same")?;
        let first = publish_store_object(&store, &first_build, input("demo-a"))?;
        let second_build = write_build(&store, "second", "same")?;

        let second = publish_store_object(&store, &second_build, input("demo-b"))?;

        assert_eq!(second.path, first.path);
        assert!(second.reused_existing);
        assert!(!second_build.exists());
        make_writable_recursive(&first.path)?;
        Ok(())
    }

    #[test]
    fn rejects_build_paths_outside_store_tmp() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let build = tmp.path().join("build");
        file::create_dir_all(&build)?;

        let err = publish_store_object(&store, &build, input("demo")).unwrap_err();

        assert!(err.to_string().contains("must be under store tmp dir"));
        Ok(())
    }

    #[test]
    fn rejects_manifest_paths_outside_object() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let build = write_build(&store, "txn", "demo")?;
        let mut input = input("demo");
        input.bin_paths = vec![PathBuf::from("../bin")];

        let err = publish_store_object(&store, &build, input).unwrap_err();

        assert!(err.to_string().contains("must be relative paths"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_relocation_tokens_in_symlink_targets() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let build = write_build(&store, "txn", "demo")?;
        std::os::unix::fs::symlink("/tmp/staging-token/bin", build.join("bin/link"))?;
        let mut input = input("demo");
        input.relocation_tokens = vec!["/tmp/staging-token".to_string()];

        let err = publish_store_object(&store, &build, input).unwrap_err();

        assert!(err.to_string().contains("relocation token remains"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_hardlinked_files() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let build = write_build(&store, "txn", "demo")?;
        fs::hard_link(build.join("bin/demo"), tmp.path().join("external-hardlink"))?;

        let err = publish_store_object(&store, &build, input("demo")).unwrap_err();

        assert!(err.to_string().contains("hardlinked file"));
        Ok(())
    }

    #[test]
    fn object_path_rejects_unsupported_object_ids() {
        let store = StoreRoot::new("/tmp/store");

        let err = store_object_path(&store, "blake3:abc", "demo").unwrap_err();

        assert!(err.to_string().contains("unsupported store object hash"));
    }

    fn assert_readonly(path: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(metadata.permissions().mode() & 0o222, 0);
        }
        #[cfg(not(unix))]
        {
            assert!(metadata.permissions().readonly());
        }
        Ok(())
    }
}
