use std::path::{Path, PathBuf};

use eyre::{Result, bail};
use serde::Serialize;

use crate::file;
use crate::hash::hash_sha256_to_str;
use crate::store::legacy::install_ref_manifest_path;
use crate::store::{
    CompatibilityRef, InstallRefManifest, InstallRefMode, ProvenanceRecord, RealisationManifest,
    SCHEMA_VERSION, StoreRoot, read_manifest, validate_object_manifest_for_tree, write_manifest,
};

#[derive(Debug, Clone)]
pub struct StoreRealisationInput {
    pub derivation_id: String,
    pub object_id: String,
    pub object_path: PathBuf,
    pub tool: String,
    pub backend: String,
    pub version: String,
    pub platform: String,
    pub options_hash: String,
    pub source_hash: String,
    pub lock_policy: String,
    pub provenance: Vec<ProvenanceRecord>,
    pub closure: Vec<String>,
    pub compatibility_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoreRealisationPublication {
    pub realisation_path: PathBuf,
    pub install_ref_path: PathBuf,
    pub realisation: RealisationManifest,
    pub install_ref: InstallRefManifest,
}

pub fn publish_store_realisation(
    store: &StoreRoot,
    input: StoreRealisationInput,
) -> Result<StoreRealisationPublication> {
    validate_compatibility_path_can_be_replaced(&input.compatibility_path)?;
    let object = validate_object_manifest_for_tree(&input.object_path)?;
    if object.object_id != input.object_id {
        bail!(
            "store object id mismatch for realisation: expected {}, got {}",
            input.object_id,
            object.object_id
        );
    }

    let compatibility_mode = store_compatibility_mode();
    let compatibility = CompatibilityRef {
        path: input.compatibility_path.clone(),
        mode: compatibility_mode,
    };
    let realisation_id = realisation_id(&input, compatibility_mode);
    let realisation = RealisationManifest {
        schema_version: SCHEMA_VERSION,
        realisation_id: realisation_id.clone(),
        derivation_id: input.derivation_id,
        object_id: input.object_id.clone(),
        tool: input.tool.clone(),
        backend: input.backend.clone(),
        version: input.version.clone(),
        platform: input.platform,
        options_hash: input.options_hash,
        source_hash: input.source_hash,
        lock_policy: input.lock_policy,
        provenance: input.provenance,
        closure: input.closure,
        compatibility,
    };
    let install_ref = InstallRefManifest {
        schema_version: SCHEMA_VERSION,
        tool: input.tool,
        version: input.version,
        backend: input.backend,
        compatibility_path: input.compatibility_path,
        realisation_id: realisation.realisation_id.clone(),
        object_id: realisation.object_id.clone(),
        mode: compatibility_mode,
    };

    let realisation_path = realisation_manifest_path(store, &realisation.realisation_id)?;
    let install_ref_path =
        install_ref_manifest_path(store, &install_ref.tool, &install_ref.version);

    write_manifest(&realisation_path, &realisation)?;
    sync_parent(&realisation_path)?;
    if let Some(parent) = install_ref.compatibility_path.parent() {
        file::create_dir_all(parent)?;
    }
    file::make_symlink_or_file(&input.object_path, &install_ref.compatibility_path)?;
    sync_parent(&install_ref.compatibility_path)?;
    write_manifest(&install_ref_path, &install_ref)?;
    sync_parent(&install_ref_path)?;

    Ok(StoreRealisationPublication {
        realisation_path,
        install_ref_path,
        realisation,
        install_ref,
    })
}

pub fn realisation_manifest_path(store: &StoreRoot, realisation_id: &str) -> Result<PathBuf> {
    let digest = sha256_digest(realisation_id)?;
    Ok(store
        .realisations_dir()
        .join("sha256")
        .join(&digest[..2])
        .join(format!("{digest}.toml")))
}

fn store_compatibility_mode() -> InstallRefMode {
    if cfg!(windows) {
        InstallRefMode::StorePointerFile
    } else {
        InstallRefMode::StoreSymlink
    }
}

fn validate_compatibility_path_can_be_replaced(path: &Path) -> Result<()> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() || metadata.is_file() {
        return Ok(());
    }
    bail!(
        "store compatibility path is a real directory, refusing to replace: {}",
        file::display_path(path)
    );
}

fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        file::sync_dir(parent)?;
    }
    Ok(())
}

