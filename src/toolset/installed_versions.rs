use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use eyre::{Result, bail};
use heck::ToKebabCase;
use walkdir::WalkDir;

use crate::file;
use crate::runtime_symlinks;
use crate::store::{InstallRefManifest, InstallRefMode, SCHEMA_VERSION, StoreRoot, read_manifest};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstalledVersionEntry {
    LegacyDir { path: PathBuf },
    StoreRef { ref_manifest: InstallRefManifest },
    RuntimeAlias { path: PathBuf },
    BrokenRef { path: PathBuf, reason: String },
}

impl InstalledVersionEntry {
    pub fn concrete_version(&self) -> Option<String> {
        match self {
            Self::LegacyDir { path } => version_name_from_path(path),
            Self::StoreRef { ref_manifest } => Some(ref_manifest.version.clone()),
            Self::RuntimeAlias { .. } | Self::BrokenRef { .. } => None,
        }
    }

    #[allow(dead_code)]
    pub fn is_runtime_alias(&self) -> bool {
        matches!(self, Self::RuntimeAlias { .. })
    }

    #[allow(dead_code)]
    pub fn is_broken_ref(&self) -> bool {
        matches!(self, Self::BrokenRef { .. })
    }
}

pub fn list_concrete_versions(installs_dir: &Path, tool: &str) -> Vec<String> {
    list_concrete_versions_in(installs_dir, tool, &StoreRoot::default())
}

pub fn list_concrete_versions_in(
    installs_dir: &Path,
    tool: &str,
    store: &StoreRoot,
) -> Vec<String> {
    discover(installs_dir, tool, store)
        .into_iter()
        .filter_map(|entry| entry.concrete_version())
        .collect()
}

pub fn broken_store_refs(store: &StoreRoot) -> Vec<InstalledVersionEntry> {
    all_store_ref_manifest_paths(store)
        .into_iter()
        .map(|path| store_ref_entry_from_path(&path))
        .filter(InstalledVersionEntry::is_broken_ref)
        .collect()
}

pub fn discover(installs_dir: &Path, tool: &str, store: &StoreRoot) -> Vec<InstalledVersionEntry> {
    let mut entries = BTreeMap::new();
    let mut seen_store_ref_versions = HashSet::new();

    if let Ok(children) = sorted_children(installs_dir) {
        for path in children {
            let Some(name) = version_name_from_path(&path) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            if runtime_symlinks::is_runtime_symlink(&path) {
                entries.insert(name, InstalledVersionEntry::RuntimeAlias { path });
                continue;
            }
            if path.join("incomplete").exists() {
                continue;
            }
            if let Some(entry) = store_ref_entry(store, tool, &name) {
                if matches!(entry, InstalledVersionEntry::StoreRef { .. }) {
                    seen_store_ref_versions.insert(name.clone());
                }
                entries.insert(name, entry);
                continue;
            }
            if path.exists() {
                entries.insert(name, InstalledVersionEntry::LegacyDir { path });
            }
        }
    }

    for path in store_ref_manifest_paths(store, tool) {
        let version = path
            .file_stem()
            .and_then(OsStr::to_str)
            .map(String::from)
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        if seen_store_ref_versions.contains(&version) || entries.contains_key(&version) {
            continue;
        }
        entries.insert(version, store_ref_entry_from_path(&path));
    }

    entries.into_values().collect()
}

pub fn find(installs_dir: &Path, tool: &str, version: &str) -> Option<InstalledVersionEntry> {
    discover(installs_dir, tool, &StoreRoot::default())
        .into_iter()
        .find(|entry| entry.concrete_version().as_deref() == Some(version))
}

pub fn remove_store_ref(ref_manifest: &InstallRefManifest, dry_run: bool) -> Result<bool> {
    remove_store_ref_in(&StoreRoot::default(), ref_manifest, dry_run)
}

pub fn remove_broken_ref(path: &Path, dry_run: bool) -> Result<bool> {
    if dry_run {
        return Ok(true);
    }
    if path.exists() {
        file::remove_file(path)?;
    }
    Ok(true)
}

