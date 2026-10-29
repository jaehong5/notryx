use crate::{Error, Session, check_response, content_type, single_header};
use notryx_domain::WorkspaceEpoch;
use notryx_protocol::HeadHint;
use reqwest::{Client, Response, header};
use std::{collections::VecDeque, time::Duration};

const READ_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_READY_EVENTS: usize = 64;

pub struct EventStream {
    response: Option<Response>,
    epoch: WorkspaceEpoch,
    line: Vec<u8>,
    event: String,
    data: String,
    has_data: bool,
    event_bytes: usize,
    skip_lf: bool,
    first_line: bool,
    ready: VecDeque<HeadHint>,
    ended: bool,
}

impl Session {
    pub async fn events(&self) -> Result<EventStream, Error> {
        self.events_with_timeout(READ_TIMEOUT).await
    }

    async fn events_with_timeout(&self, timeout: Duration) -> Result<EventStream, Error> {
        let client = events_client(timeout)?;
        let response = client
            .get(
                self.origin
                    .join("/v1/events")
                    .map_err(|_| Error::InvalidRequest)?,
            )
            .header(header::AUTHORIZATION, self.authorization.clone())
            .header(header::ACCEPT, "text/event-stream")
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|_| Error::Network)?;
        let response = check_response(response).await?;
        if !content_type(&response)?
            .is_some_and(|value| value.eq_ignore_ascii_case("text/event-stream"))
        {
            return Err(Error::Contract);
        }
        if single_header(response.headers(), "content-encoding")?
            .is_some_and(|value| !value.trim().eq_ignore_ascii_case("identity"))
        {
            return Err(Error::Contract);
        }
        Ok(EventStream {
            response: Some(response),
            epoch: self.binding.workspace_epoch,
            line: Vec::new(),
            event: String::new(),
            data: String::new(),
            has_data: false,
            event_bytes: 0,
            skip_lf: false,
            first_line: true,
            ready: VecDeque::new(),
            ended: false,
        })
    }
}

fn events_client(timeout: Duration) -> Result<Client, Error> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(timeout)
        .build()
        .map_err(|_| Error::Network)
}

impl EventStream {
    pub async fn next_head(&mut self) -> Result<Option<HeadHint>, Error> {
        loop {
            if let Some(hint) = self.ready.pop_front() {
                return Ok(Some(hint));
            }
            if self.ended {
                return Ok(None);
            }
            let chunk = self.response.as_mut().ok_or(Error::Network)?.chunk().await;
            match chunk {
                Ok(Some(bytes)) => {
                    for byte in bytes {
                        if let Err(error) = self.feed(byte) {
                            self.response.take();
                            self.ready.clear();
                            self.ended = true;
                            return Err(error);
                        }
                    }
                }
                Ok(None) => {
                    self.response.take();
                    self.ended = true;
                }
                Err(_) => {
                    self.response.take();
                    self.ended = true;
                    return Err(Error::Network);
                }
            }
        }
    }

    fn feed(&mut self, byte: u8) -> Result<(), Error> {
        if self.skip_lf {
            self.skip_lf = false;
            if byte == b'\n' {
                return Ok(());
            }
        }
        self.event_bytes = self.event_bytes.checked_add(1).ok_or(Error::TooLarge)?;
        if self.event_bytes > MAX_EVENT_BYTES {
            return Err(Error::TooLarge);
        }
        if byte == b'\r' || byte == b'\n' {
            self.skip_lf = byte == b'\r';
            self.finish_line()?;
        } else {
            self.line.push(byte);
        }
        Ok(())
    }

