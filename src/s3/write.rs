//! Escritura en S3: subir, descargar carpetas, copiar, mover, renombrar, borrar y deshacer.
//!
//! Misma política que en local: **nunca se sobrescribe nada**.
//! - Toda escritura usa `If-None-Match: *`: si el destino ya existe, S3 la rechaza.
//! - Toda copia de un origen S3 usa `x-amz-copy-source-if-match` con el ETag leído.
//! - Todo borrado usa `If-Match` con el ETag leído: si alguien cambió el objeto, no se borra.
//! - Si falla a mitad de un elemento, se borra lo que ya se había creado de ese elemento
//!   (también con `If-Match`), así no quedan carpetas a medias.
//!
//! S3 no tiene renombrar ni mover: se copia y después se borra el origen.

use std::{
    collections::HashSet,
    future::Future,
    io,
    path::{Path, PathBuf},
};

use aws_sdk_s3::{
    Client,
    error::{ProvideErrorMetadata, SdkError},
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart, EncodingType},
};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use tokio::{io::AsyncReadExt, task::JoinSet};

use super::{S3, decode_key, sdk_error, service};
use crate::{
    ops::{self, Op, OpError, Progress, Report},
    safe_fs::Snapshot,
};

/// Por encima de esto, subida multiparte (y lectura por partes, sin cargar el fichero entero).
const SINGLE_PUT_LIMIT: u64 = 16 * 1024 * 1024;
/// Límite de CopyObject; por encima, copia multiparte.
const SINGLE_COPY_LIMIT: u64 = 5 * 1024 * 1024 * 1024;
const MAX_PARTS: u64 = 10_000;
const MIN_PART: u64 = 16 * 1024 * 1024;
const COPY_PART: u64 = 512 * 1024 * 1024;

/// Un objeto o un "directorio" (prefijo terminado en `/`) de S3.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Object {
    pub profile: Option<String>,
    pub bucket: String,
    pub key: String,
}

impl Object {
    pub fn is_prefix(&self) -> bool {
        self.key.is_empty() || self.key.ends_with('/')
    }

    /// Último segmento, sin la barra final.
    pub fn name(&self) -> &str {
        let trimmed = self.key.strip_suffix('/').unwrap_or(&self.key);
        trimmed.rsplit('/').next().unwrap_or(trimmed)
    }

    /// Prefijo que lo contiene (con barra final, o vacío en la raíz del bucket).
    pub fn parent_prefix(&self) -> &str {
        let trimmed = self.key.strip_suffix('/').unwrap_or(&self.key);
        trimmed.rfind('/').map_or("", |i| &self.key[..=i])
    }

    fn with_key(&self, key: String) -> Self {
        Self { key, ..self.clone() }
    }

    pub fn uri(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.key)
    }
}

/// Una versión concreta de un objeto: lo que deshacer puede tocar sin riesgo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    pub object: Object,
    pub etag: String,
}

/// Inversa de una escritura en S3 (para la pila de deshacer).
#[derive(Clone, Debug)]
pub enum Change {
    /// Borrar lo creado, si sigue igual.
    Created(Vec<Stored>),
    /// Devolver cada objeto a su clave original, si la original sigue libre.
    Moved(Vec<(Object, Stored)>),
}

/// Objeto listado con lo necesario para copiarlo o borrarlo de forma segura.
#[derive(Clone, Debug)]
struct Listed {
    key: String,
    etag: String,
    size: u64,
}

// ------------------------------------------------------------- runtime ---

fn failed(name: &str, error: io::Error) -> Report {
    Report { errors: vec![OpError::Io(name.to_owned(), error)], ..Report::default() }
}

/// Ejecuta en el runtime de tokio del servicio. Se puede esperar desde cualquier executor.
async fn run<F, Fut>(work: F) -> Report
where
    F: FnOnce(&'static S3) -> Fut,
    Fut: Future<Output = Report> + Send + 'static,
{
    let service = match service() {
        Ok(service) => service,
        Err(error) => return failed("Amazon S3", error),
    };
    let mut task = JoinSet::new();
    task.spawn_on(work(service), service.runtime.handle());
    task.join_next().await.expect("una tarea S3").unwrap_or_else(|error| failed("Amazon S3", io::Error::other(error)))
}

/// Código de error de S3 (`PreconditionFailed`, `NoSuchKey`…).
fn code<E: ProvideErrorMetadata, R>(error: &SdkError<E, R>) -> Option<&str> {
    error.as_service_error().and_then(ProvideErrorMetadata::code)
}

/// Error de una escritura condicional: "ya existe" se traduce a `AlreadyExists`.
fn write_error<E: ProvideErrorMetadata + std::error::Error + 'static, R>(error: SdkError<E, R>) -> io::Error {
    match code(&error) {
        Some("PreconditionFailed" | "ConditionalRequestConflict") => io::ErrorKind::AlreadyExists.into(),
        _ => sdk_error(error),
    }
}