pub fn remove_store_ref_in(
    store: &StoreRoot,
    ref_manifest: &InstallRefManifest,
    dry_run: bool,
) -> Result<bool> {
    if matches!(ref_manifest.mode, InstallRefMode::LegacyRealDirectory) {
        return Ok(false);
    }
    if dry_run {
        return Ok(true);
    }
    remove_compatibility_ref(&ref_manifest.compatibility_path)?;
    let manifest_path = store
        .install_refs_dir()
        .join(ref_manifest.tool.to_kebab_case())
        .join(format!("{}.toml", ref_manifest.version));
    if manifest_path.exists() {
        file::remove_file(manifest_path)?;
    }
    Ok(true)
}

fn sorted_children(path: &Path) -> Result<Vec<PathBuf>> {
    if !path.is_dir() {
        return Ok(vec![]);
    }
    let mut children = vec![];
    for entry in path.read_dir()? {
        children.push(entry?.path());
    }
    children.sort();
    Ok(children)
}

fn version_name_from_path(path: &Path) -> Option<String> {
    path.file_name().and_then(OsStr::to_str).map(String::from)
}

fn store_ref_manifest_paths(store: &StoreRoot, tool: &str) -> Vec<PathBuf> {
    file::ls(&store.install_refs_dir().join(tool.to_kebab_case()))
        .unwrap_or_default()
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect()
}

