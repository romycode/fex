use std::{
    ffi::{CStr, CString, OsStr},
    fs, io,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    ptr::NonNull,
};

use objc2::rc::{Retained, autoreleasepool};
use objc2_foundation::{NSFileManager, NSURL, NSVolumeEnumerationOptions};

#[derive(Clone, Debug)]
pub struct Trashed {
    pub original: PathBuf,
    location: PathBuf,
    identity: (u64, u64),
}

fn file_url(path: &Path, is_dir: bool) -> io::Result<Retained<NSURL>> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    // Foundation reads a NUL-terminated path while the CString is still alive.
    let path_ptr =
        NonNull::new(path.as_ptr().cast_mut()).expect("a CString has a non-null pointer");
    Ok(unsafe {
        NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(path_ptr, is_dir, None)
    })
}

fn url_path(url: &NSURL) -> Option<PathBuf> {
    if !url.isFileURL() {
        return None;
    }
    // A file URL provides a NUL-terminated representation owned by that URL.
    let path = unsafe { CStr::from_ptr(url.fileSystemRepresentation().as_ptr()) };
    Some(PathBuf::from(OsStr::from_bytes(path.to_bytes())))
}

pub fn mounted_disks() -> io::Result<Vec<PathBuf>> {
    autoreleasepool(|_| {
        let volumes = NSFileManager::defaultManager()
            .mountedVolumeURLsIncludingResourceValuesForKeys_options(
                None,
                NSVolumeEnumerationOptions::SkipHiddenVolumes,
            )
            .ok_or_else(|| io::Error::other("No se pudieron enumerar los volúmenes"))?;
        let mut disks: Vec<_> = volumes
            .to_vec()
            .iter()
            .filter_map(|url| url_path(url))
            .filter(|path| path != Path::new("/"))
            .collect();
        disks.sort_unstable();
        disks.dedup();
        Ok(disks)
    })
}

pub fn move_to_trash(path: &Path) -> Result<Option<Trashed>, String> {
    autoreleasepool(|_| {
        let metadata = fs::symlink_metadata(path).map_err(|err| err.to_string())?;
        let url = file_url(path, metadata.is_dir()).map_err(|err| err.to_string())?;
        let mut result = None;
        NSFileManager::defaultManager()
            .trashItemAtURL_resultingItemURL_error(&url, Some(&mut result))
            .map_err(|err| err.to_string())?;
        // Foundation can rename the item in the Trash. Use its returned location.
        let Some(location) = result.as_deref().and_then(url_path) else {
            return Ok(None);
        };
        let Ok(metadata) = fs::symlink_metadata(&location) else {
            return Ok(None);
        };
        Ok(Some(Trashed {
            original: path.to_path_buf(),
            location,
            identity: (metadata.dev(), metadata.ino()),
        }))
    })
}

pub fn restore_from_trash(item: &Trashed) -> Result<(), String> {
    autoreleasepool(|_| {
        let metadata = fs::symlink_metadata(&item.location).map_err(|err| err.to_string())?;
        if (metadata.dev(), metadata.ino()) != item.identity {
            return Err(io::Error::from(io::ErrorKind::NotFound).to_string());
        }
        let source = file_url(&item.location, metadata.is_dir()).map_err(|err| err.to_string())?;
        let target = file_url(&item.original, metadata.is_dir()).map_err(|err| err.to_string())?;
        NSFileManager::defaultManager()
            .moveItemAtURL_toURL_error(&source, &target)
            .map_err(|err| err.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_refuses_to_replace_an_existing_file() {
        let dir = std::env::temp_dir().join(format!("fex-macos-restore-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let original = dir.join("document.txt");
        let location = dir.join("document 2.txt");
        fs::write(&location, "trashed").unwrap();
        fs::write(&original, "new").unwrap();
        let metadata = fs::symlink_metadata(&location).unwrap();
        let item = Trashed {
            original: original.clone(),
            location: location.clone(),
            identity: (metadata.dev(), metadata.ino()),
        };
        assert!(restore_from_trash(&item).is_err());
        assert_eq!(fs::read_to_string(&original).unwrap(), "new");
        assert_eq!(fs::read_to_string(&location).unwrap(), "trashed");
        fs::remove_file(&original).unwrap();
        restore_from_trash(&item).unwrap();
        assert_eq!(fs::read_to_string(&original).unwrap(), "trashed");
        assert!(!location.exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
