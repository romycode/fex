use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use aws_config::BehaviorVersion;
use aws_sdk_s3::{Client, config::Region, error::ProvideErrorMetadata, types::EncodingType};
use percent_encoding::percent_decode_str;
use tokio::{io::AsyncWriteExt, task::JoinSet};

use crate::{
    fs::{Entry, Kind},
    ops::{self, OpError, Report},
};

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

fn service() -> io::Result<&'static S3> {
    static SERVICE: once_cell::sync::OnceCell<S3> = once_cell::sync::OnceCell::new();
    SERVICE.get_or_try_init(|| {
        Ok(S3 {
            runtime: tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?,
            profiles: Mutex::default(),
        })
    })
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

fn destination(directory: &Path, key: &str) -> io::Result<PathBuf> {
    let name = key.rsplit('/').next().unwrap_or_default();
    if !ops::validate_name(name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid local file name"));
    }
    Ok(directory.join(name))
}

pub async fn download(location: Location, keys: Vec<String>, directory: PathBuf, progress: Arc<AtomicU64>) -> Report {
    let result = async {
        let service = service()?;
        let mut requests = JoinSet::new();
        requests.spawn_on(download_objects(service, location, keys, directory, progress), service.runtime.handle());
        requests.join_next().await.expect("one S3 download task").map_err(io::Error::other)
    }
    .await;
    result.unwrap_or_else(|error| Report { errors: vec![OpError::Io("Amazon S3".into(), error)], ..Report::default() })
}

async fn download_objects(
    service: &S3,
    location: Location,
    keys: Vec<String>,
    directory: PathBuf,
    progress: Arc<AtomicU64>,
) -> Report {
    let mut report = Report::default();
    let Location::Prefix { profile, bucket, .. } = location else {
        return report;
    };
    let client = service.bucket_client(&profile, &bucket).await;
    for key in keys {
        match download_object(&client, &bucket, &key, &directory, &progress).await {
            Ok(path) => {
                report.done += 1;
                report.created_paths.push(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                report.errors.push(OpError::Exists(key));
            }
            Err(error) => report.errors.push(OpError::Io(key, error)),
        }
    }
    report
}

async fn download_object(
    client: &Client,
    bucket: &str,
    key: &str,
    directory: &Path,
    progress: &AtomicU64,
) -> io::Result<PathBuf> {
    let path = destination(directory, key)?;
    if ops::exists(&path) {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    let pending = tempfile::Builder::new().prefix(".fex-download-").tempfile_in(directory)?;
    let mut file = tokio::fs::File::from_std(pending.reopen()?);
    let mut output = client.get_object().bucket(bucket).key(key).send().await.map_err(sdk_error)?;
    let mut written = 0;
    while let Some(chunk) = output.body.try_next().await.map_err(io::Error::other)? {
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;
        progress.fetch_add(chunk.len() as u64, Ordering::Relaxed);
    }
    if output.content_length().and_then(|n| u64::try_from(n).ok()).is_some_and(|n| n != written) {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "incomplete S3 download"));
    }
    file.flush().await?;
    drop(file);
    pending.persist_noclobber(&path).map_err(|error| error.error)?;
    Ok(path)
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
    fn local_destination_cannot_escape_the_chosen_folder() {
        let dir = Path::new("/downloads");
        assert_eq!(destination(dir, "../../file").unwrap(), dir.join("file"));
        for key in ["", ".", "..", "foo/..", "foo/", "a\0b"] {
            assert!(destination(dir, key).is_err(), "{key:?}");
        }
    }

    #[test]
    fn url_decoding_preserves_literal_plus_and_percent() {
        assert_eq!(decode_key("folder%2Fa%2Bb%252F", true).unwrap(), "folder/a+b%2F");
        assert_eq!(decode_key("a%2Bb+", false).unwrap(), "a%2Bb+");
    }

    #[test]
    fn paginates_and_downloads_via_the_sdk() {
        use std::{
            io::{BufRead, BufReader, Write},
            net::TcpListener,
            thread,
            time::{Duration, Instant},
        };

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let directory = tempfile::tempdir().unwrap();
        let occupied_after_request = directory.path().join("race.txt");
        let responder = thread::spawn(move || {
            let responses = [
                (
                    "list-type=2",
                    r#"<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>true</IsTruncated><NextContinuationToken>page-two</NextContinuationToken><CommonPrefixes><Prefix>folder%2F</Prefix></CommonPrefixes></ListBucketResult>"#,
                    None,
                ),
                (
                    "continuation-token=page-two",
                    r#"<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>false</IsTruncated><Contents><Key>file%2B%252F.txt</Key><Size>3</Size><LastModified>2026-01-01T00:00:00.000Z</LastModified></Contents></ListBucketResult>"#,
                    None,
                ),
                ("/bucket/file%2B%252F.txt", "abc", None),
                ("/bucket/truncated.txt", "short", Some(12)),
                ("/bucket/race.txt", "download", None),
            ];
            for (expected, body, length) in responses {
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
                if expected == "/bucket/race.txt" {
                    std::fs::write(&occupied_after_request, "existing").unwrap();
                }
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    length.unwrap_or(body.len()),
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
        let progress = AtomicU64::new(0);
        service.runtime.block_on(async {
            let entries = service.list(&location).await.unwrap();
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].name, "folder/");
            assert!(entries[0].kind.is_dir());
            let file = &entries[1];
            assert_eq!(file.size, Some(3));
            assert!(file.modified.is_some());
            let key = file.name.to_str().unwrap();
            let path = download_object(&client, "bucket", key, directory.path(), &progress).await.unwrap();
            assert_eq!(path.file_name().unwrap(), "file+%2F.txt");
            assert_eq!(std::fs::read(&path).unwrap(), b"abc");
            assert_eq!(progress.load(Ordering::Relaxed), 3);
            let collision = download_object(&client, "bucket", key, directory.path(), &progress).await.unwrap_err();
            assert_eq!(collision.kind(), io::ErrorKind::AlreadyExists);
            assert!(download_object(&client, "bucket", "truncated.txt", directory.path(), &progress).await.is_err());
            let race = download_object(&client, "bucket", "race.txt", directory.path(), &progress).await.unwrap_err();
            assert_eq!(race.kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(std::fs::read(directory.path().join("race.txt")).unwrap(), b"existing");
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
        });
        responder.join().unwrap();
    }
}
