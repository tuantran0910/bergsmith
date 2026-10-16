//! A proxy in front of Lakekeeper that can lose commit responses: it
//! forwards the commit, lets Lakekeeper apply it, and closes the client
//! connection instead of returning the answer.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use url::Url;

use super::server;

/// A response-dropping HTTP proxy for the Compose Lakekeeper.
pub(crate) struct LossyProxy {
    url: Url,
    drops: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl LossyProxy {
    /// Starts a proxy that forwards everything faithfully until told to
    /// drop responses.
    pub(crate) async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local address");
        let upstream = server();
        let upstream = format!(
            "{}:{}",
            upstream.host_str().expect("host"),
            upstream.port_or_known_default().expect("port")
        );
        let drops = Arc::new(AtomicUsize::new(0));
        let shared = Arc::clone(&drops);
        let task = tokio::spawn(async move {
            loop {
                let (client, _) = listener.accept().await.expect("accept");
                tokio::spawn(relay(client, upstream.clone(), Arc::clone(&shared)));
            }
        });
        Self {
            url: Url::parse(&format!("http://{addr}/")).expect("valid URL"),
            drops,
            task,
        }
    }

    /// The proxy's root URL, standing in for [`server`].
    pub(crate) fn url(&self) -> &Url {
        &self.url
    }

    /// Loses the responses to the next `count` commits.
    pub(crate) fn drop_commit_responses(&self, count: usize) {
        self.drops.store(count, Ordering::SeqCst);
    }
}

impl Drop for LossyProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Relays requests on one client connection, one upstream connection each.
async fn relay(mut client: TcpStream, upstream: String, drops: Arc<AtomicUsize>) {
    while let Some(request) = read_request(&mut client).await {
        let mut server = TcpStream::connect(&upstream).await.expect("upstream");
        server
            .write_all(&request.bytes)
            .await
            .expect("forward request");
        let mut response = Vec::new();
        server
            .read_to_end(&mut response)
            .await
            .expect("read response");
        let lose = request.is_commit
            && drops
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
        if lose {
            return;
        }
        if client.write_all(&response).await.is_err() {
            return;
        }
    }
}

struct Request {
    /// The request, rewritten to close its upstream connection.
    bytes: Vec<u8>,
    /// `POST .../namespaces/{namespace}/tables/{table}`.
    is_commit: bool,
}

/// Reads one request, or `None` when the client closed the connection.
async fn read_request(client: &mut TcpStream) -> Option<Request> {
    let mut buffer = Vec::new();
    let mut chunk = [0; 8192];
    let head_end = loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        let read = client.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while buffer.len() < head_end + length {
        let read = client.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_owned();
    let mut bytes = format!("{request_line}\r\n").into_bytes();
    for line in lines.filter(|line| !line.is_empty()) {
        if !line.to_ascii_lowercase().starts_with("connection:") {
            bytes.extend_from_slice(format!("{line}\r\n").as_bytes());
        }
    }
    bytes.extend_from_slice(b"connection: close\r\n\r\n");
    bytes.extend_from_slice(&buffer[head_end..head_end + length]);
    let path = request_line.split(' ').nth(1).unwrap_or_default();
    let is_commit = request_line.starts_with("POST ")
        && path
            .rsplit_once("/tables/")
            .is_some_and(|(_, table)| !table.is_empty() && !table.contains('/'));
    Some(Request { bytes, is_commit })
}
