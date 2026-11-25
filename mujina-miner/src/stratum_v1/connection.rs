//! TCP connection management with line-delimited I/O.
//!
//! Stratum v1 uses newline-delimited JSON over TCP. This module provides a
//! wrapper around tokio's TCP stream that handles buffered reading and writing
//! of complete JSON-RPC messages. The [`Transport`] trait abstracts message
//! I/O, allowing channel-based mocks for deterministic testing.

use async_trait::async_trait;

use super::error::{StratumError, StratumResult};
use super::messages::JsonRpcMessage;
use crate::tracing::prelude::*;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpStream, lookup_host};
use tokio::time::timeout;

/// Message-level I/O for Stratum protocol.
///
/// Abstracts reading and writing JSON-RPC messages so the client can
/// run over TCP (production) or channels (tests).
#[async_trait]
pub trait Transport: Send {
    /// Read one complete JSON-RPC message.
    ///
    /// Returns `None` on clean connection close (EOF).
    async fn read_message(&mut self) -> StratumResult<Option<JsonRpcMessage>>;

    /// Write a JSON-RPC message.
    async fn write_message(&mut self, msg: &JsonRpcMessage) -> StratumResult<()>;
}

/// Buffered TCP connection for Stratum protocol.
///
/// Wraps a TCP stream with buffered readers/writers optimized for
/// line-delimited JSON messages. Messages are automatically serialized
/// and deserialized, with newlines added/stripped.
pub struct Connection {
    /// Buffered reader for incoming messages
    reader: BufReader<OwnedReadHalf>,

    /// Buffered writer for outgoing messages
    writer: BufWriter<OwnedWriteHalf>,

    /// Line buffer for reading messages
    line_buf: String,
}

impl Connection {
    /// Create a new connection from a TCP stream.
    pub fn new(stream: TcpStream) -> Self {
        // Split the stream for independent reading and writing
        let (read_half, write_half) = stream.into_split();

        Self {
            reader: BufReader::new(read_half),
            writer: BufWriter::new(write_half),
            line_buf: String::with_capacity(4096),
        }
    }

    /// Connect to a Stratum pool.
    ///
    /// Parses the URL, establishes TCP connection, and wraps it in a buffered
    /// connection. Supports both `stratum+tcp://` and plain `tcp://` schemes.
    pub async fn connect(url: &str) -> StratumResult<Self> {
        let hostport = url
            .strip_prefix("stratum+tcp://")
            .or_else(|| url.strip_prefix("stratum+ssl://"))
            .or_else(|| url.strip_prefix("tcp://"))
            .unwrap_or(url);
        let (host, port) = split_host_port(hostport)
            .ok_or_else(|| StratumError::ConnectionFailed(format!("dns: bad pool url {url}")))?;

        debug!(url = %url, "Connecting to pool");

        let addrs = if host.parse::<IpAddr>().is_ok() {
            vec![SocketAddr::new(host.parse().unwrap(), port)]
        } else {
            let looked = lookup_host((host.as_str(), port))
                .await
                .map_err(|e| StratumError::ConnectionFailed(format!("dns: {e}")))?;
            prefer_ipv4(looked.collect())
        };
        if addrs.is_empty() {
            return Err(StratumError::ConnectionFailed(format!(
                "dns: {host} has no addresses"
            )));
        }

        let mut last = format!("dns: {host} has no addresses");
        for addr in addrs {
            match timeout(Duration::from_secs(8), TcpStream::connect(addr)).await {
                Ok(Ok(stream)) => {
                    debug!(%addr, "Connected to pool");
                    return Ok(Self::new(stream));
                }
                Ok(Err(e)) => last = format!("connect {addr}: {e}"),
                Err(_) => last = format!("connect {addr}: timed out"),
            }
            debug!(%addr, error = %last, "Pool address failed");
        }
        Err(StratumError::ConnectionFailed(last))
    }
}

/// IPv4 addresses first, then IPv6, keeping each family's original order.
pub(crate) fn prefer_ipv4(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for addr in addrs {
        if addr.is_ipv4() {
            v4.push(addr);
        } else {
            v6.push(addr);
        }
    }
    v4.extend(v6);
    v4
}

fn split_host_port(hostport: &str) -> Option<(String, u16)> {
    if let Some(rest) = hostport.strip_prefix('[') {
        let (host, port) = rest.split_once("]:")?;
        return Some((host.to_string(), port.parse().ok()?));
    }
    let (host, port) = hostport.rsplit_once(':')?;
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port.parse().ok()?))
}

#[async_trait]
impl Transport for Connection {
    async fn read_message(&mut self) -> StratumResult<Option<JsonRpcMessage>> {
        loop {
            self.line_buf.clear();

            let n = self
                .reader
                .read_line(&mut self.line_buf)
                .await
                .map_err(StratumError::Io)?;

            if n == 0 {
                // EOF - connection closed
                return Ok(None);
            }

            let line = self.line_buf.trim();
            if line.is_empty() {
                // Empty line, skip and read next
                continue;
            }

            trace!(rx = %line, "Received message");

            let msg = serde_json::from_str(line).map_err(|e| {
                StratumError::InvalidMessage(format!("Failed to parse JSON: {}, line: {}", e, line))
            })?;

            return Ok(Some(msg));
        }
    }

    async fn write_message(&mut self, msg: &JsonRpcMessage) -> StratumResult<()> {
        let json = serde_json::to_string(msg)?;
        trace!(tx = %json, "Sending message");

        self.writer.write_all(json.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;

        Ok(())
    }
}

/// Forwards `Transport` methods through a `Box<dyn Transport>`.
#[async_trait]
impl Transport for Box<dyn Transport> {
    async fn read_message(&mut self) -> StratumResult<Option<JsonRpcMessage>> {
        (**self).read_message().await
    }

