use std::path::{Path, PathBuf};

use eyre::Result;
use serde::Serialize;
use walkdir::WalkDir;

use crate::exit::exit;
use crate::file::{self, display_path};
use crate::store::{
    OBJECT_MANIFEST_FILE, RootScan, StoreRoot, read_object_manifest, read_roots,
    validate_object_manifest_for_tree,
};

/// Check the nise store for manifest and root problems
#[derive(Debug, Default, clap::Args)]
#[clap(verbatim_doc_comment)]
pub struct StoreDoctor {
    /// Recompute object tree hashes
    #[clap(long)]
    deep: bool,

    /// Output JSON
    #[clap(long, short = 'J')]
    json: bool,
}

impl StoreDoctor {
    pub fn run(self) -> Result<()> {
        let store = StoreRoot::default();
        let root_scan = read_roots(store.path());
        let mut issues = root_scan
            .errors
            .iter()
            .map(|err| StoreDoctorIssue {
                path: err.path.clone(),
                message: err.error.clone(),
            })
            .collect::<Vec<_>>();
        let object_report = check_objects(&store, self.deep, &mut issues)?;
        let report = StoreDoctorReport {
            store_root: store.path().to_path_buf(),
            exists: store.path().exists(),
            roots: RootCounts::from_scan(&root_scan),
            objects: object_report,
            issues,
        };

        if self.json {
            miseprintln!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print_report(&report, self.deep)?;
        }

        if !report.issues.is_empty() {
            exit(1);
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct StoreDoctorReport {
    store_root: PathBuf,
    exists: bool,
    roots: RootCounts,
    objects: ObjectReport,
    issues: Vec<StoreDoctorIssue>,
}

#[derive(Debug, Serialize)]
struct RootCounts {
    install_refs: usize,
    profiles: usize,
    transactions: usize,
    pins: usize,
    process_leases: usize,
}

impl RootCounts {
    fn from_scan(scan: &RootScan) -> Self {
        Self {
            install_refs: scan.install_refs.len(),
            profiles: scan.profiles.len(),
            transactions: scan.transactions.len(),
            pins: scan.pins.len(),
            process_leases: scan.process_leases.len(),
        }
    }
}

#[derive(Debug, Default, Serialize)]
struct ObjectReport {
    manifests: usize,
    verified: usize,
}

#[derive(Debug, Serialize)]
struct StoreDoctorIssue {
    path: PathBuf,
    message: String,
}

fn check_objects(
    store: &StoreRoot,
    deep: bool,
    issues: &mut Vec<StoreDoctorIssue>,
) -> Result<ObjectReport> {
    let objects_dir = store.objects_dir();
    let mut report = ObjectReport::default();
    if !objects_dir.exists() {
        return Ok(report);
    }
    for manifest_path in object_manifest_paths(&objects_dir, issues) {
        report.manifests += 1;
        let object_root = manifest_path.parent().unwrap_or(&objects_dir);
        let result = if deep {
            validate_object_manifest_for_tree(object_root).map(|_| ())
        } else {
            read_object_manifest(object_root).and_then(|manifest| manifest.validate_hash_identity())
        };
        match result {
            Ok(()) => {
                if deep {
                    report.verified += 1;
                }
            }
            Err(err) => issues.push(StoreDoctorIssue {
                path: manifest_path,
                message: err.to_string(),
            }),
        }
    }
    Ok(report)
}

fn object_manifest_paths(root: &Path, issues: &mut Vec<StoreDoctorIssue>) -> Vec<PathBuf> {
    let mut paths = vec![];
    for entry in WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
    {
        match entry {
            Ok(entry) if entry.file_type().is_file() && is_object_manifest(root, entry.path()) => {
                paths.push(entry.path().to_path_buf());
            }
            Ok(_) => {}
            Err(err) => issues.push(StoreDoctorIssue {
                path: err
                    .path()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| root.to_path_buf()),
                message: err.to_string(),
            }),
        }
    }
    paths
}

fn is_object_manifest(objects_root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(objects_root) else {
        return false;
    };
    if relative
        .file_name()
        .is_none_or(|name| name != OBJECT_MANIFEST_FILE)
    {
        return false;
    }
    relative.components().count() == 4
}

fn print_report(report: &StoreDoctorReport, deep: bool) -> Result<()> {
    miseprintln!("Store: {}", display_path(&report.store_root));
    if !report.exists {
        miseprintln!("Status: not initialized");
        return Ok(());
    }

    miseprintln!("Status: {}", status(report));
    miseprintln!("Object manifests: {}", report.objects.manifests);
    if deep {
        miseprintln!("Objects verified: {}", report.objects.verified);
    }
    miseprintln!("Install refs: {}", report.roots.install_refs);
    miseprintln!("Profiles: {}", report.roots.profiles);
    miseprintln!("Transactions: {}", report.roots.transactions);
    miseprintln!("Pins: {}", report.roots.pins);
    miseprintln!("Process leases: {}", report.roots.process_leases);

    for issue in &report.issues {
        miseprintln!();
        miseprintln!("Issue: {}", display_path(&issue.path));
        miseprintln!("  {}", file::replace_paths_in_string(&issue.message));
    }
    Ok(())
}

fn status(report: &StoreDoctorReport) -> &'static str {
    if report.issues.is_empty() {
        "ok"
    } else {
        "issues found"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{
        HASH_ALGORITHM_SHA256, OBJECT_MANIFEST_FILE, ObjectManifest, canonical_tree_hash,
        write_manifest,
    };

    #[test]
    fn ignores_payload_files_named_like_object_manifests() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path());
        let object_root = store
            .objects_dir()
            .join(HASH_ALGORITHM_SHA256)
            .join("ab")
            .join("abcdef-tool");
        let payload_manifest = object_root.join("share").join(OBJECT_MANIFEST_FILE);
        file::create_dir_all(payload_manifest.parent().unwrap())?;
        file::write(&payload_manifest, "payload-owned")?;
        file::create_dir_all(object_root.join("bin"))?;
        file::write(object_root.join("bin/tool"), "tool")?;
        let tree = canonical_tree_hash(&object_root)?;
        let mut manifest = ObjectManifest::new("tool", "test-platform", tree.hash);
        manifest.bytes = tree.bytes;
        manifest.files = tree.files;
        write_manifest(object_root.join(OBJECT_MANIFEST_FILE), &manifest)?;

        let mut issues = vec![];
        let report = check_objects(&store, true, &mut issues)?;

        assert_eq!(report.manifests, 1);
        assert_eq!(report.verified, 1);
        assert!(issues.is_empty());
        Ok(())
    }
}
