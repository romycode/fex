//! Publicación sin sobrescritura y comprobación de archivos usados por deshacer.
use std::{
    ffi::CString,
    fs, io,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    time::SystemTime,
};

pub fn rename_exclusive(source: &Path, destination: &Path) -> io::Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    // Las dos rutas son cadenas terminadas en NUL y viven durante la llamada.
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(libc::AT_FDCWD, source.as_ptr(), libc::AT_FDCWD, destination.as_ptr(), libc::RENAME_NOREPLACE)
    };
    #[cfg(target_os = "macos")]
    let result = unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Fingerprint {
    relative: PathBuf,
    device: u64,
    inode: u64,
    size: u64,
    modified: SystemTime,
    directory: bool,
    mode: u32,
    changed: (i64, i64),
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub path: PathBuf,
    entries: Vec<Fingerprint>,
}

impl Snapshot {
    pub fn capture(path: &Path) -> io::Result<Self> {
        let mut entries = Vec::new();
        capture_tree(path, Path::new(""), &mut entries)?;
        Ok(Self { path: path.to_path_buf(), entries })
    }

    pub fn verify(&self) -> io::Result<()> {
        let current = Self::capture(&self.path)?;
        if current.entries != self.entries {
            return Err(io::Error::other(format!("{} — el archivo ha cambiado", self.path.display())));
        }
        Ok(())
    }

    /// Elimina únicamente las entradas copiadas. Una entrada nueva mantiene el directorio ocupado.
    pub fn remove_verified(&self) -> io::Result<()> {
        self.verify()?;
        for entry in self.entries.iter().rev() {
            let path = if entry.relative.as_os_str().is_empty() {
                self.path.clone()
            } else {
                self.path.join(&entry.relative)
            };
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.dev() != entry.device || metadata.ino() != entry.inode {
                return Err(io::Error::other(format!("{} — el archivo ha cambiado", path.display())));
            }
            if entry.directory {
                fs::remove_dir(path)?;
            } else {
                if metadata.len() != entry.size
                    || metadata.modified()? != entry.modified
                    || metadata.mode() != entry.mode
                    || (metadata.ctime(), metadata.ctime_nsec()) != entry.changed
                {
                    return Err(io::Error::other(format!("{} — el archivo ha cambiado", path.display())));
                }
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }
}

fn capture_tree(root: &Path, relative: &Path, entries: &mut Vec<Fingerprint>) -> io::Result<()> {
    let path = if relative.as_os_str().is_empty() { root.to_path_buf() } else { root.join(relative) };
    let metadata = fs::symlink_metadata(&path)?;
    entries.push(Fingerprint {
        relative: relative.to_path_buf(),
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified: metadata.modified()?,
        directory: metadata.is_dir(),
        mode: metadata.mode(),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    });
    if metadata.is_dir() {
        let mut children =
            fs::read_dir(path)?.map(|entry| entry.map(|entry| entry.file_name())).collect::<io::Result<Vec<_>>>()?;
        children.sort_unstable();
        for child in children {
            capture_tree(root, &relative.join(child), entries)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_rename_preserves_existing_destination_and_broken_link() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::write(&source, "new").unwrap();
        fs::write(&target, "existing").unwrap();
        assert_eq!(rename_exclusive(&source, &target).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&source).unwrap(), "new");
        assert_eq!(fs::read_to_string(&target).unwrap(), "existing");
        fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink("missing", &target).unwrap();
        assert_eq!(rename_exclusive(&source, &target).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_link(target).unwrap(), PathBuf::from("missing"));
    }

    #[test]
    fn snapshot_detects_replaced_children_and_modifications() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "original").unwrap();
        let snapshot = Snapshot::capture(dir.path()).unwrap();
        snapshot.verify().unwrap();
        fs::write(&file, "modified file").unwrap();
        assert!(snapshot.verify().is_err());
    }
}
