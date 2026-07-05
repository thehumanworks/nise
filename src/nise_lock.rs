use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use eyre::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::file;
use crate::hash::hash_sha256_to_str;
use crate::lockfile::Lockfile;

pub const NISE_LOCK_FILE: &str = "nise.lock";
const NISE_LOCK_SCHEMA: &str = "nise.lock";
const NISE_LOCK_SCHEMA_VERSION: u32 = 1;
const HASH_ALGORITHM_SHA256: &str = "sha256";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NiseLock {
    pub schema: String,
    pub schema_version: u32,
    pub hash_algorithm: String,
    pub generator: String,
    pub policy: NiseLockPolicy,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, NiseLockSource>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub registries: BTreeMap<String, NiseLockRegistry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derivations: Vec<NiseDerivation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NiseLockPolicy {
    pub mode: String,
    pub offline: String,
    pub provenance: String,
    pub allow_tofu: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NiseLockSource {
    pub kind: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NiseLockRegistry {
    pub kind: String,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NiseDerivation {
    pub id: String,
    pub status: String,
    pub tool: String,
    pub request: String,
    pub request_kind: String,
    pub resolved_version: String,
    pub backend: String,
    pub backend_type: String,
    pub backend_identity: NiseBackendIdentity,
    pub source: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registries: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub platforms: BTreeMap<String, NiseDerivationPlatform>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NiseBackendIdentity {
    pub kind: String,
    pub mise_lock_backend: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NiseDerivationPlatform {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub realisation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closure: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NiseLockCheck {
    pub path: PathBuf,
    pub derivations: usize,
    pub strict: usize,
    pub legacy_unverified: usize,
}

impl NiseLock {
    pub fn import_mise_lock(path: &Path) -> Result<Self> {
        let mise_lock = Lockfile::read(path)?;
        let mut sources = BTreeMap::new();
        sources.insert(
            "src:mise-lock".to_string(),
            NiseLockSource {
                kind: "mise-lock".to_string(),
                path: path.to_path_buf(),
            },
        );
        let mut registries = BTreeMap::new();
        registries.insert(
            "mise-lock".to_string(),
            NiseLockRegistry {
                kind: "legacy-mise-lock".to_string(),
                revision: "imported".to_string(),
            },
        );
        let mut derivations = vec![];
        for (tool, versions) in mise_lock.tools() {
            for locked in versions {
                let backend = locked
                    .backend
                    .clone()
                    .unwrap_or_else(|| format!("mise:{tool}"));
                let platforms = locked
                    .platforms
                    .iter()
                    .map(|(platform, info)| {
                        (
                            platform.clone(),
                            NiseDerivationPlatform {
                                url: info.url.clone(),
                                checksum: info.checksum.clone(),
                                size: info.size,
                                provenance: info.provenance.as_ref().map(ToString::to_string),
                                realisation: None,
                                object: None,
                                closure: None,
                            },
                        )
                    })
                    .collect();
                let mut derivation = NiseDerivation {
                    id: String::new(),
                    status: "legacy-unverified".to_string(),
                    tool: tool.clone(),
                    request: locked.version.clone(),
                    request_kind: "exact-from-mise-lock".to_string(),
                    resolved_version: locked.version.clone(),
                    backend_type: backend_type(&backend),
                    backend_identity: NiseBackendIdentity {
                        kind: "mise-lock-import".to_string(),
                        mise_lock_backend: backend.clone(),
                    },
                    backend,
                    source: "src:mise-lock".to_string(),
                    registries: vec!["mise-lock".to_string()],
                    options: locked.options.clone(),
                    platforms,
                };
                derivation.id = derivation_id(&derivation);
                derivations.push(derivation);
            }
        }
        derivations.sort_by(|a, b| {
            a.tool
                .cmp(&b.tool)
                .then_with(|| a.resolved_version.cmp(&b.resolved_version))
                .then_with(|| a.backend.cmp(&b.backend))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(Self {
            schema: NISE_LOCK_SCHEMA.to_string(),
            schema_version: NISE_LOCK_SCHEMA_VERSION,
            hash_algorithm: HASH_ALGORITHM_SHA256.to_string(),
            generator: format!("nise {}", env!("CARGO_PKG_VERSION")),
            policy: NiseLockPolicy {
                mode: "advisory".to_string(),
                offline: "artifact".to_string(),
                provenance: "lock-or-reverify".to_string(),
                allow_tofu: false,
            },
            sources,
            registries,
            derivations,
        })
    }

    pub fn read(path: &Path) -> Result<Self> {
        let content = file::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            file::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        file::write(path, content)
    }

    fn validate_schema(&self) -> Result<()> {
        if self.schema != NISE_LOCK_SCHEMA {
            bail!(
                "unsupported nise lock schema {}, expected {}",
                self.schema,
                NISE_LOCK_SCHEMA
            );
        }
        if self.schema_version != NISE_LOCK_SCHEMA_VERSION {
            bail!(
                "unsupported nise lock schema version {}, expected {}",
                self.schema_version,
                NISE_LOCK_SCHEMA_VERSION
            );
        }
        if self.hash_algorithm != HASH_ALGORITHM_SHA256 {
            bail!(
                "unsupported nise lock hash algorithm {}, expected {}",
                self.hash_algorithm,
                HASH_ALGORITHM_SHA256
            );
        }
        Ok(())
    }
}

pub fn nise_lock_path_for_config(config: &Config) -> PathBuf {
    config
        .monorepo_lockfile_root()
        .or_else(|| config.project_root.clone())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join(NISE_LOCK_FILE)
}

pub fn check_nise_lock(path: &Path, require_strict: bool) -> Result<NiseLockCheck> {
    if !path.exists() {
        bail!("nise lockfile does not exist: {}", file::display_path(path));
    }
    let lock = NiseLock::read(path)?;
    lock.validate_schema()?;
    let strict = lock
        .derivations
        .iter()
        .filter(|derivation| derivation.status == "strict")
        .count();
    let legacy_unverified = lock
        .derivations
        .iter()
        .filter(|derivation| derivation.status == "legacy-unverified")
        .count();
    if require_strict && legacy_unverified > 0 {
        bail!(
            "nise lock has {legacy_unverified} legacy-unverified derivation(s); strict frozen mode requires all derivations to be strict"
        );
    }
    Ok(NiseLockCheck {
        path: path.to_path_buf(),
        derivations: lock.derivations.len(),
        strict,
        legacy_unverified,
    })
}

fn backend_type(backend: &str) -> String {
    backend
        .split_once(':')
        .map(|(kind, _)| kind)
        .unwrap_or(backend)
        .to_string()
}

fn derivation_id(derivation: &NiseDerivation) -> String {
    let platforms = derivation
        .platforms
        .iter()
        .map(|(platform, info)| {
            format!(
                "{platform}:{:?}:{:?}:{:?}:{:?}",
                info.url, info.checksum, info.size, info.provenance
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let options = derivation
        .options
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n");
    let key = format!(
        "schema_version={NISE_LOCK_SCHEMA_VERSION}\nstatus={}\ntool={}\nrequest={}\nrequest_kind={}\nresolved_version={}\nbackend={}\nbackend_type={}\noptions=\n{}\nplatforms=\n{}",
        derivation.status,
        derivation.tool,
        derivation.request,
        derivation.request_kind,
        derivation.resolved_version,
        derivation.backend,
        derivation.backend_type,
        options,
        platforms
    );
    format!("sha256:{}", hash_sha256_to_str(&key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockfile::{Lockfile, PlatformInfo};

    #[test]
    fn imports_mise_lock_as_legacy_unverified_derivations() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let mise_lock_path = tmp.path().join("mise.lock");
        let mut mise_lock = Lockfile::default();
        mise_lock.set_platform_info(
            "demo",
            "1.0.0",
            Some("aqua:demo/demo"),
            &BTreeMap::new(),
            "macos-arm64",
            PlatformInfo {
                url: Some("https://example.test/demo.tar.gz".to_string()),
                checksum: Some("sha256:demo".to_string()),
                size: Some(123),
                ..Default::default()
            },
        );
        mise_lock.write(&mise_lock_path)?;

        let nise_lock = NiseLock::import_mise_lock(&mise_lock_path)?;

        assert_eq!(nise_lock.schema, NISE_LOCK_SCHEMA);
        assert_eq!(nise_lock.derivations.len(), 1);
        let derivation = &nise_lock.derivations[0];
        assert_eq!(derivation.status, "legacy-unverified");
        assert_eq!(derivation.tool, "demo");
        assert_eq!(derivation.backend, "aqua:demo/demo");
        assert_eq!(derivation.backend_type, "aqua");
        assert_eq!(
            derivation.platforms["macos-arm64"].checksum.as_deref(),
            Some("sha256:demo")
        );
        assert!(derivation.id.starts_with("sha256:"));
        Ok(())
    }

    #[test]
    fn frozen_check_fails_legacy_unverified_derivations() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let nise_lock_path = tmp.path().join(NISE_LOCK_FILE);
        let mut nise_lock = NiseLock::import_mise_lock(&tmp.path().join("missing-mise.lock"))?;
        nise_lock.derivations.push(NiseDerivation {
            id: "sha256:legacy".to_string(),
            status: "legacy-unverified".to_string(),
            tool: "demo".to_string(),
            request: "1.0.0".to_string(),
            request_kind: "exact-from-mise-lock".to_string(),
            resolved_version: "1.0.0".to_string(),
            backend: "mise:demo".to_string(),
            backend_type: "mise".to_string(),
            backend_identity: NiseBackendIdentity {
                kind: "test".to_string(),
                mise_lock_backend: "mise:demo".to_string(),
            },
            source: "src:mise-lock".to_string(),
            registries: vec![],
            options: BTreeMap::new(),
            platforms: BTreeMap::new(),
        });
        nise_lock.write(&nise_lock_path)?;

        let err = check_nise_lock(&nise_lock_path, true).unwrap_err();

        assert!(err.to_string().contains("legacy-unverified"));
        Ok(())
    }
}
