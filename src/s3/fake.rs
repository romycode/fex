//! S3 falso en memoria para tests: habla HTTP/1.1 con el SDK real.
//!
//! Implementa lo que usa fex: ListObjectsV2 (con delimitador y `encoding-type=url`),
//! HEAD/GET/PUT/DELETE de objetos, CopyObject y las condiciones `If-Match`,
//! `If-None-Match: *` y `x-amz-copy-source-if-match`, con los mismos códigos que S3.

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
};

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

#[derive(Default)]
struct State {
    objects: BTreeMap<(String, String), (Vec<u8>, String)>,
    next_etag: u64,
}

impl State {
    fn store(&mut self, bucket: &str, key: &str, body: Vec<u8>) -> String {
        self.next_etag += 1;
        let etag = format!("\"{}\"", self.next_etag);
        self.objects.insert((bucket.to_owned(), key.to_owned()), (body, etag.clone()));
        etag
    }
}

#[derive(Clone)]
pub struct Server {
    pub endpoint: String,
    state: Arc<Mutex<State>>,
}

const KEY: &AsciiSet = &NON_ALPHANUMERIC.remove(b'/').remove(b'-').remove(b'_').remove(b'.').remove(b'~');

fn decode(value: &str) -> String {
    percent_decode_str(value).decode_utf8_lossy().into_owned()
}

impl Server {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server = Self {
            endpoint: format!("http://{}", listener.local_addr().unwrap()),
            state: Arc::default(),
        };
        let state = server.state.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = state.clone();
                thread::spawn(move || serve(stream, &state));
            }
        });
        server
    }

    pub fn put(&self, bucket: &str, key: &str, body: &[u8]) -> String {
        self.state.lock().unwrap().store(bucket, key, body.to_vec())
    }

    pub fn get(&self, bucket: &str, key: &str) -> Option<Vec<u8>> {
        self.state.lock().unwrap().objects.get(&(bucket.to_owned(), key.to_owned())).map(|(body, _)| body.clone())
    }

    pub fn etag(&self, bucket: &str, key: &str) -> Option<String> {
        self.state.lock().unwrap().objects.get(&(bucket.to_owned(), key.to_owned())).map(|(_, etag)| etag.clone())
    }

    /// Claves de un bucket que empiezan por `prefix`, ordenadas.
    pub fn keys(&self, bucket: &str, prefix: &str) -> Vec<String> {
        let state = self.state.lock().unwrap();
        state.objects.keys().filter(|(b, k)| b == bucket && k.starts_with(prefix)).map(|(_, k)| k.clone()).collect()
    }
}

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn param(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Request> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':')?;
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let query = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect();
    let mut request = Request { method, path: path.to_owned(), query, headers, body: Vec::new() };
    if request.header("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) {
        reader.get_mut().write_all(b"HTTP/1.1 100 Continue\r\n\r\n").ok()?;
    }
    let length: usize = request.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    request.body = vec![0; length];
    reader.read_exact(&mut request.body).ok()?;
    Some(request)
}

struct Response {
    status: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
    /// HEAD: anunciar la longitud sin enviar cuerpo.
    head_length: Option<usize>,
}

fn respond(status: &'static str, body: impl Into<Vec<u8>>) -> Response {
    Response { status, headers: Vec::new(), body: body.into(), head_length: None }
}

