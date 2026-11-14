//! Operaciones de ficheros. Todo bloqueante: se llama siempre desde `background_spawn`.
//!
//! Política: nunca se sobrescribe nada. Si el destino existe, se genera un nombre libre
//! ("x (copia).txt", "x (2).txt").

use std::{
    ffi::OsString,
    fs, io,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Copy,
    Move,
    Trash,
    Delete,
}

#[derive(Debug)]
pub enum OpError {
    /// Copiar/mover una carpeta dentro de sí misma.
    IntoItself(String),
    /// Al deshacer: el sitio original ya está ocupado (no se sobrescribe).
    Exists(String),
    Io(String, io::Error),
    Trash(String, String),
}

#[derive(Default, Debug)]
pub struct Report {
    /// Nombres resultantes dentro del directorio destino (para seleccionarlos al terminar).
    pub created: Vec<String>,
    /// (origen, destino) de cada elemento pegado: lo necesita el deshacer.
    pub pairs: Vec<(PathBuf, PathBuf)>,
    /// Rutas enviadas a la papelera con éxito.
    pub trashed: Vec<PathBuf>,
    pub done: usize,
    pub errors: Vec<OpError>,
}

pub fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

pub fn exists(p: &Path) -> bool {
    // No sigue symlinks: un enlace roto también "ocupa" el nombre.
    fs::symlink_metadata(p).is_ok()
}

// ------------------------------------------------------------- nombres ---

const DOUBLE_EXT: [&str; 4] = [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst"];

fn split_name(name: &str, is_dir: bool) -> (&str, &str) {
    if is_dir {
        return (name, "");
    }
    for d in DOUBLE_EXT {
        if name.len() > d.len() && name.ends_with(d) {
            return name.split_at(name.len() - d.len());
        }
    }
    match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""), // sin extensión o dotfile (".bashrc")
    }
}

/// Primer nombre libre en `dir`. Con `label` (p. ej. "copia") siempre añade sufijo.
pub fn unique_name(dir: &Path, name: &str, is_dir: bool, label: Option<&str>) -> String {
    if label.is_none() && !exists(&dir.join(name)) {
        return name.to_owned();
    }
    let (stem, ext) = split_name(name, is_dir);
    (1..)
        .filter_map(|n| match (label, n) {
            (Some(l), 1) => Some(format!("{stem} ({l}){ext}")),
            (Some(l), n) => Some(format!("{stem} ({l} {n}){ext}")),
            (None, 1) => None,
            (None, n) => Some(format!("{stem} ({n}){ext}")),
        })
        .find(|cand| !exists(&dir.join(cand)))
        .expect("rango infinito")
}

/// Posición del cursor al empezar a renombrar: antes de la extensión.
pub fn stem_len(name: &str, is_dir: bool) -> usize {
    split_name(name, is_dir).0.len()
}

pub fn validate_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

// ------------------------------------------------------------- copiar ----

/// Copia recursiva. Los symlinks se copian como symlinks (sin seguirlos: evita bucles).
pub fn copy_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)
    } else if ft.is_dir() {
        fs::create_dir(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
        // Permisos al final: un directorio 0555 no dejaría escribir los hijos.
        fs::set_permissions(dst, meta.permissions())
    } else if ft.is_file() {
        fs::copy(src, dst).map(|_| ())
    } else {
        Err(io::Error::new(io::ErrorKind::Unsupported, "tipo de fichero especial"))
    }
}

pub fn remove_any(p: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(p)?;
    if meta.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) }
}

pub const EXDEV: i32 = 18;

