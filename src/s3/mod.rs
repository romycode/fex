//! Amazon S3: listar (aquí) y escribir ([`write`]).

use std::{collections::HashMap, io, sync::Mutex, time::SystemTime};

use aws_config::BehaviorVersion;
use aws_sdk_s3::{Client, config::Region, error::ProvideErrorMetadata, types::EncodingType};
use percent_encoding::percent_decode_str;
use tokio::task::JoinSet;

use crate::fs::{Entry, Kind};

pub mod connection;
mod write;
#[cfg(test)]
mod fake;

pub use write::{Change, Object, Stored, copy, delete, download, mkdir, rename, revert, upload};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    Buckets { connection: String },
    Prefix { connection: String, bucket: String, prefix: String },
}

impl Location {
    /// Nombre de la conexión (ver [`connection`]).
    pub fn connection(&self) -> &str {
        match self {
            Self::Buckets { connection } | Self::Prefix { connection, .. } => connection,
        }
    }

    pub fn child(&self, key: &str) -> Option<Self> {
        match self {
            Self::Buckets { connection } => {
                Some(Self::Prefix { connection: connection.clone(), bucket: key.to_owned(), prefix: String::new() })
            }
            Self::Prefix { connection, bucket, prefix }
                if key.starts_with(prefix) && key.len() > prefix.len() && key.ends_with('/') =>
            {
                Some(Self::Prefix { connection: connection.clone(), bucket: bucket.clone(), prefix: key.to_owned() })
            }
            Self::Prefix { .. } => None,
        }
    }

    pub fn parent(&self) -> Option<(Self, String)> {
        let Self::Prefix { connection, bucket, prefix } = self else {
            return None;
        };
        if prefix.is_empty() {
            return Some((Self::Buckets { connection: connection.clone() }, bucket.clone()));
        }
        let parent_length = prefix.strip_suffix('/')?.rfind('/').map_or(0, |i| i + 1);
        Some((
            Self::Prefix {
                connection: connection.clone(),
                bucket: bucket.clone(),
                prefix: prefix[..parent_length].to_owned(),
            },
            prefix.clone(),
        ))
    }

    /// La carpeta actual como objeto S3 (prefijo). `None` en la lista de buckets.
    pub fn as_object(&self) -> Option<Object> {
        match self {
            Self::Buckets { .. } => None,
            Self::Prefix { connection, bucket, prefix } => {
                Some(Object { connection: connection.clone(), bucket: bucket.clone(), key: prefix.clone() })
            }
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Buckets { connection } => connection.clone(),
            Self::Prefix { bucket, prefix, .. } => {
                let Some(segment) = prefix.strip_suffix('/') else { return bucket.clone() };
                let segment = segment.rsplit('/').next().unwrap_or_default();
                if segment.is_empty() { "/".into() } else { segment.to_owned() }
            }
        }
    }
}

/// Clientes y regiones conocidas de una conexión.
#[derive(Default)]
struct ConnectionClients {
    config: Option<aws_config::SdkConfig>,
    path_style: bool,
    /// Con endpoint propio no se detecta la región por bucket: se usa la de la conexión.
    fixed_region: bool,
    regions: HashMap<String, Region>,
    clients: HashMap<Region, Client>,
}

struct S3 {
    runtime: tokio::runtime::Runtime,
    connections: Mutex<HashMap<String, ConnectionClients>>,
}

static SERVICE: once_cell::sync::OnceCell<S3> = once_cell::sync::OnceCell::new();

/// Conexiones definidas por el usuario. La app las fija al arrancar y al editarlas.
static CONNECTIONS: std::sync::RwLock<Vec<connection::Connection>> = std::sync::RwLock::new(Vec::new());

/// Sustituye las conexiones y olvida los clientes creados con las anteriores.
pub fn set_connections(connections: Vec<connection::Connection>) {
    let mut current = CONNECTIONS.write().unwrap();
    // Olvidar los clientes de las conexiones que cambian o desaparecen (y de las nuevas).
    let touched: std::collections::HashSet<String> =
        current.iter().chain(&connections).map(|c| c.name.clone()).collect();
    *current = connections;
    if let Some(service) = SERVICE.get() {
        service.connections.lock().unwrap().retain(|name, _| !touched.contains(name));
    }
}