fn error(status: &'static str, code: &str) -> Response {
    respond(status, format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code><Message>{code}</Message></Error>"))
}

fn serve(stream: TcpStream, state: &Mutex<State>) {
    let mut reader = BufReader::new(stream);
    while let Some(request) = read_request(&mut reader) {
        let response = handle(&request, &mut state.lock().unwrap());
        let length = response.head_length.unwrap_or(response.body.len());
        let mut out = format!("HTTP/1.1 {}\r\nContent-Length: {length}\r\n", response.status);
        for (name, value) in &response.headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str("\r\n");
        let stream = reader.get_mut();
        if stream.write_all(out.as_bytes()).is_err() || stream.write_all(&response.body).is_err() {
            return;
        }
    }
}

fn handle(request: &Request, state: &mut State) -> Response {
    let path = request.path.trim_start_matches('/');
    let (bucket, raw_key) = path.split_once('/').unwrap_or((path, ""));
    let bucket = bucket.to_owned();
    let key = decode(raw_key);
    let id = (bucket.clone(), key.clone());

    match request.method.as_str() {
        "GET" if key.is_empty() => list(request, state, &bucket),
        "HEAD" | "GET" => {
            let Some((body, etag)) = state.objects.get(&id) else {
                return if request.method == "HEAD" { respond("404 Not Found", "") } else { error("404 Not Found", "NoSuchKey") };
            };
            if request.header("if-match").is_some_and(|expected| expected != etag) {
                return error("412 Precondition Failed", "PreconditionFailed");
            }
            let mut response = respond("200 OK", body.clone());
            response.headers.push(("ETag", etag.clone()));
            if request.method == "HEAD" {
                response.head_length = Some(body.len());
                response.body.clear();
            }
            response
        }
        "PUT" => {
            if request.header("if-none-match") == Some("*") && state.objects.contains_key(&id) {
                return error("412 Precondition Failed", "PreconditionFailed");
            }
            if let Some(source) = request.header("x-amz-copy-source") {
                let source = decode(source.trim_start_matches('/'));
                let (source_bucket, source_key) = source.split_once('/').unwrap_or((&source, ""));
                let Some((body, etag)) = state.objects.get(&(source_bucket.to_owned(), source_key.to_owned())).cloned()
                else {
                    return error("404 Not Found", "NoSuchKey");
                };
                if request.header("x-amz-copy-source-if-match").is_some_and(|expected| expected != etag) {
                    return error("412 Precondition Failed", "PreconditionFailed");
                }
                let etag = state.store(&bucket, &key, body);
                return respond(
                    "200 OK",
                    format!("<CopyObjectResult><ETag>{etag}</ETag><LastModified>2026-01-01T00:00:00.000Z</LastModified></CopyObjectResult>"),
                );
            }
            let etag = state.store(&bucket, &key, request.body.clone());
            let mut response = respond("200 OK", "");
            response.headers.push(("ETag", etag));
            response
        }
        "DELETE" => {
            if let Some((_, etag)) = state.objects.get(&id)
                && request.header("if-match").is_some_and(|expected| expected != etag)
            {
                return error("412 Precondition Failed", "PreconditionFailed");
            }
            state.objects.remove(&id);
            respond("204 No Content", "")
        }
        _ => error("501 Not Implemented", "NotImplemented"),
    }
}

fn list(request: &Request, state: &State, bucket: &str) -> Response {
    let prefix = request.param("prefix").unwrap_or("");
    let delimiter = request.param("delimiter");
    let encode = |key: &str| utf8_percent_encode(key, KEY).to_string();
    let mut contents = String::new();
    let mut prefixes = Vec::<String>::new();
    for ((b, key), (body, etag)) in &state.objects {
        if b != bucket || !key.starts_with(prefix) {
            continue;
        }
        if let Some(delimiter) = delimiter
            && let Some(at) = key[prefix.len()..].find(delimiter)
        {
            // Como S3: todo lo que tenga el delimitador tras el prefijo (marcadores incluidos)
            // se agrupa en CommonPrefixes.
            let common = &key[..prefix.len() + at + delimiter.len()];
            if prefixes.last().map(String::as_str) != Some(common) {
                prefixes.push(common.to_owned());
            }
            continue;
        }
        contents.push_str(&format!(
            "<Contents><Key>{}</Key><ETag>{etag}</ETag><Size>{}</Size><LastModified>2026-01-01T00:00:00.000Z</LastModified></Contents>",
            encode(key),
            body.len()
        ));
    }
    let common: String =
        prefixes.iter().map(|p| format!("<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>", encode(p))).collect();
    respond(
        "200 OK",
        format!(
            "<ListBucketResult><Name>{bucket}</Name><EncodingType>url</EncodingType><IsTruncated>false</IsTruncated>{contents}{common}</ListBucketResult>"
        ),
    )
}
