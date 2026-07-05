use std::path::{Path, PathBuf};

use eyre::{Result, bail};
use serde::Serialize;
use walkdir::WalkDir;

use crate::hash::hash_sha256_to_str;
use crate::store::{
    PROFILE_MANIFEST_FILE, ProfileManifest, SCHEMA_VERSION, StoreRoot, read_manifest,
    write_profile_manifest,
};
use crate::{duration, file};

pub struct ProjectProfileInput<'a> {
    pub profile: &'a str,
    pub project_root: &'a Path,
    pub source_config_hash: String,
    pub nise_lock_hash: String,
    pub realisations: Vec<String>,
    pub path_entries: Vec<PathBuf>,
    pub env_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileGeneration {
    pub profile_id: String,
    pub generation: u64,
    pub current: bool,
    pub profile_root: PathBuf,
    pub generation_path: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: ProfileManifest,
}

pub fn project_profile_root(store: &StoreRoot, project_root: &Path, profile: &str) -> PathBuf {
    let project_hash = hash_sha256_to_str(&project_root.to_string_lossy());
    store
        .profile_refs_dir()
        .join("projects")
        .join(&project_hash[..16])
        .join(profile)
}

pub fn write_project_profile_generation(
    store: &StoreRoot,
    input: ProjectProfileInput<'_>,
) -> Result<ProfileManifest> {
    let root = project_profile_root(store, input.project_root, input.profile);
    let generations = root.join("generations");
    file::create_dir_all(&generations)?;
    let generation = next_generation(&generations)?;
    let generation_dir = generations.join(generation.to_string());
    let tmp_dir = generations.join(format!("{generation}.tmp"));
    if tmp_dir.exists() {
        file::remove_all(&tmp_dir)?;
    }
    file::create_dir_all(&tmp_dir)?;

    let profile_id = format!(
        "projects/{}/{}",
        &hash_sha256_to_str(&input.project_root.to_string_lossy())[..16],
        input.profile
    );
    let manifest = ProfileManifest {
        schema_version: SCHEMA_VERSION,
        profile_id,
        generation,
        project_root: Some(input.project_root.to_path_buf()),
        source_config_hash: input.source_config_hash,
        nise_lock_hash: input.nise_lock_hash,
        created_at: duration::process_now().to_string(),
        realisations: input.realisations,
        env_hash: input.env_hash,
        path_entries: input.path_entries,
    };
    write_profile_manifest(&tmp_dir, &manifest)?;
    file::rename(&tmp_dir, &generation_dir)?;
    file::create_dir_all(&root)?;
    file::make_symlink_or_file(
        &PathBuf::from("generations").join(generation.to_string()),
        &root.join("current"),
    )?;
    Ok(manifest)
}

pub fn list_profile_generations(store: &StoreRoot) -> Result<Vec<ProfileGeneration>> {
    let profiles_root = store.profile_refs_dir();
    if !profiles_root.exists() {
        return Ok(vec![]);
    }
    let mut generations = vec![];
    for entry in WalkDir::new(&profiles_root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
    {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.file_name() != std::ffi::OsStr::new(PROFILE_MANIFEST_FILE)
        {
            continue;
        }
        let manifest_path = entry.path().to_path_buf();
        let generation_path = manifest_path
            .parent()
            .unwrap_or(&profiles_root)
            .to_path_buf();
        let Some(profile_root) = generation_path
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
        else {
            continue;
        };
        let manifest: ProfileManifest = read_manifest(&manifest_path)?;
        let current = current_generation(&profile_root)? == Some(manifest.generation);
        generations.push(ProfileGeneration {
            profile_id: manifest.profile_id.clone(),
            generation: manifest.generation,
            current,
            profile_root,
            generation_path,
            manifest_path,
            manifest,
        });
    }
    generations.sort_by(|a, b| {
        a.profile_id
            .cmp(&b.profile_id)
            .then_with(|| a.generation.cmp(&b.generation))
    });
    Ok(generations)
}

pub fn list_project_profile_generations(
    store: &StoreRoot,
    project_root: &Path,
    profile: &str,
) -> Result<Vec<ProfileGeneration>> {
    let root = project_profile_root(store, project_root, profile);
    Ok(list_profile_generations(store)?
        .into_iter()
        .filter(|generation| generation.profile_root == root)
        .collect())
}

pub fn current_project_profile_generation(
    store: &StoreRoot,
    project_root: &Path,
    profile: &str,
) -> Result<Option<ProfileGeneration>> {
    Ok(
        list_project_profile_generations(store, project_root, profile)?
            .into_iter()
            .find(|generation| generation.current),
    )
}

pub fn rollback_project_profile(
    store: &StoreRoot,
    project_root: &Path,
    profile: &str,
    generation: u64,
) -> Result<ProfileGeneration> {
    let profile_root = project_profile_root(store, project_root, profile);
    let generation_path = profile_root
        .join("generations")
        .join(generation.to_string());
    let manifest_path = generation_path.join(PROFILE_MANIFEST_FILE);
    if !manifest_path.exists() {
        bail!(
            "profile generation does not exist: {}",
            file::display_path(&generation_path)
        );
    }
    file::make_symlink_or_file(
        &PathBuf::from("generations").join(generation.to_string()),
        &profile_root.join("current"),
    )?;
    current_project_profile_generation(store, project_root, profile)?.ok_or_else(|| {
        eyre::eyre!(
            "failed to switch current profile to generation {}",
            generation
        )
    })
}

pub fn previous_project_profile_generation(
    store: &StoreRoot,
    project_root: &Path,
    profile: &str,
) -> Result<Option<u64>> {
    let current = current_generation(&project_profile_root(store, project_root, profile))?;
    let Some(current) = current else {
        return Ok(None);
    };
    Ok(
        list_project_profile_generations(store, project_root, profile)?
            .into_iter()
            .filter_map(|generation| {
                (generation.generation < current).then_some(generation.generation)
            })
            .max(),
    )
}

fn current_generation(profile_root: &Path) -> Result<Option<u64>> {
    let current = profile_root.join("current");
    let Some(target) = file::resolve_symlink(&current)? else {
        return Ok(None);
    };
    Ok(target
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.parse::<u64>().ok()))
}