/// Error de una lectura o borrado condicional: el origen cambió desde que se leyó.
fn source_error<E: ProvideErrorMetadata + std::error::Error + 'static, R>(error: SdkError<E, R>) -> io::Error {
    match code(&error) {
        Some("PreconditionFailed") => io::Error::other("ha cambiado en S3 desde que se leyó; no se toca"),
        Some("NoSuchKey" | "NotFound") => io::ErrorKind::NotFound.into(),
        _ => sdk_error(error),
    }
}

// ------------------------------------------------------------ primitivas ---

const KEY: &AsciiSet = &NON_ALPHANUMERIC.remove(b'/').remove(b'-').remove(b'_').remove(b'.').remove(b'~');

fn copy_source(bucket: &str, key: &str) -> String {
    format!("{bucket}/{}", utf8_percent_encode(key, KEY))
}

/// Tamaño de parte para que `size` quepa en el máximo de partes de S3.
fn part_size(size: u64, minimum: u64) -> u64 {
    minimum.max(size.div_ceil(MAX_PARTS))
}

/// Todos los objetos bajo `prefix` (sin delimitador). Para un objeto suelto, solo él.
async fn list_tree(client: &Client, object: &Object) -> io::Result<Vec<Listed>> {
    if !object.is_prefix() {
        let head = client.head_object().bucket(&object.bucket).key(&object.key).send().await.map_err(source_error)?;
        return Ok(vec![Listed {
            key: object.key.clone(),
            etag: head.e_tag().unwrap_or_default().to_owned(),
            size: head.content_length().and_then(|n| u64::try_from(n).ok()).unwrap_or(0),
        }]);
    }
    let mut out = Vec::new();
    let mut pages = client
        .list_objects_v2()
        .bucket(&object.bucket)
        .prefix(&object.key)
        .encoding_type(EncodingType::Url)
        .into_paginator()
        .send();
    while let Some(page) = pages.next().await {
        let page = page.map_err(sdk_error)?;
        let encoded = page.encoding_type() == Some(&EncodingType::Url);
        for item in page.contents() {
            let Some(raw) = item.key() else { continue };
            out.push(Listed {
                key: decode_key(raw, encoded)?,
                etag: item.e_tag().unwrap_or_default().to_owned(),
                size: item.size().and_then(|n| u64::try_from(n).ok()).unwrap_or(0),
            });
        }
    }
    Ok(out)
}

/// Nombres ocupados directamente bajo `prefix` (ficheros y carpetas, sin barra final).
async fn occupied(client: &Client, bucket: &str, prefix: &str) -> io::Result<HashSet<String>> {
    let mut names = HashSet::new();
    let mut pages = client
        .list_objects_v2()
        .bucket(bucket)
        .prefix(prefix)
        .delimiter("/")
        .encoding_type(EncodingType::Url)
        .into_paginator()
        .send();
    while let Some(page) = pages.next().await {
        let page = page.map_err(sdk_error)?;
        let encoded = page.encoding_type() == Some(&EncodingType::Url);
        let keys = page
            .common_prefixes()
            .iter()
            .filter_map(|p| p.prefix())
            .chain(page.contents().iter().filter_map(|o| o.key()));
        for raw in keys {
            let key = decode_key(raw, encoded)?;
            let name = key.strip_prefix(prefix).unwrap_or(&key).trim_end_matches('/');
            if !name.is_empty() {
                names.insert(name.to_owned());
            }
        }
    }
    Ok(names)
}

async fn put_bytes(client: &Client, bucket: &str, key: &str, body: Vec<u8>) -> io::Result<String> {
    let output = client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(body))
        .if_none_match("*")
        .send()
        .await
        .map_err(write_error)?;
    Ok(output.e_tag().unwrap_or_default().to_owned())
}

/// Sube un fichero local sin sobrescribir. Grandes: multiparte, leyendo parte a parte.
async fn put_file(client: &Client, bucket: &str, key: &str, path: &Path, progress: &Progress) -> io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let size = file.metadata().await?.len();
    if size <= SINGLE_PUT_LIMIT {
        let mut body = Vec::with_capacity(size as usize);
        file.read_to_end(&mut body).await?;
        let length = body.len() as u64;
        let etag = put_bytes(client, bucket, key, body).await?;
        progress.add(length);
        return Ok(etag);
    }
    let upload = client.create_multipart_upload().bucket(bucket).key(key).send().await.map_err(sdk_error)?;
    let upload_id = upload.upload_id().unwrap_or_default().to_owned();
    let result = async {
        let part = part_size(size, MIN_PART);
        let mut parts = Vec::new();
        let mut number = 1;
        loop {
            Progress::check(Some(progress))?;
            let mut chunk = Vec::with_capacity(part as usize);
            (&mut file).take(part).read_to_end(&mut chunk).await?;
            if chunk.is_empty() {
                break;
            }
            let length = chunk.len() as u64;
            let output = client
                .upload_part()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .part_number(number)
                .body(ByteStream::from(chunk))
                .send()
                .await
                .map_err(sdk_error)?;
            parts.push(CompletedPart::builder().e_tag(output.e_tag().unwrap_or_default()).part_number(number).build());
            progress.add(length);
            number += 1;
        }
        let done = client
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build())
            .if_none_match("*")
            .send()
            .await
            .map_err(write_error)?;
        Ok(done.e_tag().unwrap_or_default().to_owned())
    }
    .await;
    if result.is_err() {
        let _ = client.abort_multipart_upload().bucket(bucket).key(key).upload_id(&upload_id).send().await;
    }
    result
}