fn all_store_ref_manifest_paths(store: &StoreRoot) -> Vec<PathBuf> {
    let root = store.install_refs_dir();
    if !root.exists() {
        return vec![];
    }
    let mut paths = WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn store_ref_entry(store: &StoreRoot, tool: &str, version: &str) -> Option<InstalledVersionEntry> {
    let path = store
        .install_refs_dir()
        .join(tool.to_kebab_case())
        .join(format!("{version}.toml"));
    path.exists().then(|| store_ref_entry_from_path(&path))
}

fn store_ref_entry_from_path(path: &Path) -> InstalledVersionEntry {
    let ref_manifest: InstallRefManifest = match read_manifest(path) {
        Ok(manifest) => manifest,
        Err(err) => {
            return InstalledVersionEntry::BrokenRef {
                path: path.to_path_buf(),
                reason: err.to_string(),
            };
        }
    };
    if ref_manifest.schema_version != SCHEMA_VERSION {
        return InstalledVersionEntry::BrokenRef {
            path: path.to_path_buf(),
            reason: format!(
                "unsupported install ref manifest schema version {}, expected {}",
                ref_manifest.schema_version, SCHEMA_VERSION
            ),
        };
    }
    if !ref_manifest.compatibility_path.exists() {
        return InstalledVersionEntry::BrokenRef {
            path: path.to_path_buf(),
            reason: format!(
                "compatibility path does not exist: {}",
                file::display_path(&ref_manifest.compatibility_path)
            ),
        };
    }
    InstalledVersionEntry::StoreRef { ref_manifest }
}

fn remove_compatibility_ref(path: &Path) -> Result<()> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() || metadata.is_file() {
        file::remove_file(path)?;
    } else if metadata.is_dir() {
        bail!(
            "store compatibility ref is a real directory, refusing to remove without legacy uninstall: {}",
            file::display_path(path)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{InstallRefMode, write_manifest};

    fn install_ref(tool: &str, version: &str, compatibility_path: PathBuf) -> InstallRefManifest {
        InstallRefManifest {
            schema_version: SCHEMA_VERSION,
            tool: tool.to_string(),
            version: version.to_string(),
            backend: format!("test:{tool}"),
            compatibility_path,
            realisation_id: format!("sha256:{tool}-{version}-realisation"),
            object_id: format!("sha256:{tool}-{version}-object"),
            mode: InstallRefMode::StorePointerFile,
        }
    }

    #[test]
    fn classifies_legacy_dirs_and_runtime_aliases_without_semver_assumptions() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let installs_dir = tmp.path().join("installs").join("demo");
        for version in ["nightly", "ref-main", "lts-iron", "20241015", "3.12.0a1"] {
            file::create_dir_all(installs_dir.join(version))?;
        }
        file::make_symlink_or_file(&PathBuf::from("./20241015"), &installs_dir.join("latest"))?;

        let entries = discover(&installs_dir, "demo", &store);
        let versions = entries
            .iter()
            .filter_map(InstalledVersionEntry::concrete_version)
            .collect::<Vec<_>>();

        assert_eq!(
            versions,
            vec!["20241015", "3.12.0a1", "lts-iron", "nightly", "ref-main"]
        );
        assert!(entries.iter().any(InstalledVersionEntry::is_runtime_alias));
        Ok(())
    }

    #[test]
    fn classifies_store_refs_as_concrete_versions() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let installs_dir = tmp.path().join("installs").join("demo");
        let compatibility_path = installs_dir.join("1.0.0");
        file::create_dir_all(&compatibility_path)?;
        let manifest = install_ref("demo", "1.0.0", compatibility_path);
        write_manifest(
            store.install_refs_dir().join("demo").join("1.0.0.toml"),
            &manifest,
        )?;

        let entries = discover(&installs_dir, "demo", &store);

        assert_eq!(
            entries,
            vec![InstalledVersionEntry::StoreRef {
                ref_manifest: manifest
            }]
        );
        assert_eq!(
            list_concrete_versions_in(&installs_dir, "demo", &store),
            vec!["1.0.0"]
        );
        Ok(())
    }

    #[test]
    fn classifies_missing_store_compatibility_path_as_broken_ref() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let installs_dir = tmp.path().join("installs").join("demo");
        let manifest = install_ref("demo", "1.0.0", installs_dir.join("1.0.0"));
        write_manifest(
            store.install_refs_dir().join("demo").join("1.0.0.toml"),
            &manifest,
        )?;

        let entries = discover(&installs_dir, "demo", &store);

        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_broken_ref());
        assert!(entries[0].concrete_version().is_none());
        Ok(())
    }

    #[test]
    fn remove_store_ref_removes_only_compatibility_ref_and_manifest() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let installs_dir = tmp.path().join("installs").join("demo");
        let compatibility_path = installs_dir.join("1.0.0");
        file::create_dir_all(&installs_dir)?;
        file::write(&compatibility_path, "/store/object")?;
        let manifest = install_ref("demo", "1.0.0", compatibility_path.clone());
        let manifest_path = store.install_refs_dir().join("demo").join("1.0.0.toml");
        write_manifest(&manifest_path, &manifest)?;

        remove_store_ref_in(&store, &manifest, false)?;

        assert!(!compatibility_path.exists());
        assert!(!manifest_path.exists());
        Ok(())
    }

    #[test]
    fn broken_store_refs_lists_only_invalid_install_refs() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let installs_dir = tmp.path().join("installs").join("demo");
        let good_compatibility_path = installs_dir.join("1.0.0");
        file::create_dir_all(&good_compatibility_path)?;
        write_manifest(
            store.install_refs_dir().join("demo").join("1.0.0.toml"),
            &install_ref("demo", "1.0.0", good_compatibility_path),
        )?;
        let broken_path = store.install_refs_dir().join("demo").join("2.0.0.toml");
        write_manifest(
            &broken_path,
            &install_ref("demo", "2.0.0", installs_dir.join("2.0.0")),
        )?;

        let broken = broken_store_refs(&store);

        assert_eq!(broken.len(), 1);
        assert!(matches!(
            &broken[0],
            InstalledVersionEntry::BrokenRef { path, reason }
                if path == &broken_path && reason.contains("compatibility path does not exist")
        ));
        Ok(())
    }

    #[test]
    fn remove_broken_ref_removes_manifest_only() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let broken_path = tmp.path().join("store/refs/installs/demo/2.0.0.toml");
        file::create_dir_all(broken_path.parent().unwrap())?;
        file::write(&broken_path, "schema_version = ")?;

        remove_broken_ref(&broken_path, false)?;

        assert!(!broken_path.exists());
        Ok(())
    }
}
