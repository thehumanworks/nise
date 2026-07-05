use std::path::{Path, PathBuf};

use eyre::Result;

use crate::rand::random_string;
use crate::store::{
    SCHEMA_VERSION, StoreRoot, StoreTransactionManifest, StoreTxnState, read_manifest,
    write_manifest,
};
use crate::{duration, file};

#[derive(Debug, Clone)]
pub struct StoreTxnInput {
    pub derivation_id: String,
    pub compatibility_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct StoreTxn {
    store: StoreRoot,
    manifest_path: PathBuf,
    tmp_dir: PathBuf,
    manifest: StoreTransactionManifest,
}

impl StoreTxn {
    pub fn begin(store: StoreRoot, input: StoreTxnInput) -> Result<Self> {
        let pid = std::process::id();
        let txn_id = format!("{pid}-{}", random_string(10));
        let tmp_dir = store.tmp_dir().join(&txn_id);
        let build_path = tmp_dir.join("build");
        file::create_dir_all(&build_path)?;

        let now = duration::process_now().to_string();
        let manifest = StoreTransactionManifest {
            schema_version: SCHEMA_VERSION,
            txn_id: txn_id.clone(),
            state: StoreTxnState::Preparing,
            created_at: now.clone(),
            updated_at: now,
            pid,
            derivation_id: input.derivation_id,
            realisation_id: None,
            object_id: None,
            build_path,
            store_path: None,
            compatibility_path: input.compatibility_path,
        };
        let manifest_path = store.transaction_refs_dir().join(format!("{txn_id}.toml"));
        write_manifest(&manifest_path, &manifest)?;

        Ok(Self {
            store,
            manifest_path,
            tmp_dir,
            manifest,
        })
    }

    pub fn manifest(&self) -> &StoreTransactionManifest {
        &self.manifest
    }

    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    pub fn tmp_dir(&self) -> &Path {
        &self.tmp_dir
    }

    pub fn build_path(&self) -> &Path {
        &self.manifest.build_path
    }

    pub fn set_state(&mut self, state: StoreTxnState) -> Result<()> {
        self.manifest.state = state;
        self.persist()
    }

    pub fn set_realisation_id(&mut self, realisation_id: impl Into<String>) -> Result<()> {
        self.manifest.realisation_id = Some(realisation_id.into());
        self.persist()
    }

    pub fn set_object(
        &mut self,
        object_id: impl Into<String>,
        store_path: impl Into<PathBuf>,
    ) -> Result<()> {
        self.manifest.object_id = Some(object_id.into());
        self.manifest.store_path = Some(store_path.into());
        self.persist()
    }

    pub fn complete(mut self) -> Result<StoreTransactionManifest> {
        self.manifest.state = StoreTxnState::Complete;
        self.persist()?;
        let manifest = self.manifest.clone();
        file::remove_file(&self.manifest_path)?;
        if self.tmp_dir.exists() {
            file::remove_all(&self.tmp_dir)?;
        }
        Ok(manifest)
    }

    pub fn fail(&mut self) -> Result<()> {
        self.manifest.state = StoreTxnState::Failed;
        self.persist()
    }

    fn persist(&mut self) -> Result<()> {
        self.manifest.updated_at = duration::process_now().to_string();
        write_manifest(&self.manifest_path, &self.manifest)
    }
}

pub fn read_transaction_manifest(path: impl AsRef<Path>) -> Result<StoreTransactionManifest> {
    read_manifest(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{RootScan, gc_dry_run, read_roots};

    fn transaction_input(tmp: &Path) -> StoreTxnInput {
        StoreTxnInput {
            derivation_id: "sha256:derivation".to_string(),
            compatibility_path: tmp.join("installs/demo/1.0.0"),
        }
    }

    #[test]
    fn transaction_manifest_roots_staging_until_complete() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let mut txn = StoreTxn::begin(store.clone(), transaction_input(tmp.path()))?;

        assert!(txn.manifest_path().exists());
        assert!(txn.build_path().exists());
        assert_eq!(txn.manifest().state, StoreTxnState::Preparing);
        assert_eq!(read_roots(store.path()).transactions.len(), 1);

        txn.set_state(StoreTxnState::Building)?;
        txn.set_realisation_id("sha256:realisation")?;
        txn.set_object(
            "sha256:object",
            store.objects_dir().join("sha256/aa/object"),
        )?;

        let persisted = read_transaction_manifest(txn.manifest_path())?;
        assert_eq!(persisted.state, StoreTxnState::Building);
        assert_eq!(
            persisted.realisation_id.as_deref(),
            Some("sha256:realisation")
        );
        assert_eq!(persisted.object_id.as_deref(), Some("sha256:object"));

        let completed = txn.complete()?;
        assert_eq!(completed.state, StoreTxnState::Complete);
        assert!(read_roots(store.path()).transactions.is_empty());
        Ok(())
    }

    #[test]
    fn transaction_object_is_gc_root_while_manifest_exists() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let mut txn = StoreTxn::begin(store.clone(), transaction_input(tmp.path()))?;
        let object_root = store.objects_dir().join("sha256/aa/object");
        file::create_dir_all(&object_root)?;
        file::write(
            object_root.join(crate::store::OBJECT_MANIFEST_FILE),
            indoc::indoc! {r#"
                schema_version = 1
                object_id = "sha256:object"
                tree_hash = "object"
                hash_algorithm = "sha256"
                name = "demo-1.0.0"
                platform = "test"
                created_by = "test"
                created_at = "2026-07-03T00:00:00Z"
                bytes = 0
                files = 0
                executable_paths = []
                bin_paths = []
                references = []
                realisations = []
            "#},
        )?;
        txn.set_object("sha256:object", object_root)?;

        let roots: RootScan = read_roots(store.path());
        assert_eq!(roots.transactions.len(), 1);
        let report = gc_dry_run(&store)?;
        assert_eq!(report.objects.marked, 1);
        assert!(report.candidates.is_empty());
        Ok(())
    }
}
