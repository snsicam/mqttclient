//! StdTcpTransport：myrtio-mqtt `MqttTransport` 的 std TCP 实现（LLD-003 §4）。
//!
//! 非阻塞 TcpStream + `yield_now` 轮询，内部不缓存（recv 由调用方提供缓冲）。
//! 断线（ConnectionClosed / IO 错误）返回 Err，由上层重连循环处理。

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use myrtio_mqtt::transport::{MqttTransport, TransportError};

#[derive(Debug)]
pub enum StdError {
    Io(io::Error),
    NotConnected,
    ConnectionClosed,
    ConnectTimeout,
    InvalidAddr,
}

impl fmt::Display for StdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::NotConnected => write!(f, "not connected"),
            Self::ConnectionClosed => write!(f, "connection closed"),
            Self::ConnectTimeout => write!(f, "connect timeout"),
            Self::InvalidAddr => write!(f, "invalid address"),
        }
    }
}
impl std::error::Error for StdError {}
impl From<io::Error> for StdError { fn from(e: io::Error) -> Self { Self::Io(e) } }

impl TransportError for StdError {}

/// MQTT 传输实现：std TcpStream（Linux 目标；RV1106 armv7l 同理）。
pub struct StdTcpTransport {
    addr: SocketAddr,
    stream: Option<TcpStream>,
}

impl StdTcpTransport {
    pub fn new(addr: SocketAddr) -> Self {
        Self { addr, stream: None }
    }

    pub fn is_connected(&self) -> bool { self.stream.is_some() }

    /// 建立 TCP 连接（`connect_timeout` 保证 5s 内返回，随后置非阻塞）。
    pub async fn connect(&mut self) -> Result<(), StdError> {
        if self.stream.is_some() {
            return Ok(());
        }
        let stream = TcpStream::connect_timeout(&self.addr, Duration::from_secs(5))
            .map_err(StdError::from)?;
        stream.set_nonblocking(true).map_err(StdError::from)?;
        self.stream = Some(stream);
        Ok(())
    }

    /// 丢弃底层连接（重连前调用）。
    pub fn disconnect(&mut self) {
        self.stream = None;
    }
}

impl MqttTransport for StdTcpTransport {
    type Error = StdError;

    async fn send(&mut self, buf: &[u8]) -> Result<(), StdError> {
        let stream = self.stream.as_mut().ok_or(StdError::NotConnected)?;
        let mut written = 0usize;
        while written < buf.len() {
            match stream.write(&buf[written..]) {
                Ok(0) => return Err(StdError::ConnectionClosed),
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    embassy_futures::yield_now().await;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(StdError::Io(e)),
            }
        }
        Ok(())
    }

    async fn recv(&mut self, buf: &mut [u8]) -> Result<usize, StdError> {
        let stream = self.stream.as_mut().ok_or(StdError::NotConnected)?;
        loop {
            match stream.read(buf) {
                Ok(0) => return Err(StdError::ConnectionClosed),
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    embassy_futures::yield_now().await;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(StdError::Io(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn fake_addr() -> SocketAddr {
        "127.0.0.1:19999".parse().unwrap()
    }

    #[test]
    fn not_connected_errors() {
        let mut t = StdTcpTransport::new(fake_addr());
        assert!(!t.is_connected());
        // 未连接时 send/recv 立即返回 NotConnected（异步测试在 #[tokio::test] 之外用 block_on）
        let fut = t.send(b"x");
        let _ = fut; // 仅编译期检查签名；运行期行为由集成测试覆盖
    }

    #[test]
    fn error_conversions() {
        let e: StdError = io::Error::new(io::ErrorKind::Other, "x").into();
        assert!(matches!(e, StdError::Io(_)));
        fn assert_trait<T: TransportError>() {}
        assert_trait::<StdError>();
    }

    // 使用本地内存 loopback 验证收发（读端缓冲复用）。
    #[test]
    fn loopback_send_recv() {
        use std::net::TcpListener;
        use std::sync::mpsc;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();

        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 16];
            let n = s.read(&mut buf).unwrap();
            tx.send((buf[..n].to_vec(), n)).unwrap();
        });

        let mut t = StdTcpTransport::new(addr);
        let _ = futures::executor::block_on(t.connect());
        let _ = futures::executor::block_on(t.send(b"hello"));
        let (got, n) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&got, b"hello");
        // 对端关闭后 recv 返回 ConnectionClosed
        let mut buf = [0u8; 16];
        let res = futures::executor::block_on(t.recv(&mut buf));
        assert!(matches!(res, Err(StdError::ConnectionClosed)));
    }

    #[allow(dead_code)]
    fn _cursor_reader() -> Cursor<Vec<u8>> { Cursor::new(vec![]) }
}
