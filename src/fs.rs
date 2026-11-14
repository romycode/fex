//! Acceso al sistema de ficheros. Nada aquí toca la UI; todo se llama desde background.

use crate::icons::Icon;
use std::{
    cmp::Ordering,
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Clone)]
pub struct Entry {
    pub name: String,
    sort_key: String,
    pub kind: Kind,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
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

/// Lee un directorio sin `stat`, salvo en symlinks (para saber si apuntan a un dir).
pub fn list_dir(path: &Path, show_hidden: bool) -> io::Result<Vec<Entry>> {
    let mut out = Vec::with_capacity(256);
    for de in fs::read_dir(path)? {
        let Ok(de) = de else { continue };
        let name = de.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let kind = match de.file_type() {
            Ok(ft) if ft.is_dir() => Kind::Dir,
            Ok(ft) if ft.is_file() => Kind::File,
            Ok(ft) if ft.is_symlink() => Kind::Symlink {
                to_dir: fs::metadata(de.path()).map(|m| m.is_dir()).unwrap_or(false),
            },
            _ => Kind::Other,
        };
        out.push(Entry {
            sort_key: name.to_lowercase(),
            name,
            kind,
            size: None,
            modified: None,
        });
    }
    out.sort_unstable_by(|a, b| match (a.kind.is_dir(), b.kind.is_dir()) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => a.sort_key.cmp(&b.sort_key),
    });
    Ok(out)
}

/// Segunda pasada: `lstat` de cada entrada, en background.
pub fn stat_all(dir: &Path, names: Vec<String>) -> Vec<(Option<u64>, Option<SystemTime>)> {
    names
        .into_iter()
        .map(|n| match fs::symlink_metadata(dir.join(n)) {
            Ok(m) => (m.is_file().then(|| m.len()), m.modified().ok()),
            Err(_) => (None, None),
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

/// Lugares de la barra lateral. Lee `~/.config/user-dirs.dirs` (fichero de pocos bytes).
pub fn places() -> Vec<Place> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into());
    let user_dirs = fs::read_to_string(home.join(".config/user-dirs.dirs")).unwrap_or_default();
    let xdg = |key: &str, fallback: &str| -> PathBuf {
        user_dirs
            .lines()
            .find_map(|l| l.strip_prefix(&format!("XDG_{key}_DIR=")))
            .map(|v| v.trim_matches('"').replace("$HOME", &home.to_string_lossy()))
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(fallback))
    };

    let mut out = vec![Place { id: PlaceId::Home, icon: Icon::Home, path: home.clone() }];
    for (id, icon, key, fallback) in [
        (PlaceId::Desktop, Icon::Desktop, "DESKTOP", "Desktop"),
        (PlaceId::Documents, Icon::Documents, "DOCUMENTS", "Documents"),
        (PlaceId::Downloads, Icon::Download, "DOWNLOAD", "Downloads"),
        (PlaceId::Pictures, Icon::Pictures, "PICTURES", "Pictures"),
        (PlaceId::Music, Icon::Music, "MUSIC", "Music"),
        (PlaceId::Videos, Icon::Videos, "VIDEOS", "Videos"),
    ] {
        let path = xdg(key, fallback);
        if path != home && path.is_dir() {
            out.push(Place { id, icon, path });
        }
    }
    out.push(Place { id: PlaceId::System, icon: Icon::System, path: "/".into() });
    out
}
