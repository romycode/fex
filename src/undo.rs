//! Deshacer (Ctrl+Z). Cada operación guarda lo necesario para su inversa.
//! La inversa nunca sobrescribe: si el sitio original está ocupado, falla con `Exists`.
//! El borrado permanente no se puede deshacer (por eso pide confirmación).

use std::{fs, io, path::PathBuf};

use crate::ops::{self, EXDEV, OpError, Report, exists, file_name};

pub const MAX_UNDO: usize = 50;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UndoKind {
    Copy,
    Move,
    Rename,
    NewFolder,
    Trash,
}

#[derive(Debug)]
pub enum Undo {
    /// Deshacer copiar = mandar las copias a la papelera (recuperable, no se borran).
    Copy { created: Vec<PathBuf> },
    /// (original, actual)
    Move { pairs: Vec<(PathBuf, PathBuf)> },
    Rename { dir: PathBuf, from: String, to: String },
    NewFolder { path: PathBuf },
    /// `since`: segundos Unix al lanzar la operación, para identificar las entradas de la papelera.
    Trash { paths: Vec<PathBuf>, since: i64 },
}

impl Undo {
    pub fn kind(&self) -> UndoKind {
        match self {
            Undo::Copy { .. } => UndoKind::Copy,
            Undo::Move { .. } => UndoKind::Move,
            Undo::Rename { .. } => UndoKind::Rename,
            Undo::NewFolder { .. } => UndoKind::NewFolder,
            Undo::Trash { .. } => UndoKind::Trash,
        }
    }

    /// Ejecuta la inversa. Bloqueante: llamar desde background.
    /// `Report::created` lleva los nombres restaurados (para seleccionarlos).
    pub fn revert(self) -> Report {
        let mut r = Report::default();
        match self {
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
                    let res = match fs::rename(&now, &orig) {
                        Err(e) if e.raw_os_error() == Some(EXDEV) => {
                            ops::copy_recursive(&now, &orig).and_then(|_| ops::remove_any(&now))
                        }
                        other => other,
                    };
                    match res {
                        Ok(()) => {
                            r.created.push(name);
                            r.done += 1;
                        }
                        Err(e) => r.errors.push(OpError::Io(file_name(&now), e)),
                    }
                }
            }
            Undo::Rename { dir, from, to } => {
                if exists(&dir.join(&from)) {
                    r.errors.push(OpError::Exists(from));
                } else {
                    match fs::rename(dir.join(&to), dir.join(&from)) {
                        Ok(()) => {
                            r.created.push(from);
                            r.done = 1;
                        }
                        Err(e) => r.errors.push(OpError::Io(to, e)),
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
            Undo::Trash { paths, since } => restore_from_trash(&paths, since, &mut r),
        }
        r
    }
}

fn restore_from_trash(paths: &[PathBuf], since: i64, r: &mut Report) {
    let items = match trash::os_limited::list() {
        Ok(items) => items,
        Err(e) => {
            r.errors.push(OpError::Trash(String::new(), e.to_string()));
            return;
        }
    };
    for p in paths {
        let name = file_name(p);
        // La entrada más reciente con ese origen, borrada después de lanzar la operación.
        // (DeletionDate tiene resolución de segundos y hora local: margen de 2 s.)
        let item = items
            .iter()
            .filter(|i| {
                Some(i.original_parent.as_path()) == p.parent()
                    && i.name == *p.file_name().unwrap_or_default()
                    && i.time_deleted >= since - 2
            })
            .max_by_key(|i| i.time_deleted);
        let Some(item) = item else {
            r.errors.push(OpError::Trash(name, "no está en la papelera".into()));
            continue;
        };
        if exists(p) {
            r.errors.push(OpError::Exists(name));
            continue;
        }
        match trash::os_limited::restore_all([item.clone()]) {
            Ok(()) => {
                r.created.push(name);
                r.done += 1;
            }
            Err(e) => r.errors.push(OpError::Trash(name, e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{Op, paste, rename};

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("fexp-undo-{tag}-{}", std::process::id()));
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
    fn undo_rename_and_new_folder() {
        let t = tmp("rename");
        fs::write(t.join("viejo.txt"), "").unwrap();
        rename(&t, "viejo.txt", "nuevo.txt").unwrap();
        let r = Undo::Rename { dir: t.clone(), from: "viejo.txt".into(), to: "nuevo.txt".into() }.revert();
        assert_eq!(r.created, ["viejo.txt"]);
        assert!(t.join("viejo.txt").exists() && !t.join("nuevo.txt").exists());

        let d = t.join(ops::mkdir(&t, "Nueva carpeta").unwrap());
        Undo::NewFolder { path: d.clone() }.revert();
        assert!(!d.exists());
        fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn undo_trash_restores_original() {
        // Papelera aislada: XDG_DATA_HOME apunta a un tmp (y el fichero vive en el mismo FS).
        let t = tmp("trash");
        let data = t.join("data");
        fs::create_dir_all(&data).unwrap();
        // SAFETY: test de un solo hilo para este env var; el resto de tests no lo leen.
        unsafe { std::env::set_var("XDG_DATA_HOME", &data) };
        let f = t.join("borrame.txt");
        fs::write(&f, "importante").unwrap();
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let rep = ops::trash(std::slice::from_ref(&f));
        assert!(rep.errors.is_empty(), "{:?}", rep.errors);
        assert!(!f.exists());

        let r = Undo::Trash { paths: rep.trashed, since }.revert();
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(fs::read_to_string(&f).unwrap(), "importante");
        fs::remove_dir_all(&t).unwrap();
    }
}