/// Copia servidor a servidor, solo si el origen sigue con `source.etag` y el destino está libre.
async fn copy_object(
    client: &Client,
    source_bucket: &str,
    source: &Listed,
    bucket: &str,
    key: &str,
    progress: &Progress,
) -> io::Result<String> {
    let from = copy_source(source_bucket, &source.key);
    if source.size <= SINGLE_COPY_LIMIT {
        let output = client
            .copy_object()
            .bucket(bucket)
            .key(key)
            .copy_source(&from)
            .copy_source_if_match(&source.etag)
            .if_none_match("*")
            .send()
            .await
            .map_err(|error| match code(&error) {
                // El destino ocupado y el origen cambiado dan el mismo 412: se distingue después.
                Some("PreconditionFailed") => io::Error::other("PreconditionFailed"),
                _ => write_error(error),
            })?;
        progress.add(source.size);
        return Ok(output.copy_object_result().and_then(|r| r.e_tag()).unwrap_or_default().to_owned());
    }
    let upload = client.create_multipart_upload().bucket(bucket).key(key).send().await.map_err(sdk_error)?;
    let upload_id = upload.upload_id().unwrap_or_default().to_owned();
    let result = async {
        let part = part_size(source.size, COPY_PART);
        let mut parts = Vec::new();
        let (mut start, mut number) = (0, 1);
        while start < source.size {
            Progress::check(Some(progress))?;
            let end = (start + part).min(source.size) - 1;
            let output = client
                .upload_part_copy()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .part_number(number)
                .copy_source(&from)
                .copy_source_range(format!("bytes={start}-{end}"))
                .copy_source_if_match(&source.etag)
                .send()
                .await
                .map_err(source_error)?;
            let etag = output.copy_part_result().and_then(|r| r.e_tag()).unwrap_or_default();
            parts.push(CompletedPart::builder().e_tag(etag).part_number(number).build());
            progress.add(end + 1 - start);
            start = end + 1;
            number += 1;
        }
        let done = client
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build())
            .if_none_match("*")
            .send()
            .await
            .map_err(write_error)?;
        Ok(done.e_tag().unwrap_or_default().to_owned())
    }
    .await;
    if result.is_err() {
        let _ = client.abort_multipart_upload().bucket(bucket).key(key).upload_id(&upload_id).send().await;
    }
    result
}

/// ¿Por qué falló una copia condicional? Si el destino existe, `AlreadyExists`; si no, cambió el origen.
async fn explain_copy_failure(client: &Client, bucket: &str, key: &str, error: io::Error) -> io::Error {
    if error.to_string() != "PreconditionFailed" {
        return error;
    }
    match client.head_object().bucket(bucket).key(key).send().await {
        Ok(_) => io::ErrorKind::AlreadyExists.into(),
        Err(_) => io::Error::other("el origen ha cambiado en S3 desde que se leyó; no se copia"),
    }
}

async fn delete_object(client: &Client, bucket: &str, key: &str, etag: &str) -> io::Result<()> {
    let mut request = client.delete_object().bucket(bucket).key(key);
    if !etag.is_empty() {
        request = request.if_match(etag);
    }
    request.send().await.map(|_| ()).map_err(source_error)
}

/// Deshace lo creado de un elemento que falló a medias.
async fn rollback(client: &Client, created: &[Stored]) {
    for stored in created.iter().rev() {
        let _ = delete_object(client, &stored.object.bucket, &stored.object.key, &stored.etag).await;
    }
}

// ------------------------------------------------------------ operaciones ---