/// Copia o mueve `srcs` dentro de `dest`.
pub fn paste(srcs: &[PathBuf], dest: &Path, op: Op, copy_label: &str) -> Report {
    let mut r = Report::default();
    for src in srcs {
        let name = file_name(src);
        let is_dir = fs::symlink_metadata(src).map(|m| m.is_dir()).unwrap_or(false);

        if is_dir && dest.starts_with(src) && !(op == Op::Move && src.parent() == Some(dest)) {
            r.errors.push(OpError::IntoItself(name));
            continue;
        }
        let same_dir = src.parent() == Some(dest);
        if op == Op::Move && same_dir {
            r.created.push(name); // mover al mismo sitio: no-op
            r.done += 1;
            continue;
        }
        let target_name = unique_name(dest, &name, is_dir, (op == Op::Copy && same_dir).then_some(copy_label));
        let target = dest.join(&target_name);

        let result = match op {
            Op::Copy => copy_recursive(src, &target),
            Op::Move => match fs::rename(src, &target) {
                Err(e) if e.raw_os_error() == Some(EXDEV) => {
                    // Otro sistema de ficheros: copiar y borrar el original.
                    copy_recursive(src, &target).and_then(|_| remove_any(src))
                }
                other => other,
            },
            Op::Trash | Op::Delete => unreachable!(),
        };
        match result {
            Ok(()) => {
                r.created.push(target_name);
                r.pairs.push((src.clone(), target));
                r.done += 1;
            }
            Err(e) => r.errors.push(OpError::Io(name, e)),
        }
    }
    r
}

pub fn trash(paths: &[PathBuf]) -> Report {
    let mut r = Report::default();
    for p in paths {
        match trash::delete(p) {
            Ok(()) => {
                r.trashed.push(p.clone());
                r.done += 1;
            }
            Err(e) => r.errors.push(OpError::Trash(file_name(p), e.to_string())),
        }
    }
    r
}

pub fn delete(paths: &[PathBuf]) -> Report {
    let mut r = Report::default();
    for p in paths {
        match remove_any(p) {
            Ok(()) => r.done += 1,
            Err(e) => r.errors.push(OpError::Io(file_name(p), e)),
        }
    }
    r
}

#[derive(Debug)]
pub enum NameError {
    Invalid,
    Exists(String),
    Io(io::Error),
}

pub fn rename(dir: &Path, from: &str, to: &str) -> Result<(), NameError> {
    if !validate_name(to) {
        return Err(NameError::Invalid);
    }
    if from == to {
        return Ok(());
    }
    if exists(&dir.join(to)) {
        return Err(NameError::Exists(to.to_owned()));
    }
    fs::rename(dir.join(from), dir.join(to)).map_err(NameError::Io)
}

pub fn mkdir(dir: &Path, base: &str) -> Result<String, NameError> {
    let name = unique_name(dir, base, true, None);
    fs::create_dir(dir.join(&name)).map(|_| name).map_err(NameError::Io)
}

// ------------------------------------------------------------ portapapeles ---

/// Interpreta el texto del portapapeles como lista de ficheros:
/// rutas absolutas, `file://` URIs, o el formato de Nautilus/GNOME (`copy|cut` + URIs).
/// Devuelve `None` si no es una lista de ficheros existentes.
pub fn parse_clipboard(text: &str) -> Option<(Vec<PathBuf>, Op)> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).peekable();
    if lines.peek() == Some(&"x-special/nautilus-clipboard") {
        lines.next();
    }
    let op = match lines.peek() {
        Some(&"cut") => {
            lines.next();
            Op::Move
        }
        Some(&"copy") => {
            lines.next();
            Op::Copy
        }
        _ => Op::Copy,
    };
    let paths: Vec<PathBuf> = lines
        .map(|l| {
            if let Some(rest) = l.strip_prefix("file://") {
                let rest = rest.strip_prefix("localhost").unwrap_or(rest);
                rest.starts_with('/').then(|| percent_decode(rest))
            } else if l.starts_with('/') {
                Some(PathBuf::from(l))
            } else {
                None
            }
        })
        .collect::<Option<_>>()?;
    (!paths.is_empty() && paths.iter().all(|p| exists(p))).then_some((paths, op))
}

fn percent_decode(s: &str) -> PathBuf {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    PathBuf::from(OsString::from_vec(out))
}

