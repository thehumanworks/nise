use std::path::{Path, PathBuf};

use crate::env;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreRoot {
    root: PathBuf,
}

impl Default for StoreRoot {
    fn default() -> Self {
        Self::from_env()
    }
}

impl StoreRoot {
    pub fn from_env() -> Self {
        Self {
            root: env::NISE_STORE_DIR.clone(),
        }
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.root.join("objects")
    }

    pub fn realisations_dir(&self) -> PathBuf {
        self.root.join("realisations")
    }

    pub fn refs_dir(&self) -> PathBuf {
        self.root.join("refs")
    }

    pub fn install_refs_dir(&self) -> PathBuf {
        self.refs_dir().join("installs")
    }

    pub fn profile_refs_dir(&self) -> PathBuf {
        self.refs_dir().join("profiles")
    }

    pub fn pin_refs_dir(&self) -> PathBuf {
        self.refs_dir().join("pins")
    }

    pub fn transaction_refs_dir(&self) -> PathBuf {
        self.refs_dir().join("transactions")
    }

    pub fn process_refs_dir(&self) -> PathBuf {
        self.refs_dir().join("processes")
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    pub fn trash_dir(&self) -> PathBuf {
        self.root.join("trash")
    }

    pub fn locks_dir(&self) -> PathBuf {
        self.root.join("locks")
    }
}
