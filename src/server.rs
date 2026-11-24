//! The per-session daemon. Owns the pty, the emulated screen, and the socket.

use crate::proto::{self, Decoder};
use crate::pty;
use crate::sys::{self, WinSize};
use crate::term::{self, Screen};
use std::io;
use std::os::unix::io::RawFd;

/// If a client can't keep up with this much buffered output, drop it.
const CLIENT_BACKLOG_LIMIT: usize = 4 * 1024 * 1024;

struct Client {
    fd: RawFd,
    dec: Decoder,
    out: Vec<u8>,
    attached: bool,
}

impl Client {
    fn queue(&mut self, kind: u8, payload: &[u8]) {
        self.out.extend_from_slice(&proto::encode(kind, payload));
    }

    /// Flush what we can without blocking. Err means the client is gone.
    fn flush(&mut self) -> io::Result<()> {
        while !self.out.is_empty() {
            match sys::write(self.fd, &self.out) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        if self.out.len() > CLIENT_BACKLOG_LIMIT {
            return Err(io::Error::other("client too slow"));
        }
        Ok(())
    }
}

pub struct Server {
    name: String,
    sock_path: String,
    listen_fd: RawFd,
    sig_fd: RawFd,
    child: pty::Child,
    screen: Box<dyn Screen>,
    size: WinSize,
    clients: Vec<Client>,
    exit_status: Option<i32>,
    cmd: String,
}

impl Server {
    /// Create the session: bind the socket and spawn the program.
    pub fn start(name: &str, sock_path: &str, cmd: &[String], size: WinSize) -> io::Result<Server> {
        let listen_fd = sys::unix_listen(sock_path)?;
        sys::set_nonblocking(listen_fd, true)?;

        let sig_fd = sys::signal_pipe(&[libc::SIGCHLD, libc::SIGTERM, libc::SIGINT, libc::SIGHUP])?;
        sys::ignore_signal(libc::SIGPIPE);

        let env = vec![("PEAR".to_string(), name.to_string())];
        let child = match pty::spawn(cmd, size, &env) {
            Ok(c) => c,
            Err(e) => {
                sys::close(listen_fd);
                let _ = std::fs::remove_file(sock_path);
                return Err(e);
            }
        };
        sys::set_nonblocking(child.master, true)?;

        Ok(Server {
            name: name.to_string(),
            sock_path: sock_path.to_string(),
            listen_fd,
            sig_fd,
            child,
            screen: term::new_screen(size),
            size,
            clients: Vec::new(),
            exit_status: None,
            cmd: cmd.join(" "),
        })
    }

