use std::path::{Path, PathBuf};

use eyre::{Context, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::store::{
    HASH_ALGORITHM_SHA256, OBJECT_MANIFEST_FILE, PROFILE_MANIFEST_FILE, SCHEMA_VERSION,
    canonical_tree_hash,
};
use crate::{Result, file};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ObjectManifest {
    pub schema_version: u32,
    pub object_id: String,
    pub tree_hash: String,
    pub hash_algorithm: String,
    pub name: String,
    pub platform: String,
    pub created_by: String,
    pub created_at: String,
    pub bytes: u64,
    pub files: u64,
    pub executable_paths: Vec<PathBuf>,
    pub bin_paths: Vec<PathBuf>,
    pub references: Vec<String>,
    pub realisations: Vec<String>,
}

impl ObjectManifest {
    pub fn new(name: impl Into<String>, platform: impl Into<String>, tree_hash: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            object_id: format!("{HASH_ALGORITHM_SHA256}:{tree_hash}"),
            tree_hash,
            hash_algorithm: HASH_ALGORITHM_SHA256.to_string(),
            name: name.into(),
            platform: platform.into(),
            created_by: "nise".to_string(),
            created_at: String::new(),
            bytes: 0,
            files: 0,
            executable_paths: vec![],
            bin_paths: vec![],
            references: vec![],
            realisations: vec![],
        }
    }

    pub fn validate_hash_identity(&self) -> Result<()> {
        validate_schema_version("object manifest", self.schema_version)?;
        if self.hash_algorithm != HASH_ALGORITHM_SHA256 {
            bail!(
                "unsupported object hash algorithm {}, expected {}",
                self.hash_algorithm,
                HASH_ALGORITHM_SHA256
            );
        }
        let expected = format!("{}:{}", self.hash_algorithm, self.tree_hash);
        if self.object_id != expected {
            bail!(
                "object id mismatch: expected {}, got {}",
                expected,
                self.object_id
            );
        }
        Ok(())
    }
}

pub(super) trait StoreManifest {
    const KIND: &'static str;

    fn schema_version(&self) -> u32;

    fn validate_schema(&self) -> Result<()> {
        validate_schema_version(Self::KIND, self.schema_version())
    }
}

