use clap::Subcommand;
use eyre::Result;

mod doctor;
mod gc;
mod repair;

/// Manage the nise store
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment, after_long_help = AFTER_LONG_HELP)]
pub struct Store {
    #[clap(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Doctor(doctor::StoreDoctor),
    Gc(gc::StoreGc),
    Repair(repair::StoreRepair),
}

impl Commands {
    pub fn run(self) -> Result<()> {
        match self {
            Self::Doctor(cmd) => cmd.run(),
            Self::Gc(cmd) => cmd.run(),
            Self::Repair(cmd) => cmd.run(),
        }
    }
}

impl Store {
    pub fn run(self) -> Result<()> {
        self.command.unwrap_or_default().run()
    }
}

impl Default for Commands {
    fn default() -> Self {
        Self::Doctor(doctor::StoreDoctor::default())
    }
}

static AFTER_LONG_HELP: &str = color_print::cstr!(
    r#"<bold><underline>Examples:</underline></bold>

    $ <bold>nise store doctor</bold>
    Store: ~/.local/share/mise/nise/store
    Status: not initialized

    $ <bold>NISE_STORE_DIR=/nise/store mise store doctor --json</bold>

    $ <bold>nise store gc --dry-run</bold>

    $ <bold>nise store repair --remove-broken-refs</bold>
"#
);
