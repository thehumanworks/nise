use std::path::PathBuf;

use eyre::Result;
use serde::Serialize;

use crate::exit::exit;
use crate::file::display_path;
use crate::store::StoreRoot;
use crate::toolset::installed_versions::{self, InstalledVersionEntry};

/// Repair explicit nise store problems
#[derive(Debug, Default, clap::Args)]
#[clap(verbatim_doc_comment)]
pub struct StoreRepair {
    /// Output JSON
    #[clap(long, short = 'J')]
    json: bool,

    /// Do not actually change anything
    #[clap(long, short = 'n')]
    dry_run: bool,

    /// Remove broken compatibility install-ref manifests
    #[clap(long)]
    remove_broken_refs: bool,
}

impl StoreRepair {
    pub fn run(self) -> Result<()> {
        let store = StoreRoot::default();
        let report = repair_store(&store, self.remove_broken_refs, self.dry_run)?;

        if self.json {
            miseprintln!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print_report(&report)?;
        }

        if report.has_unrepaired_issues() {
            exit(1);
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct StoreRepairReport {
    store_root: PathBuf,
    dry_run: bool,
    remove_broken_refs: bool,
    broken_refs: Vec<BrokenInstallRef>,
    actions: Vec<RepairAction>,
}

impl StoreRepairReport {
    fn has_unrepaired_issues(&self) -> bool {
        !self.broken_refs.is_empty() && (!self.remove_broken_refs || self.dry_run)
    }
}

#[derive(Debug, Serialize)]
struct BrokenInstallRef {
    path: PathBuf,
    reason: String,
}

#[derive(Debug, Serialize)]
struct RepairAction {
    path: PathBuf,
    action: &'static str,
}

fn repair_store(
    store: &StoreRoot,
    remove_broken_refs: bool,
    dry_run: bool,
) -> Result<StoreRepairReport> {
    let mut broken_refs = vec![];
    let mut actions = vec![];

    for entry in installed_versions::broken_store_refs(store) {
        let InstalledVersionEntry::BrokenRef { path, reason } = entry else {
            continue;
        };
        if remove_broken_refs {
            installed_versions::remove_broken_ref(&path, dry_run)?;
            actions.push(RepairAction {
                path: path.clone(),
                action: if dry_run {
                    "would-remove-broken-ref"
                } else {
                    "removed-broken-ref"
                },
            });
        }
        broken_refs.push(BrokenInstallRef { path, reason });
    }

    Ok(StoreRepairReport {
        store_root: store.path().to_path_buf(),
        dry_run,
        remove_broken_refs,
        broken_refs,
        actions,
    })
}

fn print_report(report: &StoreRepairReport) -> Result<()> {
    miseprintln!("Store: {}", display_path(&report.store_root));
    if report.broken_refs.is_empty() {
        miseprintln!("Status: ok");
    } else if report.has_unrepaired_issues() {
        miseprintln!("Status: issues remain");
    } else {
        miseprintln!("Status: repaired");
    }

    if !report.broken_refs.is_empty() {
        miseprintln!("Broken install refs: {}", report.broken_refs.len());
    }
    for broken in &report.broken_refs {
        miseprintln!();
        miseprintln!("Broken ref: {}", display_path(&broken.path));
        miseprintln!("  {}", broken.reason);
    }
    for action in &report.actions {
        miseprintln!();
        miseprintln!("Action: {}", action.action);
        miseprintln!("  {}", display_path(&action.path));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file;
    use crate::store::{InstallRefManifest, InstallRefMode, SCHEMA_VERSION, write_manifest};

    #[test]
    fn repair_store_reports_broken_refs_without_removing_by_default() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let broken_path = store.install_refs_dir().join("demo").join("1.0.0.toml");
        write_manifest(
            &broken_path,
            &InstallRefManifest {
                schema_version: SCHEMA_VERSION,
                tool: "demo".to_string(),
                version: "1.0.0".to_string(),
                backend: "test:demo".to_string(),
                compatibility_path: tmp.path().join("missing"),
                realisation_id: "sha256:demo-realisation".to_string(),
                object_id: "sha256:demo-object".to_string(),
                mode: InstallRefMode::StoreSymlink,
            },
        )?;

        let report = repair_store(&store, false, false)?;

        assert!(report.has_unrepaired_issues());
        assert_eq!(report.broken_refs.len(), 1);
        assert!(broken_path.exists());
        Ok(())
    }

    #[test]
    fn repair_store_removes_broken_ref_when_requested() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let broken_path = store.install_refs_dir().join("demo").join("bad.toml");
        file::create_dir_all(broken_path.parent().unwrap())?;
        file::write(&broken_path, "schema_version = ")?;

        let report = repair_store(&store, true, false)?;

        assert!(!report.has_unrepaired_issues());
        assert_eq!(report.actions.len(), 1);
        assert!(!broken_path.exists());
        Ok(())
    }

    #[test]
    fn repair_store_dry_run_leaves_broken_ref() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let broken_path = store.install_refs_dir().join("demo").join("bad.toml");
        file::create_dir_all(broken_path.parent().unwrap())?;
        file::write(&broken_path, "schema_version = ")?;

        let report = repair_store(&store, true, true)?;

        assert!(report.has_unrepaired_issues());
        assert_eq!(report.actions[0].action, "would-remove-broken-ref");
        assert!(broken_path.exists());
        Ok(())
    }
}
