//! Real HTTP socket fixture; no Docker daemon, kube cluster, Git, or AWS.
use hops_cli::package_dev::registry::{digest, Blob, Image, RegistryClient, MANIFEST_MEDIA_TYPE};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Clone, Copy)]
enum Fault {
    None,
    PartialUpload,
    BadReadback,
    Redirect,
}

struct Server {
    port: u16,
    stop: Arc<AtomicBool>,
    writes: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn new(fault: Fault) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let writes = Arc::new(Mutex::new(Vec::new()));
        let writes_thread = writes.clone();
        let thread = std::thread::spawn(move || {
            let mut blobs = HashMap::<String, Vec<u8>>::new();
            let mut manifests = HashMap::<String, Vec<u8>>::new();
            while !stop_thread.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(stream) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap().to_owned();
                let path = parts.next().unwrap().to_owned();
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = header.split_once(':') {
                        if key.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse::<usize>().unwrap();
                        }
                    }
                }
                assert!(length < 1024 * 1024, "fixture request too large");
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                if method == "POST" || method == "PUT" {
                    writes_thread
                        .lock()
                        .unwrap()
                        .push(format!("{method} {}", path.split('?').next().unwrap()));
                }
                if path == "/v2/" {
                    respond(&mut stream, 200, &[], b"{}");
                } else if method == "HEAD" {
                    let expected = path.rsplit('/').next().unwrap();
                    let present = if path.contains("/manifests/") {
                        manifests.contains_key(expected)
                    } else {
                        blobs.contains_key(expected)
                    };
                    if present {
                        respond(
                            &mut stream,
                            200,
                            &[("Docker-Content-Digest", expected)],
                            &[],
                        );
                    } else {
                        respond(&mut stream, 404, &[], &[]);
                    }
                } else if method == "POST" {
                    let location = if matches!(fault, Fault::Redirect) {
                        "https://public.example/steal?token=synthetic-secret-canary"
                    } else {
                        "/v2/org/pkg/blobs/uploads/abc-123?_state=YWJj%3D"
                    };
                    respond(&mut stream, 202, &[("Location", location)], &[]);
                } else if method == "PUT" && path.contains("/blobs/uploads/") {
                    if matches!(fault, Fault::PartialUpload) {
                        break;
                    }
                    let expected = path.split("digest=").nth(1).unwrap();
                    assert_eq!(digest(&body), expected);
                    blobs.insert(expected.into(), body);
                    respond(
                        &mut stream,
                        201,
                        &[("Docker-Content-Digest", expected)],
                        &[],
                    );
                } else if method == "PUT" && path.contains("/manifests/") {
                    let expected = path.rsplit('/').next().unwrap();
                    assert_eq!(digest(&body), expected);
                    manifests.insert(expected.into(), body);
                    respond(
                        &mut stream,
                        201,
                        &[("Docker-Content-Digest", expected)],
                        &[],
                    );
                } else if method == "GET" && path.contains("/manifests/") {
                    let expected = path.rsplit('/').next().unwrap();
                    let bytes = manifests.get(expected).unwrap();
                    let bytes = if matches!(fault, Fault::BadReadback) {
                        b"corrupt".as_slice()
                    } else {
                        bytes.as_slice()
                    };
                    respond(
                        &mut stream,
                        200,
                        &[("Docker-Content-Digest", expected)],
                        bytes,
                    );
                } else {
                    panic!("unexpected request: {method} {path}");
                }
            }
        });
        Self {
            port,
            stop,
            writes,
            thread: Some(thread),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn respond(stream: &mut TcpStream, status: u16, headers: &[(&str, &str)], body: &[u8]) {
    write!(
        stream,
        "HTTP/1.1 {status} Fixture\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    )
    .unwrap();
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n").unwrap();
    }
    stream.write_all(b"\r\n").unwrap();
    stream.write_all(body).unwrap();
}

fn fixture() -> Image {
    let config = Blob {
        bytes: br#"{"architecture":"arm64","os":"linux"}"#.to_vec(),
        media_type: "application/vnd.oci.image.config.v1+json".into(),
    };
    let layer = Blob {
        bytes: b"fixture-layer".to_vec(),
        media_type: "application/vnd.oci.image.layer.v1.tar".into(),
    };
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2, "mediaType": MANIFEST_MEDIA_TYPE,
        "config": config.descriptor(), "layers": [layer.descriptor()]
    }))
    .unwrap();
    Image {
        manifest,
        blobs: vec![config, layer],
    }
}

#[test]
fn uploads_digest_closure_reads_it_back_and_skips_unchanged_publication() {
    let server = Server::new(Fault::None);
    let client = RegistryClient::loopback(server.port, Duration::from_secs(2)).unwrap();
    client.ready().unwrap();
    let image = fixture();
    assert!(client.publish("org/pkg", &image).unwrap());
    assert_eq!(server.writes.lock().unwrap().len(), 5); // 2 blobs (POST+PUT), manifest PUT.
    assert!(!client.publish("org/pkg", &image).unwrap());
    assert_eq!(server.writes.lock().unwrap().len(), 5);
}

#[test]
fn partial_upload_never_publishes_manifest_and_diagnostics_do_not_echo_server_data() {
    let server = Server::new(Fault::PartialUpload);
    let client = RegistryClient::loopback(server.port, Duration::from_secs(2)).unwrap();
    assert!(client.publish("org/pkg", &fixture()).is_err());
    assert!(server
        .writes
        .lock()
        .unwrap()
        .iter()
        .all(|request| !request.contains("/manifests/")));
    let server = Server::new(Fault::Redirect);
    let client = RegistryClient::loopback(server.port, Duration::from_secs(2)).unwrap();
    let error = client
        .publish("org/pkg", &fixture())
        .unwrap_err()
        .to_string();
    assert!(!error.contains("synthetic-secret-canary"));
    assert_eq!(server.writes.lock().unwrap().len(), 1);
}

#[test]
fn digest_header_alone_cannot_mask_corrupt_manifest_readback() {
    let server = Server::new(Fault::BadReadback);
    let client = RegistryClient::loopback(server.port, Duration::from_secs(2)).unwrap();
    assert!(client
        .publish("org/pkg", &fixture())
        .unwrap_err()
        .to_string()
        .contains("readback bytes"));
}

#[test]
fn malformed_artifacts_and_repository_paths_fail_before_any_upload() {
    let server = Server::new(Fault::None);
    let client = RegistryClient::loopback(server.port, Duration::from_secs(2)).unwrap();
    let mut image = fixture();
    image.blobs.clear();
    assert!(client.publish("org/pkg", &image).is_err());
    assert!(client.publish("../escape", &fixture()).is_err());
    assert!(server.writes.lock().unwrap().is_empty());
}
