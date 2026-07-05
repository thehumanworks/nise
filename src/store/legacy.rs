use std::path::{Path, PathBuf};

use eyre::Result;
use heck::ToKebabCase;

use crate::file;
use crate::hash::hash_sha256_to_str;
use crate::store::{
    CompatibilityRef, InstallRefManifest, InstallRefMode, ProvenanceRecord, RealisationManifest,
    SCHEMA_VERSION, StoreRoot, write_manifest,
};

pub struct LegacyInstallManifestInput<'a> {
    pub tool: &'a str,
    pub backend: &'a str,
    pub version: &'a str,
    pub platform: &'a str,
    pub compatibility_path: &'a Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyInstallManifests {
    pub realisation_path: PathBuf,
    pub install_ref_path: PathBuf,
    pub realisation: RealisationManifest,
    pub install_ref: InstallRefManifest,
}

pub fn write_legacy_install_manifests(
    store: &StoreRoot,
    input: LegacyInstallManifestInput<'_>,
) -> Result<LegacyInstallManifests> {
    let ids = LegacyInstallIds::new(&input);
    let compatibility_path = input.compatibility_path.to_path_buf();
    let realisation = RealisationManifest {
        schema_version: SCHEMA_VERSION,
        realisation_id: ids.realisation_id.clone(),
        derivation_id: ids.derivation_id,
        object_id: ids.object_id.clone(),
        tool: input.tool.to_string(),
        backend: input.backend.to_string(),
        version: input.version.to_string(),
        platform: input.platform.to_string(),
        options_hash: "sha256:legacy-options".to_string(),
        source_hash: "sha256:legacy-source".to_string(),
        lock_policy: "legacy-unverified".to_string(),
        provenance: vec![ProvenanceRecord {
            kind: "legacy-install".to_string(),
            source: Some(file::display_path(&compatibility_path)),
            value: None,
            verified: Some(false),
        }],
        closure: vec![],
        compatibility: CompatibilityRef {
            path: compatibility_path.clone(),
            mode: InstallRefMode::LegacyRealDirectory,
        },
    };
    let install_ref = InstallRefManifest {
        schema_version: SCHEMA_VERSION,
        tool: input.tool.to_string(),
        version: input.version.to_string(),
        backend: input.backend.to_string(),
        compatibility_path,
        realisation_id: realisation.realisation_id.clone(),
        object_id: realisation.object_id.clone(),
        mode: InstallRefMode::LegacyRealDirectory,
    };
    let realisation_path = legacy_realisation_path(store, &ids.realisation_digest);
    let install_ref_path = install_ref_manifest_path(store, input.tool, input.version);

    write_manifest(&realisation_path, &realisation)?;
    write_manifest(&install_ref_path, &install_ref)?;

    Ok(LegacyInstallManifests {
        realisation_path,
        install_ref_path,
        realisation,
        install_ref,
    })
}

pub fn remove_install_ref_manifest(
    store: &StoreRoot,
    tool: &str,
    version: &str,
    dry_run: bool,
) -> Result<bool> {
    let path = install_ref_manifest_path(store, tool, version);
    if dry_run {
        return Ok(path.exists());
    }
    if path.exists() {
        file::remove_file(path)?;
        return Ok(true);
    }
    Ok(false)
}

pub fn install_ref_manifest_path(store: &StoreRoot, tool: &str, version: &str) -> PathBuf {
    store
        .install_refs_dir()
        .join(tool.to_kebab_case())
        .join(format!("{version}.toml"))
}

fn legacy_realisation_path(store: &StoreRoot, digest: &str) -> PathBuf {
    store
        .realisations_dir()
        .join("sha256")
        .join(&digest[..2])
        .join(format!("{digest}.toml"))
}

struct LegacyInstallIds {
    derivation_id: String,
    realisation_id: String,
    object_id: String,
    realisation_digest: String,
}

impl LegacyInstallIds {
    fn new(input: &LegacyInstallManifestInput<'_>) -> Self {
        let key = format!(
            "schema_version={SCHEMA_VERSION}\ncreated_by=nise-legacy-manifest\ntool={}\nbackend={}\nversion={}\nplatform={}\ncompatibility_path={}",
            input.tool,
            input.backend,
            input.version,
            input.platform,
            input.compatibility_path.to_string_lossy(),
        );
        let realisation_digest = hash_sha256_to_str(&key);
        Self {
            derivation_id: format!(
                "sha256:{}",
                hash_sha256_to_str(&format!("derivation\n{key}"))
            ),
            realisation_id: format!("sha256:{realisation_digest}"),
            object_id: format!("legacy:{realisation_digest}"),
            realisation_digest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::read_manifest;

    #[test]
    fn writes_manifest_only_legacy_install_refs() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let compatibility_path = tmp.path().join("installs/demo/1.0.0");
        file::create_dir_all(&compatibility_path)?;

        let written = write_legacy_install_manifests(
            &store,
            LegacyInstallManifestInput {
                tool: "demo",
                backend: "test:demo",
                version: "1.0.0",
                platform: "test-platform",
                compatibility_path: &compatibility_path,
            },
        )?;

        assert!(written.realisation_path.exists());
        assert!(written.install_ref_path.exists());
        assert_eq!(
            written.install_ref.mode,
            InstallRefMode::LegacyRealDirectory
        );
        assert_eq!(written.install_ref.compatibility_path, compatibility_path);
        assert!(written.install_ref.object_id.starts_with("legacy:"));
        let install_ref: InstallRefManifest = read_manifest(&written.install_ref_path)?;
        assert_eq!(install_ref, written.install_ref);
        Ok(())
    }

    #[test]
    fn removes_install_ref_manifest_without_touching_compatibility_path() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let compatibility_path = tmp.path().join("installs/demo/1.0.0");
        file::create_dir_all(&compatibility_path)?;
        let written = write_legacy_install_manifests(
            &store,
            LegacyInstallManifestInput {
                tool: "demo",
                backend: "test:demo",
                version: "1.0.0",
                platform: "test-platform",
                compatibility_path: &compatibility_path,
            },
        )?;

        assert!(remove_install_ref_manifest(&store, "demo", "1.0.0", false)?);

        assert!(!written.install_ref_path.exists());
        assert!(compatibility_path.exists());
        assert!(!remove_install_ref_manifest(&store, "demo", "1.0.0", true)?);
        Ok(())
    }
}
