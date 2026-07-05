use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use eyre::{Context, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::Result;
use crate::file;
use crate::store::{HASH_ALGORITHM_SHA256, OBJECT_MANIFEST_FILE};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TreeHash {
    pub hash_algorithm: String,
    pub hash: String,
    pub bytes: u64,
    pub files: u64,
}

pub fn canonical_tree_hash(root: impl AsRef<Path>) -> Result<TreeHash> {
    let root = root.as_ref();
    if !root.is_dir() {
        bail!(
            "tree hash root is not a directory: {}",
            file::display_path(root)
        );
    }

    let mut hasher = Sha256::new();
    let mut bytes = 0;
    let mut files = 0;

    for entry in WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
    {
        let entry = entry?;
        let path = entry.path();
        if path == root {
            continue;
        }

        let relative = path
            .strip_prefix(root)
            .wrap_err_with(|| format!("failed to relativize {}", file::display_path(path)))?;
        if relative == Path::new(OBJECT_MANIFEST_FILE) {
            continue;
        }

        let relative = canonical_path_bytes(relative);
        let file_type = entry.file_type();
        feed(&mut hasher, b"path", &relative);

        if file_type.is_dir() {
            feed(&mut hasher, b"type", b"dir");
        } else if file_type.is_symlink() {
            let target = fs::read_link(path).wrap_err_with(|| {
                format!(
                    "failed to read symlink target: {}",
                    file::display_path(path)
                )
            })?;
            feed(&mut hasher, b"type", b"symlink");
            feed(&mut hasher, b"target", &canonical_path_bytes(&target));
            files += 1;
        } else if file_type.is_file() {
            let metadata = entry.metadata()?;
            feed(&mut hasher, b"type", b"file");
            feed(
                &mut hasher,
                b"executable",
                if executable(&metadata) { b"1" } else { b"0" },
            );
            hash_file_contents(path, &mut hasher)?;
            bytes += metadata.len();
            files += 1;
        } else {
            feed(&mut hasher, b"type", b"other");
        }
    }

    let hash = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(TreeHash {
        hash_algorithm: HASH_ALGORITHM_SHA256.to_string(),
        hash,
        bytes,
        files,
    })
}

fn hash_file_contents(path: &Path, hasher: &mut Sha256) -> Result<()> {
    let mut file = file::open(path)?;
    let mut buf = [0; 32 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        feed(hasher, b"bytes", &buf[..n]);
    }
    Ok(())
}

fn feed(hasher: &mut Sha256, key: &[u8], value: &[u8]) {
    hasher.update(key);
    hasher.update([0]);
    hasher.update(value.len().to_string().as_bytes());
    hasher.update([0]);
    hasher.update(value);
    hasher.update([0]);
}

#[cfg(unix)]
fn executable(metadata: &fs::Metadata) -> bool {
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_metadata: &fs::Metadata) -> bool {
    false
}

fn canonical_path_bytes(path: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    for component in path.components() {
        if !out.is_empty() {
            out.push(b'/');
        }
        out.extend(component_bytes(component.as_os_str()));
    }
    out
}

#[cfg(unix)]
fn component_bytes(component: &std::ffi::OsStr) -> Vec<u8> {
    component.as_bytes().to_vec()
}

#[cfg(windows)]
fn component_bytes(component: &std::ffi::OsStr) -> Vec<u8> {
    component.encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(not(any(unix, windows)))]
fn component_bytes(component: &std::ffi::OsStr) -> Vec<u8> {
    component.to_string_lossy().as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_hash_ignores_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bin").join("tool");
        file::create_dir_all(path.parent().unwrap()).unwrap();
        file::write(&path, "tool").unwrap();

        let first = canonical_tree_hash(tmp.path()).unwrap();
        let atime = filetime::FileTime::from_unix_time(1, 0);
        let mtime = filetime::FileTime::from_unix_time(2, 0);
        filetime::set_file_times(&path, atime, mtime).unwrap();
        let second = canonical_tree_hash(tmp.path()).unwrap();

        assert_eq!(first.hash, second.hash);
    }

    #[cfg(unix)]
    #[test]
    fn tree_hash_includes_executable_bit() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tool");
        file::write(&path, "tool").unwrap();

        let first = canonical_tree_hash(tmp.path()).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        let second = canonical_tree_hash(tmp.path()).unwrap();

        assert_ne!(first.hash, second.hash);
    }

    #[cfg(unix)]
    #[test]
    fn tree_hash_includes_symlink_target() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("tool");
        std::os::unix::fs::symlink("first", &link).unwrap();
        let first = canonical_tree_hash(tmp.path()).unwrap();
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("second", &link).unwrap();
        let second = canonical_tree_hash(tmp.path()).unwrap();

        assert_ne!(first.hash, second.hash);
    }

    #[test]
    fn object_manifest_file_is_not_hashed() {
        let tmp = tempfile::tempdir().unwrap();
        file::write(tmp.path().join("tool"), "tool").unwrap();
        let first = canonical_tree_hash(tmp.path()).unwrap();
        file::write(
            tmp.path().join(OBJECT_MANIFEST_FILE),
            "tree_hash = 'ignored'",
        )
        .unwrap();
        let second = canonical_tree_hash(tmp.path()).unwrap();

        assert_eq!(first.hash, second.hash);
    }

    #[cfg(unix)]
    #[test]
    fn tree_hash_distinguishes_non_utf8_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let first = Path::new(&OsString::from_vec(vec![0xff])).to_path_buf();
        let second = Path::new(&OsString::from_vec(vec![0xfe])).to_path_buf();

        assert_ne!(canonical_path_bytes(&first), canonical_path_bytes(&second));
    }
}
