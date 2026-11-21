//! Deshacer (Ctrl+Z). Cada operación guarda lo necesario para su inversa.
//! La inversa nunca sobrescribe: si el sitio original está ocupado, falla con `Exists`.
//! El borrado permanente no se puede deshacer (por eso pide confirmación).

use std::{ffi::OsString, fs, io, path::PathBuf};

use crate::ops::{self, OpError, Report, Trashed, exists, file_name};

pub const MAX_UNDO: usize = 50;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UndoKind {
    Copy,
    Move,
    Rename,
    NewFolder,
    Trash,
}

#[derive(Clone, Debug)]
pub enum Undo {
    Guarded {
        action: Box<Undo>,
        snapshots: Vec<crate::safe_fs::Snapshot>,
    },
    /// Deshacer copiar = mandar las copias a la papelera (recuperable, no se borran).
    Copy {
        created: Vec<PathBuf>,
    },
    /// (original, actual)
    Move {
        pairs: Vec<(PathBuf, PathBuf)>,
    },
    Rename {
        dir: PathBuf,
        from: OsString,
        to: OsString,
    },
    NewFolder {
        path: PathBuf,
    },
    /// Guarda la ubicación exacta de cada archivo en la papelera.
    Trash {
        paths: Vec<Trashed>,
    },
}

impl Undo {
    pub fn kind(&self) -> UndoKind {
        match self {
            Undo::Guarded { action, .. } => action.kind(),
            Undo::Copy { .. } => UndoKind::Copy,
            Undo::Move { .. } => UndoKind::Move,
            Undo::Rename { .. } => UndoKind::Rename,
            Undo::NewFolder { .. } => UndoKind::NewFolder,
            Undo::Trash { .. } => UndoKind::Trash,
        }
    }

    pub fn guard(self) -> io::Result<Self> {
        let paths = match &self {
            Undo::Copy { created } => created.clone(),
            Undo::Move { pairs } => pairs.iter().map(|(_, path)| path.clone()).collect(),
            Undo::Rename { dir, to, .. } => vec![dir.join(to)],
            Undo::NewFolder { path } => vec![path.clone()],
            Undo::Trash { .. } | Undo::Guarded { .. } => return Ok(self),
        };
        let snapshots =
            paths.iter().map(|path| crate::safe_fs::Snapshot::capture(path)).collect::<io::Result<Vec<_>>>()?;
        Ok(Undo::Guarded { action: Box::new(self), snapshots })
    }

    pub fn is_new_folder(&self, candidate: &std::path::Path) -> bool {
        match self {
            Undo::NewFolder { path } => path == candidate,
            Undo::Guarded { action, .. } => action.is_new_folder(candidate),
            _ => false,
        }
    }

    /// Ejecuta la inversa. Bloqueante: llamar desde background.
    /// `Report::created` lleva los nombres restaurados (para seleccionarlos).
    pub fn revert(self) -> Report {
        let mut r = Report::default();
        match self {
            Undo::Guarded { action, snapshots } => {
                for snapshot in snapshots {
                    if let Err(error) = snapshot.verify() {
                        r.errors.push(OpError::Io(file_name(&snapshot.path), error));
                    }
                }
                if r.errors.is_empty() {
                    return action.revert();
                }
            }
            Undo::Copy { created } => {
                let present: Vec<PathBuf> = created.into_iter().filter(|p| exists(p)).collect();
                r = ops::trash(&present);
                r.trashed.clear();
            }
            Undo::Move { pairs } => {
                for (orig, now) in pairs {
                    let name = file_name(&orig);
                    if exists(&orig) {
                        r.errors.push(OpError::Exists(name));
                        continue;
                    }
                    let res = ops::move_exclusive(&now, &orig);
                    match res {
                        Ok(()) => {
                            r.created_paths.push(orig.clone());
                            r.created.push(orig.file_name().unwrap_or_default().to_os_string());
                            r.done += 1;
                        }
                        Err(e) => r.errors.push(OpError::Io(file_name(&now), e)),
                    }
                }
            }
            Undo::Rename { dir, from, to } => {
                if exists(&dir.join(&from)) {
                    r.errors.push(OpError::Exists(from.to_string_lossy().into_owned()));
                } else {
                    match crate::safe_fs::rename_exclusive(&dir.join(&to), &dir.join(&from)) {
                        Ok(()) => {
                            r.created_paths.push(dir.join(&from));
                            r.created.push(from);
                            r.done = 1;
                        }
                        Err(e) => r.errors.push(OpError::Io(to.to_string_lossy().into_owned(), e)),
                    }
                }
            }
            Undo::NewFolder { path } => match fs::remove_dir(&path) {
                Ok(()) => r.done = 1,
                // Si ya tiene contenido, a la papelera: nada se pierde.
                Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => {
                    r = ops::trash(std::slice::from_ref(&path));
                    r.trashed.clear();
                }
                Err(e) => r.errors.push(OpError::Io(file_name(&path), e)),
            },
            Undo::Trash { paths } => restore_from_trash(&paths, &mut r),
        }
        r
    }
}

