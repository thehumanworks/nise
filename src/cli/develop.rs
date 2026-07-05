use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use clap::{ValueEnum, ValueHint};
use eyre::{Result, bail};
use itertools::Itertools;

use crate::config::{Config, Settings};
use crate::env;
use crate::exit::exit;
use crate::hash::{file_hash_sha256, hash_sha256_to_str};
use crate::install_context::NiseStoreInstallMode;
use crate::nise_lock::{self, check_nise_lock};
use crate::store::{
    ProfileGeneration, ProjectProfileInput, StoreRoot, acquire_process_lease,
    current_project_profile_generation, write_project_profile_generation,
};
use crate::toolset::{InstallOptions, ToolsetBuilder};

/// Enter a nise development environment backed by a profile generation
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment, after_long_help = AFTER_LONG_HELP)]
pub struct Develop {
    /// Directory to develop in
    #[clap(default_value = ".", verbatim_doc_comment, value_hint = ValueHint::DirPath)]
    dir: PathBuf,

    /// Shell to start when no command is provided
    ///
    /// Defaults to $SHELL
    #[clap(long, short = 's', verbatim_doc_comment)]
    shell: Option<String>,

    /// Keep the inherited process environment and PATH
    #[clap(long, conflicts_with = "pure")]
    impure: bool,

    /// Isolation policy for this development environment
    #[clap(long, value_enum, default_value_t = NiseIsolationMode::Off, value_name = "MODE")]
    isolate: NiseIsolationMode,

    /// Do not install missing configured tools before entering the environment
    #[clap(long)]
    no_realize: bool,

    /// Offline policy for this development environment
    #[clap(long, value_enum, value_name = "MODE")]
    offline: Option<NiseOfflineMode>,

    /// Project profile name
    #[clap(long, default_value = "default")]
    profile: String,

    /// Drop inherited environment variables and user PATH entries that nise did not compute
    ///
    /// This is the default; the flag is retained for explicitness. Pure shells still keep
    /// a small OS baseline PATH for commands like ls and sh.
    #[clap(long, conflicts_with = "impure")]
    pure: bool,

    /// Install missing configured tools before entering the environment
    #[clap(long, conflicts_with = "no_realize")]
    realize: bool,

    /// Command to run instead of an interactive shell
    #[clap(last = true)]
    command: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, strum::Display)]
#[strum(serialize_all = "kebab-case")]
enum NiseOfflineMode {
    Artifact,
    Derivation,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, strum::Display)]
#[strum(serialize_all = "kebab-case")]
enum NiseIsolationMode {
    Strict,
    BestEffort,
    Off,
}

impl Develop {
    pub async fn run(self) -> Result<()> {
        env::set_current_dir(&self.dir)?;
        if self.offline.is_some() {
            crate::config::settings::Settings::override_with(|settings| {
                settings.offline = Some(true);
            });
        }
        let loaded_config = Config::get().await?;
        let local_config_files = loaded_config
            .config_files
            .iter()
            .filter(|(path, _)| !crate::config::is_global_config(path))
            .map(|(path, config_file)| (path.clone(), config_file.clone()))
            .collect();
        let mut config = loaded_config.with_config_files(local_config_files);
        let settings = Settings::get();
        self.enforce_requested_policy(&config, &settings)?;
        let mut toolset = ToolsetBuilder::new()
            .with_default_to_latest(true)
            .build(&config)
            .await?;

        let project_root = config
            .project_root
            .clone()
            .unwrap_or(std::env::current_dir()?);
        if !self.no_realize {
            let opts = InstallOptions {
                force: false,
                jobs: None,
                raw: false,
                missing_args_only: false,
                nise_store_install_mode: self.store_install_mode(),
                skip_auto_install: !Settings::get().exec_auto_install
                    || !Settings::get().auto_install,
                ..Default::default()
            };
            let (_, missing) = toolset.install_missing_versions(&mut config, &opts).await?;
            toolset.notify_missing_versions(missing);
        } else {
            toolset.notify_if_versions_missing(&config).await;
        }

        let pure = self.pure_env_enabled();
        let (mut child_env, env_results) = toolset.final_env(&config).await?;
        let store = StoreRoot::default();
        let profile = if self.no_realize {
            current_project_profile_generation(&store, &project_root, &self.profile)?.ok_or_else(
                || {
                    eyre::eyre!(
                        "no current nise profile generation for {}; run `nise develop` without --no-realize first",
                        crate::file::display_path(&project_root)
                    )
                },
            )?
        } else {
            let path_entries = develop_profile_path_entries(&toolset, &config, env_results).await?;
            let realisations = toolset
                .list_current_installed_versions(&config)
                .into_iter()
                .map(|(backend, tv)| format!("legacy:{}@{}", backend.id(), tv.tv_pathname()))
                .sorted()
                .collect::<Vec<_>>();
            let source_config_hash = source_config_hash(&config);
            let env_hash = env_map_hash(&child_env);
            let manifest = write_project_profile_generation(
                &store,
                ProjectProfileInput {
                    profile: &self.profile,
                    project_root: &project_root,
                    source_config_hash,
                    nise_lock_hash: nise_lock_hash(&config)?,
                    realisations,
                    path_entries,
                    env_hash,
                },
            )?;
            current_project_profile_generation(&store, &project_root, &self.profile)?
                .filter(|generation| generation.generation == manifest.generation)
                .ok_or_else(|| eyre::eyre!("failed to read generated profile"))?
        };
        let lease = acquire_process_lease(&store, &profile.profile_id, profile.generation)?;
        child_env.insert("__NISE_PROFILE".to_string(), profile.profile_id.clone());
        child_env.insert(
            "__NISE_PROFILE_GENERATION".to_string(),
            profile.generation.to_string(),
        );
        child_env.insert(
            "__NISE_PROCESS_LEASE".to_string(),
            lease.path().to_string_lossy().to_string(),
        );
        child_env.insert(
            "NISE_STORE_DIR".to_string(),
            store.path().to_string_lossy().to_string(),
        );
        if let Some(offline) = self.offline {
            child_env.insert("__NISE_OFFLINE_MODE".to_string(), offline.to_string());
        }
        child_env.insert("__NISE_ISOLATE".to_string(), self.isolate.to_string());

        apply_profile_path(&mut child_env, &profile, pure)?;

        if pure {
            child_env = pure_env(child_env);
        }

        let (program, args) = self.program_and_args()?;
        let status = run_child(program, args, child_env, pure)?;
        drop(lease);
        match status.code() {
            Some(0) => Ok(()),
            Some(code) => exit(code),
            None => bail!("develop child process terminated by signal"),
        }
    }

