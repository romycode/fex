//! Backends (sistema de ficheros local, Amazon S3) detrás de una interfaz común.
//!
//! - [`Location`]: dónde está el usuario (un directorio, un prefijo de S3…).
//! - [`Item`]: algo concreto sobre lo que actuar (un fichero, una carpeta, un objeto…).
//! - [`Capabilities`]: qué sabe hacer cada backend. La interfaz se adapta sola.
//! - Operaciones ([`paste`], [`delete`], [`rename`], [`mkdir`]): eligen el motor según el
//!   origen y el destino. Todas devuelven un [`Report`] y se ejecutan en un hilo de trabajo.
//!
//! Para añadir un backend: una variante en `Location` e `Item`, sus capacidades y los
//! brazos que falten en estos `match`. El compilador señala todos los sitios.

use std::{
    ffi::{OsStr, OsString},
    io,
    path::PathBuf,
};

use crate::{
    fs::{self, Entry},
    ops::{self, Op, OpError, Progress, Report},
    s3,
    undo::{Undo, UndoKind},
};

/// Each backend retains its own location and entry identifiers.
#[derive(Clone, PartialEq, Eq)]
pub enum Location {
    Local(PathBuf),
    S3(s3::Location),
}

/// Un elemento concreto de algún backend: lo que se copia, mueve o borra.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Item {
    Local(PathBuf),
    S3(s3::Object),
}

impl Item {
    pub fn name(&self) -> String {
        match self {
            Self::Local(path) => ops::file_name(path),
            Self::S3(object) => object.name().to_owned(),
        }
    }

    /// Texto para el portapapeles del sistema: rutas o URIs `s3://`.
    pub fn to_clipboard(&self) -> String {
        match self {
            Self::Local(path) => path.to_string_lossy().into_owned(),
            Self::S3(object) => object.uri(),
        }
    }
}

/// Qué sabe hacer un backend. Los comandos consultan esto en vez de preguntar
/// "¿es local?", así un backend nuevo solo declara lo que soporta.
#[derive(Clone, Copy, Default)]
pub struct Capabilities {
    /// Pegar, renombrar, borrar y crear carpetas aquí.
    pub write: bool,
    /// Mover a la papelera (si no, borrar es permanente).
    pub trash: bool,
    /// Abrir ficheros con la aplicación del sistema.
    pub open_files: bool,
    /// Descargar entradas a un directorio local elegido.
    pub download: bool,
    /// Detectar cambios y refrescar solo.
    pub watch: bool,
    /// Tiene ficheros ocultos que alternar.
    pub hidden_files: bool,
    /// Rellenar tamaño y fecha en una segunda pasada.
    pub stat_pass: bool,
    /// Se puede fijar en la barra lateral.
    pub pin: bool,
}

