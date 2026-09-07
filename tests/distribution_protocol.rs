//! Optional native Distribution protocol proof. No Kubernetes or container daemon.
//! Build the pinned Distribution source to a temporary binary, then:
//! HOPS_DISTRIBUTION_BINARY=/absolute/path/registry cargo test --test distribution_protocol -- --ignored
use hops_cli::package_dev::registry::{digest, Blob, Image, RegistryClient, MANIFEST_MEDIA_TYPE};
use serde::Deserialize;
use serde_yaml::Value;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
}

fn port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start(binary: &Path, config: &Path, log: &Path) -> Process {
    let stdout = fs::File::create(log).unwrap();
    let stderr = stdout.try_clone().unwrap();
    Process(
        Command::new(binary)
            .arg("serve")
            .arg(config)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .unwrap(),
    )
}

fn curl(root: &Path, port: u16, method: &str, path: &str, trusted: bool) -> std::process::Output {
    let mut command = Command::new("curl");
    command
        .args([
            "--silent",
            "--show-error",
            "--max-time",
            "3",
            "--noproxy",
            "*",
            "--proto",
            "=https",
            "--request",
            method,
            "--header",
        ])
        .arg(format!("Accept: {MANIFEST_MEDIA_TYPE}"));
    if trusted {
        command.arg("--cacert").arg(root.join("tls.crt"));
    }
    command
        .args(["--write-out", "\n%{http_code}"])
        .arg(format!("https://localhost:{port}{path}"));
    command.output().unwrap()
}

#[test]
#[ignore = "requires HOPS_DISTRIBUTION_BINARY built from the pinned Distribution source; no cluster deployment"]
fn distribution_chart_protocol_tls_readonly_and_restart_durability() {
    let binary = PathBuf::from(
        std::env::var("HOPS_DISTRIBUTION_BINARY")
            .expect("explicit native Distribution binary required"),
    );
    assert!(binary.is_absolute() && binary.is_file());
    let root = Fixture(
        std::env::temp_dir().join(format!("hops-distribution-proof-{}", uuid::Uuid::new_v4())),
    );
    fs::create_dir_all(root.0.join("data")).unwrap();
    let certificate = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=DNS:localhost",
            "-keyout",
        ])
        .arg(root.0.join("tls.key"))
        .arg("-out")
        .arg(root.0.join("tls.crt"))
        .output()
        .unwrap();
    assert!(
        certificate.status.success(),
        "fixture certificate generation failed"
    );
    let chart = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("charts/oci-registry");
    let rendered = Command::new("helm")
        .args(["template", "registry"])
        .arg(&chart)
        .arg("-f")
        .arg(chart.join("ci/local.yaml"))
        .output()
        .unwrap();
    assert!(rendered.status.success());
    let documents: Vec<Value> = serde_yaml::Deserializer::from_slice(&rendered.stdout)
        .map(|d| Value::deserialize(d).unwrap())
        .collect();
    let config = documents.iter().find(|d| d["kind"] == "ConfigMap").unwrap();
    let write_port = port();
    let read_port = port();
    for (mode, port) in [("write", write_port), ("read", read_port)] {
        let mut value: Value =
            serde_yaml::from_str(config["data"][format!("{mode}.yml")].as_str().unwrap()).unwrap();
        value["storage"]["filesystem"]["rootdirectory"] =
            root.0.join("data").to_str().unwrap().into();
        value["http"]["addr"] = format!("127.0.0.1:{port}").into();
        if mode == "read" {
            value["http"]["tls"]["certificate"] = root.0.join("tls.crt").to_str().unwrap().into();
            value["http"]["tls"]["key"] = root.0.join("tls.key").to_str().unwrap().into();
        }
        fs::write(
            root.0.join(format!("{mode}.yaml")),
            serde_yaml::to_string(&value).unwrap(),
        )
        .unwrap();
    }
    let writer = start(
        &binary,
        &root.0.join("write.yaml"),
        &root.0.join("write.log"),
    );
    let reader = start(&binary, &root.0.join("read.yaml"), &root.0.join("read.log"));
    let client = RegistryClient::loopback(write_port, Duration::from_secs(2)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if client.ready().is_ok()
            && curl(&root.0, read_port, "GET", "/v2/", true)
                .stdout
                .ends_with(b"\n200")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "registry failed to start; fixture logs: {}",
            root.0.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let config = Blob {
        bytes: b"{}".to_vec(),
        media_type: "application/vnd.oci.image.config.v1+json".into(),
    };
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion":2, "mediaType":MANIFEST_MEDIA_TYPE, "config":config.descriptor(), "layers":[]
    })).unwrap();
    let image = Image {
        manifest,
        blobs: vec![config],
    };
    assert!(
        client.publish("org/pkg", &image).unwrap(),
        "fixture logs: {}",
        root.0.display()
    );
    assert!(!client.publish("org/pkg", &image).unwrap());
    let manifest_path = format!("/v2/org/pkg/manifests/{}", digest(&image.manifest));
    let read = curl(&root.0, read_port, "GET", &manifest_path, true);
    assert!(read.status.success());
    assert_eq!(read.stdout, [image.manifest.as_slice(), b"\n200"].concat());
    assert!(
        !curl(&root.0, read_port, "GET", &manifest_path, false)
            .status
            .success(),
        "pull must fail without trusting the fixture CA"
    );
    for (method, path) in [
        ("POST", "/v2/org/pkg/blobs/uploads/"),
        ("PATCH", "/v2/org/pkg/blobs/uploads/abc"),
        ("PUT", manifest_path.as_str()),
        ("DELETE", manifest_path.as_str()),
    ] {
        let result = curl(&root.0, read_port, method, path, true);
        assert!(result.status.success());
        // Distribution validates upload session state before method dispatch;
        // an unknown PATCH session is 404, not 405. Neither path may mutate.
        let denied = if method == "PATCH" { b"\n404" } else { b"\n405" };
        assert!(
            result.stdout.ends_with(denied),
            "{method}: {}",
            String::from_utf8_lossy(&result.stdout)
        );
    }
    drop(writer);
    drop(reader);
    let _writer = start(
        &binary,
        &root.0.join("write.yaml"),
        &root.0.join("write-restart.log"),
    );
    let _reader = start(
        &binary,
        &root.0.join("read.yaml"),
        &root.0.join("read-restart.log"),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let result = curl(&root.0, read_port, "GET", &manifest_path, true);
        if result.stdout == [image.manifest.as_slice(), b"\n200"].concat() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "digest not readable after restart; logs: {}",
            root.0.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