    fn enforce_requested_policy(&self, config: &Config, settings: &Settings) -> Result<()> {
        if self.isolate == NiseIsolationMode::BestEffort {
            warn!(
                "nise develop best-effort isolation is not implemented yet; running without an additional sandbox"
            );
        }
        if self.isolate == NiseIsolationMode::Strict {
            bail!(
                "strict nise develop isolation is not implemented yet; use --isolate=best-effort or --isolate=off"
            );
        }
        let requires_strict_lock = settings.locked
            || matches!(
                self.offline,
                Some(NiseOfflineMode::Derivation | NiseOfflineMode::Full)
            );
        if requires_strict_lock {
            let lock_path = nise_lock::nise_lock_path_for_config(config);
            check_nise_lock(&lock_path, true)?;
        } else if self.offline == Some(NiseOfflineMode::Artifact) {
            let lock_path = nise_lock::nise_lock_path_for_config(config);
            if lock_path.exists() {
                check_nise_lock(&lock_path, false)?;
            }
        }
        Ok(())
    }

    fn store_install_mode(&self) -> NiseStoreInstallMode {
        if self.offline == Some(NiseOfflineMode::Full) {
            NiseStoreInstallMode::Immutable
        } else {
            NiseStoreInstallMode::Legacy
        }
    }

    fn pure_env_enabled(&self) -> bool {
        !self.impure
    }

    fn program_and_args(&self) -> Result<(String, Vec<String>)> {
        if let Some(command) = &self.command {
            let Some((program, args)) = command.split_first() else {
                bail!("develop command is empty");
            };
            return Ok((program.clone(), args.to_vec()));
        }
        let shell = self.shell.clone().unwrap_or((*env::SHELL).clone());
        let mut command = shell_words::split(&shell).map_err(eyre::Report::msg)?;
        let Some(program) = command.first().cloned() else {
            bail!("shell command is empty");
        };
        command.remove(0);
        Ok((program, command))
    }
}

fn run_child(
    program: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    pure: bool,
) -> Result<std::process::ExitStatus> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if pure {
        command.env_clear();
    }
    command.envs(env);
    Ok(command.status()?)
}

fn pure_env(env: BTreeMap<String, String>) -> BTreeMap<String, String> {
    let allowlist = [
        "HOME", "USER", "USERNAME", "SHELL", "TERM", "LANG", "LC_ALL", "TMPDIR", "TEMP", "TMP",
    ];
    env.into_iter()
        .filter(|(key, value)| {
            key == &*env::PATH_KEY
                || allowlist.contains(&key.as_str())
                || key.starts_with("MISE_")
                || key.starts_with("__MISE")
                || key.starts_with("NISE_")
                || key.starts_with("__NISE")
                || env::PRISTINE_ENV.get(key) != Some(value)
        })
        .collect()
}

fn apply_profile_path(
    env: &mut BTreeMap<String, String>,
    profile: &ProfileGeneration,
    pure: bool,
) -> Result<()> {
    let mut paths = profile.manifest.path_entries.clone();
    if let Some(bin_dir) = crate::env::MISE_BIN.parent() {
        paths.push(bin_dir.to_path_buf());
    }
    if pure {
        paths.extend(pure_shell_base_paths());
    } else {
        paths.extend(crate::env::PATH.iter().cloned());
    }
    let path = std::env::join_paths(paths)?;
    env.insert(
        (*crate::env::PATH_KEY).clone(),
        path.to_string_lossy().to_string(),
    );
    Ok(())
}

