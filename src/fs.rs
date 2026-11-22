//! Acceso al sistema de ficheros. Nada aquí toca la UI; todo se llama desde background.

use crate::icons::Icon;
use std::{
    cmp::Ordering,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Clone)]
pub struct Entry {
    pub name: OsString,
    sort_key: String,
    pub kind: Kind,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
}

impl Entry {
    pub fn remote(key: &str, label: &str, kind: Kind, size: Option<u64>, modified: Option<SystemTime>) -> Self {
        Self { name: key.into(), sort_key: label.to_lowercase(), kind, size, modified }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Symlink { to_dir: bool },
    Other,
}

impl Kind {
    pub fn is_dir(self) -> bool {
        matches!(self, Kind::Dir | Kind::Symlink { to_dir: true })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    Name,
    Size,
    Modified,
}

pub fn sort_entries(entries: &mut [Entry], column: SortColumn, descending: bool) {
    entries.sort_unstable_by(|a, b| {
        let directories = b.kind.is_dir().cmp(&a.kind.is_dir());
        if directories != Ordering::Equal {
            return directories;
        }
        let order = match column {
            SortColumn::Name => a.sort_key.cmp(&b.sort_key),
            SortColumn::Size => a.size.cmp(&b.size),
            SortColumn::Modified => a.modified.cmp(&b.modified),
        }
        .then_with(|| a.name.cmp(&b.name));
        if descending { order.reverse() } else { order }
    });
}

/// Lee un directorio sin `stat`, salvo en symlinks (para saber si apuntan a un dir).
pub fn list_dir(path: &Path, show_hidden: bool) -> io::Result<Vec<Entry>> {
    let mut out = Vec::with_capacity(256);
    for de in fs::read_dir(path)? {
        let de = de?;
        let name = de.file_name();
        if !show_hidden && name.as_encoded_bytes().starts_with(b".") {
            continue;
        }
        let kind = match de.file_type() {
            Ok(ft) if ft.is_dir() => Kind::Dir,
            Ok(ft) if ft.is_file() => Kind::File,
            Ok(ft) if ft.is_symlink() => {
                Kind::Symlink { to_dir: fs::metadata(de.path()).map(|m| m.is_dir()).unwrap_or(false) }
            }
            _ => Kind::Other,
        };
        out.push(Entry { sort_key: name.to_string_lossy().to_lowercase(), name, kind, size: None, modified: None });
    }
    out.sort_unstable_by(|a, b| match (a.kind.is_dir(), b.kind.is_dir()) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => a.sort_key.cmp(&b.sort_key),
    });
    Ok(out)
}

/// Segunda pasada: `lstat` de cada entrada, en background.
pub fn stat_all(dir: &Path, names: Vec<OsString>) -> Vec<(OsString, Option<u64>, Option<SystemTime>)> {
    names
        .into_iter()
        .map(|n| match fs::symlink_metadata(dir.join(&n)) {
            Ok(m) => (n, m.is_file().then(|| m.len()), m.modified().ok()),
            Err(_) => (n, None, None),
        })
        .collect()
}

#[derive(Clone, Copy)]
pub enum PlaceId {
    Home,
    Desktop,
    Documents,
    Downloads,
    Pictures,
    Music,
    Videos,
    System,
}

impl PlaceId {
    pub fn label(self, t: &crate::i18n::Strings) -> &'static str {
        match self {
            PlaceId::Home => t.home,
            PlaceId::Desktop => t.desktop,
            PlaceId::Documents => t.documents,
            PlaceId::Downloads => t.downloads,
            PlaceId::Pictures => t.pictures,
            PlaceId::Music => t.music,
            PlaceId::Videos => t.videos,
            PlaceId::System => t.system,
        }
    }
}

pub struct Place {
    pub id: PlaceId,
    pub icon: crate::icons::Icon,
    pub path: PathBuf,
}

/// Lugares habituales de la barra lateral.
pub fn places() -> Vec<Place> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into());
    #[cfg(target_os = "linux")]
    let user_dirs = fs::read_to_string(home.join(".config/user-dirs.dirs")).unwrap_or_default();
    #[cfg(target_os = "linux")]
    let user_dir = |key: &str, fallback: &str| -> PathBuf {
        user_dirs
            .lines()
            .find_map(|l| l.strip_prefix(&format!("XDG_{key}_DIR=")))
            .map(|v| v.trim_matches('"').replace("$HOME", &home.to_string_lossy()))
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(fallback))
    };
    #[cfg(target_os = "macos")]
    let user_dir = |_: &str, fallback: &str| home.join(fallback);
    let videos = if cfg!(target_os = "macos") { "Movies" } else { "Videos" };

    let mut out = vec![Place { id: PlaceId::Home, icon: Icon::Home, path: home.clone() }];
    for (id, icon, key, fallback) in [
        (PlaceId::Desktop, Icon::Desktop, "DESKTOP", "Desktop"),
        (PlaceId::Documents, Icon::Documents, "DOCUMENTS", "Documents"),
        (PlaceId::Downloads, Icon::Download, "DOWNLOAD", "Downloads"),
        (PlaceId::Pictures, Icon::Pictures, "PICTURES", "Pictures"),
        (PlaceId::Music, Icon::Music, "MUSIC", "Music"),
        (PlaceId::Videos, Icon::Videos, "VIDEOS", videos),
    ] {
        let path = user_dir(key, fallback);
        if path != home && path.is_dir() {
            out.push(Place { id, icon, path });
        }
    }
    out.push(Place { id: PlaceId::System, icon: Icon::System, path: "/".into() });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn listing_keeps_distinct_names() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(target_os = "linux")]
        let original = OsString::from_vec(b"name-\xff".to_vec());
        #[cfg(target_os = "macos")]
        let original = OsString::from("name-文件");
        let display = OsString::from("name-�");
        fs::write(directory.path().join(&original), "one").unwrap();
        fs::write(directory.path().join(&display), "two").unwrap();
        let entries = list_dir(directory.path(), false).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|entry| entry.name == original));
        assert!(entries.iter().any(|entry| entry.name == display));
    }

    #[test]
    fn sort_keeps_directories_first_in_both_directions() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("folder")).unwrap();
        fs::write(directory.path().join("small"), "x").unwrap();
        fs::write(directory.path().join("large"), "longer").unwrap();
        let mut entries = list_dir(directory.path(), false).unwrap();
        for entry in &mut entries {
            entry.size = fs::metadata(directory.path().join(&entry.name)).ok().map(|metadata| metadata.len());
        }
        sort_entries(&mut entries, SortColumn::Size, false);
        assert_eq!(entries[0].name, "folder");
        assert_eq!(entries[1].name, "small");
        sort_entries(&mut entries, SortColumn::Size, true);
        assert_eq!(entries[0].name, "folder");
        assert_eq!(entries[1].name, "large");
    }
}
