//! Wire protocol between `pear` clients and the per-session server.
//!
//! Every message is `[type: u8][len: u32 LE][payload]`. That's it.

use crate::sys;
use std::io;
use std::os::unix::io::RawFd;

// client -> server
pub const C_HELLO: u8 = 0x01; // payload: cols u16 LE, rows u16 LE  (attach)
pub const C_INPUT: u8 = 0x02; // payload: raw bytes for the program
pub const C_RESIZE: u8 = 0x03; // payload: cols u16 LE, rows u16 LE
pub const C_KILL: u8 = 0x04; // payload: none. Kill the program and exit.
pub const C_INFO: u8 = 0x05; // payload: none. Ask for a status line.

// server -> client
pub const S_OUTPUT: u8 = 0x81; // payload: raw bytes for the terminal
pub const S_EXIT: u8 = 0x82; // payload: i32 LE exit status of the program
pub const S_INFO: u8 = 0x83; // payload: utf8 text

pub const MAX_FRAME: usize = 16 * 1024 * 1024;

pub fn encode(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + payload.len());
    v.push(kind);
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

pub fn encode_size(kind: u8, size: sys::WinSize) -> Vec<u8> {
    let mut p = [0u8; 4];
    p[..2].copy_from_slice(&size.cols.to_le_bytes());
    p[2..].copy_from_slice(&size.rows.to_le_bytes());
    encode(kind, &p)
}

pub fn decode_size(p: &[u8]) -> Option<sys::WinSize> {
    if p.len() < 4 {
        return None;
    }
    let cols = u16::from_le_bytes([p[0], p[1]]);
    let rows = u16::from_le_bytes([p[2], p[3]]);
    if cols == 0 || rows == 0 {
        return None;
    }
    Some(sys::WinSize { cols, rows })
}

/// Incremental frame parser over a byte stream.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Pop the next complete frame, if any. Err if the stream is corrupt.
    pub fn next(&mut self) -> io::Result<Option<(u8, Vec<u8>)>> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let kind = self.buf[0];
        let len = u32::from_le_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if len > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        if self.buf.len() < 5 + len {
            return Ok(None);
        }
        let payload = self.buf[5..5 + len].to_vec();
        self.buf.drain(..5 + len);
        Ok(Some((kind, payload)))
    }
}

/// Blocking send of one frame.
pub fn send(fd: RawFd, kind: u8, payload: &[u8]) -> io::Result<()> {
    sys::write_all(fd, &encode(kind, payload))
}

/// Blocking receive of one frame (used by short-lived control connections).
pub fn recv(fd: RawFd, dec: &mut Decoder) -> io::Result<(u8, Vec<u8>)> {
    let mut buf = [0u8; 4096];
    loop {
        if let Some(f) = dec.next()? {
            return Ok(f);
        }
        let n = sys::read(fd, &mut buf)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed",
            ));
        }
        dec.push(&buf[..n]);
    }
}
