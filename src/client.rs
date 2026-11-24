//! Attach to a running session: raw-mode terminal <-> unix socket.

use crate::keys::KeyMatcher;
use crate::proto::{self, Decoder};
use crate::sys::{self, RawMode};
use crate::term;
use std::io;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

pub enum Outcome {
    Detached,
    Exited(i32),
    ServerGone,
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
    sys::close(sock);
    sys::close(sig_fd);

    // Leave the user's terminal usable regardless of what the program was doing,
    // and make sure our message starts on a fresh line.
    let _ = sys::write_all(tty_out, term::sane_terminal());
    if cursor_column(tty_in, tty_out) != Some(1) {
        let _ = sys::write_all(tty_out, b"\r\n");
    }
    drop(raw);
    result
}

/// Ask the terminal where the cursor is (DSR). Discards any other pending
/// input, such as key releases the terminal still reports in kitty mode.
fn cursor_column(tty_in: RawFd, tty_out: RawFd) -> Option<u16> {
    sys::write_all(tty_out, b"\x1b[6n").ok()?;
    let deadline = Instant::now() + Duration::from_millis(250);
    let mut reply = Vec::new();
    let mut buf = [0u8; 256];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let mut pfd = [libc::pollfd {
            fd: tty_in,
            events: libc::POLLIN,
            revents: 0,
        }];
        let ms = i32::try_from(left.as_millis()).unwrap_or(i32::MAX);
        if sys::poll(&mut pfd, ms).ok()? == 0 {
            return None;
        }
        let n = sys::read(tty_in, &mut buf).ok()?;
        if n == 0 {
            return None;
        }
        reply.extend_from_slice(&buf[..n]);
        // Reply: ESC [ row ; col R
        if let Some(end) = reply.iter().rposition(|&b| b == b'R') {
            let start = reply[..end].iter().rposition(|&b| b == 0x1b)?;
            let body = std::str::from_utf8(reply.get(start + 2..end)?).ok()?;
            return body.split_once(';')?.1.parse().ok();
        }
    }
    None
}

fn run_loop(
    sock: RawFd,
    sig_fd: RawFd,
    tty_in: RawFd,
    tty_out: RawFd,
    detach_key: &[u8],
) -> io::Result<Outcome> {
    let mut dec = Decoder::default();
    let mut matcher = KeyMatcher::new(detach_key);
    let mut buf = vec![0u8; 64 * 1024];
    let mut sock_out: Vec<u8> = Vec::new();

    loop {
        let sock_events = libc::POLLIN
            | if sock_out.is_empty() {
                0
            } else {
                libc::POLLOUT
            };
        let mut pfds = [
            libc::pollfd {
                fd: tty_in,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: sock,
                events: sock_events,
                revents: 0,
            },
            libc::pollfd {
                fd: sig_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // If we're holding a partial detach key, wait at most 500ms for the rest.
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
                                    .map_or(-1, |b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]));
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

const CONTROL_TIMEOUT: Duration = Duration::from_secs(3);

/// One-shot control request (info/kill). Returns the S_INFO text if any.
pub fn control(sock_path: &str, kind: u8) -> io::Result<Option<String>> {
    let sock = sys::unix_connect(sock_path)?;
    sys::ignore_signal(libc::SIGPIPE);
    // A stopped or foreign server must not hang `pear ls`/`kill`.
    sys::set_timeouts(sock, CONTROL_TIMEOUT)?;
    proto::send(sock, kind, &[])?;
    let mut dec = Decoder::default();
    let r = match proto::recv(sock, &mut dec) {
        Ok((proto::S_INFO, payload)) => Ok(Some(String::from_utf8_lossy(&payload).into_owned())),
        Ok(_) => Ok(None),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    };
    sys::close(sock);
    r
}
