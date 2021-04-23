use std::io::{Error, ErrorKind, Result};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

use propolis::chardev::{Source, UDSock};
use propolis::hw::uart::LpcUart;

// Represents the client side of the connection.
struct SerialConnection {
    stream: tokio::net::UnixStream,
}

impl SerialConnection {
    async fn new() -> Result<SerialConnection> {
        let stream = tokio::net::UnixStream::connect("./ttya").await?;
        Ok(SerialConnection { stream })
    }
}

/// Represents a serial connection into the VM.
pub struct Serial {
    uart: Arc<LpcUart>,
    #[allow(dead_code)]
    uds: Arc<UDSock>,
    conn: Option<SerialConnection>,
}

impl Serial {
    pub fn new(uart: Arc<LpcUart>, uds: Arc<UDSock>) -> Result<Serial> {
        Ok(Serial { uart, uds, conn: None })
    }

    pub async fn ensure_connected(&mut self) -> Result<()> {
        if self.conn.is_none() {
            self.uart.source_set_autodiscard(false);
            self.conn = Some(SerialConnection::new().await?);
        }
        Ok(())
    }

    #[allow(dead_code)]
    fn disconnect(&mut self) {
        self.uart.source_set_autodiscard(true);
        self.conn = None;
    }

    pub async fn read(&mut self) -> Result<Vec<u8>> {
        let conn = self.conn.as_mut().ok_or_else(|| {
            Error::new(ErrorKind::NotFound, "not connected".to_string())
        })?;
        let mut buf = [0u8; 1 << 12];
        let n = match conn.stream.try_read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                if e.kind() != ErrorKind::WouldBlock {
                    return Err(e);
                }
                0
            }
        };
        Ok(buf[..n].to_vec())
    }

    pub async fn write(&mut self, buf: &[u8]) -> Result<()> {
        let conn = self.conn.as_mut().ok_or_else(|| {
            Error::new(ErrorKind::NotFound, "not connected".to_string())
        })?;
        conn.stream.write_all(&buf).await?;
        Ok(())
    }
}