/// Local → S3. `dest` es un prefijo. Mover = subir, comprobar que el origen no cambió y borrarlo.
pub async fn upload(sources: Vec<PathBuf>, dest: Object, op: Op, progress: Progress) -> Report {
    run(move |service| async move {
        let mut report = Report::default();
        let client = service.bucket_client(&dest.profile, &dest.bucket).await;
        let mut taken = match occupied(&client, &dest.bucket, &dest.key).await {
            Ok(names) => names,
            Err(error) => return failed(&dest.uri(), error),
        };
        for source in &sources {
            let name = ops::file_name(source);
            if progress.is_cancelled() {
                report.errors.push(OpError::Io(name, io::Error::new(io::ErrorKind::Interrupted, "cancelado")));
                break;
            }
            let Some(utf8) = source.file_name().and_then(|n| n.to_str()) else {
                report.errors.push(OpError::Io(name, io::Error::other("S3 solo admite nombres UTF-8")));
                continue;
            };
            let snapshot = match Snapshot::capture(source) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    report.errors.push(OpError::Io(name, error));
                    continue;
                }
            };
            let is_dir = source.is_dir();
            let target = ops::unique_name_by(utf8, is_dir, None, |n| taken.contains(n));
            let key = format!("{}{target}{}", dest.key, if is_dir { "/" } else { "" });
            let mut created = Vec::new();
            let result = upload_tree(&client, &dest, source, &key, &progress, &mut created).await;
            let result = match result {
                Ok(()) if op == Op::Move => snapshot
                    .verify()
                    .and_then(|()| snapshot.remove_verified())
                    .map_err(|error| io::Error::other(format!("{error}; copia completa en {}; origen conservado", dest.with_key(key.clone()).uri()))),
                other => other,
            };
            match result {
                Ok(()) => {
                    taken.insert(target);
                    report.done += 1;
                    report.created.push(key.into());
                    if op == Op::Copy {
                        report.s3_created.extend(created);
                    }
                }
                Err(error) => {
                    // Si la subida terminó, el error es al borrar el origen: la copia se queda.
                    if !error.to_string().contains("copia completa") {
                        rollback(&client, &created).await;
                    }
                    report.errors.push(OpError::Io(name, error));
                }
            }
        }
        report
    })
    .await
}

/// Sube un fichero o una carpeta entera (con marcadores para las carpetas, también vacías).
async fn upload_tree(
    client: &Client,
    dest: &Object,
    path: &Path,
    key: &str,
    progress: &Progress,
    created: &mut Vec<Stored>,
) -> io::Result<()> {
    Progress::check(Some(progress))?;
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_symlink() {
        return Err(io::Error::other("S3 no tiene enlaces simbólicos; no se sube"));
    }
    if meta.is_file() {
        let etag = put_file(client, &dest.bucket, key, path, progress).await?;
        created.push(Stored { object: dest.with_key(key.to_owned()), etag });
        return Ok(());
    }
    if !meta.is_dir() {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "tipo de fichero especial"));
    }
    let etag = put_bytes(client, &dest.bucket, key, Vec::new()).await?;
    created.push(Stored { object: dest.with_key(key.to_owned()), etag });
    let mut children: Vec<_> = std::fs::read_dir(path)?.collect::<io::Result<_>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let name = child.file_name();
        let Some(name) = name.to_str() else {
            return Err(io::Error::other(format!("{}: S3 solo admite nombres UTF-8", child.path().display())));
        };
        let is_dir = child.file_type()?.is_dir();
        let child_key = format!("{key}{name}{}", if is_dir { "/" } else { "" });
        Box::pin(upload_tree(client, dest, &child.path(), &child_key, progress, created)).await?;
    }
    Ok(())
}

/// S3 → local. Las carpetas se montan en un temporal del destino y se publican enteras.
pub async fn download(sources: Vec<Object>, dest: PathBuf, op: Op, progress: Progress) -> Report {
    run(move |service| async move {
        let mut report = Report::default();
        for source in sources {
            let name = source.name().to_owned();
            if progress.is_cancelled() {
                report.errors.push(OpError::Io(name, io::Error::new(io::ErrorKind::Interrupted, "cancelado")));
                break;
            }
            let client = service.bucket_client(&source.profile, &source.bucket).await;
            match download_one(&client, &source, &dest, op, &progress).await {
                Ok(path) => {
                    report.done += 1;
                    report.created.push(path.file_name().unwrap_or_default().to_os_string());
                    report.created_paths.push(path);
                }
                Err(error) => report.errors.push(OpError::Io(name, error)),
            }
        }
        report
    })
    .await
}

