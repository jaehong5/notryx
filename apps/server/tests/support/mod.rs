use notryx_domain::WorkspaceEpoch;
use notryx_protocol::{AdminRequest, AdminResponse, AdminResult, CONTRACT, PairRequest};
use notryx_server::admin;
use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
use notryx_server::runtime::WorkspaceRuntime;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub async fn initialized() -> (tempfile::TempDir, Config) {
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let config = Config {
        server: ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            admin_socket: directory.path().join("admin.sock"),
        },
        workspace: WorkspaceConfig {
            notes_root: directory.path().join("notes"),
            state_root: directory.path().join("state"),
        },
    };
    std::fs::create_dir(&config.workspace.notes_root).unwrap();
    std::fs::create_dir(&config.workspace.state_root).unwrap();
    WorkspaceRuntime::initialize(config.workspace.clone())
        .await
        .unwrap();
    (directory, config)
}

pub async fn pairing_request(config: &Config) -> (PairRequest, WorkspaceEpoch, String) {
    let status = admin::request(&config.server.admin_socket, &AdminRequest::Status {})
        .await
        .unwrap();
    let AdminResponse::Ok {
        result:
            AdminResult::Status {
                workspace_id,
                workspace_epoch,
                owner_principal_id,
            },
    } = status
    else {
        panic!("local admin status must identify the owner");
    };
    let response = admin::request(
        &config.server.admin_socket,
        &AdminRequest::IssuePairing {
            principal_id: owner_principal_id.clone(),
        },
    )
    .await
    .unwrap();
    let AdminResponse::Ok {
        result: AdminResult::Pairing { pairing_secret, .. },
    } = response
    else {
        panic!("local admin must issue a pairing secret");
    };
    (
        PairRequest {
            protocol: CONTRACT.into(),
            workspace_id,
            pairing_secret,
        },
        workspace_epoch,
        owner_principal_id,
    )
}

// HTTP bodies may contain credentials; deliberately omit Debug from this helper.
pub struct HttpResponse {
    pub status: u16,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

pub async fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> HttpResponse {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = TcpStream::connect(address).await.unwrap();
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        let (mut reader, mut writer) = stream.split();
        let mut wire = Vec::new();
        let (written, read) = tokio::join!(
            async {
                writer.write_all(head.as_bytes()).await?;
                writer.write_all(body).await
            },
            reader.read_to_end(&mut wire)
        );
        read.unwrap();
        let boundary = wire
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .expect("HTTP response headers must end with CRLF");
        let head = std::str::from_utf8(&wire[..boundary]).unwrap();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u16>()
            .unwrap();
        // An early rejection may close the connection before a large request finishes writing.
        assert!(written.is_ok() || status >= 400);
        let response = HttpResponse {
            status,
            headers: lines
                .map(|line| {
                    let (name, value) = line.split_once(':').unwrap();
                    (name.to_ascii_lowercase(), value.trim().to_owned())
                })
                .collect(),
            body: wire[boundary + 4..].to_vec(),
        };
        assert!(response.header("transfer-encoding").is_none());
        if let Some(length) = response.header("content-length") {
            assert_eq!(response.body.len(), length.parse::<usize>().unwrap());
        }
        response
    })
    .await
    .expect("local HTTP request must complete within five seconds")
}

pub async fn post_json(address: SocketAddr, body: &[u8]) -> HttpResponse {
    request(
        address,
        "POST",
        "/v1/pair",
        &[("Content-Type", "application/json")],
        body,
    )
    .await
}