fn validate_schema_version(kind: &str, schema_version: u32) -> Result<()> {
    if schema_version != SCHEMA_VERSION {
        bail!(
            "unsupported {} schema version {}, expected {}",
            kind,
            schema_version,
            SCHEMA_VERSION
        );
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct RealisationManifest {
    pub schema_version: u32,
    pub realisation_id: String,
    pub derivation_id: String,
    pub object_id: String,
    pub tool: String,
    pub backend: String,
    pub version: String,
    pub platform: String,
    pub options_hash: String,
    pub source_hash: String,
    pub lock_policy: String,
    pub provenance: Vec<ProvenanceRecord>,
    pub closure: Vec<String>,
    pub compatibility: CompatibilityRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ProvenanceRecord {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct CompatibilityRef {
    pub path: PathBuf,
    pub mode: InstallRefMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct InstallRefManifest {
    pub schema_version: u32,
    pub tool: String,
    pub version: String,
    pub backend: String,
    pub compatibility_path: PathBuf,
    pub realisation_id: String,
    pub object_id: String,
    pub mode: InstallRefMode,
}

impl StoreManifest for InstallRefManifest {
    const KIND: &'static str = "install ref manifest";

    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallRefMode {
    StoreSymlink,
    StorePointerFile,
    LegacyRealDirectory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ProfileManifest {
    pub schema_version: u32,
    pub profile_id: String,
    pub generation: u64,
    pub project_root: Option<PathBuf>,
    pub source_config_hash: String,
    pub nise_lock_hash: String,
    pub created_at: String,
    pub realisations: Vec<String>,
    pub env_hash: String,
    pub path_entries: Vec<PathBuf>,
}

impl StoreManifest for ProfileManifest {
    const KIND: &'static str = "profile manifest";

    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct StoreTransactionManifest {
    pub schema_version: u32,
    pub txn_id: String,
    pub state: StoreTxnState,
    pub created_at: String,
    pub updated_at: String,
    pub pid: u32,
    pub derivation_id: String,
    pub realisation_id: Option<String>,
    pub object_id: Option<String>,
    pub build_path: PathBuf,
    pub store_path: Option<PathBuf>,
    pub compatibility_path: PathBuf,
}

impl StoreManifest for StoreTransactionManifest {
    const KIND: &'static str = "transaction manifest";

    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoreTxnState {
    Preparing,
    Building,
    Sealing,
    PublishedObject,
    LinkedCompatibility,
    ProfileRooted,
    Complete,
    Failed,
}

pub fn read_manifest<T>(path: impl AsRef<Path>) -> Result<T>
where
    T: DeserializeOwned,
{
    let path = path.as_ref();
    let contents = file::read_to_string(path)?;
    toml::from_str(&contents)
        .wrap_err_with(|| format!("failed to parse manifest: {}", file::display_path(path)))
}

pub fn write_manifest<T>(path: impl AsRef<Path>, manifest: &T) -> Result<()>
where
    T: Serialize,
{
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        file::create_dir_all(parent)?;
    }
    let contents = toml::to_string_pretty(manifest)
        .wrap_err_with(|| format!("failed to serialize manifest: {}", file::display_path(path)))?;
    file::write(path, contents)
}

pub fn read_object_manifest(object_root: impl AsRef<Path>) -> Result<ObjectManifest> {
    read_manifest(object_root.as_ref().join(OBJECT_MANIFEST_FILE))
}

pub fn write_object_manifest(
    object_root: impl AsRef<Path>,
    manifest: &ObjectManifest,
) -> Result<()> {
    write_manifest(object_root.as_ref().join(OBJECT_MANIFEST_FILE), manifest)
}

pub fn write_profile_manifest(
    profile_root: impl AsRef<Path>,
    manifest: &ProfileManifest,
) -> Result<()> {
    write_manifest(profile_root.as_ref().join(PROFILE_MANIFEST_FILE), manifest)
}

pub fn validate_object_manifest_for_tree(object_root: impl AsRef<Path>) -> Result<ObjectManifest> {
    let object_root = object_root.as_ref();
    let manifest = read_object_manifest(object_root)?;
    manifest.validate_hash_identity()?;
    let tree = canonical_tree_hash(object_root)?;
    if manifest.tree_hash != tree.hash {
        bail!(
            "tree hash mismatch for {}: expected {}, got {}",
            file::display_path(object_root),
            manifest.tree_hash,
            tree.hash
        );
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> ObjectManifest {
        ObjectManifest {
            schema_version: SCHEMA_VERSION,
            object_id: "sha256:abc123".to_string(),
            tree_hash: "abc123".to_string(),
            hash_algorithm: HASH_ALGORITHM_SHA256.to_string(),
            name: "ripgrep-14.1.1".to_string(),
            platform: "test-platform".to_string(),
            created_by: "nise-test".to_string(),
            created_at: "2026-07-03T00:00:00Z".to_string(),
            bytes: 10,
            files: 1,
            executable_paths: vec![PathBuf::from("bin/rg")],
            bin_paths: vec![PathBuf::from("bin")],
            references: vec!["sha256:def456".to_string()],
            realisations: vec!["sha256:realisation".to_string()],
        }
    }

    #[test]
    fn manifest_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("object.toml");
        let manifest = sample_manifest();

        write_manifest(&path, &manifest).unwrap();
        let actual: ObjectManifest = read_manifest(&path).unwrap();

        assert_eq!(actual, manifest);
    }

    #[test]
    fn corrupt_manifest_detection() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("object.toml");
        file::write(&path, "schema_version = ").unwrap();

        let err = read_manifest::<ObjectManifest>(&path).unwrap_err();

        assert!(err.to_string().contains("failed to parse manifest"));
    }

    #[test]
    fn object_id_mismatch_is_detected() {
        let mut manifest = sample_manifest();
        manifest.object_id = "sha256:not-the-tree".to_string();

        let err = manifest.validate_hash_identity().unwrap_err();

        assert!(err.to_string().contains("object id mismatch"));
    }
}
