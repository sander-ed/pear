//! Attach to a running session: raw-mode terminal <-> unix socket.

use crate::proto::{self, Decoder};
use crate::sys::{self, RawMode};
use crate::term;
use std::io;
use std::os::unix::io::RawFd;

pub enum Outcome {
    Detached,
    Exited(i32),
    ServerGone,
}

/// Matches the detach key sequence incrementally across reads.
struct KeyMatcher<'a> {
    seq: &'a [u8],
    pos: usize,
}

impl<'a> KeyMatcher<'a> {
    fn new(seq: &'a [u8]) -> Self {
        KeyMatcher { seq, pos: 0 }
    }

    /// Feed input. Returns (bytes to forward to the program, detach triggered).
    /// Bytes that are a *prefix* of the detach sequence are held back until
    /// we know whether the sequence completes.
    fn feed(&mut self, input: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::with_capacity(input.len());
        for &b in input {
            if b == self.seq[self.pos] {
                self.pos += 1;
                if self.pos == self.seq.len() {
                    self.pos = 0;
                    return (out, true);
                }
            } else if self.pos > 0 {
                // Mismatch: flush the held prefix, then re-evaluate this byte.
                out.extend_from_slice(&self.seq[..self.pos]);
                self.pos = 0;
                if b == self.seq[0] {
                    self.pos = 1;
                    if self.seq.len() == 1 {
                        self.pos = 0;
                        return (out, true);
                    }
                } else {
                    out.push(b);
                }
            } else {
                out.push(b);
            }
        }
        (out, false)
    }

    /// Give up on a pending partial match (e.g. after a timeout) and return the held bytes.
    fn flush(&mut self) -> Vec<u8> {
        let held = self.seq[..self.pos].to_vec();
        self.pos = 0;
        held
    }

    fn pending(&self) -> bool {
        self.pos > 0
    }
}

pub fn attach(sock_path: &str, detach_key: &[u8]) -> io::Result<Outcome> {
    let tty_in: RawFd = 0;
    let tty_out: RawFd = 1;
    if !sys::isatty(tty_in) || !sys::isatty(tty_out) {
        return Err(io::Error::other("stdin/stdout must be a terminal"));
    }

    let sock = sys::unix_connect(sock_path)?;
    sys::set_nonblocking(sock, true)?;
    let sig_fd = sys::signal_pipe(&[libc::SIGWINCH, libc::SIGTERM, libc::SIGINT, libc::SIGHUP])?;
    sys::ignore_signal(libc::SIGPIPE);

    let size = sys::get_winsize(tty_out).unwrap_or_default();
    sys::write_all(sock, &proto::encode_size(proto::C_HELLO, size))?;

    let raw = RawMode::enable(tty_in)?;
    let result = run_loop(sock, sig_fd, tty_in, tty_out, detach_key);
    drop(raw);
    sys::close(sock);
    sys::close(sig_fd);

    // Leave the user's terminal usable regardless of what the program was doing.
    let _ = sys::write_all(tty_out, term::sane_terminal());
    result
}

