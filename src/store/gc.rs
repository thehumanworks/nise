use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use eyre::Result;
use serde::Serialize;
use walkdir::WalkDir;

use crate::file;
use crate::store::{
    OBJECT_MANIFEST_FILE, ObjectManifest, RealisationManifest, RootScanError, StoreRoot,
    read_manifest, read_roots,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GcReport {
    pub store_root: PathBuf,
    pub dry_run: bool,
    pub roots: GcRootCounts,
    pub objects: GcObjectCounts,
    pub marked_objects: BTreeMap<String, Vec<String>>,
    pub candidates: Vec<GcCandidate>,
    pub issues: Vec<RootScanError>,
}

impl GcReport {
    pub fn has_issues(&self) -> bool {
        !self.issues.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GcRootCounts {
    pub install_refs: usize,
    pub profiles: usize,
    pub transactions: usize,
    pub pins: usize,
    pub process_leases: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GcObjectCounts {
    pub total: usize,
    pub marked: usize,
    pub candidates: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GcCandidate {
    pub object_id: String,
    pub path: PathBuf,
}

pub fn gc_dry_run(store: &StoreRoot) -> Result<GcReport> {
    let root_scan = read_roots(store.path());
    let mut issues = root_scan.errors.clone();
    let realisations = read_realisations(store, &mut issues)?;
    let objects = read_objects(store, &mut issues)?;
    let mut marked_objects: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for install_ref in &root_scan.install_refs {
        mark_object(
            &mut marked_objects,
            &install_ref.object_id,
            format!("install-ref:{}@{}", install_ref.tool, install_ref.version),
        );
    }

    for profile in &root_scan.profiles {
        for realisation_id in &profile.realisations {
            if let Some(object_id) = realisations.get(realisation_id) {
                mark_object(
                    &mut marked_objects,
                    object_id,
                    format!("profile:{}#{}", profile.profile_id, profile.generation),
                );
            }
        }
    }

    for transaction in &root_scan.transactions {
        if let Some(object_id) = &transaction.object_id {
            mark_object(
                &mut marked_objects,
                object_id,
                format!("transaction:{}", transaction.txn_id),
            );
        }
    }

    for pin in &root_scan.pins {
        if let Some(object_id) = pin_object_id(pin) {
            mark_object(&mut marked_objects, &object_id, "pin".to_string());
        }
    }

    let candidates = objects
        .iter()
        .filter(|(object_id, _)| !marked_objects.contains_key(*object_id))
        .map(|(object_id, path)| GcCandidate {
            object_id: object_id.clone(),
            path: path.clone(),
        })
        .collect::<Vec<_>>();
    let marked_objects = marked_objects
        .into_iter()
        .map(|(object_id, roots)| (object_id, roots.into_iter().collect()))
        .collect::<BTreeMap<_, _>>();

    Ok(GcReport {
        store_root: store.path().to_path_buf(),
        dry_run: true,
        roots: GcRootCounts {
            install_refs: root_scan.install_refs.len(),
            profiles: root_scan.profiles.len(),
            transactions: root_scan.transactions.len(),
            pins: root_scan.pins.len(),
            process_leases: root_scan.process_leases.len(),
        },
        objects: GcObjectCounts {
            total: objects.len(),
            marked: marked_objects.len(),
            candidates: candidates.len(),
        },
        marked_objects,
        candidates,
        issues,
    })
}

fn mark_object(
    marked_objects: &mut BTreeMap<String, BTreeSet<String>>,
    object_id: &str,
    root: String,
) {
    marked_objects
        .entry(object_id.to_string())
        .or_default()
        .insert(root);
}

fn read_realisations(
    store: &StoreRoot,
    issues: &mut Vec<RootScanError>,
) -> Result<BTreeMap<String, String>> {
    let mut realisations = BTreeMap::new();
    for path in toml_paths(&store.realisations_dir()) {
        match read_manifest::<RealisationManifest>(&path) {
            Ok(realisation) => {
                realisations.insert(realisation.realisation_id, realisation.object_id);
            }
            Err(err) => issues.push(RootScanError {
                path,
                error: err.to_string(),
            }),
        }
    }
    Ok(realisations)
}

fn read_objects(
    store: &StoreRoot,
    issues: &mut Vec<RootScanError>,
) -> Result<BTreeMap<String, PathBuf>> {
    let mut objects = BTreeMap::new();
    for path in object_manifest_paths(&store.objects_dir()) {
        match read_manifest::<ObjectManifest>(&path) {
            Ok(object) => {
                let object_path = path.parent().unwrap_or(store.path()).to_path_buf();
                objects.insert(object.object_id, object_path);
            }
            Err(err) => issues.push(RootScanError {
                path,
                error: err.to_string(),
            }),
        }
    }
    Ok(objects)
}

fn pin_object_id(path: &Path) -> Option<String> {
    file::read_to_string(path)
        .ok()
        .map(|content| content.trim().to_string())
        .filter(|content| !content.is_empty())
        .or_else(|| {
            path.file_stem()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
}

fn toml_paths(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return vec![];
    }
    WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect()
}

fn object_manifest_paths(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return vec![];
    }
    WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name == OBJECT_MANIFEST_FILE)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{
        CompatibilityRef, HASH_ALGORITHM_SHA256, InstallRefMode, ObjectManifest,
        PROFILE_MANIFEST_FILE, ProfileManifest, ProvenanceRecord, SCHEMA_VERSION, write_manifest,
    };

    fn write_object(store: &StoreRoot, name: &str) -> Result<String> {
        let object_root = store
            .objects_dir()
            .join(HASH_ALGORITHM_SHA256)
            .join("aa")
            .join(name);
        file::create_dir_all(&object_root)?;
        let manifest = ObjectManifest {
            schema_version: SCHEMA_VERSION,
            object_id: format!("sha256:{name}"),
            tree_hash: name.to_string(),
            hash_algorithm: HASH_ALGORITHM_SHA256.to_string(),
            name: name.to_string(),
            platform: "test".to_string(),
            created_by: "test".to_string(),
            created_at: "2026-07-03T00:00:00Z".to_string(),
            bytes: 0,
            files: 0,
            executable_paths: vec![],
            bin_paths: vec![],
            references: vec![],
            realisations: vec![],
        };
        write_manifest(object_root.join(OBJECT_MANIFEST_FILE), &manifest)?;
        Ok(manifest.object_id)
    }

    fn write_realisation(store: &StoreRoot, id: &str, object_id: &str) -> Result<()> {
        write_manifest(
            store
                .realisations_dir()
                .join(HASH_ALGORITHM_SHA256)
                .join("aa")
                .join(format!("{id}.toml")),
            &RealisationManifest {
                schema_version: SCHEMA_VERSION,
                realisation_id: format!("sha256:{id}"),
                derivation_id: format!("sha256:derivation-{id}"),
                object_id: object_id.to_string(),
                tool: "demo".to_string(),
                backend: "test:demo".to_string(),
                version: id.to_string(),
                platform: "test".to_string(),
                options_hash: "sha256:options".to_string(),
                source_hash: "sha256:source".to_string(),
                lock_policy: "legacy".to_string(),
                provenance: Vec::<ProvenanceRecord>::new(),
                closure: vec![],
                compatibility: CompatibilityRef {
                    path: PathBuf::from("/tmp/demo"),
                    mode: InstallRefMode::LegacyRealDirectory,
                },
            },
        )
    }

    #[test]
    fn dry_run_marks_profile_reachable_objects_and_reports_candidates() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let kept_object = write_object(&store, "kept-object")?;
        let candidate_object = write_object(&store, "candidate-object")?;
        write_realisation(&store, "kept-realisation", &kept_object)?;
        write_manifest(
            store
                .profile_refs_dir()
                .join("projects/hash/default/generations/1")
                .join(PROFILE_MANIFEST_FILE),
            &ProfileManifest {
                schema_version: SCHEMA_VERSION,
                profile_id: "projects/hash/default".to_string(),
                generation: 1,
                project_root: Some(tmp.path().join("project")),
                source_config_hash: "sha256:config".to_string(),
                nise_lock_hash: "sha256:lock".to_string(),
                created_at: "2026-07-03T00:00:00Z".to_string(),
                realisations: vec!["sha256:kept-realisation".to_string()],
                env_hash: "sha256:env".to_string(),
                path_entries: vec![],
            },
        )?;

        let report = gc_dry_run(&store)?;

        assert_eq!(report.objects.total, 2);
        assert!(report.marked_objects.contains_key(&kept_object));
        assert_eq!(report.candidates.len(), 1);
        assert_eq!(report.candidates[0].object_id, candidate_object);
        Ok(())
    }
}
