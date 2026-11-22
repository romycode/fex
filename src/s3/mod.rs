//! Amazon S3: listar (aquí) y escribir ([`write`]).

use std::{collections::HashMap, io, sync::Mutex, time::SystemTime};

use aws_config::BehaviorVersion;
use aws_sdk_s3::{Client, config::Region, error::ProvideErrorMetadata, types::EncodingType};
use percent_encoding::percent_decode_str;
use tokio::task::JoinSet;

use crate::fs::{Entry, Kind};

mod write;
#[cfg(test)]
mod fake;

pub use write::{Change, Object, Stored, copy, delete, download, mkdir, rename, revert, upload};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    Buckets { profile: Option<String> },
    Prefix { profile: Option<String>, bucket: String, prefix: String },
}

impl Location {
    pub fn profile(&self) -> Option<&str> {
        match self {
            Self::Buckets { profile } | Self::Prefix { profile, .. } => profile.as_deref(),
        }
    }

    pub fn child(&self, key: &str) -> Option<Self> {
        match self {
            Self::Buckets { profile } => {
                Some(Self::Prefix { profile: profile.clone(), bucket: key.to_owned(), prefix: String::new() })
            }
            Self::Prefix { profile, bucket, prefix }
                if key.starts_with(prefix) && key.len() > prefix.len() && key.ends_with('/') =>
            {
                Some(Self::Prefix { profile: profile.clone(), bucket: bucket.clone(), prefix: key.to_owned() })
            }
            Self::Prefix { .. } => None,
        }
    }

    pub fn parent(&self) -> Option<(Self, String)> {
        let Self::Prefix { profile, bucket, prefix } = self else {
            return None;
        };
        if prefix.is_empty() {
            return Some((Self::Buckets { profile: profile.clone() }, bucket.clone()));
        }
        let parent_length = prefix.strip_suffix('/')?.rfind('/').map_or(0, |i| i + 1);
        Some((
            Self::Prefix {
                profile: profile.clone(),
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
            Self::Prefix { profile, bucket, prefix } => {
                Some(Object { profile: profile.clone(), bucket: bucket.clone(), key: prefix.clone() })
            }
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Buckets { profile } => {
                profile.as_ref().map_or_else(|| "Amazon S3".into(), |profile| format!("Amazon S3 · {profile}"))
            }
            Self::Prefix { bucket, prefix, .. } => {
                let Some(segment) = prefix.strip_suffix('/') else { return bucket.clone() };
                let segment = segment.rsplit('/').next().unwrap_or_default();
                if segment.is_empty() { "/".into() } else { segment.to_owned() }
            }
        }
    }
}

#[derive(Default)]
struct ProfileClients {
    config: Option<aws_config::SdkConfig>,
    regions: HashMap<String, Region>,
    clients: HashMap<Region, Client>,
}

struct S3 {
    runtime: tokio::runtime::Runtime,
    profiles: Mutex<HashMap<Option<String>, ProfileClients>>,
}

static SERVICE: once_cell::sync::OnceCell<S3> = once_cell::sync::OnceCell::new();

fn service() -> io::Result<&'static S3> {
    SERVICE.get_or_try_init(|| {
        Ok(S3 {
            runtime: tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?,
            profiles: Mutex::default(),
        })
    })
}

/// Tests: todas las llamadas a S3 del proceso van a `endpoint` (un S3 falso en memoria).
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
    let regions = ["bucket", "other"].map(|b| (b.to_owned(), region.clone())).into();
    let service = S3 {
        runtime: tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap(),
        profiles: Mutex::new(HashMap::from([(
            None,
            ProfileClients { config: Some(config), clients: HashMap::from([(region, client)]), regions },
        )])),
    };
    assert!(SERVICE.set(service).is_ok(), "el S3 de tests solo se instala una vez");
}

impl S3 {
    async fn config(&self, profile: Option<String>) -> aws_config::SdkConfig {
        if let Some(config) = self.profiles.lock().unwrap().get(&profile).and_then(|p| p.config.clone()) {
            return config;
        }
        let mut loader = aws_config::defaults(BehaviorVersion::latest());
        let mut region_chain = aws_config::default_provider::region::DefaultRegionChain::builder();
        if let Some(profile) = &profile {
            region_chain = region_chain.profile_name(profile);
        }
        // A profile can omit the region. ListBuckets still needs a signing region.
        let region = region_chain.build().region().await.unwrap_or_else(|| Region::new("us-east-1"));
        if let Some(profile) = &profile {
            let provider_config =
                aws_config::provider_config::ProviderConfig::default().with_region(Some(region.clone()));
            let credentials = aws_config::profile::ProfileFileCredentialsProvider::builder()
                .configure(&provider_config)
                .profile_name(profile)
                .build();
            loader = loader.profile_name(profile).credentials_provider(credentials);
        }
        let config = loader.region(region).load().await;
        self.profiles.lock().unwrap().entry(profile).or_default().config = Some(config.clone());
        config
    }

    async fn client(&self, profile: &Option<String>, bucket: Option<&str>) -> Client {
        let config = self.config(profile.clone()).await;
        let mut profiles = self.profiles.lock().unwrap();
        let state = profiles.entry(profile.clone()).or_default();
        let region = bucket
            .and_then(|bucket| state.regions.get(bucket))
            .or_else(|| config.region())
            .cloned()
            .unwrap_or_else(|| Region::new("us-east-1"));
        state
            .clients
            .entry(region.clone())
            .or_insert_with(|| Client::from_conf(aws_sdk_s3::config::Builder::from(&config).region(region).build()))
            .clone()
    }

    async fn bucket_client(&self, profile: &Option<String>, bucket: &str) -> Client {
        let known_region =
            self.profiles.lock().unwrap().get(profile).is_some_and(|state| state.regions.contains_key(bucket));
        if !known_region {
            // HeadBucket reports the signing region even on many access-denied responses.
            let client = self.client(profile, None).await;
            let response = client.head_bucket().bucket(bucket).send().await;
            let region = match &response {
                Ok(output) => output.bucket_region(),
                Err(error) => error.raw_response().and_then(|response| response.headers().get("x-amz-bucket-region")),
            };
            if let Some(region) = region {
                self.profiles
                    .lock()
                    .unwrap()
                    .entry(profile.clone())
                    .or_default()
                    .regions
                    .insert(bucket.to_owned(), Region::new(region.to_owned()));
            }
        }
        self.client(profile, Some(bucket)).await
    }

    async fn list(&self, location: &Location) -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        match location {
            Location::Buckets { profile } => {
                let client = self.client(profile, None).await;
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
                self.profiles.lock().unwrap().entry(profile.clone()).or_default().regions.extend(regions);
            }
            Location::Prefix { profile, bucket, prefix } => {
                let client = self.bucket_client(profile, bucket).await;
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
        let root = Location::Buckets { profile: Some("test".into()) };
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
            profiles: Mutex::new(HashMap::from([(
                None,
                ProfileClients {
                    config: Some(config),
                    clients: HashMap::from([(region.clone(), client.clone())]),
                    regions: HashMap::from([("bucket".into(), region)]),
                },
            )])),
        };
        let location = Location::Buckets { profile: None }.child("bucket").unwrap();
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
