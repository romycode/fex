//! Operaciones de ficheros. Todo bloqueante: se llama siempre desde un hilo de trabajo.
//!
//! Política: nunca se sobrescribe nada. Si el destino existe, se genera un nombre libre
//! ("x (copia).txt", "x (2).txt").

use std::{
    ffi::{OsStr, OsString},
    fs, io,
    os::unix::{ffi::OsStringExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

/// Progreso compartido entre el hilo de trabajo y la UI: bytes copiados y petición de cancelar.
#[derive(Clone, Default, Debug)]
pub struct Progress {
    pub bytes: Arc<AtomicU64>,
    cancelled: Arc<AtomicBool>,
}

impl Progress {
    pub fn reset(&self) {
        self.bytes.store(0, Ordering::Relaxed);
        self.cancelled.store(false, Ordering::Relaxed);
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    fn add(&self, bytes: u64) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Error `Interrupted` si se pidió cancelar. Se comprueba entre bloques y entre entradas.
    fn check(progress: Option<&Self>) -> io::Result<()> {
        if progress.is_some_and(Self::is_cancelled) {
            Err(io::Error::new(io::ErrorKind::Interrupted, "cancelado"))
        } else {
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
pub struct Trashed {
    pub original: PathBuf,
    pub location: PathBuf,
    pub info: PathBuf,
    pub snapshot: crate::safe_fs::Snapshot,
}
#[cfg(target_os = "macos")]
pub use crate::macos::Trashed;

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

#[derive(Clone, Debug)]
pub struct Retry {
    pub sources: Vec<PathBuf>,
    pub destination: PathBuf,
    pub operation: Op,
}

#[derive(Default, Debug)]
pub struct Report {
    pub created_paths: Vec<PathBuf>,
    pub retry: Option<Retry>,
    /// Nombres resultantes dentro del directorio destino (para seleccionarlos al terminar).
    pub created: Vec<OsString>,
    /// (origen, destino) de cada elemento pegado: lo necesita el deshacer.
    pub pairs: Vec<(PathBuf, PathBuf)>,
    /// Rutas enviadas a la papelera con éxito.
    pub trashed: Vec<Trashed>,
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

/// Copia a un directorio temporal del destino y publica el resultado completo.
#[cfg(test)]
pub fn copy_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    copy_with_progress(src, dst, None)
}

fn copy_with_progress(src: &Path, dst: &Path, progress: Option<&Progress>) -> io::Result<()> {
    let parent = dst.parent().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let source = crate::safe_fs::Snapshot::capture(src)?;
    let pending = tempfile::Builder::new().prefix(".fex-copy-").tempdir_in(parent)?;
    let staged = pending.path().join("content");
    copy_tree(src, &staged, progress)?;
    source.verify()?;
    crate::safe_fs::rename_exclusive(&staged, dst)
}

fn copy_tree(src: &Path, dst: &Path, progress: Option<&Progress>) -> io::Result<()> {
    Progress::check(progress)?;
    let meta = fs::symlink_metadata(src)?;
    if meta.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)
    } else if meta.is_dir() {
        fs::create_dir(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()), progress)?;
        }
        let source = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(src)?;
        let destination = fs::File::open(dst)?;
        copy_metadata(&source, &destination, &meta)
    } else if meta.is_file() {
        let mut input = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(src)?;
        let mut output = fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
        let mut buffer = vec![0; 256 * 1024];
        loop {
            use io::{Read, Write};
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            output.write_all(&buffer[..count])?;
            if let Some(progress) = progress {
                progress.add(count as u64);
                Progress::check(Some(progress))?;
            }
        }
        copy_metadata(&input, &output, &meta)?;
        output.sync_all()
    } else {
        Err(io::Error::new(io::ErrorKind::Unsupported, "tipo de fichero especial"))
    }
}

fn copy_metadata(source: &fs::File, destination: &fs::File, metadata: &fs::Metadata) -> io::Result<()> {
    use xattr::FileExt;
    for name in source.list_xattr()? {
        if let Some(value) = source.get_xattr(&name)? {
            destination.set_xattr(&name, &value)?;
        }
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        // Los descriptores se mantienen abiertos. Foundation conserva ACL con copyfile.
        let result = unsafe {
            libc::fcopyfile(source.as_raw_fd(), destination.as_raw_fd(), std::ptr::null_mut(), libc::COPYFILE_ACL)
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    destination.set_permissions(metadata.permissions())?;
    destination.set_times(fs::FileTimes::new().set_modified(metadata.modified()?))
}

/// Mueve sin reemplazar un destino. La copia entre discos se completa antes de borrar.
pub fn move_exclusive(src: &Path, dst: &Path) -> io::Result<()> {
    move_with_progress(src, dst, None)
}

fn move_with_progress(src: &Path, dst: &Path, progress: Option<&Progress>) -> io::Result<()> {
    match crate::safe_fs::rename_exclusive(src, dst) {
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            let source = crate::safe_fs::Snapshot::capture(src)?;
            copy_with_progress(src, dst, progress)?;
            source.verify().map_err(|error| {
                io::Error::other(format!("{}; copia completa en {}; origen conservado", error, dst.display()))
            })?;
            source.remove_verified().map_err(|error| {
                io::Error::other(format!(
                    "{}; copia completa en {}; eliminación del origen incompleta",
                    error,
                    dst.display()
                ))
            })
        }
        result => result,
    }
}

pub fn remove_any(p: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(p)?;
    if meta.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) }
}

/// Copia o mueve `srcs` dentro de `dest`.
#[cfg(test)]
pub fn paste(srcs: &[PathBuf], dest: &Path, op: Op, copy_label: &str) -> Report {
    paste_progress(srcs, dest, op, copy_label, None)
}

pub fn paste_progress(srcs: &[PathBuf], dest: &Path, op: Op, copy_label: &str, progress: Option<&Progress>) -> Report {
    let mut r = Report::default();
    let mut retry_sources = Vec::new();
    for (index, src) in srcs.iter().enumerate() {
        if progress.is_some_and(Progress::is_cancelled) {
            // Lo que no se llegó a empezar queda pendiente para reintentar.
            retry_sources.extend(srcs[index..].iter().cloned());
            r.errors.push(OpError::Io(file_name(src), io::Error::new(io::ErrorKind::Interrupted, "cancelado")));
            break;
        }
        let name = file_name(src);
        let is_dir = fs::symlink_metadata(src).map(|m| m.is_dir()).unwrap_or(false);

        let canonical_source = fs::canonicalize(src).unwrap_or_else(|_| src.clone());
        let canonical_dest = fs::canonicalize(dest).unwrap_or_else(|_| dest.to_path_buf());
        if is_dir && canonical_dest.starts_with(&canonical_source) && !(op == Op::Move && src.parent() == Some(dest)) {
            r.errors.push(OpError::IntoItself(name));
            continue;
        }
        let same_dir = src.parent() == Some(dest);
        if op == Op::Move && same_dir {
            r.created.push(src.file_name().unwrap_or_default().to_os_string()); // mover al mismo sitio: no-op
            r.done += 1;
            continue;
        }
        // El nombre original se conserva incluso cuando no es UTF-8.
        let mut target = dest.join(src.file_name().unwrap_or_default());
        if op == Op::Copy && same_dir {
            target = dest.join(unique_name(dest, &name, is_dir, Some(copy_label)));
        }
        let result = loop {
            let result = match op {
                Op::Copy => copy_with_progress(src, &target, progress),
                Op::Move => move_with_progress(src, &target, progress),
                Op::Trash | Op::Delete => unreachable!(),
            };
            if result.as_ref().is_err_and(|error| error.kind() == io::ErrorKind::AlreadyExists) {
                target = dest.join(unique_name(dest, &name, is_dir, None));
                continue;
            }
            break result;
        };
        match result {
            Ok(()) => {
                r.created.push(target.file_name().unwrap_or_default().to_os_string());
                r.created_paths.push(target.clone());
                r.pairs.push((src.clone(), target));
                r.done += 1;
            }
            Err(e) => {
                if !exists(&target) && exists(src) {
                    retry_sources.push(src.clone());
                }
                r.errors.push(OpError::Io(name, e));
            }
        }
    }
    if !retry_sources.is_empty() {
        r.retry = Some(Retry { sources: retry_sources, destination: dest.to_path_buf(), operation: op });
    }
    r
}

pub fn trash(paths: &[PathBuf]) -> Report {
    let mut r = Report::default();
    for p in paths {
        #[cfg(target_os = "linux")]
        let moved = trash_linux(p);
        #[cfg(target_os = "macos")]
        let moved = crate::macos::move_to_trash(p);
        match moved {
            Ok(item) => {
                r.trashed.extend(item);
                r.done += 1;
            }
            Err(error) => r.errors.push(OpError::Trash(file_name(p), error)),
        }
    }
    r
}

#[cfg(target_os = "linux")]
fn trash_linux(path: &Path) -> Result<Option<Trashed>, String> {
    use std::os::unix::fs::MetadataExt;
    let original = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    trash::delete(path).map_err(|error| error.to_string())?;
    let items = trash::os_limited::list().map_err(|error| error.to_string())?;
    for item in items.into_iter().filter(|item| item.original_path() == path) {
        let info = PathBuf::from(&item.id);
        let Some(root) = info.parent().and_then(Path::parent) else { continue };
        let Some(name) = info.file_stem() else { continue };
        let location = root.join("files").join(name);
        let Ok(metadata) = fs::symlink_metadata(&location) else { continue };
        if metadata.dev() == original.dev() && metadata.ino() == original.ino() {
            let snapshot = crate::safe_fs::Snapshot::capture(&location).map_err(|error| error.to_string())?;
            return Ok(Some(Trashed { original: path.to_path_buf(), location, info, snapshot }));
        }
    }
    Ok(None)
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

pub fn rename(dir: &Path, from: &OsStr, to: &str) -> Result<(), NameError> {
    if !validate_name(to) {
        return Err(NameError::Invalid);
    }
    if from == OsStr::new(to) {
        return Ok(());
    }
    if exists(&dir.join(to)) {
        return Err(NameError::Exists(to.to_owned()));
    }
    crate::safe_fs::rename_exclusive(&dir.join(from), &dir.join(to)).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            NameError::Exists(to.to_owned())
        } else {
            NameError::Io(error)
        }
    })
}