fn run_loop(sock: RawFd, sig_fd: RawFd, tty_in: RawFd, tty_out: RawFd, detach_key: &[u8]) -> io::Result<Outcome> {
    let mut dec = Decoder::default();
    let mut matcher = KeyMatcher::new(detach_key);
    let mut buf = vec![0u8; 64 * 1024];
    let mut sock_out: Vec<u8> = Vec::new();

    loop {
        let mut pfds = [
            libc::pollfd { fd: tty_in, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: sock, events: libc::POLLIN | if sock_out.is_empty() { 0 } else { libc::POLLOUT }, revents: 0 },
            libc::pollfd { fd: sig_fd, events: libc::POLLIN, revents: 0 },
        ];
        // If we're holding a partial detach prefix, wait at most 500ms for the rest.
        let timeout = if matcher.pending() { 500 } else { -1 };
        let n = sys::poll(&mut pfds, timeout)?;
        if n == 0 {
            let held = matcher.flush();
            if !held.is_empty() {
                sock_out.extend_from_slice(&proto::encode(proto::C_INPUT, &held));
            }
            continue;
        }

        if pfds[2].revents != 0 {
            for sig in sys::drain_signals(sig_fd) {
                match sig {
                    libc::SIGWINCH => {
                        if let Some(size) = sys::get_winsize(tty_out) {
                            sock_out.extend_from_slice(&proto::encode_size(proto::C_RESIZE, size));
                        }
                    }
                    libc::SIGHUP | libc::SIGTERM => return Ok(Outcome::Detached),
                    _ => {} // SIGINT in raw mode never fires; keep Ctrl-C for the program
                }
            }
        }

        if pfds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            match sys::read(tty_in, &mut buf) {
                Ok(0) => return Ok(Outcome::Detached),
                Ok(n) => {
                    let (fwd, detach) = matcher.feed(&buf[..n]);
                    if !fwd.is_empty() {
                        sock_out.extend_from_slice(&proto::encode(proto::C_INPUT, &fwd));
                    }
                    if detach {
                        let _ = sys::write_all(sock, &sock_out);
                        return Ok(Outcome::Detached);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }

        if pfds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            match sys::read(sock, &mut buf) {
                Ok(0) => return Ok(Outcome::ServerGone),
                Ok(n) => {
                    dec.push(&buf[..n]);
                    while let Some((kind, payload)) = dec.next()? {
                        match kind {
                            proto::S_OUTPUT => sys::write_all(tty_out, &payload)?,
                            proto::S_EXIT => {
                                let status = payload
                                    .get(..4)
                                    .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                                    .unwrap_or(-1);
                                return Ok(Outcome::Exited(status));
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return Ok(Outcome::ServerGone),
            }
        }
        if pfds[1].revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Ok(Outcome::ServerGone);
        }

        while !sock_out.is_empty() {
            match sys::write(sock, &sock_out) {
                Ok(0) => return Ok(Outcome::ServerGone),
                Ok(n) => {
                    sock_out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => return Ok(Outcome::ServerGone),
            }
        }
    }
}

/// One-shot control request (info/kill). Returns the S_INFO text if any.
pub fn control(sock_path: &str, kind: u8) -> io::Result<Option<String>> {
    let sock = sys::unix_connect(sock_path)?;
    sys::ignore_signal(libc::SIGPIPE);
    proto::send(sock, kind, &[])?;
    let mut dec = Decoder::default();
    let r = match proto::recv(sock, &mut dec) {
        Ok((proto::S_INFO, payload)) => Ok(Some(String::from_utf8_lossy(&payload).into_owned())),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::ConnectionReset => Ok(None),
        Err(e) => Err(e),
    };
    sys::close(sock);
    r
}

#[cfg(test)]
mod tests {
    use super::KeyMatcher;

    #[test]
    fn two_key_sequence() {
        let seq = [1u8, b'd'];
        let mut m = KeyMatcher::new(&seq);
        assert_eq!(m.feed(b"hello"), (b"hello".to_vec(), false));
        assert_eq!(m.feed(&[1]), (vec![], false)); // held
        assert_eq!(m.feed(b"x"), (vec![1, b'x'], false)); // flushed prefix + byte
        assert_eq!(m.feed(&[1, b'd']), (vec![], true));
        // Split across reads
        assert_eq!(m.feed(&[1]), (vec![], false));
        assert_eq!(m.feed(b"d"), (vec![], true));
        // Double prefix: C-a C-a d  -> first C-a forwarded, second begins the sequence
        assert_eq!(m.feed(&[1, 1, b'd']), (vec![1], true));
    }

    #[test]
    fn single_key() {
        let seq = [0x1cu8];
        let mut m = KeyMatcher::new(&seq);
        assert_eq!(m.feed(b"ab\x1c"), (b"ab".to_vec(), true));
    }

    #[test]
    fn flush_pending() {
        let seq = [1u8, b'd'];
        let mut m = KeyMatcher::new(&seq);
        m.feed(&[1]);
        assert!(m.pending());
        assert_eq!(m.flush(), vec![1]);
        assert!(!m.pending());
    }
}