fn find_connection(name: &str) -> io::Result<connection::Connection> {
    CONNECTIONS
        .read()
        .unwrap()
        .iter()
        .find(|c| c.name == name)
        .cloned()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("conexión S3 «{name}» no existe")))
}

fn service() -> io::Result<&'static S3> {
    SERVICE.get_or_try_init(|| {
        Ok(S3 {
            runtime: tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?,
            connections: Mutex::default(),
        })
    })
}

/// Cliente HTTPS que además confía en el certificado CA del fichero (PEM).
fn http_client_with_ca(path: &std::path::Path) -> io::Result<aws_sdk_s3::config::SharedHttpClient> {
    use aws_smithy_http_client::tls::{self, TlsContext, TrustStore};
    let pem = std::fs::read(path).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    let context = TlsContext::builder()
        .with_trust_store(TrustStore::default().with_pem_certificate(pem))
        .build()
        .map_err(io::Error::other)?;
    Ok(aws_smithy_http_client::Builder::new()
        .tls_provider(tls::Provider::Rustls(tls::rustls_provider::CryptoMode::AwsLc))
        .tls_context(context)
        .build_https())
}

/// Configuración del SDK para una conexión: credenciales, región, endpoint y CA.
async fn sdk_config(c: &connection::Connection) -> io::Result<aws_config::SdkConfig> {
    use connection::Credentials;
    let mut loader = aws_config::defaults(BehaviorVersion::latest());
    let region = match (&c.region, &c.credentials) {
        (Some(region), _) => Region::new(region.clone()),
        (None, credentials) => {
            let mut chain = aws_config::default_provider::region::DefaultRegionChain::builder();
            if let Credentials::System { profile: Some(profile) } = credentials {
                chain = chain.profile_name(profile);
            }
            // Un perfil puede no tener región. ListBuckets necesita una para firmar.
            chain.build().region().await.unwrap_or_else(|| Region::new("us-east-1"))
        }
    };
    match &c.credentials {
        Credentials::System { profile: Some(profile) } => {
            let provider_config =
                aws_config::provider_config::ProviderConfig::default().with_region(Some(region.clone()));
            let credentials = aws_config::profile::ProfileFileCredentialsProvider::builder()
                .configure(&provider_config)
                .profile_name(profile)
                .build();
            loader = loader.profile_name(profile).credentials_provider(credentials);
        }
        Credentials::System { profile: None } => {}
        Credentials::Keys { access_key, secret_key, session_token } => {
            loader = loader.credentials_provider(aws_sdk_s3::config::Credentials::new(
                access_key,
                secret_key,
                session_token.clone(),
                None,
                "fex",
            ));
        }
    }
    if let Some(endpoint) = &c.endpoint {
        loader = loader.endpoint_url(endpoint);
    }
    if let Some(ca) = &c.ca_file {
        loader = loader.http_client(http_client_with_ca(ca)?);
    }
    Ok(loader.region(region).load().await)
}

/// Tests: todas las llamadas a S3 del proceso van a `endpoint` (un S3 falso en memoria),
/// con la conexión [`TEST_CONNECTION`].
#[cfg(test)]
const TEST_CONNECTION: &str = "test";

#[cfg(test)]
fn use_endpoint_for_tests(endpoint: &str) {
    let region = Region::new("us-east-1");
    let client = Client::from_conf(
        aws_sdk_s3::config::Builder::new()
            .behavior_version_latest()
            .region(region.clone())
            .credentials_provider(aws_sdk_s3::config::Credentials::new("test", "test", None, None, "test"))
            .endpoint_url(endpoint)
            .force_path_style(true)
            // Cuerpos planos (sin aws-chunked) para que el servidor falso sea sencillo.
            .request_checksum_calculation(aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired)
            .build(),
    );
    let config = aws_config::SdkConfig::builder().region(region.clone()).build();
    let service = S3 {
        runtime: tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap(),
        connections: Mutex::new(HashMap::from([(
            TEST_CONNECTION.to_owned(),
            ConnectionClients {
                config: Some(config),
                path_style: true,
                fixed_region: true,
                regions: HashMap::new(),
                clients: HashMap::from([(region, client)]),
            },
        )])),
    };
    assert!(SERVICE.set(service).is_ok(), "el S3 de tests solo se instala una vez");
}

