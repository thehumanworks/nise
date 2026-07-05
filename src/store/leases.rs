use std::path::PathBuf;

use eyre::Result;
use serde::Serialize;

use crate::rand::random_string;
use crate::store::{SCHEMA_VERSION, StoreRoot, write_manifest};
use crate::{duration, file};

#[derive(Debug, Serialize)]
struct ProcessLeaseManifest {
    schema_version: u32,
    pid: u32,
    nonce: String,
    profile_id: String,
    profile_generation: u64,
    created_at: String,
}

#[derive(Debug)]
pub struct ProcessLeaseGuard {
    path: PathBuf,
}

impl ProcessLeaseGuard {
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

impl Drop for ProcessLeaseGuard {
    fn drop(&mut self) {
        let _ = file::remove_file(&self.path);
    }
}

pub fn acquire_process_lease(
    store: &StoreRoot,
    profile_id: impl Into<String>,
    profile_generation: u64,
) -> Result<ProcessLeaseGuard> {
    let pid = std::process::id();
    let nonce = random_string(10);
    let path = store.process_refs_dir().join(format!("{pid}-{nonce}.toml"));
    let manifest = ProcessLeaseManifest {
        schema_version: SCHEMA_VERSION,
        pid,
        nonce,
        profile_id: profile_id.into(),
        profile_generation,
        created_at: duration::process_now().to_string(),
    };
    write_manifest(&path, &manifest)?;
    Ok(ProcessLeaseGuard { path })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_lease_guard_removes_manifest_on_drop() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = StoreRoot::new(tmp.path().join("store"));
        let path = {
            let guard = acquire_process_lease(&store, "projects/demo/default", 7)?;
            let path = guard.path().clone();
            assert!(path.exists());
            path
        };

        assert!(!path.exists());
        Ok(())
    }
}
