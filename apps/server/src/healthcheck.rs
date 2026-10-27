use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

pub fn run(arguments: &[OsString]) -> Result<(), &'static str> {
    let [flag, address] = arguments else {
        return Err("invalid_request");
    };
    if flag != "--address" {
        return Err("invalid_request");
    }
    let address: SocketAddr = address
        .to_str()
        .ok_or("invalid_request")?
        .parse()
        .map_err(|_| "invalid_request")?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or("healthcheck_failed")
    };
    let mut stream =
        TcpStream::connect_timeout(&address, remaining()?).map_err(|_| "healthcheck_failed")?;
    stream
        .set_write_timeout(Some(remaining()?))
        .map_err(|_| "healthcheck_failed")?;
    write!(
        stream,
        "GET /health/ready HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|_| "healthcheck_failed")?;
    let mut status_line = Vec::new();
    while status_line.len() < 1024 {
        stream
            .set_read_timeout(Some(remaining()?))
            .map_err(|_| "healthcheck_failed")?;
        let mut byte = [0];
        stream
            .read_exact(&mut byte)
            .map_err(|_| "healthcheck_failed")?;
        status_line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    let line = status_line
        .strip_suffix(b"\r\n")
        .ok_or("healthcheck_failed")?;
    let status = line
        .strip_prefix(b"HTTP/1.1 ")
        .or_else(|| line.strip_prefix(b"HTTP/1.0 "))
        .ok_or("healthcheck_failed")?;
    if status.len() < 4
        || status[0] != b'2'
        || !status[1..3].iter().all(u8::is_ascii_digit)
        || status[3] != b' '
    {
        return Err("healthcheck_failed");
    }
    Ok(())
}