fn realisation_id(input: &StoreRealisationInput, compatibility_mode: InstallRefMode) -> String {
    let key = format!(
        "schema_version={SCHEMA_VERSION}\nderivation_id={}\nobject_id={}\ntool={}\nbackend={}\nversion={}\nplatform={}\noptions_hash={}\nsource_hash={}\nlock_policy={}\ncompatibility_path={}\ncompatibility_mode={compatibility_mode:?}\nclosure={}",
        input.derivation_id,
        input.object_id,
        input.tool,
        input.backend,
        input.version,
        input.platform,
        input.options_hash,
        input.source_hash,
        input.lock_policy,
        input.compatibility_path.to_string_lossy(),
        input.closure.join(","),
    );
    format!("sha256:{}", hash_sha256_to_str(&key))
}

fn sha256_digest(id: &str) -> Result<&str> {
    let Some((algorithm, digest)) = id.split_once(':') else {
        bail!("id must include hash algorithm: {id}");
    };
    if algorithm != "sha256" || digest.len() < 2 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid sha256 id: {id}");
    }
    Ok(digest)
}

pub fn read_realisation_manifest(path: impl AsRef<Path>) -> Result<RealisationManifest> {
    read_manifest(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{StoreObjectPublishInput, gc_dry_run, publish_store_object};

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

    fn realisation_input(
        object_id: String,
        object_path: PathBuf,
        compatibility_path: PathBuf,
    ) -> StoreRealisationInput {
        StoreRealisationInput {
            derivation_id: "sha256:derivation".to_string(),
            object_id,
            object_path,
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
            compatibility_path,
        }
    }

    fn publish_object(store: &StoreRoot) -> Result<(String, PathBuf)> {
        let build = store.tmp_dir().join("txn").join("build");
        file::create_dir_all(build.join("bin"))?;
        file::write(build.join("bin/demo"), "demo")?;
        let published = publish_store_object(store, &build, object_input())?;
        Ok((published.object_id, published.path))
    }

    #[test]
    fn publishes_realisation_and_store_backed_install_ref() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let installs_dir = tmp.path().join("installs").join("demo");
        let compatibility_path = installs_dir.join("1.0.0");
        let (object_id, object_path) = publish_object(&store)?;

        let published = publish_store_realisation(
            &store,
            realisation_input(
                object_id.clone(),
                object_path.clone(),
                compatibility_path.clone(),
            ),
        )?;

        assert!(published.realisation_path.exists());
        assert!(published.install_ref_path.exists());
        assert_eq!(published.realisation.object_id, object_id);
        assert_eq!(published.install_ref.object_id, object_id);
        assert_eq!(published.install_ref.compatibility_path, compatibility_path);
        assert!(compatibility_path.exists());
        assert_eq!(
            read_realisation_manifest(&published.realisation_path)?,
            published.realisation
        );
        #[cfg(unix)]
        {
            assert_eq!(published.install_ref.mode, InstallRefMode::StoreSymlink);
            assert!(compatibility_path.is_symlink());
        }

        let discovered =
            crate::toolset::installed_versions::discover(&installs_dir, "demo", &store);
        assert_eq!(
            discovered,
            vec![
                crate::toolset::installed_versions::InstalledVersionEntry::StoreRef {
                    ref_manifest: published.install_ref.clone()
                }
            ]
        );

        let gc = gc_dry_run(&store)?;
        assert_eq!(gc.objects.marked, 1);
        assert!(gc.candidates.is_empty());
        Ok(())
    }

    #[test]
    fn refuses_to_replace_legacy_real_directory() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let compatibility_path = tmp.path().join("installs/demo/1.0.0");
        file::create_dir_all(&compatibility_path)?;
        let (object_id, object_path) = publish_object(&store)?;

        let err = publish_store_realisation(
            &store,
            realisation_input(object_id, object_path, compatibility_path),
        )
        .unwrap_err();

        assert!(err.to_string().contains("real directory"));
        Ok(())
    }

    #[test]
    fn validates_object_id_before_writing_refs() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let compatibility_path = tmp.path().join("installs/demo/1.0.0");
        let (_object_id, object_path) = publish_object(&store)?;

        let err = publish_store_realisation(
            &store,
            realisation_input(
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_string(),
                object_path,
                compatibility_path.clone(),
            ),
        )
        .unwrap_err();

        assert!(err.to_string().contains("store object id mismatch"));
        assert!(!compatibility_path.exists());
        Ok(())
    }

    #[test]
    fn rejects_invalid_realisation_ids_for_paths() {
        let store = StoreRoot::new("/tmp/store");

        let err = realisation_manifest_path(&store, "legacy:abc").unwrap_err();

        assert!(err.to_string().contains("invalid sha256 id"));
    }
}
