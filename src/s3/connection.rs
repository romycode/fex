//! Conexiones S3 guardadas: con qué credenciales y contra qué servidor.
//!
//! Se guardan en el fichero `s3` del directorio de configuración (permisos 0600, porque
//! puede llevar claves). Formato:
//!
//! ```text
//! [AWS]
//! # credenciales del sistema (~/.aws, variables de entorno)
//! profile = default
//!
//! [Emulador]
//! endpoint = https://127.0.0.1:9297
//! region = us-east-1
//! access_key = AKIA...
//! secret_key = ...
//! path_style = true
//! ca_file = ~/.aws/emulator/9297/ca.pem
//! ```
//!
//! Con `access_key` se usan esas claves; si no, las del sistema (con `profile` opcional).

use std::{
    io,
    path::{Path, PathBuf},
};

pub const FILE_NAME: &str = "s3";
/// Conexión por defecto cuando no hay fichero: credenciales del sistema, perfil por defecto.
pub const DEFAULT_NAME: &str = "AWS";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Credentials {
    /// Cadena estándar de AWS: variables de entorno, `~/.aws/credentials`, SSO, IMDS…
    System { profile: Option<String> },
    /// Claves escritas por el usuario.
    Keys { access_key: String, secret_key: String, session_token: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    pub name: String,
    pub credentials: Credentials,
    /// Servidor propio (emulador, MinIO, R2…). `None` = AWS.
    pub endpoint: Option<String>,
    pub region: Option<String>,
    /// `https://host/bucket/clave` en vez de `https://bucket.host/clave`. Necesario en casi
    /// todos los servidores compatibles y cuando el endpoint es una IP.
    pub path_style: bool,
    /// Certificado CA adicional en PEM (servidores con certificado propio).
    pub ca_file: Option<PathBuf>,
}

impl Connection {
    pub fn system(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            credentials: Credentials::System { profile: None },
            endpoint: None,
            region: None,
            path_style: false,
            ca_file: None,
        }
    }

    /// Descripción corta para la barra lateral y la paleta.
    pub fn summary(&self) -> String {
        let who = match &self.credentials {
            Credentials::System { profile: Some(profile) } => profile.clone(),
            Credentials::System { profile: None } => "default".into(),
            Credentials::Keys { access_key, .. } => {
                let visible: String = access_key.chars().take(4).collect();
                format!("{visible}…")
            }
        };
        match &self.endpoint {
            Some(endpoint) => format!("{who} · {endpoint}"),
            None => who,
        }
    }
}

/// `~/x` → `$HOME/x`.
pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// Lee el fichero. Devuelve las conexiones válidas y un mensaje por error (con nº de línea).
pub fn parse(text: &str) -> (Vec<Connection>, Vec<String>) {
    let mut out: Vec<Connection> = Vec::new();
    let mut errors = Vec::new();
    // Valores en bruto de la sección actual.
    struct Raw {
        name: String,
        line: usize,
        fields: Vec<(String, String)>,
    }
    let mut sections: Vec<Raw> = Vec::new();
    for (number, raw) in text.lines().enumerate() {
        let number = number + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sections.push(Raw { name: name.trim().to_owned(), line: number, fields: Vec::new() });
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            errors.push(format!("{number}: falta '='"));
            continue;
        };
        let Some(section) = sections.last_mut() else {
            errors.push(format!("{number}: clave fuera de una sección [nombre]"));
            continue;
        };
        // Sin comentarios al final de línea: un secreto puede contener '#'.
        let value = value.trim();
        section.fields.push((key.trim().to_owned(), value.to_owned()));
    }
    for section in sections {
        let get = |key: &str| {
            section.fields.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone()).filter(|v| !v.is_empty())
        };
        if section.name.is_empty() {
            errors.push(format!("{}: conexión sin nombre", section.line));
            continue;
        }
        if out.iter().any(|c| c.name == section.name) {
            errors.push(format!("{}: conexión repetida «{}»", section.line, section.name));
            continue;
        }
        if let Some((unknown, _)) = section.fields.iter().find(|(k, _)| !KEYS.contains(&k.as_str())) {
            errors.push(format!("{}: clave desconocida «{unknown}» en «{}»", section.line, section.name));
            continue;
        }
        let credentials = match (get("access_key"), get("secret_key")) {
            (Some(access_key), Some(secret_key)) => {
                Credentials::Keys { access_key, secret_key, session_token: get("session_token") }
            }
            (None, None) => Credentials::System { profile: get("profile") },
            _ => {
                errors.push(format!("{}: «{}» necesita access_key y secret_key", section.line, section.name));
                continue;
            }
        };
        let path_style = match get("path_style").as_deref() {
            None => false,
            Some("true" | "yes" | "1") => true,
            Some("false" | "no" | "0") => false,
            Some(other) => {
                errors.push(format!("{}: path_style no válido «{other}»", section.line));
                continue;
            }
        };
        let endpoint = get("endpoint");
        if let Some(endpoint) = &endpoint
            && !(endpoint.starts_with("http://") || endpoint.starts_with("https://"))
        {
            errors.push(format!("{}: el endpoint debe empezar por http:// o https://", section.line));
            continue;
        }
        out.push(Connection {
            name: section.name,
            credentials,
            endpoint,
            region: get("region"),
            path_style,
            ca_file: get("ca_file").map(|p| expand_home(&p)),
        });
    }
    (out, errors)
}

