use std::path::{Path, PathBuf};

use serde::Serialize;
use walkdir::WalkDir;

use crate::store::{
    InstallRefManifest, PROFILE_MANIFEST_FILE, ProfileManifest, StoreRoot,
    StoreTransactionManifest, read_manifest,
};

use super::manifests::StoreManifest;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RootScan {
    pub store_root: PathBuf,
    pub install_refs: Vec<InstallRefManifest>,
    pub profiles: Vec<ProfileManifest>,
    pub transactions: Vec<StoreTransactionManifest>,
    pub pins: Vec<PathBuf>,
    pub process_leases: Vec<PathBuf>,
    pub errors: Vec<RootScanError>,
}

impl RootScan {
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RootScanError {
    pub path: PathBuf,
    pub error: String,
}

pub fn read_roots(store_root: impl AsRef<Path>) -> RootScan {
    let store = StoreRoot::new(store_root.as_ref());
    let mut scan = RootScan {
        store_root: store.path().to_path_buf(),
        install_refs: vec![],
        profiles: vec![],
        transactions: vec![],
        pins: vec![],
        process_leases: vec![],
        errors: vec![],
    };

    scan_manifests(
        &store.install_refs_dir(),
        |path| path.extension().is_some_and(|ext| ext == "toml"),
        &mut scan.install_refs,
        &mut scan.errors,
    );
    scan_manifests(
        &store.profile_refs_dir(),
        |path| {
            path.file_name()
                .is_some_and(|name| name == PROFILE_MANIFEST_FILE)
        },
        &mut scan.profiles,
        &mut scan.errors,
    );
    scan_manifests(
        &store.transaction_refs_dir(),
        |path| path.extension().is_some_and(|ext| ext == "toml"),
        &mut scan.transactions,
        &mut scan.errors,
    );
    scan_paths(&store.pin_refs_dir(), &mut scan.pins, &mut scan.errors);
    scan_paths(
        &store.process_refs_dir(),
        &mut scan.process_leases,
        &mut scan.errors,
    );

    scan
}

fn scan_manifests<T>(
    root: &Path,
    matches: impl Fn(&Path) -> bool,
    manifests: &mut Vec<T>,
    errors: &mut Vec<RootScanError>,
) where
    T: serde::de::DeserializeOwned + StoreManifest,
{
    if !root.exists() {
        return;
    }
    for path in manifest_paths(root, matches, errors) {
        match read_manifest::<T>(&path) {
            Ok(manifest) => match manifest.validate_schema() {
                Ok(()) => manifests.push(manifest),
                Err(err) => errors.push(RootScanError {
                    path,
                    error: err.to_string(),
                }),
            },
            Err(err) => errors.push(RootScanError {
                path,
                error: err.to_string(),
            }),
        }
    }
}

fn scan_paths(root: &Path, paths: &mut Vec<PathBuf>, errors: &mut Vec<RootScanError>) {
    if !root.exists() {
        return;
    }
    for path in manifest_paths(root, |_| true, errors) {
        paths.push(path);
    }
}

fn manifest_paths(
    root: &Path,
    matches: impl Fn(&Path) -> bool,
    errors: &mut Vec<RootScanError>,
) -> Vec<PathBuf> {
    let mut paths = vec![];
    for entry in WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
    {
        match entry {
            Ok(entry) if entry.file_type().is_file() && matches(entry.path()) => {
                paths.push(entry.path().to_path_buf());
            }
            Ok(_) => {}
            Err(err) => errors.push(RootScanError {
                path: err
                    .path()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| root.to_path_buf()),
                error: err.to_string(),
            }),
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{
        InstallRefMode, ProfileManifest, SCHEMA_VERSION, StoreTxnState, write_manifest,
    };
    use crate::{Result, file};

    #[test]
    fn reads_store_roots_and_reports_corrupt_roots() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path());
        let install_ref = InstallRefManifest {
            schema_version: SCHEMA_VERSION,
            tool: "ripgrep".to_string(),
            version: "14.1.1".to_string(),
            backend: "aqua:BurntSushi/ripgrep".to_string(),
            compatibility_path: tmp.path().join("installs/ripgrep/14.1.1"),
            realisation_id: "sha256:realisation".to_string(),
            object_id: "sha256:object".to_string(),
            mode: InstallRefMode::StoreSymlink,
        };
        write_manifest(
            store.install_refs_dir().join("ripgrep").join("14.1.1.toml"),
            &install_ref,
        )?;
        let profile = ProfileManifest {
            schema_version: SCHEMA_VERSION,
            profile_id: "project/default".to_string(),
            generation: 1,
            project_root: Some(tmp.path().join("project")),
            source_config_hash: "sha256:config".to_string(),
            nise_lock_hash: "sha256:lock".to_string(),
            created_at: "2026-07-03T00:00:00Z".to_string(),
            realisations: vec!["sha256:realisation".to_string()],
            env_hash: "sha256:env".to_string(),
            path_entries: vec![tmp.path().join("profile/bin")],
        };
        write_manifest(
            store
                .profile_refs_dir()
                .join("projects/hash/default/generations/1")
                .join(PROFILE_MANIFEST_FILE),
            &profile,
        )?;
        let txn = StoreTransactionManifest {
            schema_version: SCHEMA_VERSION,
            txn_id: "txn".to_string(),
            state: StoreTxnState::Building,
            created_at: "2026-07-03T00:00:00Z".to_string(),
            updated_at: "2026-07-03T00:00:01Z".to_string(),
            pid: 123,
            derivation_id: "sha256:derivation".to_string(),
            realisation_id: None,
            object_id: None,
            build_path: tmp.path().join("tmp/txn/build"),
            store_path: None,
            compatibility_path: tmp.path().join("installs/ripgrep/14.1.1"),
        };
        write_manifest(store.transaction_refs_dir().join("txn.toml"), &txn)?;
        file::write(
            store.install_refs_dir().join("bad.toml"),
            "schema_version = ",
        )?;
        file::write(
            store.install_refs_dir().join("future.toml"),
            indoc::indoc! {r#"
                schema_version = 999
                tool = "future"
                version = "1"
                backend = "test:future"
                compatibility_path = "/tmp/future"
                realisation_id = "sha256:future-realisation"
                object_id = "sha256:future-object"
                mode = "store-symlink"
            "#},
        )?;

        let scan = read_roots(tmp.path());

        assert_eq!(scan.install_refs, vec![install_ref]);
        assert_eq!(scan.profiles, vec![profile]);
        assert_eq!(scan.transactions, vec![txn]);
        assert_eq!(scan.errors.len(), 2);
        assert!(
            scan.errors
                .iter()
                .any(|err| err.error.contains("unsupported install ref manifest"))
        );
        assert!(!scan.is_clean());
        Ok(())
    }
}