impl S3 {
    async fn config(&self, name: &str) -> io::Result<aws_config::SdkConfig> {
        if let Some(config) = self.connections.lock().unwrap().get(name).and_then(|c| c.config.clone()) {
            return Ok(config);
        }
        let definition = find_connection(name)?;
        let config = sdk_config(&definition).await?;
        let mut connections = self.connections.lock().unwrap();
        let state = connections.entry(name.to_owned()).or_default();
        state.config = Some(config.clone());
        state.path_style = definition.path_style;
        state.fixed_region = definition.endpoint.is_some();
        Ok(config)
    }

    async fn client(&self, connection: &str, bucket: Option<&str>) -> io::Result<Client> {
        let config = self.config(connection).await?;
        let mut connections = self.connections.lock().unwrap();
        let state = connections.entry(connection.to_owned()).or_default();
        let region = bucket
            .and_then(|bucket| state.regions.get(bucket))
            .or_else(|| config.region())
            .cloned()
            .unwrap_or_else(|| Region::new("us-east-1"));
        let path_style = state.path_style;
        Ok(state
            .clients
            .entry(region.clone())
            .or_insert_with(|| {
                Client::from_conf(
                    aws_sdk_s3::config::Builder::from(&config).region(region).force_path_style(path_style).build(),
                )
            })
            .clone())
    }

    async fn bucket_client(&self, connection: &str, bucket: &str) -> io::Result<Client> {
        let known_region = self
            .connections
            .lock()
            .unwrap()
            .get(connection)
            .is_some_and(|state| state.fixed_region || state.regions.contains_key(bucket));
        if !known_region {
            // HeadBucket reports the signing region even on many access-denied responses.
            let client = self.client(connection, None).await?;
            let response = client.head_bucket().bucket(bucket).send().await;
            let region = match &response {
                Ok(output) => output.bucket_region(),
                Err(error) => error.raw_response().and_then(|response| response.headers().get("x-amz-bucket-region")),
            };
            if let Some(region) = region {
                self.connections
                    .lock()
                    .unwrap()
                    .entry(connection.to_owned())
                    .or_default()
                    .regions
                    .insert(bucket.to_owned(), Region::new(region.to_owned()));
            }
        }
        self.client(connection, Some(bucket)).await
    }

    async fn list(&self, location: &Location) -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        match location {
            Location::Buckets { connection } => {
                let client = self.client(connection, None).await?;
                let mut pages = client.list_buckets().max_buckets(1000).into_paginator().send();
                let mut regions = HashMap::new();
                while let Some(page) = pages.next().await {
                    for bucket in page.map_err(sdk_error)?.buckets() {
                        let Some(name) = bucket.name() else { continue };
                        if let Some(region) = bucket.bucket_region() {
                            regions.insert(name.to_owned(), Region::new(region.to_owned()));
                        }
                        entries.push(Entry::remote(name, name, Kind::Dir, None, None));
                    }
                }
                self.connections.lock().unwrap().entry(connection.clone()).or_default().regions.extend(regions);
            }
            Location::Prefix { connection, bucket, prefix, .. } => {
                let client = self.bucket_client(connection, bucket).await?;
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
                    for folder in page.common_prefixes() {
                        let Some(raw) = folder.prefix() else { continue };
                        let key = decode_key(raw, encoded)?;
                        let name = key.strip_prefix(prefix).unwrap_or(&key);
                        entries.push(Entry::remote(&key, name, Kind::Dir, None, None));
                    }
                    for object in page.contents() {
                        let Some(raw) = object.key() else { continue };
                        let key = decode_key(raw, encoded)?;
                        if key == *prefix {
                            continue; // directory marker
                        }
                        let name = key.strip_prefix(prefix).unwrap_or(&key);
                        let size = object.size().and_then(|size| u64::try_from(size).ok());
                        let modified = object.last_modified().and_then(|time| SystemTime::try_from(*time).ok());
                        entries.push(Entry::remote(&key, name, Kind::File, size, modified));
                    }
                }
            }
        }
        Ok(entries)
    }
}