pub fn mkdir(dir: &Path, base: &str) -> Result<OsString, NameError> {
    let name = unique_name(dir, base, true, None);
    fs::create_dir(dir.join(&name)).map(|_| name.into()).map_err(NameError::Io)
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
        assert!(matches!(rename(&t.0, OsStr::new("a"), "b"), Err(NameError::Exists(_))));
        assert!(matches!(rename(&t.0, OsStr::new("a"), "x/y"), Err(NameError::Invalid)));
        rename(&t.0, OsStr::new("a"), "c").unwrap();
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
    fn concurrent_pastes_publish_distinct_complete_files() {
        let t = Tmp::new();
        let source = t.file("source/item", "complete payload");
        let destination = t.0.join("target");
        fs::create_dir(&destination).unwrap();
        let workers = (0..4)
            .map(|_| {
                let source = source.clone();
                let destination = destination.clone();
                std::thread::spawn(move || paste(&[source], &destination, Op::Copy, "copy"))
            })
            .collect::<Vec<_>>();
        let mut created = Vec::new();
        for worker in workers {
            let report = worker.join().unwrap();
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            created.extend(report.created);
        }
        created.sort();
        created.dedup();
        assert_eq!(created.len(), 4);
        for name in created {
            assert_eq!(fs::read_to_string(destination.join(name)).unwrap(), "complete payload");
        }
    }

    #[test]
    fn failed_recursive_copy_keeps_source_and_hides_partial_destination() {
        let t = Tmp::new();
        t.file("source/file", "original");
        std::os::unix::fs::symlink("missing", t.0.join("source/link")).unwrap();
        let fifo = std::ffi::CString::new(t.0.join("source/pipe").as_os_str().as_encoded_bytes()).unwrap();
        // Ruta válida terminada en NUL, sin referencias retenidas por mkfifo.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(copy_recursive(&t.0.join("source"), &t.0.join("copy")).is_err());
        assert!(!exists(&t.0.join("copy")));
        assert!(
            !fs::read_dir(&t.0).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .as_encoded_bytes()
                .starts_with(b".fex-copy-"))
        );
        assert_eq!(fs::read_to_string(t.0.join("source/file")).unwrap(), "original");
    }

    #[test]
    fn copying_through_a_symlink_into_itself_is_rejected() {
        let t = Tmp::new();
        t.file("directory/sub/file", "original");
        std::os::unix::fs::symlink(t.0.join("directory/sub"), t.0.join("alias")).unwrap();
        let report = paste(&[t.0.join("directory")], &t.0.join("alias"), Op::Copy, "copy");
        assert!(matches!(report.errors.as_slice(), [OpError::IntoItself(_)]));
    }

    #[test]
    fn copy_preserves_original_names() {
        let t = Tmp::new();
        #[cfg(target_os = "linux")]
        let name = OsString::from_vec(b"file-\xff".to_vec());
        #[cfg(target_os = "macos")]
        let name = OsString::from("file-文件");
        let source = t.0.join("source").join(&name);
        fs::create_dir(t.0.join("source")).unwrap();
        fs::create_dir(t.0.join("target")).unwrap();
        fs::write(&source, "data").unwrap();
        let report = paste(&[source], &t.0.join("target"), Op::Copy, "copy");
        assert!(report.errors.is_empty());
        assert_eq!(report.created, [name.clone()]);
        assert_eq!(fs::read_to_string(t.0.join("target").join(name)).unwrap(), "data");
    }

    #[test]
    fn failed_items_can_be_retried_after_destination_is_fixed() {
        let t = Tmp::new();
        let source = t.file("source/item", "original");
        let destination = t.0.join("not-yet-created");
        let report = paste(&[source.clone()], &destination, Op::Move, "copy");
        assert_eq!(report.done, 0);
        assert!(source.exists());
        let retry = report.retry.unwrap();
        fs::create_dir(&destination).unwrap();
        let report = paste(&retry.sources, &retry.destination, retry.operation, "copy");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(fs::read_to_string(destination.join("item")).unwrap(), "original");
        assert!(!source.exists());
    }

    #[test]
    fn cancelled_paste_publishes_nothing_and_keeps_everything_for_retry() {
        let t = Tmp::new();
        let first = t.file("source/first", "1");
        let second = t.file("source/second", "2");
        let destination = t.0.join("destination");
        fs::create_dir(&destination).unwrap();
        let progress = Progress::default();
        progress.cancel();
        let report = paste_progress(&[first.clone(), second.clone()], &destination, Op::Move, "copy", Some(&progress));
        assert_eq!(report.done, 0);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.retry.unwrap().sources, [first.clone(), second.clone()]);
        assert!(first.exists() && second.exists());
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);

        // Cancelar a mitad de una copia descarta el temporal y no publica nada.
        let progress = Progress::default();
        let big = t.file("source/big", &"x".repeat(1024 * 1024));
        progress.cancel();
        let error = copy_with_progress(&big, &destination.join("big"), Some(&progress)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
        progress.reset();
        assert!(!progress.is_cancelled());
        copy_with_progress(&big, &destination.join("big"), Some(&progress)).unwrap();
        assert_eq!(progress.bytes.load(std::sync::atomic::Ordering::Relaxed), 1024 * 1024);
    }

    #[test]
    fn copy_preserves_user_attributes_and_modified_time() {
        use xattr::FileExt;
        let t = Tmp::new();
        let source = t.file("source", "data");
        let source_file = fs::File::open(&source).unwrap();
        source_file.set_xattr("user.fex-test", b"attribute").unwrap();
        let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        source_file.set_times(fs::FileTimes::new().set_modified(modified)).unwrap();
        let destination = t.0.join("copied");
        copy_recursive(&source, &destination).unwrap();
        let copied = fs::File::open(destination).unwrap();
        assert_eq!(copied.get_xattr("user.fex-test").unwrap(), Some(b"attribute".to_vec()));
        assert_eq!(copied.metadata().unwrap().modified().unwrap(), modified);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn move_between_filesystems_keeps_contents_and_removes_only_original() {
        use std::os::unix::fs::MetadataExt;
        let source_dir = tempfile::tempdir().unwrap();
        let destination_dir = tempfile::tempdir_in("/dev/shm").unwrap();
        if source_dir.path().metadata().unwrap().dev() == destination_dir.path().metadata().unwrap().dev() {
            return;
        }
        let source = source_dir.path().join("tree");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("data"), "cross-device data").unwrap();
        let report = paste(&[source.clone()], destination_dir.path(), Op::Move, "copy");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(fs::read_to_string(destination_dir.path().join("tree/data")).unwrap(), "cross-device data");
        assert!(!source.exists());
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
