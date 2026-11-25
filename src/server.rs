//! The per-session daemon. Owns the pty, the emulated screen, and the socket.

use crate::proto::{self, Decoder};
use crate::pty;
use crate::sys::{self, WinSize};
use crate::term::{self, Screen};
use std::collections::VecDeque;
use std::io;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

/// A client this far behind gets a fresh screen snapshot instead of the backlog.
const CLIENT_BACKLOG_LIMIT: usize = 4 * 1024 * 1024;
/// How long `pear kill` waits after SIGHUP before using SIGKILL.
const KILL_GRACE: Duration = Duration::from_millis(500);

struct Client {
    id: u64,
    fd: RawFd,
    dec: Decoder,
    /// Encoded frames waiting to be written; `sent` bytes of the first are already out.
    frames: VecDeque<Vec<u8>>,
    sent: usize,
    queued: usize,
    attached: bool,
    /// Waiting for the program to exit after `pear kill`.
    killer: bool,
    size: Option<WinSize>,
    /// Attach order, so the most recent client's size wins after a detach.
    seq: u64,
}

impl Client {
    fn new(id: u64, fd: RawFd) -> Self {
        Client {
            id,
            fd,
            dec: Decoder::default(),
            frames: VecDeque::new(),
            sent: 0,
            queued: 0,
            attached: false,
            killer: false,
            size: None,
            seq: 0,
        }
    }

    fn queue(&mut self, kind: u8, payload: &[u8]) {
        let frame = proto::encode(kind, payload);
        self.queued += frame.len();
        self.frames.push_back(frame);
    }

    fn pending(&self) -> bool {
        !self.frames.is_empty()
    }

    /// Too far behind: drop the backlog (except a half-sent frame) and send
    /// the current screen instead.
    fn resync(&mut self, snapshot: &[u8]) {
        let keep = if self.sent > 0 {
            self.frames.pop_front()
        } else {
            None
        };
        self.frames.clear();
        self.queued = 0;
        if let Some(f) = keep {
            self.queued = f.len() - self.sent;
            self.frames.push_back(f);
        }
        self.queue(proto::S_OUTPUT, snapshot);
    }