fn sdk_error<E: ProvideErrorMetadata + std::error::Error + 'static, R>(
    error: aws_sdk_s3::error::SdkError<E, R>,
) -> io::Error {
    let message = error.as_service_error().and_then(|error| error.message().or_else(|| error.code()));
    io::Error::other(message.map_or_else(|| error.to_string(), str::to_owned))
}

fn decode_key(value: &str, encoded: bool) -> io::Result<String> {
    if !encoded {
        return Ok(value.to_owned());
    }
    percent_decode_str(value).decode_utf8().map(|s| s.into_owned()).map_err(io::Error::other)
}

pub async fn list(location: Location) -> io::Result<Vec<Entry>> {
    let service = service()?;
    // Dropping a superseded listing aborts its requests without blocking GPUI.
    let mut requests = JoinSet::new();
    requests.spawn_on(async move { service.list(&location).await }, service.runtime.handle());
    requests.join_next().await.expect("one S3 listing task").map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_preserve_dot_and_empty_segments() {
        let root = Location::Buckets { connection: "test".into() };
        let bucket = root.child("bucket").unwrap();
        let dot = bucket.child("./").unwrap();
        let empty = dot.child(".//").unwrap();
        assert_eq!(empty.parent(), Some((dot.clone(), ".//".into())));
        assert_eq!(dot.parent(), Some((bucket.clone(), "./".into())));
        assert_eq!(bucket.parent(), Some((root, "bucket".into())));
    }

    #[test]
    fn url_decoding_preserves_literal_plus_and_percent() {
        assert_eq!(decode_key("folder%2Fa%2Bb%252F", true).unwrap(), "folder/a+b%2F");
        assert_eq!(decode_key("a%2Bb+", false).unwrap(), "a%2Bb+");
    }

    #[test]
    fn paginates_listings_via_the_sdk() {
        use std::{
            io::{BufRead, BufReader, Write},
            net::TcpListener,
            thread,
            time::{Duration, Instant},
        };

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let responder = thread::spawn(move || {
            let responses = [
                (
                    "list-type=2",
                    r#"<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>true</IsTruncated><NextContinuationToken>page-two</NextContinuationToken><CommonPrefixes><Prefix>folder%2F</Prefix></CommonPrefixes></ListBucketResult>"#,
                ),
                (
                    "continuation-token=page-two",
                    r#"<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>false</IsTruncated><Contents><Key>file%2B%252F.txt</Key><Size>3</Size><LastModified>2026-01-01T00:00:00.000Z</LastModified></Contents></ListBucketResult>"#,
                ),
            ];
            for (expected, body) in responses {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("waiting for {expected}: {error}"),
                    }
                };
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut reader = BufReader::new(&socket);
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                assert!(request.contains(expected), "{request:?}");
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap() == 0 || header == "\r\n" {
                        break;
                    }
                }
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                )
                .unwrap();
            }
        });

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let region = Region::new("us-east-1");
        let config = aws_config::SdkConfig::builder().region(region.clone()).build();
        let client = Client::from_conf(
            aws_sdk_s3::config::Builder::new()
                .behavior_version_latest()
                .region(region.clone())
                .credentials_provider(aws_sdk_s3::config::Credentials::new("test", "test", None, None, "test"))
                .endpoint_url(endpoint)
                .force_path_style(true)
                .build(),
        );
        let service = S3 {
            runtime,
            connections: Mutex::new(HashMap::from([(
                TEST_CONNECTION.to_owned(),
                ConnectionClients {
                    config: Some(config),
                    path_style: true,
                    fixed_region: false,
                    clients: HashMap::from([(region.clone(), client.clone())]),
                    regions: HashMap::from([("bucket".into(), region)]),
                },
            )])),
        };
        let location = Location::Buckets { connection: TEST_CONNECTION.into() }.child("bucket").unwrap();
        service.runtime.block_on(async {
            let entries = service.list(&location).await.unwrap();
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].name, "folder/");
            assert!(entries[0].kind.is_dir());
            let file = &entries[1];
            assert_eq!(file.size, Some(3));
            assert!(file.modified.is_some());
        });
        responder.join().unwrap();
    }
}
