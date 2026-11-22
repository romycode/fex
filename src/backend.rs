use std::{
    ffi::{OsStr, OsString},
    io,
    path::PathBuf,
    sync::{Arc, atomic::AtomicU64},
};

use crate::{
    fs::{self, Entry},
    ops::Report,
    s3,
};

/// Each backend retains its own location and entry identifiers.
#[derive(Clone, PartialEq, Eq)]
pub enum Location {
    Local(PathBuf),
    S3(s3::Location),
}

impl Location {
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    pub async fn list(self, show_hidden: bool) -> io::Result<Vec<Entry>> {
        match self {
            Self::Local(path) => fs::list_dir(&path, show_hidden),
            Self::S3(location) => s3::list(location).await,
        }
    }

    pub fn entry_label(&self, entry: &Entry) -> String {
        let key = entry.name.to_string_lossy();
        if let Self::S3(s3::Location::Prefix { prefix, .. }) = self
            && let Some(label) = key.strip_prefix(prefix)
        {
            return label.to_owned();
        }
        key.into_owned()
    }

    pub fn download_key<'a>(&self, entry: &'a Entry) -> Option<&'a str> {
        match self {
            Self::S3(s3::Location::Prefix { .. }) if entry.kind == fs::Kind::File => entry.name.to_str(),
            _ => None,
        }
    }

    pub async fn download(self, keys: Vec<String>, directory: PathBuf, progress: Arc<AtomicU64>) -> Report {
        match self {
            Self::S3(location) => s3::download(location, keys, directory, progress).await,
            Self::Local(_) => unreachable!("local selections do not provide download keys"),
        }
    }

    pub fn child(&self, entry: &Entry) -> Option<Self> {
        if !entry.kind.is_dir() {
            return None;
        }
        match self {
            Self::Local(path) => Some(Self::Local(path.join(&entry.name))),
            Self::S3(location) => location.child(entry.name.to_str()?).map(Self::S3),
        }
    }

    pub fn parent(&self) -> Option<(Self, OsString)> {
        match self {
            Self::Local(path) => Some((
                Self::Local(path.parent()?.to_path_buf()),
                path.file_name().unwrap_or(OsStr::new("")).to_os_string(),
            )),
            Self::S3(location) => {
                let (parent, child) = location.parent()?;
                Some((Self::S3(parent), child.into()))
            }
        }
    }
}