    /// Flush what we can without blocking. Err means the client is gone.
    fn flush(&mut self) -> io::Result<()> {
        while let Some(front) = self.frames.front() {
            match sys::write(self.fd, &front[self.sent..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.sent += n;
                    self.queued -= n;
                    if self.sent == front.len() {
                        self.frames.pop_front();
                        self.sent = 0;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

pub struct Server {
    name: String,
    sock_path: String,
    log_path: Option<String>,
    listen_fd: RawFd,
    sig_fd: RawFd,
    child: pty::Child,
    screen: Box<dyn Screen>,
    size: WinSize,
    clients: Vec<Client>,
    exit_status: Option<i32>,
    cmd: String,
    attach_seq: u64,
    /// After `pear kill`: when to escalate to SIGKILL.
    kill_deadline: Option<Instant>,
    started: Instant,
    last_output: Option<Instant>,
    /// `pear NAME -- CMD` input waiting for the program to be ready: (client id, bytes).
    sends: Vec<(u64, Vec<u8>)>,
    next_client_id: u64,
}

/// Input sent with `pear NAME -- CMD` is typed once the program has printed
/// something (its prompt) and then been quiet for this long...
const SEND_QUIET: Duration = Duration::from_millis(200);
/// ...or this long after the session started, whichever comes first.
const SEND_MAX_WAIT: Duration = Duration::from_secs(3);

impl Server {
    /// Create the session: bind the socket and spawn the program.
    pub fn start(name: &str, sock_path: &str, cmd: &[String], size: WinSize) -> io::Result<Server> {
        let listen_fd = sys::unix_listen(sock_path)?;
        sys::set_nonblocking(listen_fd, true)?;

        let sig_fd = sys::signal_pipe(&[libc::SIGCHLD, libc::SIGTERM, libc::SIGINT, libc::SIGHUP])?;
        sys::ignore_signal(libc::SIGPIPE);

        let abs_sock = std::fs::canonicalize(sock_path).map_or_else(
            |_| sock_path.to_string(),
            |p| p.to_string_lossy().into_owned(),
        );
        let env = vec![
            ("PEAR".to_string(), name.to_string()),
            ("PEAR_SOCKET".to_string(), abs_sock),
        ];
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
            log_path: None,
            listen_fd,
            sig_fd,
            child,
            screen: term::new_screen(size),
            size,
            clients: Vec::new(),
            exit_status: None,
            cmd: cmd.join(" "),
            attach_seq: 0,
            kill_deadline: None,
            started: Instant::now(),
            last_output: None,
            sends: Vec::new(),
            next_client_id: 0,
        })
    }

    /// Remove this file (the server's stderr log) when the session ends.
    pub fn set_log_path(&mut self, path: Option<String>) {
        self.log_path = path;
    }

    pub fn run(mut self) -> i32 {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut pfds = Vec::with_capacity(3 + self.clients.len());
            pfds.push(libc::pollfd {
                fd: self.listen_fd,
                events: libc::POLLIN,
                revents: 0,
            });
            pfds.push(libc::pollfd {
                fd: self.sig_fd,
                events: libc::POLLIN,
                revents: 0,
            });
            let pty_alive = self.exit_status.is_none();
            if pty_alive {
                pfds.push(libc::pollfd {
                    fd: self.child.master,
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            let base = pfds.len();
            for c in &self.clients {
                let mut ev = libc::POLLIN;
                if c.pending() {
                    ev |= libc::POLLOUT;
                }
                pfds.push(libc::pollfd {
                    fd: c.fd,
                    events: ev,
                    revents: 0,
                });
            }

            let send_at = (!self.sends.is_empty()).then(|| self.send_ready_at());
            let timeout = [self.kill_deadline, send_at]
                .into_iter()
                .flatten()
                .min()
                .map_or(-1, |d| {
                    // Round up so we don't wake just before the deadline.
                    let left = d.saturating_duration_since(Instant::now()).as_millis() + 1;
                    i32::try_from(left).unwrap_or(i32::MAX)
                });
            if sys::poll(&mut pfds, timeout).is_err() {
                continue;
            }
            self.check_kill_deadline();

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
                        self.next_client_id += 1;
                        self.clients.push(Client::new(self.next_client_id, fd));
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
                if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
                    && !dead.contains(&i)
                {
                    // Still try to flush, then drop.
                    let _ = self.clients[i].flush();
                    dead.push(i);
                }
            }
            self.deliver_sends();
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
            let before = self.clients.iter().filter(|c| c.attached).count();
            self.clients.retain(|c| c.fd >= 0);
            if self.clients.iter().filter(|c| c.attached).count() < before {
                self.restore_size();
            }

            // Done?
            if let Some(status) = self.exit_status {
                let all_flushed = self.clients.iter().all(|c| !c.pending());
                if all_flushed {
                    for c in &self.clients {
                        sys::close(c.fd);
                    }
                    self.clients.clear();
                    let _ = std::fs::remove_file(&self.sock_path);
                    if let Some(log) = &self.log_path {
                        let _ = std::fs::remove_file(log);
                    }
                    return status;
                }
            }
        }
    }

    /// When the program is ready for sent input: it printed something (its
    /// prompt) and has been quiet since, or it has been running a while.
    fn send_ready_at(&self) -> Instant {
        let latest = self.started + SEND_MAX_WAIT;
        match self.last_output {
            Some(t) => (t + SEND_QUIET).min(latest).max(self.started),
            None => latest,
        }
    }

    /// Type queued `pear NAME -- CMD` input into the program once it's ready,
    /// then tell the sender it's done.
    fn deliver_sends(&mut self) {
        if self.sends.is_empty() {
            return;
        }
        if self.exit_status.is_some() {
            // Too late: closing the connection tells the sender.
            for (id, _) in self.sends.drain(..) {
                if let Some(c) = self.clients.iter_mut().find(|c| c.id == id) {
                    sys::close(c.fd);
                    c.fd = -1;
                }
            }
            return;
        }
        if Instant::now() < self.send_ready_at() {
            return;
        }
        for (id, bytes) in std::mem::take(&mut self.sends) {
            let _ = sys::write_all(self.child.master, &bytes);
            if let Some(c) = self.clients.iter_mut().find(|c| c.id == id) {
                c.queue(proto::S_INFO, b"sent");
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
        // The program may have closed the terminal but still be running. Don't
        // block on it; SIGCHLD will tell us when it exits.
        if let Some(status) = pty::try_wait(self.child.pid) {
            self.finish(status);
        } else if self.kill_deadline.is_none() {
            pty::kill(-self.child.pid, libc::SIGHUP);
            self.kill_deadline = Some(Instant::now() + KILL_GRACE);
        }
    }

    fn check_kill_deadline(&mut self) {
        if self.exit_status.is_some() {
            self.kill_deadline = None;
            return;
        }
        if self.kill_deadline.is_some_and(|d| Instant::now() >= d) {
            self.kill_deadline = None;
            pty::kill(-self.child.pid, libc::SIGKILL);
            pty::kill(self.child.pid, libc::SIGKILL);
        }
    }

    fn finish(&mut self, status: i32) {
        if self.exit_status.is_some() {
            return;
        }
        self.exit_status = Some(status);
        self.kill_deadline = None;
        // Drain whatever the program wrote before exiting.
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = sys::read(self.child.master, &mut buf) {
            if n == 0 {
                break;
            }
            self.broadcast_output(&buf[..n]);
        }
        let mut killers = false;
        for c in &mut self.clients {
            if c.attached {
                c.queue(proto::S_EXIT, &status.to_le_bytes());
            }
            killers |= c.killer;
        }
        if killers {
            // `pear kill` waits for this connection to close.
            for c in self.clients.iter_mut().filter(|c| c.killer) {
                sys::close(c.fd);
                c.fd = -1;
            }
            self.clients.retain(|c| c.fd >= 0);
        }
    }

    fn broadcast_output(&mut self, data: &[u8]) {
        self.screen.write(data);
        let resp = self.screen.take_pty_responses();
        if !resp.is_empty()
            && self.exit_status.is_none()
            && !self.clients.iter().any(|c| c.attached)
        {
            // Nobody's attached, so answer terminal queries ourselves.
            let _ = sys::write(self.child.master, &resp);
        }
        let mut snapshot: Option<Vec<u8>> = None;
        for c in &mut self.clients {
            if !c.attached {
                continue;
            }
            if c.queued > CLIENT_BACKLOG_LIMIT {
                let snap = snapshot.get_or_insert_with(|| self.screen.snapshot());
                c.resync(snap);
            } else {
                c.queue(proto::S_OUTPUT, data);
            }
        }
    }

    fn pump_pty(&mut self, buf: &mut [u8]) {
        // Bounded so a program that never stops writing can't starve clients
        // (and `pear kill`).
        for _ in 0..64 {
            match sys::read(self.child.master, buf) {
                // macOS reports a closed slave as EOF, Linux as EIO.
                Ok(0) => {
                    self.pty_hangup();
                    break;
                }
                Ok(n) => {
                    self.last_output = Some(Instant::now());
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
                    self.attach_seq += 1;
                    let c = &mut self.clients[i];
                    c.attached = true;
                    c.seq = self.attach_seq;
                    c.size = proto::decode_size(&payload);
                    if let Some(size) = c.size {
                        self.apply_resize(size);
                    }
                    let snap = self.screen.snapshot();
                    let c = &mut self.clients[i];
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
                        self.clients[i].size = Some(size);
                        self.clients[i].seq = {
                            self.attach_seq += 1;
                            self.attach_seq
                        };
                        self.apply_resize(size);
                    }
                }
                proto::C_KILL => {
                    if self.exit_status.is_some() {
                        return Err(io::ErrorKind::ConnectionAborted.into());
                    }
                    // The child is a session leader, so -pid hits its whole
                    // process group. Keep serving (and draining the pty) while
                    // it exits; the connection closes once it has.
                    self.clients[i].killer = true;
                    pty::kill(-self.child.pid, libc::SIGHUP);
                    self.kill_deadline
                        .get_or_insert_with(|| Instant::now() + KILL_GRACE);
                }
                proto::C_SEND => {
                    if self.exit_status.is_some() {
                        return Err(io::ErrorKind::ConnectionAborted.into());
                    }
                    let id = self.clients[i].id;
                    self.sends.push((id, payload));
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

    /// After a client detaches, follow the size of the most recently active one left.
    fn restore_size(&mut self) {
        let size = self
            .clients
            .iter()
            .filter(|c| c.attached)
            .max_by_key(|c| c.seq)
            .and_then(|c| c.size);
        if let Some(size) = size {
            self.apply_resize(size);
        }
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
