//! Owned, minimal Firecracker API client: HTTP/1.1 over the VMM's Unix socket (design §10).
//!
//! One keep-alive connection, used sequentially. Requests are
//! `<METHOD> <path> HTTP/1.1` with `Host`, `Content-Type` and `Content-Length`; responses are
//! parsed from the status line, `Content-Length` and the body (Firecracker never chunks).
//! A non-2xx response becomes [`ApiError::Status`] carrying Firecracker's `fault_message`.
//!
//! The request bodies mirror `src/firecracker/swagger/firecracker.yaml` (v1.18) for the few
//! endpoints V1 uses. No hyper: ~8 fixed requests do not justify its dependency tree.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Largest accepted response head (status line + headers).
const MAX_HEAD: usize = 16 * 1024;
/// Largest accepted response body (Firecracker bodies are tiny JSON documents).
const MAX_BODY: usize = 1024 * 1024;
/// Default per-request timeout.
pub const DEFAULT_API_TIMEOUT: Duration = Duration::from_secs(5);

/// API client failure.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("firecracker API {method} {path}: {source}")]
    Io {
        method: &'static str,
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("firecracker API {method} {path} timed out after {after:?}")]
    Timeout {
        method: &'static str,
        path: String,
        after: Duration,
    },
    #[error("firecracker API {method} {path}: malformed response: {detail}")]
    Malformed {
        method: &'static str,
        path: String,
        detail: String,
    },
    #[error("firecracker API {method} {path} returned {status}: {fault}")]
    Status {
        method: &'static str,
        path: String,
        status: u16,
        /// `fault_message` from the error body, else the raw body.
        fault: String,
    },
}

/// `PUT /machine-config`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineConfig {
    pub vcpu_count: u32,
    pub mem_size_mib: u64,
    pub smt: bool,
}

/// `PUT /boot-source`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootSource {
    pub kernel_image_path: PathBuf,
    pub boot_args: String,
}

/// Block device backend of a drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IoEngine {
    Sync,
    Async,
}

/// `PUT /drives/{drive_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drive {
    pub drive_id: String,
    pub path_on_host: PathBuf,
    pub is_root_device: bool,
    pub is_read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_engine: Option<IoEngine>,
}

/// `PUT /network-interfaces/{iface_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkInterface {
    pub iface_id: String,
    pub host_dev_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_mac: Option<String>,
}

/// `PUT /vsock`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vsock {
    pub guest_cid: u32,
    pub uds_path: PathBuf,
}

/// `PUT /metrics`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metrics {
    pub metrics_path: PathBuf,
}

/// `PUT /logger`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Logger {
    pub log_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
}

/// `PUT /actions` action types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionType {
    InstanceStart,
    SendCtrlAltDel,
    FlushMetrics,
}

/// `PUT /actions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceAction {
    pub action_type: ActionType,
}

/// `GET /` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub vmm_version: String,
    #[serde(default)]
    pub app_name: String,
}

#[derive(Deserialize)]
struct Fault {
    fault_message: String,
}

/// A parsed HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Firecracker API client bound to one `api.sock`.
pub struct ApiClient {
    socket: PathBuf,
    timeout: Duration,
    conn: Option<UnixStream>,
}

impl fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiClient")
            .field("socket", &self.socket)
            .field("timeout", &self.timeout)
            .field("connected", &self.conn.is_some())
            .finish()
    }
}

impl ApiClient {
    /// A client for `socket`; connects lazily on the first request.
    #[must_use]
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            timeout: DEFAULT_API_TIMEOUT,
            conn: None,
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// `GET /`: instance info (also the liveness probe that marks `vmm_started`).
    pub async fn instance_info(&mut self) -> Result<InstanceInfo, ApiError> {
        let resp = self.request("GET", "/", None).await?;
        serde_json::from_slice(&resp.body).map_err(|e| ApiError::Malformed {
            method: "GET",
            path: "/".into(),
            detail: format!("instance info: {e}"),
        })
    }

    pub async fn put_machine_config(&mut self, body: &MachineConfig) -> Result<(), ApiError> {
        self.put("/machine-config", body).await
    }

    pub async fn put_boot_source(&mut self, body: &BootSource) -> Result<(), ApiError> {
        self.put("/boot-source", body).await
    }

    pub async fn put_drive(&mut self, body: &Drive) -> Result<(), ApiError> {
        self.put(&format!("/drives/{}", body.drive_id), body).await
    }

    pub async fn put_network_interface(&mut self, body: &NetworkInterface) -> Result<(), ApiError> {
        self.put(&format!("/network-interfaces/{}", body.iface_id), body)
            .await
    }