async fn download_one(client: &Client, source: &Object, dest: &Path, op: Op, progress: &Progress) -> io::Result<PathBuf> {
    let name = source.name();
    if !ops::validate_name(name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "nombre local no válido"));
    }
    let objects = list_tree(client, source).await?;
    let staging = tempfile::Builder::new().prefix(".fex-download-").tempdir_in(dest)?;
    let staged = staging.path().join("content");
    if source.is_prefix() {
        std::fs::create_dir(&staged)?;
    }
    for object in &objects {
        Progress::check(Some(progress))?;
        let relative = &object.key[source.key.len()..];
        if source.is_prefix() && relative.is_empty() {
            continue; // marcador de la propia carpeta
        }
        let mut path = staged.clone();
        if source.is_prefix() {
            let segments: Vec<&str> = relative.split('/').collect();
            for (i, segment) in segments.iter().enumerate() {
                let last = i + 1 == segments.len();
                if last && segment.is_empty() {
                    break; // marcador de subcarpeta: ya creada
                }
                if !ops::validate_name(segment) {
                    return Err(io::Error::other(format!("{}: nombre local no válido", object.key)));
                }
                path.push(segment);
                if !last && !path.exists() {
                    std::fs::create_dir(&path)?;
                }
            }
            if relative.ends_with('/') {
                std::fs::create_dir_all(&path)?;
                continue;
            }
        }
        fetch(client, &source.bucket, object, &path, progress).await?;
    }
    // Publicar con un nombre libre, como al pegar en local.
    let mut target = dest.join(name);
    loop {
        match crate::safe_fs::rename_exclusive(&staged, &target) {
            Ok(()) => break,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                target = dest.join(ops::unique_name(dest, name, source.is_prefix(), None));
            }
            Err(error) => return Err(error),
        }
    }
    if op == Op::Move {
        for object in objects.iter().rev() {
            delete_object(client, &source.bucket, &object.key, &object.etag).await.map_err(|error| {
                io::Error::other(format!("{error}; copia completa en {}; origen incompleto", target.display()))
            })?;
        }
    }
    Ok(target)
}

/// Descarga un objeto exacto (por ETag) a `path`, que no debe existir.
async fn fetch(client: &Client, bucket: &str, object: &Listed, path: &Path, progress: &Progress) -> io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut request = client.get_object().bucket(bucket).key(&object.key);
    if !object.etag.is_empty() {
        request = request.if_match(&object.etag);
    }
    let mut output = request.send().await.map_err(source_error)?;
    let mut file = tokio::fs::OpenOptions::new().write(true).create_new(true).open(path).await?;
    let mut written = 0;
    while let Some(chunk) = output.body.try_next().await.map_err(io::Error::other)? {
        Progress::check(Some(progress))?;
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;
        progress.add(chunk.len() as u64);
    }
    if written != object.size {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "descarga incompleta"));
    }
    file.flush().await
}

/// S3 → S3, mismo perfil. Copia servidor a servidor; mover = copiar y borrar el origen exacto.
pub async fn copy(sources: Vec<Object>, dest: Object, op: Op, label: &'static str, progress: Progress) -> Report {
    run(move |service| async move {
        let mut report = Report::default();
        let client = service.bucket_client(&dest.profile, &dest.bucket).await;
        let mut taken = match occupied(&client, &dest.bucket, &dest.key).await {
            Ok(names) => names,
            Err(error) => return failed(&dest.uri(), error),
        };
        for source in sources {
            let name = source.name().to_owned();
            if progress.is_cancelled() {
                report.errors.push(OpError::Io(name, io::Error::new(io::ErrorKind::Interrupted, "cancelado")));
                break;
            }
            if source.profile != dest.profile {
                report.errors.push(OpError::Io(name, io::Error::other("copiar entre perfiles de AWS no está soportado")));
                continue;
            }
            let same_place = source.bucket == dest.bucket && source.parent_prefix() == dest.key;
            if source.is_prefix() && source.bucket == dest.bucket && dest.key.starts_with(&source.key) {
                report.errors.push(OpError::IntoItself(name));
                continue;
            }
            if op == Op::Move && same_place {
                report.created.push(source.key.clone().into());
                report.done += 1;
                continue;
            }
            let label = (op == Op::Copy && same_place).then_some(label);
            let target = ops::unique_name_by(&name, source.is_prefix(), label, |n| taken.contains(n));
            let key = format!("{}{target}{}", dest.key, if source.is_prefix() { "/" } else { "" });
            match transfer(&client, &source, &dest.with_key(key.clone()), op, &progress).await {
                Ok(moved) => {
                    taken.insert(target);
                    report.done += 1;
                    report.created.push(key.into());
                    match op {
                        Op::Copy => report.s3_created.extend(moved.into_iter().map(|(_, now)| now)),
                        _ => report.s3_moved.extend(moved),
                    }
                }
                Err(error) => report.errors.push(OpError::Io(name, error)),
            }
        }
        report
    })
    .await
}

/// Copia (y si `op` es mover, borra) todo lo que cuelga de `source` hacia `target`.
/// Devuelve (origen, destino creado) por objeto. Si algo falla, deshace lo creado.
async fn transfer(
    client: &Client,
    source: &Object,
    target: &Object,
    op: Op,
    progress: &Progress,
) -> io::Result<Vec<(Object, Stored)>> {
    let objects = list_tree(client, source).await?;
    if objects.is_empty() {
        return Err(io::ErrorKind::NotFound.into());
    }
    let mut done = Vec::new();
    for object in &objects {
        let key = format!("{}{}", target.key, &object.key[source.key.len()..]);
        let result = match Progress::check(Some(progress)) {
            Ok(()) => copy_object(client, &source.bucket, object, &target.bucket, &key, progress).await,
            Err(error) => Err(error),
        };
        match result {
            Ok(etag) => done.push((source.with_key(object.key.clone()), Stored { object: target.with_key(key), etag })),
            Err(error) => {
                let error = explain_copy_failure(client, &target.bucket, &key, error).await;
                rollback(client, &done.iter().map(|(_, now)| now.clone()).collect::<Vec<_>>()).await;
                return Err(error);
            }
        }
    }
    if op == Op::Move {
        for object in objects.iter().rev() {
            delete_object(client, &source.bucket, &object.key, &object.etag).await.map_err(|error| {
                io::Error::other(format!("{error}; copia completa en {}; origen incompleto", target.uri()))
            })?;
        }
    }
    Ok(done)
}