    pub fn run(mut self) -> i32 {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut pfds = Vec::with_capacity(3 + self.clients.len());
            pfds.push(libc::pollfd { fd: self.listen_fd, events: libc::POLLIN, revents: 0 });
            pfds.push(libc::pollfd { fd: self.sig_fd, events: libc::POLLIN, revents: 0 });
            let pty_alive = self.exit_status.is_none();
            if pty_alive {
                pfds.push(libc::pollfd { fd: self.child.master, events: libc::POLLIN, revents: 0 });
            }
            let base = pfds.len();
            for c in &self.clients {
                let mut ev = libc::POLLIN;
                if !c.out.is_empty() {
                    ev |= libc::POLLOUT;
                }
                pfds.push(libc::pollfd { fd: c.fd, events: ev, revents: 0 });
            }

            if sys::poll(&mut pfds, -1).is_err() {
                continue;
            }

            // Signals
            if pfds[1].revents != 0 {
                for sig in sys::drain_signals(self.sig_fd) {
                    match sig {
                        libc::SIGCHLD => self.reap(),
                        libc::SIGTERM | libc::SIGINT | libc::SIGHUP => {
                            pty::kill(-self.child.pid, libc::SIGHUP);
                        }
                        _ => {}
                    }
                }
            }

            // Program output
            if pty_alive && pfds[2].revents != 0 {
                self.pump_pty(&mut buf);
            }

            // New connections
            if pfds[0].revents != 0 {
                while let Ok(fd) = sys::unix_accept(self.listen_fd) {
                    if sys::set_nonblocking(fd, true).is_ok() {
                        self.clients.push(Client { fd, dec: Decoder::default(), out: Vec::new(), attached: false });
                    } else {
                        sys::close(fd);
                    }
                }
            }

            // Client traffic
            let mut dead = Vec::new();
            for (i, pfd) in pfds[base..].iter().enumerate() {
                if pfd.revents == 0 {
                    continue;
                }
                if pfd.revents & libc::POLLIN != 0 {
                    match sys::read(pfd.fd, &mut buf) {
                        Ok(0) => {
                            dead.push(i);
                            continue;
                        }
                        Ok(n) => {
                            let data = buf[..n].to_vec();
                            if self.handle_client_data(i, &data).is_err() {
                                dead.push(i);
                                continue;
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => {
                            dead.push(i);
                            continue;
                        }
                    }
                }
                if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 && !dead.contains(&i) {
                    // Still try to flush, then drop.
                    let _ = self.clients[i].flush();
                    dead.push(i);
                }
            }
            for c in &mut self.clients {
                if c.flush().is_err() {
                    sys::close(c.fd);
                    c.fd = -1;
                }
            }
            for i in dead {
                if self.clients[i].fd >= 0 {
                    sys::close(self.clients[i].fd);
                    self.clients[i].fd = -1;
                }
            }
            self.clients.retain(|c| c.fd >= 0);

            // Done?
            if let Some(status) = self.exit_status {
                let all_flushed = self.clients.iter().all(|c| c.out.is_empty());
                if all_flushed {
                    for c in &self.clients {
                        sys::close(c.fd);
                    }
                    self.clients.clear();
                    let _ = std::fs::remove_file(&self.sock_path);
                    return status;
                }
            }
        }
    }

    /// SIGCHLD: see if the program exited.
    fn reap(&mut self) {
        if self.exit_status.is_some() {
            return;
        }
        if let Some(status) = pty::try_wait(self.child.pid) {
            self.finish(status);
        }
    }

    /// The pty hung up (EOF/EIO on the master): the program is gone or about to be.
    fn pty_hangup(&mut self) {
        if self.exit_status.is_some() {
            return;
        }
        let status = pty::try_wait(self.child.pid).unwrap_or_else(|| pty::wait(self.child.pid));
        self.finish(status);
    }

    fn finish(&mut self, status: i32) {
        if self.exit_status.is_some() {
            return;
        }
        self.exit_status = Some(status);
        // Drain whatever the program wrote before exiting.
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = sys::read(self.child.master, &mut buf) {
            if n == 0 {
                break;
            }
            self.broadcast_output(&buf[..n]);
        }
        for c in &mut self.clients {
            if c.attached {
                c.queue(proto::S_EXIT, &status.to_le_bytes());
            }
        }
    }

    fn broadcast_output(&mut self, data: &[u8]) {
        self.screen.write(data);
        let resp = self.screen.take_pty_responses();
        if !resp.is_empty() && self.exit_status.is_none() && !self.clients.iter().any(|c| c.attached) {
            // Nobody's attached, so answer terminal queries ourselves.
            let _ = sys::write_all(self.child.master, &resp);
        }
        for c in &mut self.clients {
            if c.attached {
                c.queue(proto::S_OUTPUT, data);
            }
        }
    }

    fn pump_pty(&mut self, buf: &mut [u8]) {
        loop {
            match sys::read(self.child.master, buf) {
                // macOS reports a closed slave as EOF, Linux as EIO.
                Ok(0) => {
                    self.pty_hangup();
                    break;
                }
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    self.broadcast_output(&data);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.pty_hangup();
                    break;
                }
            }
        }
    }

    fn handle_client_data(&mut self, i: usize, data: &[u8]) -> io::Result<()> {
        self.clients[i].dec.push(data);
        while let Some((kind, payload)) = self.clients[i].dec.next()? {
            match kind {
                proto::C_HELLO => {
                    if let Some(size) = proto::decode_size(&payload) {
                        self.apply_resize(size);
                    }
                    let snap = self.screen.snapshot();
                    let c = &mut self.clients[i];
                    c.attached = true;
                    c.queue(proto::S_OUTPUT, &snap);
                    if let Some(status) = self.exit_status {
                        c.queue(proto::S_EXIT, &status.to_le_bytes());
                    }
                }
                proto::C_INPUT => {
                    if self.exit_status.is_none() {
                        let _ = sys::write_all(self.child.master, &payload);
                    }
                }
                proto::C_RESIZE => {
                    if let Some(size) = proto::decode_size(&payload) {
                        self.apply_resize(size);
                    }
                }
                proto::C_KILL => {
                    if self.exit_status.is_none() {
                        // The child is a session leader, so -pid hits its whole process group.
                        pty::kill(-self.child.pid, libc::SIGHUP);
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        let status = match pty::try_wait(self.child.pid) {
                            Some(s) => s,
                            None => {
                                pty::kill(-self.child.pid, libc::SIGKILL);
                                pty::wait(self.child.pid)
                            }
                        };
                        self.finish(status);
                    }
                    return Err(io::ErrorKind::ConnectionAborted.into());
                }
                proto::C_INFO => {
                    let attached = self.clients.iter().filter(|c| c.attached).count();
                    let state = match self.exit_status {
                        None => format!("running pid {}", self.child.pid),
                        Some(s) => format!("exited {s}"),
                    };
                    let info = format!(
                        "{}\t{}x{}\t{} attached\t{}\t{}",
                        self.name, self.size.cols, self.size.rows, attached, state, self.cmd
                    );
                    self.clients[i].queue(proto::S_INFO, info.as_bytes());
                }
                _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown frame")),
            }
        }
        Ok(())
    }

    fn apply_resize(&mut self, size: WinSize) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.screen.resize(size);
        if self.exit_status.is_none() {
            // The kernel delivers SIGWINCH to the foreground process group for us.
            sys::set_winsize(self.child.master, size);
        }
    }
}
