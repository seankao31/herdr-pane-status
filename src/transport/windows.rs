//! herdr's named pipe, opened for overlapped I/O so a read can wait on an
//! event with a timeout instead of blocking (a blocking pipe read ignores
//! timeouts, and closing the pipe under it can hang).

use std::io;
use std::ptr;
use std::time::Instant;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_PENDING, ERROR_NO_DATA,
    ERROR_PIPE_BUSY, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::{LineBuf, ReadLine, Stream};

const CHUNK: usize = 64 * 1024;

pub struct PipeStream {
    handle: HANDLE,
    read_event: HANDLE,
    write_event: HANDLE,
    // Heap-allocated: the kernel writes to both while a read is pending.
    read_ov: Box<OVERLAPPED>,
    read_data: Vec<u8>,
    read_pending: bool,
    buf: LineBuf,
}

// The handles belong to this stream alone and are only used by its owner.
unsafe impl Send for PipeStream {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn last_error() -> u32 {
    unsafe { GetLastError() }
}

fn os_error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

/// Milliseconds until `deadline`, rounded up; never INFINITE.
fn remaining_ms(deadline: Instant) -> u32 {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let ms = remaining.as_micros().div_ceil(1000);
    ms.min(u128::from(u32::MAX - 1)) as u32
}

fn is_closed(code: u32) -> bool {
    matches!(
        code,
        ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED | ERROR_HANDLE_EOF
    )
}

fn create_event() -> io::Result<HANDLE> {
    // Manual reset: ReadFile/WriteFile reset it when they start.
    let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(event)
    }
}

fn new_overlapped(event: HANDLE) -> OVERLAPPED {
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = event;
    ov
}

impl PipeStream {
    /// Connect, waiting while every pipe instance is busy. herdr needs a
    /// moment to put up the next instance after each connection, so
    /// back-to-back requests often see ERROR_PIPE_BUSY.
    pub fn connect(name: &str, deadline: Instant) -> io::Result<Self> {
        let name = wide(name);
        let handle = loop {
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                break handle;
            }
            let code = last_error();
            let remaining = remaining_ms(deadline);
            if code != ERROR_PIPE_BUSY || remaining == 0 {
                return Err(os_error(code));
            }
            // Returns once an instance is free (another client may still
            // win it, hence the loop) or the timeout passes.
            unsafe { WaitNamedPipeW(name.as_ptr(), remaining) };
        };
        let events = create_event().and_then(|read| match create_event() {
            Ok(write) => Ok((read, write)),
            Err(e) => {
                unsafe { CloseHandle(read) };
                Err(e)
            }
        });
        let (read_event, write_event) = match events {
            Ok(pair) => pair,
            Err(e) => {
                unsafe { CloseHandle(handle) };
                return Err(e);
            }
        };
        Ok(PipeStream {
            handle,
            read_event,
            write_event,
            read_ov: Box::new(new_overlapped(read_event)),
            read_data: vec![0; CHUNK],
            read_pending: false,
            buf: LineBuf::default(),
        })
    }

    fn start_read(&mut self) -> io::Result<()> {
        *self.read_ov = new_overlapped(self.read_event);
        let ok = unsafe {
            ReadFile(
                self.handle,
                self.read_data.as_mut_ptr(),
                CHUNK as u32,
                ptr::null_mut(),
                &mut *self.read_ov,
            )
        };
        if ok == 0 {
            let code = last_error();
            if is_closed(code) {
                self.buf.eof = true;
                return Ok(());
            }
            if code != ERROR_IO_PENDING {
                return Err(os_error(code));
            }
        }
        // Pending, or already complete; either way the event signals it.
        self.read_pending = true;
        Ok(())
    }

    fn finish_read(&mut self) -> io::Result<()> {
        let mut transferred = 0u32;
        let ok = unsafe { GetOverlappedResult(self.handle, &*self.read_ov, &mut transferred, 0) };
        self.read_pending = false;
        if ok == 0 {
            let code = last_error();
            if is_closed(code) {
                self.buf.eof = true;
                return Ok(());
            }
            return Err(os_error(code));
        }
        self.buf
            .data
            .extend_from_slice(&self.read_data[..transferred as usize]);
        Ok(())
    }
}

impl Stream for PipeStream {
    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut sent = 0;
        while sent < bytes.len() {
            let rest = &bytes[sent..];
            let mut ov = new_overlapped(self.write_event);
            let ok = unsafe {
                WriteFile(
                    self.handle,
                    rest.as_ptr(),
                    rest.len() as u32,
                    ptr::null_mut(),
                    &mut ov,
                )
            };
            if ok == 0 && last_error() != ERROR_IO_PENDING {
                return Err(io::Error::last_os_error());
            }
            // Wait for it: `ov` must outlive the write.
            let mut transferred = 0u32;
            if unsafe { GetOverlappedResult(self.handle, &ov, &mut transferred, 1) } == 0 {
                return Err(io::Error::last_os_error());
            }
            sent += transferred as usize;
        }
        Ok(())
    }

    fn read_line(&mut self, deadline: Instant) -> io::Result<ReadLine> {
        loop {
            if let Some(line) = self.buf.pop_line() {
                return Ok(ReadLine::Line(line));
            }
            if self.buf.eof {
                return Ok(self.buf.at_eof());
            }
            if !self.read_pending {
                self.start_read()?;
                continue;
            }
            match unsafe { WaitForSingleObject(self.read_event, remaining_ms(deadline)) } {
                WAIT_OBJECT_0 => self.finish_read()?,
                // The read stays pending; the next call waits on it again.
                WAIT_TIMEOUT => return Ok(ReadLine::Timeout),
                _ => return Err(io::Error::last_os_error()),
            }
        }
    }
}

impl Drop for PipeStream {
    fn drop(&mut self) {
        unsafe {
            if self.read_pending {
                CancelIoEx(self.handle, &*self.read_ov);
                let mut transferred = 0u32;
                GetOverlappedResult(self.handle, &*self.read_ov, &mut transferred, 1);
            }
            CloseHandle(self.handle);
            CloseHandle(self.read_event);
            CloseHandle(self.write_event);
        }
    }
}