/// Renombra dentro del mismo prefijo. Si el nombre está ocupado, no hace nada.
pub async fn rename(source: Object, new_name: String) -> Report {
    run(move |service| async move {
        let name = source.name().to_owned();
        if new_name == name {
            return Report { created: vec![source.key.clone().into()], ..Report::default() };
        }
        if !ops::validate_name(&new_name) {
            return failed(&name, io::Error::new(io::ErrorKind::InvalidInput, "nombre no válido"));
        }
        let client = service.bucket_client(&source.profile, &source.bucket).await;
        let parent = source.parent_prefix().to_owned();
        match occupied(&client, &source.bucket, &parent).await {
            Ok(taken) if taken.contains(&new_name) => {
                return Report { errors: vec![OpError::Exists(new_name)], ..Report::default() };
            }
            Ok(_) => {}
            Err(error) => return failed(&name, error),
        }
        let key = format!("{parent}{new_name}{}", if source.is_prefix() { "/" } else { "" });
        match transfer(&client, &source, &source.with_key(key.clone()), Op::Move, &Progress::default()).await {
            Ok(moved) => Report { done: 1, created: vec![key.into()], s3_moved: moved, ..Report::default() },
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Report { errors: vec![OpError::Exists(new_name)], ..Report::default() }
            }
            Err(error) => failed(&name, error),
        }
    })
    .await
}

/// Crea una "carpeta" (marcador vacío `nombre/`) con un nombre libre.
pub async fn mkdir(dest: Object, base: &'static str) -> Report {
    run(move |service| async move {
        let client = service.bucket_client(&dest.profile, &dest.bucket).await;
        let taken = match occupied(&client, &dest.bucket, &dest.key).await {
            Ok(names) => names,
            Err(error) => return failed(base, error),
        };
        let name = ops::unique_name_by(base, true, None, |n| taken.contains(n));
        let key = format!("{}{name}/", dest.key);
        match put_bytes(&client, &dest.bucket, &key, Vec::new()).await {
            Ok(etag) => Report {
                done: 1,
                created: vec![key.clone().into()],
                s3_created: vec![Stored { object: dest.with_key(key), etag }],
                ..Report::default()
            },
            Err(error) => failed(&name, error),
        }
    })
    .await
}

/// Borrado permanente (S3 no tiene papelera). Solo borra versiones que no hayan cambiado.
pub async fn delete(sources: Vec<Object>, progress: Progress) -> Report {
    run(move |service| async move {
        let mut report = Report::default();
        for source in sources {
            let name = source.name().to_owned();
            if progress.is_cancelled() {
                report.errors.push(OpError::Io(name, io::Error::new(io::ErrorKind::Interrupted, "cancelado")));
                break;
            }
            let client = service.bucket_client(&source.profile, &source.bucket).await;
            let result = async {
                for object in list_tree(&client, &source).await? {
                    Progress::check(Some(&progress))?;
                    delete_object(&client, &source.bucket, &object.key, &object.etag).await?;
                }
                io::Result::Ok(())
            }
            .await;
            match result {
                Ok(()) => report.done += 1,
                Err(error) => report.errors.push(OpError::Io(name, error)),
            }
        }
        report
    })
    .await
}

