use crate::Result;
use crate::config::Config;
use crate::file;
use crate::task::Task;
use clap::ValueHint;
use std::path::PathBuf;
use xx::file::display_path;

/// Generates shims to run nise tasks
///
/// By default, this will build shims like ./bin/<task>. These can be paired with `nise generate bootstrap`
/// so contributors to a project can execute nise tasks without installing nise into their system.
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment, after_long_help = AFTER_LONG_HELP)]
pub struct TaskStubs {
    /// Directory to create task stubs inside of
    #[clap(long, short, verbatim_doc_comment, default_value="bin", value_hint=ValueHint::DirPath)]
    dir: PathBuf,

    /// Path to a nise bin to use when running the task stub.
    ///
    /// Use `--mise-bin=./bin/nise` to use a nise bin generated from `nise generate bootstrap`
    #[clap(long, short, verbatim_doc_comment, default_value = "nise")]
    mise_bin: PathBuf,
}

impl TaskStubs {
    pub async fn run(self) -> eyre::Result<()> {
        let config = Config::get().await?;
        for task in config.tasks().await?.values() {
            let bin = self.dir.join(task.name_to_path());
            let output = self.generate(task)?;
            if let Some(parent) = bin.parent() {
                file::create_dir_all(parent)?;
            }
            file::write(&bin, &output)?;
            file::make_executable(&bin)?;
            miseprintln!("Wrote to {}", display_path(&bin));
        }
        Ok(())
    }

    fn generate(&self, task: &Task) -> Result<String> {
        let mise_bin = self.mise_bin.to_string_lossy();
        let mise_bin = shell_words::quote(&mise_bin);
        let display_name = &task.display_name;
        let script = format!(
            r#"
#!/bin/sh
exec {mise_bin} run {display_name} "$@"
"#
        );
        Ok(script.trim().to_string())
    }
}

static AFTER_LONG_HELP: &str = color_print::cstr!(
    r#"<bold><underline>Examples:</underline></bold>

    $ <bold>nise tasks add test -- echo 'running tests'</bold>
    $ <bold>nise generate task-stubs</bold>
    $ <bold>./bin/test</bold>
    running tests
"#
);
