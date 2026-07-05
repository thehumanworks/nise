use std::path::PathBuf;

use eyre::{Result, bail};
use serde::Serialize;

use crate::file;
use crate::store::{
    ProvenanceRecord, PublishedStoreObject, StoreObjectPublishInput, StoreRealisationInput,
    StoreRealisationPublication, StoreRoot, StoreTxn, StoreTxnState, publish_store_object,
    publish_store_realisation,
};

#[derive(Debug, Clone)]
pub struct StoreRealisationMetadata {
    pub tool: String,
    pub backend: String,
    pub version: String,
    pub platform: String,
    pub options_hash: String,
    pub source_hash: String,
    pub lock_policy: String,
    pub provenance: Vec<ProvenanceRecord>,
    pub closure: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct StagedStoreInstallPublishInput {
    pub object: StoreObjectPublishInput,
    pub realisation: StoreRealisationMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StagedStoreInstallPublication {
    pub object: PublishedStoreObject,
    pub realisation: StoreRealisationPublication,
    pub compatibility_path: PathBuf,
}

pub fn publish_staged_store_install(
    store: &StoreRoot,
    txn: &mut StoreTxn,
    input: StagedStoreInstallPublishInput,
) -> Result<StagedStoreInstallPublication> {
    let incomplete = txn.build_path().join("incomplete");
    if incomplete.exists() {
        bail!(
            "store install transaction is incomplete: {}",
            file::display_path(incomplete)
        );
    }

    txn.set_state(StoreTxnState::Sealing)?;
    let object = publish_store_object(store, txn.build_path(), input.object)?;
    txn.set_object(object.object_id.clone(), object.path.clone())?;
    txn.set_state(StoreTxnState::PublishedObject)?;

    let realisation = publish_store_realisation(
        store,
        StoreRealisationInput {
            derivation_id: txn.manifest().derivation_id.clone(),
            object_id: object.object_id.clone(),
            object_path: object.path.clone(),
            tool: input.realisation.tool,
            backend: input.realisation.backend,
            version: input.realisation.version,
            platform: input.realisation.platform,
            options_hash: input.realisation.options_hash,
            source_hash: input.realisation.source_hash,
            lock_policy: input.realisation.lock_policy,
            provenance: input.realisation.provenance,
            closure: input.realisation.closure,
            compatibility_path: txn.manifest().compatibility_path.clone(),
        },
    )?;
    txn.set_realisation_id(realisation.realisation.realisation_id.clone())?;
    txn.set_state(StoreTxnState::LinkedCompatibility)?;

    Ok(StagedStoreInstallPublication {
        compatibility_path: txn.manifest().compatibility_path.clone(),
        object,
        realisation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{StoreTxnInput, gc_dry_run, read_roots, read_transaction_manifest};

    fn object_input() -> StoreObjectPublishInput {
        StoreObjectPublishInput {
            name: "demo-1.0.0".to_string(),
            platform: "test-platform".to_string(),
            created_by: "nise-test".to_string(),
            executable_paths: vec![PathBuf::from("bin/demo")],
            bin_paths: vec![PathBuf::from("bin")],
            references: vec![],
            realisations: vec![],
            relocation_tokens: vec![],
        }
    }

    fn realisation_metadata() -> StoreRealisationMetadata {
        StoreRealisationMetadata {
            tool: "demo".to_string(),
            backend: "test:demo".to_string(),
            version: "1.0.0".to_string(),
            platform: "test-platform".to_string(),
            options_hash: "sha256:options".to_string(),
            source_hash: "sha256:source".to_string(),
            lock_policy: "strict".to_string(),
            provenance: vec![ProvenanceRecord {
                kind: "test".to_string(),
                source: None,
                value: Some("ok".to_string()),
                verified: Some(true),
            }],
            closure: vec![],
        }
    }

    fn write_build(txn: &StoreTxn) -> Result<()> {
        file::create_dir_all(txn.build_path().join("bin"))?;
        file::write(txn.build_path().join("bin/demo"), "demo")?;
        Ok(())
    }

    #[test]
    fn publishes_staged_install_and_advances_transaction_state() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let installs_dir = tmp.path().join("installs").join("demo");
        let compatibility_path = installs_dir.join("1.0.0");
        let mut txn = StoreTxn::begin(
            store.clone(),
            StoreTxnInput {
                derivation_id: "sha256:derivation".to_string(),
                compatibility_path: compatibility_path.clone(),
            },
        )?;
        write_build(&txn)?;

        let published = publish_staged_store_install(
            &store,
            &mut txn,
            StagedStoreInstallPublishInput {
                object: object_input(),
                realisation: realisation_metadata(),
            },
        )?;

        assert_eq!(txn.manifest().state, StoreTxnState::LinkedCompatibility);
        assert_eq!(
            txn.manifest().object_id.as_deref(),
            Some(published.object.object_id.as_str())
        );
        assert_eq!(
            txn.manifest().realisation_id.as_deref(),
            Some(published.realisation.realisation.realisation_id.as_str())
        );
        assert_eq!(published.compatibility_path, compatibility_path);
        assert!(!txn.build_path().exists());
        assert!(published.object.path.exists());
        assert!(published.realisation.realisation_path.exists());
        assert!(published.realisation.install_ref_path.exists());
        assert!(compatibility_path.exists());

        let persisted = read_transaction_manifest(txn.manifest_path())?;
        assert_eq!(persisted.state, StoreTxnState::LinkedCompatibility);

        let discovered =
            crate::toolset::installed_versions::discover(&installs_dir, "demo", &store);
        assert_eq!(
            discovered,
            vec![
                crate::toolset::installed_versions::InstalledVersionEntry::StoreRef {
                    ref_manifest: published.realisation.install_ref.clone()
                }
            ]
        );

        let roots = read_roots(store.path());
        assert_eq!(roots.transactions.len(), 1);
        let gc = gc_dry_run(&store)?;
        assert_eq!(gc.objects.marked, 1);
        assert!(
            gc.marked_objects[&published.object.object_id]
                .iter()
                .any(|root| root.starts_with("transaction:"))
        );
        assert!(
            gc.marked_objects[&published.object.object_id]
                .iter()
                .any(|root| root.starts_with("install-ref:demo@1.0.0"))
        );

        let completed = txn.complete()?;
        assert_eq!(completed.state, StoreTxnState::Complete);
        let gc = gc_dry_run(&store)?;
        assert_eq!(gc.objects.marked, 1);
        assert!(
            gc.marked_objects[&published.object.object_id]
                .iter()
                .any(|root| root.starts_with("install-ref:demo@1.0.0"))
        );
        Ok(())
    }

    #[test]
    fn rejects_incomplete_staged_install_before_sealing() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let mut txn = StoreTxn::begin(
            store.clone(),
            StoreTxnInput {
                derivation_id: "sha256:derivation".to_string(),
                compatibility_path: tmp.path().join("installs/demo/1.0.0"),
            },
        )?;
        write_build(&txn)?;
        file::write(txn.build_path().join("incomplete"), "")?;

        let err = publish_staged_store_install(
            &store,
            &mut txn,
            StagedStoreInstallPublishInput {
                object: object_input(),
                realisation: realisation_metadata(),
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("transaction is incomplete"));
        assert_eq!(txn.manifest().state, StoreTxnState::Preparing);
        Ok(())
    }

    #[test]
    fn rejects_missing_declared_executable_before_publish() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let mut txn = StoreTxn::begin(
            store.clone(),
            StoreTxnInput {
                derivation_id: "sha256:derivation".to_string(),
                compatibility_path: tmp.path().join("installs/demo/1.0.0"),
            },
        )?;
        file::create_dir_all(txn.build_path().join("bin"))?;
        let mut object = object_input();
        object.executable_paths = vec![PathBuf::from("bin/missing")];

        let err = publish_staged_store_install(
            &store,
            &mut txn,
            StagedStoreInstallPublishInput {
                object,
                realisation: realisation_metadata(),
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("declared executable path"));
        assert_eq!(txn.manifest().state, StoreTxnState::Sealing);
        Ok(())
    }
}
