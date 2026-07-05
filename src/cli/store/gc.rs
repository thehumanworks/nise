use eyre::{Result, bail};

use crate::exit::exit;
use crate::file::display_path;
use crate::store::{StoreRoot, gc_dry_run};

/// Dry-run nise store garbage collection
#[derive(Debug, Default, clap::Args)]
#[clap(verbatim_doc_comment)]
pub struct StoreGc {
    /// Delete unreachable objects
    #[clap(long, conflicts_with = "dry_run")]
    delete: bool,

    /// Show what would be collected
    #[clap(long, conflicts_with = "delete")]
    dry_run: bool,

    /// Output JSON
    #[clap(long, short = 'J')]
    json: bool,
}

impl StoreGc {
    pub fn run(self) -> Result<()> {
        if self.delete {
            bail!(
                "store gc --delete is not implemented yet; trash, grace-period, and final-reachability checks are required first"
            );
        }
        let store = StoreRoot::default();
        let report = gc_dry_run(&store)?;
        if self.json {
            miseprintln!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print_report(&report)?;
        }
        if report.has_issues() {
            exit(1);
        }
        Ok(())
    }
}

fn print_report(report: &crate::store::GcReport) -> Result<()> {
    miseprintln!("Store: {}", display_path(&report.store_root));
    miseprintln!("Mode: dry-run");
    miseprintln!("Objects: {}", report.objects.total);
    miseprintln!("Marked: {}", report.objects.marked);
    miseprintln!("Candidates: {}", report.objects.candidates);
    miseprintln!("Profiles: {}", report.roots.profiles);
    miseprintln!("Install refs: {}", report.roots.install_refs);
    miseprintln!("Transactions: {}", report.roots.transactions);
    miseprintln!("Pins: {}", report.roots.pins);
    miseprintln!("Process leases: {}", report.roots.process_leases);
    for candidate in &report.candidates {
        miseprintln!();
        miseprintln!("Candidate: {}", candidate.object_id);
        miseprintln!("  {}", display_path(&candidate.path));
    }
    for issue in &report.issues {
        miseprintln!();
        miseprintln!("Issue: {}", display_path(&issue.path));
        miseprintln!("  {}", issue.error);
    }
    Ok(())
}