    pub async fn put_vsock(&mut self, body: &Vsock) -> Result<(), ApiError> {
        self.put("/vsock", body).await
    }

    pub async fn put_metrics(&mut self, body: &Metrics) -> Result<(), ApiError> {
        self.put("/metrics", body).await
    }

    pub async fn put_logger(&mut self, body: &Logger) -> Result<(), ApiError> {
        self.put("/logger", body).await
    }

    pub async fn action(&mut self, action_type: ActionType) -> Result<(), ApiError> {
        self.put("/actions", &InstanceAction { action_type }).await
    }

    async fn put<T: Serialize>(&mut self, path: &str, body: &T) -> Result<(), ApiError> {
        let json = serde_json::to_vec(body).map_err(|e| ApiError::Malformed {
            method: "PUT",
            path: path.into(),
            detail: format!("request body: {e}"),
        })?;
        self.request("PUT", path, Some(&json)).await.map(drop)
    }

    /// Send one request and return the 2xx response; anything else is an error. On any
    /// transport error the connection is dropped (the next request reconnects).
    pub async fn request(
        &mut self,
        method: &'static str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<Response, ApiError> {
        let timeout = self.timeout;
        let result = tokio::time::timeout(timeout, self.exchange(method, path, body)).await;
        let result = match result {
            Ok(r) => r,
            Err(_) => Err(ApiError::Timeout {
                method,
                path: path.into(),
                after: timeout,
            }),
        };
        match result {
            Ok(resp) if (200..300).contains(&resp.status) => Ok(resp),
            Ok(resp) => Err(ApiError::Status {
                method,
                path: path.into(),
                status: resp.status,
                fault: fault_message(&resp.body),
            }),
            Err(e) => {
                self.conn = None;
                Err(e)
            }
        }
    }

    async fn exchange(
        &mut self,
        method: &'static str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<Response, ApiError> {
        let io = |source: io::Error| ApiError::Io {
            method,
            path: path.into(),
            source,
        };
        if self.conn.is_none() {
            self.conn = Some(UnixStream::connect(&self.socket).await.map_err(io)?);
        }
        let conn = self.conn.as_mut().expect("connected above");
        let req = encode_request(method, path, body);
        conn.write_all(&req).await.map_err(io)?;
        let resp = read_response(conn).await.map_err(|e| match e {
            ReadError::Io(e) => io(e),
            ReadError::Malformed(detail) => ApiError::Malformed {
                method,
                path: path.into(),
                detail,
            },
        })?;
        if resp.close {
            self.conn = None;
        }
        Ok(Response {
            status: resp.status,
            body: resp.body,
        })
    }
}

/// Serialize one request.
#[must_use]
pub fn encode_request(method: &str, path: &str, body: Option<&[u8]>) -> Vec<u8> {
    let body = body.unwrap_or_default();
    let mut out =
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\n");
    if !body.is_empty() || method == "PUT" || method == "PATCH" {
        out.push_str("Content-Type: application/json\r\n");
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// The `fault_message` of a Firecracker error body, else the (lossy) body itself.
#[must_use]
pub fn fault_message(body: &[u8]) -> String {
    match serde_json::from_slice::<Fault>(body) {
        Ok(f) => f.fault_message,
        Err(_) if body.is_empty() => "(empty body)".into(),
        Err(_) => String::from_utf8_lossy(body).trim().to_owned(),
    }
}

#[derive(Debug)]
enum ReadError {
    Io(io::Error),
    Malformed(String),
}

struct RawResponse {
    status: u16,
    body: Vec<u8>,
    close: bool,
}

/// Read exactly one response: head up to `\r\n\r\n`, then `Content-Length` bytes. Reads one
/// byte at a time for the head so nothing past the response is consumed (keep-alive).
async fn read_response<R: tokio::io::AsyncRead + Unpin>(
    r: &mut R,
) -> Result<RawResponse, ReadError> {
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEAD {
            return Err(ReadError::Malformed(format!(
                "response head exceeds {MAX_HEAD} bytes"
            )));
        }
        let n = r.read(&mut byte).await.map_err(ReadError::Io)?;
        if n == 0 {
            return Err(ReadError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                if head.is_empty() {
                    "connection closed before a response".to_owned()
                } else {
                    "connection closed mid-response".to_owned()
                },
            )));
        }
        head.push(byte[0]);
    }
    let head = std::str::from_utf8(&head)
        .map_err(|_| ReadError::Malformed("response head is not UTF-8".into()))?;
    let (status, content_length, close) = parse_head(head).map_err(ReadError::Malformed)?;
    let len = match content_length {
        Some(n) if n > MAX_BODY => {
            return Err(ReadError::Malformed(format!(
                "body of {n} bytes exceeds {MAX_BODY}"
            )));
        }
        Some(n) => n,
        None => 0,
    };
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await.map_err(ReadError::Io)?;
    Ok(RawResponse {
        status,
        body,
        close,
    })
}

