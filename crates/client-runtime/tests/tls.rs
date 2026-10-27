use notryx_client_runtime::{Error, Session};
use std::{
    io::Read,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct TestServer(Child);

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn self_signed_tls_is_rejected_before_pairing_secret_is_sent() {
    let directory = tempfile::tempdir().unwrap();
    let key = directory.path().join("synthetic-key.pem");
    let cert = directory.path().join("synthetic-cert.pem");
    let generated = Command::new("openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout"])
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .args([
            "-subj",
            "/CN=127.0.0.1",
            "-addext",
            "subjectAltName=IP:127.0.0.1",
            "-days",
            "1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("openssl CLI is required for this TLS test");
    assert!(
        generated.success(),
        "synthetic certificate generation failed"
    );

    let address = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let child = Command::new("openssl")
        .arg("s_server")
        .arg("-accept")
        .arg(address.to_string())
        .arg("-cert")
        .arg(&cert)
        .arg("-key")
        .arg(&key)
        .args(["-quiet", "-state", "-naccept", "1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("openssl TLS test server failed to start");
    let mut server = TestServer(child);
    let origin = format!("https://{address}");
    let workspace = uuid::Uuid::new_v4().to_string().parse().unwrap();
    let secret = "synthetic-pairing-secret-not-for-logging";
    let mut finished = false;
    for _ in 0..50 {
        let response = tokio::time::timeout(
            Duration::from_secs(3),
            Session::pair(&origin, workspace, secret),
        )
        .await;
        assert!(
            matches!(response, Ok(Err(Error::Network))),
            "pairing must fail during TLS setup"
        );
        if server.0.try_wait().unwrap().is_some() {
            finished = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert!(
        finished,
        "TLS test server did not receive and reject a handshake"
    );
    let mut plaintext = Vec::new();
    server
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut plaintext)
        .unwrap();
    let mut diagnostics = Vec::new();
    server
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut diagnostics)
        .unwrap();
    assert!(plaintext.is_empty(), "TLS server received application data");
    assert!(
        !diagnostics
            .windows(secret.len())
            .any(|value| value == secret.as_bytes())
    );
    let diagnostics = String::from_utf8_lossy(&diagnostics).to_ascii_lowercase();
    assert!(
        diagnostics.contains("unknown ca") || diagnostics.contains("certificate unknown"),
        "expected certificate trust rejection"
    );
}
