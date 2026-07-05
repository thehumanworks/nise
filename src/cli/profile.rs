use std::path::PathBuf;

use clap::Subcommand;
use eyre::Result;

use crate::config::Config;
use crate::file::display_path;
use crate::store::{
    ProfileGeneration, StoreRoot, current_project_profile_generation, list_profile_generations,
    previous_project_profile_generation, rollback_project_profile,
};

/// Inspect and move nise profile generations
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment, after_long_help = AFTER_LONG_HELP)]
pub struct Profile {
    #[clap(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    List(ProfileList),
    Rollback(ProfileRollback),
    Show(ProfileShow),
}

impl Default for Commands {
    fn default() -> Self {
        Self::List(ProfileList::default())
    }
}

impl Profile {
    pub async fn run(self) -> Result<()> {
        self.command.unwrap_or_default().run().await
    }
}

impl Commands {
    async fn run(self) -> Result<()> {
        match self {
            Self::List(cmd) => cmd.run(),
            Self::Rollback(cmd) => cmd.run().await,
            Self::Show(cmd) => cmd.run().await,
        }
    }
}

/// List nise profile generations
#[derive(Debug, Default, clap::Args)]
struct ProfileList {
    /// Output JSON
    #[clap(long, short = 'J')]
    json: bool,
}

impl ProfileList {
    fn run(self) -> Result<()> {
        let store = StoreRoot::default();
        let generations = list_profile_generations(&store)?;
        if self.json {
            miseprintln!("{}", serde_json::to_string_pretty(&generations)?);
        } else if generations.is_empty() {
            miseprintln!("No nise profiles found");
        } else {
            for generation in generations {
                print_generation_summary(&generation)?;
            }
        }
        Ok(())
    }
}

/// Show the current project profile generation
#[derive(Debug, clap::Args)]
struct ProfileShow {
    /// Project profile name
    #[clap(default_value = "default")]
    profile: String,

    /// Output JSON
    #[clap(long, short = 'J')]
    json: bool,
}

impl ProfileShow {
    async fn run(self) -> Result<()> {
        let store = StoreRoot::default();
        let project_root = current_project_root().await?;
        let generation = current_project_profile_generation(&store, &project_root, &self.profile)?
            .ok_or_else(|| {
                eyre::eyre!(
                    "no current nise profile generation for {}",
                    display_path(&project_root)
                )
            })?;
        if self.json {
            miseprintln!("{}", serde_json::to_string_pretty(&generation)?);
        } else {
            print_generation_detail(&generation)?;
        }
        Ok(())
    }
}

/// Roll back the current project profile
#[derive(Debug, clap::Args)]
struct ProfileRollback {
    /// Project profile name
    #[clap(default_value = "default")]
    profile: String,

    /// Generation to switch to; defaults to the previous generation
    generation: Option<u64>,

    /// Output JSON
    #[clap(long, short = 'J')]
    json: bool,
}

impl ProfileRollback {
    async fn run(self) -> Result<()> {
        let store = StoreRoot::default();
        let project_root = current_project_root().await?;
        let generation = match self.generation {
            Some(generation) => generation,
            None => previous_project_profile_generation(&store, &project_root, &self.profile)?
                .ok_or_else(|| {
                    eyre::eyre!("no previous generation for profile {}", self.profile)
                })?,
        };
        let generation =
            rollback_project_profile(&store, &project_root, &self.profile, generation)?;
        if self.json {
            miseprintln!("{}", serde_json::to_string_pretty(&generation)?);
        } else {
            miseprintln!(
                "Switched {} to generation {}",
                generation.profile_id,
                generation.generation
            );
            print_generation_detail(&generation)?;
        }
        Ok(())
    }
}

async fn current_project_root() -> Result<PathBuf> {
    let config = Config::get().await?;
    Ok(config
        .project_root
        .clone()
        .unwrap_or(std::env::current_dir()?))
}

fn print_generation_summary(generation: &ProfileGeneration) -> Result<()> {
    let marker = if generation.current { "*" } else { " " };
    miseprintln!(
        "{} {} generation {} ({})",
        marker,
        generation.profile_id,
        generation.generation,
        display_path(&generation.generation_path)
    );
    Ok(())
}

fn print_generation_detail(generation: &ProfileGeneration) -> Result<()> {
    let current = if generation.current { "yes" } else { "no" };
    miseprintln!("Profile: {}", generation.profile_id);
    miseprintln!("Generation: {}", generation.generation);
    miseprintln!("Current: {current}");
    if let Some(project_root) = &generation.manifest.project_root {
        miseprintln!("Project root: {}", display_path(project_root));
    }
    miseprintln!("Created at: {}", generation.manifest.created_at);
    miseprintln!("Env hash: {}", generation.manifest.env_hash);
    miseprintln!("Lock hash: {}", generation.manifest.nise_lock_hash);
    if !generation.manifest.path_entries.is_empty() {
        miseprintln!("PATH entries:");
        for path in &generation.manifest.path_entries {
            miseprintln!("  {}", display_path(path));
        }
    }
    if !generation.manifest.realisations.is_empty() {
        miseprintln!("Realisations:");
        for realisation in &generation.manifest.realisations {
            miseprintln!("  {realisation}");
        }
    }
    Ok(())
}

static AFTER_LONG_HELP: &str = color_print::cstr!(
    r#"<bold><underline>Examples:</underline></bold>

    $ <bold>nise profile list</bold>
    $ <bold>nise profile show</bold>
    $ <bold>nise profile rollback default 1</bold>
"#
);