/// Inversa de una escritura. Nunca sobrescribe ni borra nada que haya cambiado.
pub async fn revert(change: Change) -> Report {
    run(move |service| async move {
        let mut report = Report::default();
        match change {
            Change::Created(created) => {
                for stored in created.iter().rev() {
                    let client = service.bucket_client(&stored.object.profile, &stored.object.bucket).await;
                    match delete_object(&client, &stored.object.bucket, &stored.object.key, &stored.etag).await {
                        Ok(()) => report.done += 1,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => report.errors.push(OpError::Io(stored.object.name().to_owned(), error)),
                    }
                }
            }
            Change::Moved(pairs) => {
                for (original, now) in pairs.iter().rev() {
                    let client = service.bucket_client(&original.profile, &original.bucket).await;
                    let listed = Listed { key: now.object.key.clone(), etag: now.etag.clone(), size: 0 };
                    let result = async {
                        copy_object(&client, &now.object.bucket, &listed, &original.bucket, &original.key, &Progress::default())
                            .await
                            .map_err(|error| io::Error::other(error.to_string()))?;
                        delete_object(&client, &now.object.bucket, &now.object.key, &now.etag).await
                    }
                    .await;
                    match result {
                        Ok(()) => {
                            report.done += 1;
                            report.created.push(original.key.clone().into());
                        }
                        Err(error) => {
                            let error = explain_copy_failure(&client, &original.bucket, &original.key, error).await;
                            if error.kind() == io::ErrorKind::AlreadyExists {
                                report.errors.push(OpError::Exists(original.name().to_owned()));
                            } else {
                                report.errors.push(OpError::Io(original.name().to_owned(), error));
                            }
                        }
                    }
                }
            }
        }
        report
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(key: &str) -> Object {
        Object { profile: None, bucket: "b".into(), key: key.into() }
    }

    impl Object {
        fn with_bucket(mut self, bucket: &str) -> Self {
            self.bucket = bucket.into();
            self
        }
    }

    #[test]
    fn object_names_and_parents() {
        assert_eq!(object("a/b/c.txt").name(), "c.txt");
        assert_eq!(object("a/b/c.txt").parent_prefix(), "a/b/");
        assert_eq!(object("a/b/").name(), "b");
        assert_eq!(object("a/b/").parent_prefix(), "a/");
        assert_eq!(object("top.txt").parent_prefix(), "");
        assert!(object("a/").is_prefix() && !object("a").is_prefix());
    }

    #[test]
    fn copy_source_encodes_but_keeps_slashes() {
        assert_eq!(copy_source("bk", "a b/ñ+%.txt"), "bk/a%20b/%C3%B1%2B%25.txt");
    }

    #[test]
    fn parts_fit_the_s3_limit() {
        assert_eq!(part_size(1, MIN_PART), MIN_PART);
        let huge = 5 * 1024 * 1024 * 1024 * 1024; // 5 TiB, máximo de S3
        assert!(part_size(huge, MIN_PART).saturating_mul(MAX_PARTS) >= huge);
    }

    /// Un único test porque el servicio S3 del proceso solo se instala una vez.
    #[test]
    fn writes_never_overwrite_and_undo_only_touches_unchanged_objects() {
        use crate::backend::{self, Item, Location};
        let fake = super::super::fake::Server::start();
        super::super::use_endpoint_for_tests(&fake.endpoint);
        let prefix = |key: &str| object(key).with_bucket("bucket");
        let progress = Progress::default;
        let keys = |p: &str| fake.keys("bucket", p);

        // ---- subir: ficheros, carpetas (también vacías) y nombres ocupados
        let local = tempfile::tempdir().unwrap();
        let a = local.path().join("a.txt");
        std::fs::write(&a, "hola").unwrap();
        let d = local.path().join("d");
        std::fs::create_dir_all(d.join("e")).unwrap();
        std::fs::write(d.join("x.txt"), "equis").unwrap();
        let r = futures::executor::block_on(upload(vec![a.clone(), d.clone()], prefix("in/"), Op::Copy, progress()));
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.done, 2);
        assert_eq!(keys("in/"), ["in/a.txt", "in/d/", "in/d/e/", "in/d/x.txt"]);
        assert_eq!(r.s3_created.len(), 4);
        assert_eq!(fake.get("bucket", "in/d/x.txt").unwrap(), b"equis");
        let r = futures::executor::block_on(upload(vec![a.clone(), d.clone()], prefix("in/"), Op::Copy, progress()));
        let created: Vec<_> = r.created.iter().map(|n| n.to_string_lossy().into_owned()).collect();
        assert_eq!(created, ["in/a (2).txt", "in/d (2)/"]);

        // ---- If-None-Match: una escritura directa sobre algo existente se rechaza
        let service = super::super::service().unwrap();
        let clash = service.runtime.block_on(async {
            let client = service.bucket_client(&None, "bucket").await;
            put_bytes(&client, "bucket", "in/a.txt", b"pisado".to_vec()).await
        });
        assert_eq!(clash.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fake.get("bucket", "in/a.txt").unwrap(), b"hola");

        // ---- copiar en el mismo sitio genera "(copia)"
        let r = futures::executor::block_on(copy(vec![prefix("in/a.txt")], prefix("in/"), Op::Copy, "copia", progress()));
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(fake.get("bucket", "in/a (copia).txt").unwrap(), b"hola");

        // ---- mover una carpeta y deshacerlo
        let r = futures::executor::block_on(copy(vec![prefix("in/d/")], prefix("out/"), Op::Move, "copia", progress()));
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(keys("out/"), ["out/d/", "out/d/e/", "out/d/x.txt"]);
        assert!(keys("in/d/").is_empty());
        let r = futures::executor::block_on(revert(Change::Moved(r.s3_moved)));
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(keys("in/d/"), ["in/d/", "in/d/e/", "in/d/x.txt"]);
        assert!(keys("out/").is_empty());

        // ---- una carpeta no se mete dentro de sí misma
        let r = futures::executor::block_on(copy(vec![prefix("in/d/")], prefix("in/d/e/"), Op::Move, "copia", progress()));
        assert!(matches!(r.errors[..], [OpError::IntoItself(_)]));

        // ---- renombrar; a un nombre ocupado, no
        let r = futures::executor::block_on(rename(prefix("in/a.txt"), "b.txt".into()));
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(fake.get("bucket", "in/a.txt").is_none());
        assert_eq!(fake.get("bucket", "in/b.txt").unwrap(), b"hola");
        let r = futures::executor::block_on(rename(prefix("in/b.txt"), "a (2).txt".into()));
        assert!(matches!(r.errors[..], [OpError::Exists(_)]));
        assert_eq!(fake.get("bucket", "in/b.txt").unwrap(), b"hola");

        // ---- borrar con un ETag viejo no borra
        let stale = fake.etag("bucket", "in/b.txt").unwrap();
        fake.put("bucket", "in/b.txt", b"cambiado por otro");
        let refused = service.runtime.block_on(async {
            let client = service.bucket_client(&None, "bucket").await;
            delete_object(&client, "bucket", "in/b.txt", &stale).await
        });
        assert!(refused.is_err());
        assert_eq!(fake.get("bucket", "in/b.txt").unwrap(), b"cambiado por otro");
        let r = futures::executor::block_on(delete(vec![prefix("in/b.txt"), prefix("in/d (2)/")], progress()));
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(fake.get("bucket", "in/b.txt").is_none() && keys("in/d (2)/").is_empty());

        // ---- deshacer una subida no borra lo que otro haya cambiado después
        let c = local.path().join("c.txt");
        std::fs::write(&c, "c").unwrap();
        let r = futures::executor::block_on(upload(vec![c.clone()], prefix("in/"), Op::Copy, progress()));
        fake.put("bucket", "in/c.txt", b"editado");
        let r = futures::executor::block_on(revert(Change::Created(r.s3_created)));
        assert_eq!(r.errors.len(), 1);
        assert_eq!(fake.get("bucket", "in/c.txt").unwrap(), b"editado");

        // ---- carpetas nuevas con nombre libre
        let r = futures::executor::block_on(mkdir(prefix("in/"), "Nueva carpeta"));
        assert_eq!(r.created, ["in/Nueva carpeta/"]);
        let r = futures::executor::block_on(mkdir(prefix("in/"), "Nueva carpeta"));
        assert_eq!(r.created, ["in/Nueva carpeta (2)/"]);

        // ---- descargar una carpeta: se publica entera y con nombre libre
        let downloads = tempfile::tempdir().unwrap();
        let r = futures::executor::block_on(download(vec![prefix("in/d/")], downloads.path().into(), Op::Copy, progress()));
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(std::fs::read_to_string(downloads.path().join("d/x.txt")).unwrap(), "equis");
        assert!(downloads.path().join("d/e").is_dir());
        let r = futures::executor::block_on(download(vec![prefix("in/d/")], downloads.path().into(), Op::Copy, progress()));
        assert_eq!(r.created_paths, [downloads.path().join("d (2)")]);

        // ---- una clave con ".." no escapa del destino y no deja nada a medias
        fake.put("bucket", "evil/ok.txt", b"ok");
        fake.put("bucket", "evil/../../fuera.txt", b"malo");
        let r = futures::executor::block_on(download(vec![prefix("evil/")], downloads.path().into(), Op::Copy, progress()));
        assert_eq!(r.errors.len(), 1);
        let names: Vec<_> = std::fs::read_dir(downloads.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 2, "solo d y d (2): {names:?}");

        // ---- cancelar antes de empezar no crea nada
        let cancelled = Progress::default();
        cancelled.cancel();
        let before = keys("");
        let r = futures::executor::block_on(upload(vec![a.clone()], prefix("in/"), Op::Copy, cancelled));
        assert_eq!(r.done, 0);
        assert_eq!(keys(""), before);

        // ---- capa común: S3 → local y local → S3 moviendo
        let dest = Location::Local(downloads.path().into());
        let item = Item::S3(prefix("in/a (2).txt"));
        let r = futures::executor::block_on(backend::paste(vec![item], dest, Op::Move, "copia", progress()));
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(std::fs::read_to_string(downloads.path().join("a (2).txt")).unwrap(), "hola");
        assert!(fake.get("bucket", "in/a (2).txt").is_none());
        assert!(backend::paste_undo(&r, Op::Move).is_none(), "mover entre backends no se deshace");
        let s3_dest = Location::S3(super::super::Location::Prefix {
            profile: None,
            bucket: "other".into(),
            prefix: String::new(),
        });
        let r = futures::executor::block_on(backend::paste(vec![Item::Local(c.clone())], s3_dest, Op::Move, "copia", progress()));
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(fake.get("other", "c.txt").unwrap(), b"c");
        assert!(!c.exists(), "el origen local se borra tras subirlo");
    }
}