fn pure_shell_base_paths() -> Vec<PathBuf> {
    #[cfg(unix)]
    let candidates = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .into_iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();

    #[cfg(windows)]
    let candidates = {
        let windows = std::env::var_os("SystemRoot")
            .or_else(|| std::env::var_os("WINDIR"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        vec![
            windows.join("System32"),
            windows.clone(),
            windows.join("System32").join("Wbem"),
            windows
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0"),
        ]
    };

    candidates
        .into_iter()
        .filter(|path| path.is_dir())
        .collect()
}

async fn develop_profile_path_entries(
    toolset: &crate::toolset::Toolset,
    config: &std::sync::Arc<Config>,
    env_results: crate::config::env_directive::EnvResults,
) -> Result<Vec<PathBuf>> {
    let (user_paths, tool_paths) = toolset.list_final_paths_split(config, env_results).await?;
    Ok(user_paths.into_iter().chain(tool_paths).collect())
}

fn source_config_hash(config: &Config) -> String {
    let input = config
        .config_files
        .keys()
        .map(|path| path.to_string_lossy())
        .sorted()
        .join("\n");
    format!("sha256:{}", hash_sha256_to_str(&input))
}

fn nise_lock_hash(config: &Config) -> Result<String> {
    let path = nise_lock::nise_lock_path_for_config(config);
    if !path.exists() {
        return Ok("sha256:legacy-unlocked".to_string());
    }
    Ok(format!("sha256:{}", file_hash_sha256(&path, None)?))
}

fn env_map_hash(env: &BTreeMap<String, String>) -> String {
    let input = env
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .join("\n");
    format!("sha256:{}", hash_sha256_to_str(&input))
}

static AFTER_LONG_HELP: &str = color_print::cstr!(
    r#"<bold><underline>Examples:</underline></bold>

    $ <bold>nise develop</bold>
    $ <bold>nise develop -- env</bold>
    $ <bold>nise develop --impure . -- npm test</bold>

This is the first nise profile-backed develop mode. It writes a project profile generation
and a process lease, then runs with the existing mise tool environment. Strict lock and
offline requests validate nise.lock and fail closed while derivations remain legacy-unverified.
Strict isolation fails closed until develop sandboxing is implemented.
"#
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_env_keeps_computed_values_and_drops_unchanged_inherited_values() {
        let mut env = BTreeMap::new();
        env.insert((*env::PATH_KEY).clone(), "/tool/bin:/usr/bin".to_string());
        env.insert("HOME".to_string(), "/home/user".to_string());
        env.insert(
            "__NISE_PROFILE".to_string(),
            "projects/demo/default".to_string(),
        );
        env.insert("PROJECT_ONLY".to_string(), "1".to_string());
        if let Some((key, value)) = env::PRISTINE_ENV.iter().next() {
            env.insert(key.clone(), value.clone());
        }

        let pure = pure_env(env);

        assert_eq!(
            pure.get(&*env::PATH_KEY),
            Some(&"/tool/bin:/usr/bin".to_string())
        );
        assert_eq!(
            pure.get("__NISE_PROFILE"),
            Some(&"projects/demo/default".to_string())
        );
        assert_eq!(pure.get("PROJECT_ONLY"), Some(&"1".to_string()));
    }

    #[test]
    fn apply_profile_path_omits_inherited_path_when_pure() {
        let mut env = BTreeMap::new();
        let profile = ProfileGeneration {
            profile_id: "projects/demo/default".to_string(),
            generation: 1,
            current: true,
            profile_root: PathBuf::from("/tmp/profile"),
            generation_path: PathBuf::from("/tmp/profile/generations/1"),
            manifest_path: PathBuf::from("/tmp/profile/generations/1/.nise-profile.toml"),
            manifest: crate::store::ProfileManifest {
                schema_version: crate::store::SCHEMA_VERSION,
                profile_id: "projects/demo/default".to_string(),
                generation: 1,
                project_root: None,
                source_config_hash: "sha256:source".to_string(),
                nise_lock_hash: "sha256:lock".to_string(),
                created_at: "now".to_string(),
                realisations: vec![],
                env_hash: "sha256:env".to_string(),
                path_entries: vec![PathBuf::from("/project/bin")],
            },
        };

        apply_profile_path(&mut env, &profile, true).unwrap();
        let path = env.get(&*env::PATH_KEY).unwrap();
        let entries = std::env::split_paths(path).collect::<Vec<_>>();
        let base_paths = pure_shell_base_paths();

        assert!(path.contains("/project/bin"));
        assert!(base_paths.iter().any(|base| entries.contains(base)));
        for inherited in crate::env::PATH.iter().filter(|path| {
            !base_paths.contains(path) && path.as_path() != PathBuf::from("/project/bin")
        }) {
            assert!(!entries.contains(inherited));
        }
    }
}