// ------------------------------------------------------------------ tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "fex-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
        fn file(&self, rel: &str, body: &str) -> PathBuf {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, body).unwrap();
            p
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn names() {
        let t = Tmp::new();
        t.file("a.txt", "");
        t.file("a (copia).txt", "");
        t.file("r.tar.gz", "");
        assert_eq!(unique_name(&t.0, "a.txt", false, Some("copia")), "a (copia 2).txt");
        assert_eq!(unique_name(&t.0, "a.txt", false, None), "a (2).txt");
        assert_eq!(unique_name(&t.0, "b.txt", false, None), "b.txt");
        assert_eq!(unique_name(&t.0, "r.tar.gz", false, None), "r (2).tar.gz");
        assert_eq!(unique_name(&t.0, ".bashrc", false, Some("copy")), ".bashrc (copy)");
        assert_eq!(stem_len("foto.final.jpg", false), "foto.final".len());
        assert_eq!(stem_len("v1.2", true), 4);
        assert!(!validate_name("a/b") && !validate_name("..") && !validate_name(""));
    }

    #[test]
    fn copy_same_dir_and_recursive() {
        let t = Tmp::new();
        t.file("d/x.txt", "hola");
        t.file("d/sub/y.txt", "y");
        std::os::unix::fs::symlink("x.txt", t.0.join("d/link")).unwrap();
        let r = paste(&[t.0.join("d")], &t.0, Op::Copy, "copia");
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.created, ["d (copia)"]);
        let c = t.0.join("d (copia)");
        assert_eq!(fs::read_to_string(c.join("x.txt")).unwrap(), "hola");
        assert_eq!(fs::read_to_string(c.join("sub/y.txt")).unwrap(), "y");
        assert_eq!(fs::read_link(c.join("link")).unwrap(), PathBuf::from("x.txt"));
    }

    #[test]
    fn copy_into_itself_is_rejected() {
        let t = Tmp::new();
        t.file("d/sub/f", "");
        let r = paste(&[t.0.join("d")], &t.0.join("d/sub"), Op::Copy, "copia");
        assert!(matches!(r.errors.as_slice(), [OpError::IntoItself(n)] if n == "d"));
        assert!(!t.0.join("d/sub/d").exists());
    }

    #[test]
    fn move_with_conflict_never_overwrites() {
        let t = Tmp::new();
        let src = t.file("a/n.txt", "nuevo");
        t.file("b/n.txt", "viejo");
        let r = paste(&[src.clone()], &t.0.join("b"), Op::Move, "copia");
        assert_eq!(r.created, ["n (2).txt"]);
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(t.0.join("b/n.txt")).unwrap(), "viejo");
        assert_eq!(fs::read_to_string(t.0.join("b/n (2).txt")).unwrap(), "nuevo");
    }

    #[test]
    fn rename_and_mkdir() {
        let t = Tmp::new();
        t.file("a", "");
        t.file("b", "");
        assert!(matches!(rename(&t.0, "a", "b"), Err(NameError::Exists(_))));
        assert!(matches!(rename(&t.0, "a", "x/y"), Err(NameError::Invalid)));
        rename(&t.0, "a", "c").unwrap();
        assert!(t.0.join("c").exists());
        assert_eq!(mkdir(&t.0, "Nueva carpeta").unwrap(), "Nueva carpeta");
        assert_eq!(mkdir(&t.0, "Nueva carpeta").unwrap(), "Nueva carpeta (2)");
    }

    #[test]
    fn delete_does_not_follow_symlinks() {
        let t = Tmp::new();
        t.file("keep/f", "x");
        std::os::unix::fs::symlink(t.0.join("keep"), t.0.join("l")).unwrap();
        let r = delete(&[t.0.join("l")]);
        assert!(r.errors.is_empty());
        assert!(t.0.join("keep/f").exists());
    }

    #[test]
    fn clipboard_formats() {
        let t = Tmp::new();
        let a = t.file("con espacio.txt", "");
        let uri = format!("file://{}", a.display().to_string().replace(' ', "%20"));
        assert_eq!(parse_clipboard(&uri).unwrap().0, [a.clone()]);
        let gnome = format!("x-special/nautilus-clipboard\ncut\n{uri}\n");
        assert_eq!(parse_clipboard(&gnome).unwrap().1, Op::Move);
        assert_eq!(parse_clipboard(&a.display().to_string()).unwrap().1, Op::Copy);
        assert!(parse_clipboard("hola mundo").is_none());
        assert!(parse_clipboard("/no/existe/fex").is_none());
    }
}