impl Location {
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    pub fn capabilities(&self) -> Capabilities {
        match self {
            Self::Local(_) => Capabilities {
                write: true,
                trash: true,
                open_files: true,
                download: false,
                watch: true,
                hidden_files: true,
                stat_pass: true,
                pin: true,
            },
            Self::S3(s3::Location::Prefix { .. }) => Capabilities { write: true, download: true, ..Default::default() },
            Self::S3(_) => Capabilities::default(),
        }
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

    /// Nombre editable de una entrada (sin prefijo ni barra final).
    pub fn entry_name(&self, entry: &Entry) -> String {
        let label = self.entry_label(entry);
        match self {
            Self::S3(_) => label.trim_end_matches('/').to_owned(),
            Self::Local(_) => label,
        }
    }

    /// El elemento que representa una entrada del listado. `None` para buckets.
    pub fn item(&self, entry: &Entry) -> Option<Item> {
        match self {
            Self::Local(path) => Some(Item::Local(path.join(&entry.name))),
            Self::S3(location) => {
                let mut object = location.as_object()?;
                object.key = entry.name.to_str()?.to_owned();
                Some(Item::S3(object))
            }
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

// ------------------------------------------------------------ operaciones ---

fn unsupported(what: &str) -> Report {
    Report {
        errors: vec![OpError::Io(what.to_owned(), io::Error::new(io::ErrorKind::Unsupported, "no se puede escribir aquí"))],
        ..Report::default()
    }
}

/// Separa los elementos por backend, conservando el orden dentro de cada grupo.
fn split(items: Vec<Item>) -> (Vec<PathBuf>, Vec<s3::Object>) {
    let mut local = Vec::new();
    let mut remote = Vec::new();
    for item in items {
        match item {
            Item::Local(path) => local.push(path),
            Item::S3(object) => remote.push(object),
        }
    }
    (local, remote)
}

fn merge(mut a: Report, b: Report) -> Report {
    a.created.extend(b.created);
    a.created_paths.extend(b.created_paths);
    a.pairs.extend(b.pairs);
    a.trashed.extend(b.trashed);
    a.s3_created.extend(b.s3_created);
    a.s3_moved.extend(b.s3_moved);
    a.done += b.done;
    a.errors.extend(b.errors);
    a.retry = a.retry.or(b.retry);
    a
}

/// Copia o mueve `items` (de cualquier backend) a `dest`.
pub async fn paste(items: Vec<Item>, dest: Location, op: Op, label: &'static str, progress: Progress) -> Report {
    let (local, remote) = split(items);
    let mut report = Report::default();
    match &dest {
        Location::Local(dir) => {
            if !local.is_empty() {
                report = merge(report, ops::paste_progress(&local, dir, op, label, Some(&progress)));
            }
            if !remote.is_empty() {
                report = merge(report, s3::download(remote, dir.clone(), op, progress.clone()).await);
            }
        }
        Location::S3(location) => {
            let Some(prefix) = location.as_object() else { return unsupported(&location.label()) };
            if !local.is_empty() {
                report = merge(report, s3::upload(local, prefix.clone(), op, progress.clone()).await);
            }
            if !remote.is_empty() {
                report = merge(report, s3::copy(remote, prefix, op, label, progress).await);
            }
        }
    }
    report
}

/// Borrado permanente.
pub async fn delete(items: Vec<Item>, progress: Progress) -> Report {
    let (local, remote) = split(items);
    let mut report = Report::default();
    if !local.is_empty() {
        report = merge(report, ops::delete(&local));
    }
    if !remote.is_empty() {
        report = merge(report, s3::delete(remote, progress).await);
    }
    report
}

/// Renombrar en el sitio. Local es un único `rename(2)`; S3 copia y borra.
pub async fn rename(item: Item, new_name: String) -> Report {
    match item {
        Item::Local(path) => {
            let (Some(dir), Some(from)) = (path.parent(), path.file_name()) else { return unsupported(&ops::file_name(&path)) };
            match ops::rename(dir, from, &new_name) {
                Ok(()) => Report {
                    done: 1,
                    created: vec![new_name.clone().into()],
                    created_paths: vec![dir.join(&new_name)],
                    pairs: vec![(path.clone(), dir.join(&new_name))],
                    ..Report::default()
                },
                Err(ops::NameError::Exists(name)) => Report { errors: vec![OpError::Exists(name)], ..Report::default() },
                Err(ops::NameError::Invalid) => Report {
                    errors: vec![OpError::Io(new_name, io::Error::new(io::ErrorKind::InvalidInput, "nombre no válido"))],
                    ..Report::default()
                },
                Err(ops::NameError::Io(error)) => {
                    Report { errors: vec![OpError::Io(ops::file_name(&path), error)], ..Report::default() }
                }
            }
        }
        Item::S3(object) => s3::rename(object, new_name).await,
    }
}

/// Nueva carpeta con un nombre libre. `created` lleva el nombre/clave elegido.
pub async fn mkdir(dest: Location, base: &'static str) -> Report {
    match dest {
        Location::Local(dir) => match ops::mkdir(&dir, base) {
            Ok(name) => Report {
                done: 1,
                created_paths: vec![dir.join(&name)],
                created: vec![name],
                ..Report::default()
            },
            Err(ops::NameError::Io(error)) => Report { errors: vec![OpError::Io(base.into(), error)], ..Report::default() },
            Err(_) => unsupported(base),
        },
        Location::S3(location) => match location.as_object() {
            Some(prefix) => s3::mkdir(prefix, base).await,
            None => unsupported(&location.label()),
        },
    }
}

/// Qué apilar en "deshacer" tras pegar. Mover entre backends no se puede deshacer: el
/// origen ya no existe en el mismo sitio.
pub fn paste_undo(report: &Report, op: Op) -> Option<Undo> {
    if !report.s3_created.is_empty() {
        return Some(Undo::S3 { kind: UndoKind::Copy, change: s3::Change::Created(report.s3_created.clone()) });
    }
    if !report.s3_moved.is_empty() {
        return Some(Undo::S3 { kind: UndoKind::Move, change: s3::Change::Moved(report.s3_moved.clone()) });
    }
    match op {
        // Copias locales y descargas: deshacer = mandar lo creado a la papelera.
        Op::Copy if !report.created_paths.is_empty() => Some(Undo::Copy { created: report.created_paths.clone() }),
        Op::Move if !report.pairs.is_empty() => Some(Undo::Move { pairs: report.pairs.clone() }),
        _ => None,
    }
}