/// Parse `HTTP/1.x <code> <reason>` + headers → (status, content length, connection close).
fn parse_head(head: &str) -> Result<(u16, Option<usize>, bool), String> {
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(format!("bad status line `{status_line}`"));
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .filter(|s| (100..600).contains(s))
        .ok_or_else(|| format!("bad status code in `{status_line}`"))?;
    let mut content_length = None;
    let mut close = false;
    for line in lines.filter(|l| !l.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(format!("bad header line `{line}`"));
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| format!("bad Content-Length `{value}`"))?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(format!("unsupported Transfer-Encoding `{value}`"));
        } else if name.eq_ignore_ascii_case("connection") && value.eq_ignore_ascii_case("close") {
            close = true;
        }
    }
    Ok((status, content_length, close))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::net::UnixListener;

    use super::*;

    #[test]
    fn request_encoding() {
        let r = encode_request(
            "PUT",
            "/actions",
            Some(br#"{"action_type":"InstanceStart"}"#),
        );
        let s = String::from_utf8(r).unwrap();
        assert!(s.starts_with("PUT /actions HTTP/1.1\r\nHost: localhost\r\n"));
        assert!(s.contains("Content-Length: 31\r\n"));
        assert!(s.ends_with("\r\n\r\n{\"action_type\":\"InstanceStart\"}"));
        let g = String::from_utf8(encode_request("GET", "/", None)).unwrap();
        assert!(!g.contains("Content-Length"));
    }

    #[test]
    fn head_parsing() {
        assert_eq!(
            parse_head("HTTP/1.1 204 No Content\r\nServer: Firecracker API\r\n\r\n").unwrap(),
            (204, None, false)
        );
        assert_eq!(
            parse_head("HTTP/1.1 400 \r\ncontent-length: 12\r\nConnection: close\r\n\r\n").unwrap(),
            (400, Some(12), true)
        );
        for bad in [
            "SPDY 200 OK\r\n\r\n",
            "HTTP/1.1 abc\r\n\r\n",
            "HTTP/1.1 200 OK\r\nno-colon\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
        ] {
            assert!(parse_head(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn fault_messages() {
        assert_eq!(
            fault_message(br#"{"fault_message":"Invalid drive"}"#),
            "Invalid drive"
        );
        assert_eq!(fault_message(b"oops "), "oops");
        assert_eq!(fault_message(b""), "(empty body)");
    }

    #[test]
    fn bodies_match_swagger_field_names() {
        let d = Drive {
            drive_id: "rootfs".into(),
            path_on_host: "/r.ext4".into(),
            is_root_device: true,
            is_read_only: true,
            io_engine: Some(IoEngine::Sync),
        };
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            serde_json::json!({"drive_id":"rootfs","path_on_host":"/r.ext4",
                "is_root_device":true,"is_read_only":true,"io_engine":"Sync"})
        );
        let a = InstanceAction {
            action_type: ActionType::SendCtrlAltDel,
        };
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"{"action_type":"SendCtrlAltDel"}"#
        );
        let n = NetworkInterface {
            iface_id: "eth0".into(),
            host_dev_name: "sctap3".into(),
            guest_mac: None,
        };
        assert_eq!(
            serde_json::to_string(&n).unwrap(),
            r#"{"iface_id":"eth0","host_dev_name":"sctap3"}"#
        );
    }

    type Log = Arc<Mutex<Vec<(String, String, String)>>>;

    /// Fake API server: one connection, keep-alive; records (method, path, body). Replies
    /// 204 to PUTs, an instance-info JSON to `GET /`, 400 with a fault to `/drives/bad`,
    /// and splits replies across writes to exercise the reader.
    async fn fake_server(listener: UnixListener, log: Log) {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let log = log.clone();
            tokio::spawn(async move {
                loop {
                    let mut head = Vec::new();
                    let mut b = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if s.read(&mut b).await.unwrap_or(0) == 0 {
                            return;
                        }
                        head.push(b[0]);
                    }
                    let head = String::from_utf8(head).unwrap();
                    let mut first = head.lines().next().unwrap().split(' ');
                    let method = first.next().unwrap().to_owned();
                    let path = first.next().unwrap().to_owned();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .map_or(0, |v| v.trim().parse().unwrap());
                    let mut body = vec![0u8; len];
                    s.read_exact(&mut body).await.unwrap();
                    log.lock().unwrap().push((
                        method.clone(),
                        path.clone(),
                        String::from_utf8(body).unwrap(),
                    ));
                    let resp: String = match (method.as_str(), path.as_str()) {
                        ("GET", "/") => {
                            let b = r#"{"id":"x","state":"Not started","vmm_version":"1.18.0","app_name":"Firecracker"}"#;
                            format!(
                                "HTTP/1.1 200 \r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}",
                                b.len()
                            )
                        }
                        ("PUT", "/drives/bad") => {
                            let b = r#"{"fault_message":"Unable to open drive"}"#;
                            format!("HTTP/1.1 400 \r\nContent-Length: {}\r\n\r\n{b}", b.len())
                        }
                        ("PUT", "/close") => {
                            "HTTP/1.1 204 \r\nConnection: close\r\n\r\n".to_owned()
                        }
                        ("PUT", "/hang") => {
                            tokio::time::sleep(Duration::from_secs(10)).await;
                            return;
                        }
                        _ => "HTTP/1.1 204 \r\nServer: Firecracker API\r\n\r\n".to_owned(),
                    };
                    let (a, b) = resp.as_bytes().split_at(resp.len() / 2);
                    s.write_all(a).await.unwrap();
                    s.flush().await.unwrap();
                    tokio::task::yield_now().await;
                    s.write_all(b).await.unwrap();
                    if path == "/close" {
                        return;
                    }
                }
            });
        }
    }

    #[tokio::test]
    async fn client_against_fake_server() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("api.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let log: Log = Arc::default();
        tokio::spawn(fake_server(listener, log.clone()));

        let mut c = ApiClient::new(&sock).with_timeout(Duration::from_millis(500));
        let info = c.instance_info().await.unwrap();
        assert_eq!(info.vmm_version, "1.18.0");
        c.put_machine_config(&MachineConfig {
            vcpu_count: 2,
            mem_size_mib: 512,
            smt: false,
        })
        .await
        .unwrap();
        c.put_vsock(&Vsock {
            guest_cid: 3,
            uds_path: "/run/v.sock".into(),
        })
        .await
        .unwrap();
        c.action(ActionType::InstanceStart).await.unwrap();

        let err = c
            .put_drive(&Drive {
                drive_id: "bad".into(),
                path_on_host: "/nope".into(),
                is_root_device: false,
                is_read_only: true,
                io_engine: None,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ApiError::Status { status: 400, fault, .. } if fault == "Unable to open drive"),
            "{err}"
        );
        // The connection survives an error status (keep-alive) ...
        c.request("PUT", "/close", Some(b"{}")).await.unwrap();
        // ... and a server-side close is followed by a transparent reconnect.
        c.action(ActionType::FlushMetrics).await.unwrap();

        let err = c.request("PUT", "/hang", Some(b"{}")).await.unwrap_err();
        assert!(matches!(err, ApiError::Timeout { .. }), "{err}");
        // After a timeout the next request uses a fresh connection.
        c.instance_info().await.unwrap();

        let log = log.lock().unwrap();
        let paths: Vec<_> = log.iter().map(|(m, p, _)| format!("{m} {p}")).collect();
        assert_eq!(
            paths,
            [
                "GET /",
                "PUT /machine-config",
                "PUT /vsock",
                "PUT /actions",
                "PUT /drives/bad",
                "PUT /close",
                "PUT /actions",
                "PUT /hang",
                "GET /"
            ]
        );
        assert_eq!(
            log[1].2,
            r#"{"vcpu_count":2,"mem_size_mib":512,"smt":false}"#
        );
        assert_eq!(log[3].2, r#"{"action_type":"InstanceStart"}"#);
    }

    #[tokio::test]
    async fn missing_socket_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = ApiClient::new(dir.path().join("nope.sock"));
        let err = c.instance_info().await.unwrap_err();
        assert!(matches!(err, ApiError::Io { .. }), "{err}");
    }

    #[tokio::test]
    async fn truncated_response_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("api.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 256];
            let _ = s.read(&mut buf).await;
            s.write_all(b"HTTP/1.1 200 \r\nContent-Length: 10\r\n\r\nabc")
                .await
                .unwrap();
        });
        let mut c = ApiClient::new(&sock);
        let err = c.instance_info().await.unwrap_err();
        assert!(matches!(err, ApiError::Io { .. }), "{err}");
    }
}