    async fn write_message(&mut self, msg: &JsonRpcMessage) -> StratumResult<()> {
        (**self).write_message(msg).await
    }
}

/// Factory for creating transport connections.
///
/// Production code uses [`TcpConnector`] (TCP via [`Connection::connect`]);
/// tests use [`MockConnector`] to inject channel-backed transports.
#[async_trait]
pub trait Connector: Send {
    /// Create a new transport connection.
    async fn connect(&mut self) -> StratumResult<Box<dyn Transport>>;
}

/// Connects to a Stratum pool over TCP.
pub struct TcpConnector {
    url: String,
}

impl TcpConnector {
    pub fn new(url: String) -> Self {
        Self { url }
    }
}

#[async_trait]
impl Connector for TcpConnector {
    async fn connect(&mut self) -> StratumResult<Box<dyn Transport>> {
        let conn = Connection::connect(&self.url).await?;
        Ok(Box::new(conn))
    }
}

/// Channel-based transport backed by tokio mpsc channels instead of TCP.
///
/// Create a pair with [`MockTransport::pair()`]; the transport is the
/// client's side, the handle is the test's side.
#[cfg(test)]
pub(crate) struct MockTransport {
    rx: tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
    tx: tokio::sync::mpsc::UnboundedSender<JsonRpcMessage>,
}

/// Test-side handle for a [`MockTransport`].
///
/// Use `send()` to feed messages to the client and `recv()` to read
/// messages the client wrote.
#[cfg(test)]
pub(crate) struct MockTransportHandle {
    tx: tokio::sync::mpsc::UnboundedSender<JsonRpcMessage>,
    rx: tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>,
}

#[cfg(test)]
impl MockTransport {
    /// Create a linked (transport, handle) pair.
    pub fn pair() -> (Self, MockTransportHandle) {
        let (client_tx, handle_rx) = tokio::sync::mpsc::unbounded_channel();
        let (handle_tx, client_rx) = tokio::sync::mpsc::unbounded_channel();

        let transport = MockTransport {
            rx: client_rx,
            tx: client_tx,
        };
        let handle = MockTransportHandle {
            tx: handle_tx,
            rx: handle_rx,
        };
        (transport, handle)
    }
}

#[cfg(test)]
#[async_trait]
impl Transport for MockTransport {
    async fn read_message(&mut self) -> StratumResult<Option<JsonRpcMessage>> {
        match self.rx.recv().await {
            Some(msg) => Ok(Some(msg)),
            None => Ok(None),
        }
    }

    async fn write_message(&mut self, msg: &JsonRpcMessage) -> StratumResult<()> {
        self.tx
            .send(msg.clone())
            .map_err(|_| StratumError::Disconnected)
    }
}

#[cfg(test)]
impl MockTransportHandle {
    /// Send a message to the client.
    pub fn send(&self, msg: JsonRpcMessage) {
        self.tx.send(msg).expect("transport dropped");
    }

    /// Receive a message the client wrote.
    pub async fn recv(&mut self) -> JsonRpcMessage {
        self.rx.recv().await.expect("transport dropped")
    }
}

/// Connector that pulls pre-built transports from a channel.
///
/// Each call to `connect()` receives the next `MockTransport` from the
/// channel, letting tests supply exactly the transports they need.
#[cfg(test)]
pub(crate) struct MockConnector {
    rx: tokio::sync::mpsc::Receiver<MockTransport>,
}

#[cfg(test)]
impl MockConnector {
    pub fn new(rx: tokio::sync::mpsc::Receiver<MockTransport>) -> Self {
        Self { rx }
    }
}

#[cfg(test)]
#[async_trait]
impl Connector for MockConnector {
    async fn connect(&mut self) -> StratumResult<Box<dyn Transport>> {
        match self.rx.recv().await {
            Some(transport) => Ok(Box::new(transport)),
            None => Err(StratumError::ConnectionFailed(
                "no more mock transports".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use tokio::net::TcpListener;

    #[test]
    fn hostname_pool_tries_ipv4_before_ipv6() {
        let v4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(166, 117, 239, 148)), 3333);
        let v6a = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 3333);
        let v6b = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 3333);
        let ordered = prefer_ipv4(vec![v6a, v4, v6b]);
        assert!(ordered[0].is_ipv4());
        assert_eq!(ordered.len(), 3);
        assert!(split_host_port("solo.atlaspool.io:3333").unwrap().0 == "solo.atlaspool.io");
        assert_eq!(split_host_port("solo.atlaspool.io:3333").unwrap().1, 3333);
    }

    #[tokio::test]
    async fn test_message_roundtrip() {
        // Create a local test server
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Spawn server task
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut conn = Connection::new(socket);

            // Echo messages back
            while let Ok(Some(msg)) = conn.read_message().await {
                conn.write_message(&msg).await.unwrap();
            }
        });

        // Connect client
        let stream = TcpStream::connect(addr).await.unwrap();
        let mut conn = Connection::new(stream);

        // Send a message
        let request = JsonRpcMessage::request(1, "test.method", json!(["param1", "param2"]));
        conn.write_message(&request).await.unwrap();

        // Read it back
        let response = conn.read_message().await.unwrap().unwrap();
        assert_eq!(response.id(), Some(1));
        assert_eq!(response.method(), Some("test.method"));
    }
}
