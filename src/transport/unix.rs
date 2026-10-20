use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use super::{LineBuf, ReadLine, Stream};

const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct SocketStream {
    sock: UnixStream,
    buf: LineBuf,
    chunk: Vec<u8>,
}

impl SocketStream {
    pub fn connect(path: &str) -> io::Result<Self> {
        let sock = UnixStream::connect(path)?;
        sock.set_write_timeout(Some(WRITE_TIMEOUT))?;
        Ok(SocketStream {
            sock,
            buf: LineBuf::default(),
            chunk: vec![0; 64 * 1024],
        })
    }
}

impl Stream for SocketStream {
    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.sock.write_all(bytes)
    }

    fn read_line(&mut self, deadline: Instant) -> io::Result<ReadLine> {
        loop {
            if let Some(line) = self.buf.pop_line() {
                return Ok(ReadLine::Line(line));
            }
            if self.buf.eof {
                return Ok(self.buf.at_eof());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(ReadLine::Timeout);
            }
            self.sock.set_read_timeout(Some(remaining))?;
            match self.sock.read(&mut self.chunk) {
                Ok(0) => self.buf.eof = true,
                Ok(n) => self.buf.data.extend_from_slice(&self.chunk[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(ReadLine::Timeout);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => self.buf.eof = true,
                Err(e) => return Err(e),
            }
        }
    }
}