    fn finish_line(&mut self) -> Result<(), Error> {
        let mut line = std::str::from_utf8(&self.line).map_err(|_| Error::Contract)?;
        if self.first_line {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
            self.first_line = false;
        }
        if line.is_empty() {
            if self.has_data && self.event == "head" {
                let hint: HeadHint =
                    serde_json::from_str(&self.data).map_err(|_| Error::Contract)?;
                if hint.workspace_epoch != self.epoch {
                    return Err(Error::Binding);
                }
                if self.ready.len() == MAX_READY_EVENTS {
                    let latest = self.ready.back_mut().ok_or(Error::Contract)?;
                    if hint.head_seq > latest.head_seq {
                        *latest = hint;
                    }
                } else {
                    self.ready.push_back(hint);
                }
            }
            self.event.clear();
            self.data.clear();
            self.has_data = false;
            self.event_bytes = 0;
        } else if !line.starts_with(':') {
            let (name, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match name {
                "event" => self.event = value.to_owned(),
                "data" => {
                    if self.has_data {
                        self.data.push('\n');
                    }
                    self.data.push_str(value);
                    self.has_data = true;
                }
                _ => {}
            }
        }
        self.line.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::timeout,
    };

    #[tokio::test]
    async fn parser_coalesces_excess_hints_and_still_rejects_a_different_epoch() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n").await.unwrap();
        });
        let response = events_client(Duration::from_secs(1))
            .unwrap()
            .get(format!("http://{address}/v1/events"))
            .send()
            .await
            .unwrap();
        let epoch: WorkspaceEpoch = uuid::Uuid::new_v4().to_string().parse().unwrap();
        let mut events = EventStream {
            response: Some(response),
            epoch,
            line: Vec::new(),
            event: String::new(),
            data: String::new(),
            has_data: false,
            event_bytes: 0,
            skip_lf: false,
            first_line: true,
            ready: VecDeque::new(),
            ended: false,
        };
        for seq in 0..200 {
            let event = format!(
                "event: head\ndata: {{\"workspace_epoch\":\"{epoch}\",\"head_seq\":\"{seq}\"}}\n\n"
            );
            for byte in event.bytes() {
                events.feed(byte).unwrap();
            }
        }
        assert_eq!(events.ready.len(), MAX_READY_EVENTS);
        assert_eq!(events.ready.back().unwrap().head_seq.get(), 199);
        let other = uuid::Uuid::new_v4();
        let invalid = format!(
            "event: head\ndata: {{\"workspace_epoch\":\"{other}\",\"head_seq\":\"200\"}}\n\n"
        );
        let mut error = None;
        for byte in invalid.bytes() {
            if let Err(value) = events.feed(byte) {
                error = Some(value);
                break;
            }
        }
        assert_eq!(error, Some(Error::Binding));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn heartbeat_activity_resets_idle_read_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let epoch: WorkspaceEpoch = uuid::Uuid::new_v4().to_string().parse().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
                .await
                .unwrap();
            for _ in 0..6 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                socket.write_all(b": heartbeat\n\n").await.unwrap();
            }
            let head = format!(
                "event: head\ndata: {{\"workspace_epoch\":\"{epoch}\",\"head_seq\":\"2\"}}\n\n"
            );
            socket.write_all(head.as_bytes()).await.unwrap();
        });
        let response = events_client(Duration::from_millis(200))
            .unwrap()
            .get(format!("http://{address}/v1/events"))
            .send()
            .await
            .unwrap();
        let mut events = EventStream {
            response: Some(response),
            epoch,
            line: Vec::new(),
            event: String::new(),
            data: String::new(),
            has_data: false,
            event_bytes: 0,
            skip_lf: false,
            first_line: true,
            ready: VecDeque::new(),
            ended: false,
        };
        let hint = timeout(Duration::from_secs(2), events.next_head())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(hint.head_seq.get(), 2);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn idle_read_timeout_ends_the_stream() {
        assert_eq!(READ_TIMEOUT, Duration::from_secs(45));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
        });
        let response = events_client(Duration::from_millis(50))
            .unwrap()
            .get(format!("http://{address}/v1/events"))
            .send()
            .await
            .unwrap();
        let mut events = EventStream {
            response: Some(response),
            epoch: uuid::Uuid::new_v4().to_string().parse().unwrap(),
            line: Vec::new(),
            event: String::new(),
            data: String::new(),
            has_data: false,
            event_bytes: 0,
            skip_lf: false,
            first_line: true,
            ready: VecDeque::new(),
            ended: false,
        };
        assert_eq!(
            timeout(Duration::from_secs(1), events.next_head())
                .await
                .unwrap(),
            Err(Error::Network)
        );
        assert!(events.response.is_none());
        server.abort();
    }
}
