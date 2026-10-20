//! One NDJSON connection to herdr's API.
//!
//! herdr reads one request line per connection, then either writes one
//! response line and closes, or (for `events.subscribe`) keeps writing event
//! lines. On macOS/Linux the API is a Unix socket at `HERDR_SOCKET_PATH`; on
//! Windows it is the named pipe `\\.\pipe\` + that path, verbatim.
//!
//! Reads wait on the OS (a socket timeout, or an overlapped read on the
//! pipe) rather than polling, and always honour their deadline.

use std::io;
use std::time::Instant;

#[cfg(not(windows))]
mod unix;
#[cfg(windows)]
mod windows;

pub enum ReadLine {
    Line(Vec<u8>),
    /// No complete line before the deadline.
    Timeout,
    /// herdr closed the connection.
    Eof,
}

pub trait Stream: Send {
    fn send(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn read_line(&mut self, deadline: Instant) -> io::Result<ReadLine>;
}

pub fn connect(path: &str, deadline: Instant) -> io::Result<Box<dyn Stream>> {
    #[cfg(windows)]
    {
        Ok(Box::new(windows::PipeStream::connect(
            &pipe_name(path),
            deadline,
        )?))
    }
    #[cfg(not(windows))]
    {
        let _ = deadline; // a Unix socket connect is immediate
        Ok(Box::new(unix::SocketStream::connect(path)?))
    }
}

pub const PIPE_PREFIX: &str = r"\\.\pipe\";

/// The named pipe herdr serves for `path`; herdr uses the path verbatim.
pub fn pipe_name(path: &str) -> String {
    if path.starts_with(PIPE_PREFIX) {
        path.to_string()
    } else {
        format!("{PIPE_PREFIX}{path}")
    }
}

/// Bytes read so far, split into lines.
#[derive(Default)]
struct LineBuf {
    data: Vec<u8>,
    eof: bool,
}

impl LineBuf {
    fn pop_line(&mut self) -> Option<Vec<u8>> {
        let end = self.data.iter().position(|&b| b == b'\n')?;
        let line = self.data[..end].to_vec();
        self.data.drain(..=end);
        Some(line)
    }

    /// After herdr closed: unterminated trailing bytes as a last line, then Eof.
    fn at_eof(&mut self) -> ReadLine {
        if self.data.is_empty() {
            ReadLine::Eof
        } else {
            ReadLine::Line(std::mem::take(&mut self.data))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_prefixes_a_plain_path_verbatim() {
        assert_eq!(
            pipe_name(r"C:\Users\me\AppData\Roaming\herdr\herdr.sock"),
            r"\\.\pipe\C:\Users\me\AppData\Roaming\herdr\herdr.sock"
        );
        assert_eq!(
            pipe_name("C:/Users/Me/herdr.sock"),
            r"\\.\pipe\C:/Users/Me/herdr.sock"
        );
        assert_eq!(pipe_name(r"\\.\pipe\herdr-test"), r"\\.\pipe\herdr-test");
    }

    #[test]
    fn line_buffer_splits_lines_and_keeps_the_rest() {
        let mut buf = LineBuf::default();
        buf.data.extend_from_slice(b"one\ntwo\nthr");
        assert_eq!(buf.pop_line().as_deref(), Some(&b"one"[..]));
        assert_eq!(buf.pop_line().as_deref(), Some(&b"two"[..]));
        assert_eq!(buf.pop_line(), None);
        match buf.at_eof() {
            ReadLine::Line(rest) => assert_eq!(rest, b"thr"),
            _ => panic!("trailing bytes should come back as a line"),
        }
        assert!(matches!(buf.at_eof(), ReadLine::Eof));
    }
}
