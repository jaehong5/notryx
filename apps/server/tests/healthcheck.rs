use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::process::{Command, Output};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

fn cli(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_notryx-server"))
        .arg("healthcheck")
        .args(arguments)
        .output()
        .unwrap()
}

fn response_test(response: Option<Vec<u8>>) -> (Output, Vec<u8>, Duration) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = Arc::clone(&stop);
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut request = Vec::new();
        while !server_stop.load(Ordering::Acquire) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut buffer = [0; 256];
                    while request.len() < 1024 && !request.ends_with(b"\r\n\r\n") {
                        match stream.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(size) => request.extend_from_slice(&buffer[..size]),
                        }
                    }
                    if let Some(response) = response {
                        let _ = stream.write_all(&response);
                    } else {
                        while !server_stop.load(Ordering::Acquire) && Instant::now() < deadline {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("test listener failed: {error}"),
            }
        }
        request
    });
    let start = Instant::now();
    let output = cli(&["--address", &address.to_string()]);
    let elapsed = start.elapsed();
    stop.store(true, Ordering::Release);
    let request = server.join().unwrap();
    (output, request, elapsed)
}

#[test]
fn ready_healthcheck_accepts_only_a_successful_http_status() {
    for status in [200, 204, 299] {
        let response = format!("HTTP/1.1 {status} Ready\r\nContent-Length: 0\r\n\r\n");
        let (output, request, _) = response_test(Some(response.into_bytes()));
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert!(request.starts_with(b"GET /health/ready HTTP/1.1\r\nHost: 127.0.0.1:"));
        assert!(request.ends_with(b"Connection: close\r\n\r\n"));
    }
}

#[test]
fn degraded_auth_errors_and_redirects_are_unhealthy() {
    for status in [301, 401, 503] {
        let response = format!("HTTP/1.1 {status} Unavailable\r\nContent-Length: 0\r\n\r\n");
        let (output, _, _) = response_test(Some(response.into_bytes()));
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn malformed_and_unbounded_status_lines_are_unhealthy() {
    for response in [
        b"HTTP/2 200 Ready\r\n\r\n".to_vec(),
        b"HTTP/1.1 2000 Ready\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 Ready\n\n".to_vec(),
        b"HTTP/1.1 200 Ready".to_vec(),
        format!("HTTP/1.1 200 {}\r\n", "x".repeat(1024)).into_bytes(),
    ] {
        let (output, _, _) = response_test(Some(response));
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn missing_invalid_and_extra_arguments_fail_without_a_config_file() {
    for arguments in [
        vec![],
        vec!["--address"],
        vec!["--url", "http://127.0.0.1:8080/health/ready"],
        vec!["--address", "localhost:8080"],
        vec!["--address", "127.0.0.1:8080", "extra"],
    ] {
        assert!(!cli(&arguments).status.success());
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address: SocketAddr = listener.local_addr().unwrap();
    drop(listener);
    assert!(!cli(&["--address", &address.to_string()]).status.success());
}

#[test]
fn unresponsive_http_peer_is_bounded() {
    let (output, _, elapsed) = response_test(None);
    assert!(!output.status.success());
    assert!(elapsed < Duration::from_secs(5));
}