#[cfg(target_os = "linux")]
fn restore_from_trash(items: &[Trashed], report: &mut Report) {
    for item in items {
        let name = file_name(&item.original);
        let result = item.snapshot.verify().and_then(|()| ops::move_exclusive(&item.location, &item.original));
        match result {
            Ok(()) => {
                report.created_paths.push(item.original.clone());
                report.created.push(item.original.file_name().unwrap_or_default().to_os_string());
                report.done += 1;
                if let Err(error) = fs::remove_file(&item.info) {
                    report.errors.push(OpError::Io(name, error));
                }
            }
            Err(error) => report.errors.push(OpError::Io(name, error)),
        }
    }
}

#[cfg(target_os = "macos")]
fn restore_from_trash(items: &[Trashed], r: &mut Report) {
    for item in items {
        let name = file_name(&item.original);
        if exists(&item.original) {
            r.errors.push(OpError::Exists(name));
            continue;
        }
        match crate::macos::restore_from_trash(item) {
            Ok(()) => {
                r.created_paths.push(item.original.clone());
                r.created.push(item.original.file_name().unwrap_or_default().to_os_string());
                r.done += 1;
            }
            Err(error) => r.errors.push(OpError::Trash(name, error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{Op, paste, rename};

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("fex-undo-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn undo_move_restores_and_never_overwrites() {
        let t = tmp("move");
        fs::create_dir_all(t.join("a")).unwrap();
        fs::create_dir_all(t.join("b")).unwrap();
        fs::write(t.join("a/f.txt"), "x").unwrap();
        let rep = paste(&[t.join("a/f.txt")], &t.join("b"), Op::Move, "copia");
        assert!(t.join("b/f.txt").exists() && !t.join("a/f.txt").exists());

        let r = Undo::Move { pairs: rep.pairs }.revert();
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(fs::read_to_string(t.join("a/f.txt")).unwrap(), "x");
        assert!(!t.join("b/f.txt").exists());

        // Si el origen vuelve a estar ocupado, no se pisa.
        let rep = paste(&[t.join("a/f.txt")], &t.join("b"), Op::Move, "copia");
        fs::write(t.join("a/f.txt"), "nuevo").unwrap();
        let r = Undo::Move { pairs: rep.pairs }.revert();
        assert!(matches!(r.errors.as_slice(), [OpError::Exists(_)]));
        assert_eq!(fs::read_to_string(t.join("a/f.txt")).unwrap(), "nuevo");
        fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn guarded_undo_refuses_modified_copies() {
        let t = tmp("guarded");
        let file = t.join("copy.txt");
        fs::write(&file, "original").unwrap();
        let undo = Undo::Copy { created: vec![file.clone()] }.guard().unwrap();
        fs::write(&file, "new contents").unwrap();
        let result = undo.revert();
        assert_eq!(result.done, 0);
        assert!(!result.errors.is_empty());
        assert_eq!(fs::read_to_string(file).unwrap(), "new contents");
        fs::remove_dir_all(t).unwrap();
    }

    #[test]
    fn undo_rename_and_new_folder() {
        let t = tmp("rename");
        fs::write(t.join("viejo.txt"), "").unwrap();
        rename(&t, std::ffi::OsStr::new("viejo.txt"), "nuevo.txt").unwrap();
        let r = Undo::Rename { dir: t.clone(), from: "viejo.txt".into(), to: "nuevo.txt".into() }.revert();
        assert_eq!(r.created, ["viejo.txt"]);
        assert!(t.join("viejo.txt").exists() && !t.join("nuevo.txt").exists());

        let d = t.join(ops::mkdir(&t, "Nueva carpeta").unwrap());
        Undo::NewFolder { path: d.clone() }.revert();
        assert!(!d.exists());
        fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn undo_trash_restores_original() {
        // Papelera aislada: XDG_DATA_HOME apunta a un tmp (y el fichero vive en el mismo FS).
        let t = tmp("trash");
        let data = t.join("data");
        fs::create_dir_all(&data).unwrap();
        // SAFETY: test de un solo hilo para este env var; el resto de tests no lo leen.
        unsafe { std::env::set_var("XDG_DATA_HOME", &data) };
        let f = t.join("borrame.txt");
        fs::write(&f, "importante").unwrap();
        let rep = ops::trash(std::slice::from_ref(&f));
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
        assert!(!f.exists());

        let r = Undo::Trash { paths: rep.trashed }.revert();
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(fs::read_to_string(&f).unwrap(), "importante");
        fs::remove_dir_all(&t).unwrap();
    }
}