const KEYS: &[&str] =
    &["profile", "access_key", "secret_key", "session_token", "endpoint", "region", "path_style", "ca_file"];

pub fn serialize(connections: &[Connection]) -> String {
    let mut out = String::from("# Conexiones S3 de fex. Ver el README para el formato.\n");
    for c in connections {
        out.push_str(&format!("\n[{}]\n", c.name));
        let mut field = |key: &str, value: &str| out.push_str(&format!("{key} = {value}\n"));
        match &c.credentials {
            Credentials::System { profile } => {
                if let Some(profile) = profile {
                    field("profile", profile);
                }
            }
            Credentials::Keys { access_key, secret_key, session_token } => {
                field("access_key", access_key);
                field("secret_key", secret_key);
                if let Some(token) = session_token {
                    field("session_token", token);
                }
            }
        }
        if let Some(endpoint) = &c.endpoint {
            field("endpoint", endpoint);
        }
        if let Some(region) = &c.region {
            field("region", region);
        }
        if c.path_style {
            field("path_style", "true");
        }
        if let Some(ca) = &c.ca_file {
            field("ca_file", &ca.to_string_lossy());
        }
    }
    out
}

/// Conexiones guardadas. Sin fichero, una conexión con las credenciales del sistema.
pub fn load() -> io::Result<(Vec<Connection>, Vec<String>)> {
    let path = crate::config::file(FILE_NAME)?;
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let (connections, errors) = parse(&text);
            let errors = errors.into_iter().map(|e| format!("{}:{e}", path.display())).collect();
            Ok((connections, errors))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok((vec![Connection::system(DEFAULT_NAME)], Vec::new())),
        Err(error) => Err(error),
    }
}

/// Guarda de forma atómica y con permisos 0600 (lleva secretos).
pub fn save(connections: &[Connection]) -> io::Result<()> {
    write_private(&crate::config::file(FILE_NAME)?, &serialize(connections))
}

fn write_private(path: &Path, text: &str) -> io::Result<()> {
    use std::{io::Write, os::unix::fs::PermissionsExt};
    let dir = path.parent().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    std::fs::create_dir_all(dir)?;
    let mut pending = tempfile::Builder::new().prefix(".s3-").tempfile_in(dir)?;
    // tempfile ya crea con 0600; se fija por si el umask o la plataforma difieren.
    std::fs::set_permissions(pending.path(), std::fs::Permissions::from_mode(0o600))?;
    pending.write_all(text.as_bytes())?;
    pending.as_file().sync_all()?;
    pending.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_system_keys_and_custom_endpoints() {
        let (connections, errors) = parse(
            "# comentario\n\
             [AWS]\n\
             profile = prod\n\
             \n\
             [Emulador]\n\
             endpoint = https://127.0.0.1:9297\n\
             region = us-east-1\n\
             access_key = AKIATEST\n\
             secret_key = s3cr#t\n\
             path_style = true\n\
             ca_file = /tmp/ca.pem\n",
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(connections.len(), 2);
        assert_eq!(connections[0].credentials, Credentials::System { profile: Some("prod".into()) });
        let emulator = &connections[1];
        assert_eq!(
            emulator.credentials,
            Credentials::Keys { access_key: "AKIATEST".into(), secret_key: "s3cr#t".into(), session_token: None }
        );
        assert_eq!(emulator.endpoint.as_deref(), Some("https://127.0.0.1:9297"));
        assert!(emulator.path_style);
        assert_eq!(emulator.ca_file.as_deref(), Some(Path::new("/tmp/ca.pem")));
        assert_eq!(emulator.summary(), "AKIA… · https://127.0.0.1:9297");
    }

    #[test]
    fn reports_bad_sections_and_keeps_the_good_ones() {
        let (connections, errors) = parse(
            "huérfana = 1\n\
             [Sin secreto]\naccess_key = A\n\
             [Mal host]\nendpoint = 127.0.0.1\n\
             [Rara]\nfoo = bar\n\
             [Bien]\n\
             [Bien]\n",
        );
        assert_eq!(connections.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Bien"]);
        assert_eq!(errors.len(), 5, "{errors:?}");
    }

    #[test]
    fn serialize_round_trips() {
        let connections = vec![
            Connection::system("AWS"),
            Connection {
                name: "MinIO local".into(),
                credentials: Credentials::Keys {
                    access_key: "minio".into(),
                    secret_key: "a=b # c".into(),
                    session_token: Some("tok".into()),
                },
                endpoint: Some("http://127.0.0.1:9000".into()),
                region: Some("eu-west-1".into()),
                path_style: true,
                ca_file: Some("/etc/ca.pem".into()),
            },
        ];
        let (parsed, errors) = parse(&serialize(&connections));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(parsed, connections);
    }

    #[test]
    fn saved_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s3");
        write_private(&path, "[x]\n").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