fn next_generation(generations: &Path) -> Result<u64> {
    let mut max = 0;
    for path in file::ls(generations)? {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Ok(generation) = name.parse::<u64>() {
            max = max.max(generation);
        }
    }
    Ok(max + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{PROFILE_MANIFEST_FILE, read_manifest};

    #[test]
    fn writes_project_profile_generation_and_advances_current() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let project_root = tmp.path().join("project");
        file::create_dir_all(&project_root)?;

        let first = write_project_profile_generation(
            &store,
            ProjectProfileInput {
                profile: "default",
                project_root: &project_root,
                source_config_hash: "sha256:config-a".to_string(),
                nise_lock_hash: "sha256:lock-a".to_string(),
                realisations: vec!["legacy:demo@1.0.0".to_string()],
                path_entries: vec![tmp.path().join("bin-a")],
                env_hash: "sha256:env-a".to_string(),
            },
        )?;
        let second = write_project_profile_generation(
            &store,
            ProjectProfileInput {
                profile: "default",
                project_root: &project_root,
                source_config_hash: "sha256:config-b".to_string(),
                nise_lock_hash: "sha256:lock-b".to_string(),
                realisations: vec!["legacy:demo@2.0.0".to_string()],
                path_entries: vec![tmp.path().join("bin-b")],
                env_hash: "sha256:env-b".to_string(),
            },
        )?;

        assert_eq!(first.generation, 1);
        assert_eq!(second.generation, 2);
        let profile_root = project_profile_root(&store, &project_root, "default");
        let current = file::resolve_symlink(&profile_root.join("current"))?.unwrap();
        assert_eq!(current, PathBuf::from("generations/2"));
        let written: ProfileManifest = read_manifest(
            profile_root
                .join("generations/2")
                .join(PROFILE_MANIFEST_FILE),
        )?;
        assert_eq!(written.path_entries, vec![tmp.path().join("bin-b")]);
        Ok(())
    }

    #[test]
    fn lists_generations_and_rolls_back_current_profile() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let project_root = tmp.path().join("project");
        file::create_dir_all(&project_root)?;

        for version in ["1.0.0", "2.0.0"] {
            write_project_profile_generation(
                &store,
                ProjectProfileInput {
                    profile: "default",
                    project_root: &project_root,
                    source_config_hash: format!("sha256:config-{version}"),
                    nise_lock_hash: "sha256:legacy-unlocked".to_string(),
                    realisations: vec![format!("legacy:demo@{version}")],
                    path_entries: vec![tmp.path().join(version).join("bin")],
                    env_hash: format!("sha256:env-{version}"),
                },
            )?;
        }

        let generations = list_project_profile_generations(&store, &project_root, "default")?;
        assert_eq!(generations.len(), 2);
        assert!(!generations[0].current);
        assert!(generations[1].current);
        assert_eq!(
            previous_project_profile_generation(&store, &project_root, "default")?,
            Some(1)
        );

        let rolled_back = rollback_project_profile(&store, &project_root, "default", 1)?;

        assert_eq!(rolled_back.generation, 1);
        assert!(rolled_back.current);
        assert_eq!(
            current_project_profile_generation(&store, &project_root, "default")?
                .unwrap()
                .manifest
                .path_entries,
            vec![tmp.path().join("1.0.0").join("bin")]
        );
        Ok(())
    }
}
