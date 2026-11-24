//! Thin, safe-ish wrappers around the handful of libc calls pear needs.

use std::ffi::CString;
use std::io;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};

pub fn errno() -> io::Error {
    io::Error::last_os_error()
}

pub fn set_nonblocking(fd: RawFd, on: bool) -> io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(errno());
        }
        let flags = if on { flags | libc::O_NONBLOCK } else { flags & !libc::O_NONBLOCK };
        if libc::fcntl(fd, libc::F_SETFL, flags) < 0 {
            return Err(errno());
        }
    }
    Ok(())
}

pub fn set_cloexec(fd: RawFd) -> io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
            return Err(errno());
        }
    }
    Ok(())
}

pub fn close(fd: RawFd) {
    unsafe {
        libc::close(fd);
    }
}

/// Read into `buf`. Returns Ok(0) on EOF, Err(WouldBlock) if nothing is ready.
pub fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = errno();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Single write attempt. Err(WouldBlock) on non-blocking fds that are full.
pub fn write(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const _, buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = errno();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Write everything, blocking (polls on EAGAIN so it also works on non-blocking fds).
pub fn write_all(fd: RawFd, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match write(fd, buf) {
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let mut pfd = [libc::pollfd { fd, events: libc::POLLOUT, revents: 0 }];
                poll(&mut pfd, -1)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub fn poll(fds: &mut [libc::pollfd], timeout_ms: i32) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = errno();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

pub fn pipe() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0 as RawFd; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(errno());
    }
    for fd in fds {
        set_cloexec(fd)?;
    }
    Ok((fds[0], fds[1]))
}

// ---------------------------------------------------------------------------
// Terminal size

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WinSize {
    pub cols: u16,
    pub rows: u16,
}

impl Default for WinSize {
    fn default() -> Self {
        WinSize { cols: 80, rows: 24 }
    }
}

pub fn get_winsize(fd: RawFd) -> Option<WinSize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } < 0 || ws.ws_col == 0 || ws.ws_row == 0 {
        return None;
    }
    Some(WinSize { cols: ws.ws_col, rows: ws.ws_row })
}

pub fn set_winsize(fd: RawFd, size: WinSize) {
    let ws = libc::winsize { ws_row: size.rows, ws_col: size.cols, ws_xpixel: 0, ws_ypixel: 0 };
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
    }
}

pub fn isatty(fd: RawFd) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

// ---------------------------------------------------------------------------
// Raw mode

pub struct RawMode {
    fd: RawFd,
    saved: libc::termios,
}

impl RawMode {
    pub fn enable(fd: RawFd) -> io::Result<RawMode> {
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } < 0 {
            return Err(errno());
        }
        let mut raw = saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } < 0 {
            return Err(errno());
        }
        Ok(RawMode { fd, saved })
    }

    pub fn restore(&self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved);
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        self.restore();
    }
}

// ---------------------------------------------------------------------------
// Signals via self-pipe

static SIG_PIPE_W: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(sig: libc::c_int) {
    let fd = SIG_PIPE_W.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = [sig as u8];
        unsafe {
            libc::write(fd, b.as_ptr() as *const _, 1);
        }
    }
}

/// Install a self-pipe handler for `signals`. Returns the read end; each byte read is a signal number.
pub fn signal_pipe(signals: &[libc::c_int]) -> io::Result<RawFd> {
    let (r, w) = pipe()?;
    set_nonblocking(r, true)?;
    set_nonblocking(w, true)?;
    SIG_PIPE_W.store(w, Ordering::Relaxed);
    for &sig in signals {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            if libc::sigaction(sig, &sa, std::ptr::null_mut()) < 0 {
                return Err(errno());
            }
        }
    }
    Ok(r)
}

pub fn ignore_signal(sig: libc::c_int) {
    unsafe {
        libc::signal(sig, libc::SIG_IGN);
    }
}

/// Drain the signal pipe and return the signals seen.
pub fn drain_signals(fd: RawFd) -> Vec<libc::c_int> {
    let mut out = Vec::new();
    let mut buf = [0u8; 64];
    while let Ok(n) = read(fd, &mut buf) {
        if n == 0 {
            break;
        }
        out.extend(buf[..n].iter().map(|&b| b as libc::c_int));
    }
    out
}

// ---------------------------------------------------------------------------
// Unix sockets

fn sockaddr_un(path: &str) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "socket path too long"));
    }
    for (dst, &src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = src as libc::c_char;
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
    Ok((addr, len as libc::socklen_t))
}

fn unix_socket() -> io::Result<RawFd> {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(errno());
    }
    set_cloexec(fd)?;
    Ok(fd)
}

pub fn unix_listen(path: &str) -> io::Result<RawFd> {
    let fd = unix_socket()?;
    let (addr, len) = sockaddr_un(path)?;
    let r = unsafe { libc::bind(fd, &addr as *const _ as *const libc::sockaddr, len) };
    if r < 0 {
        let e = errno();
        close(fd);
        return Err(e);
    }
    if unsafe { libc::listen(fd, 8) } < 0 {
        let e = errno();
        close(fd);
        return Err(e);
    }
    Ok(fd)
}

pub fn unix_connect(path: &str) -> io::Result<RawFd> {
    let fd = unix_socket()?;
    let (addr, len) = sockaddr_un(path)?;
    let r = unsafe { libc::connect(fd, &addr as *const _ as *const libc::sockaddr, len) };
    if r < 0 {
        let e = errno();
        close(fd);
        return Err(e);
    }
    Ok(fd)
}

pub fn unix_accept(listen_fd: RawFd) -> io::Result<RawFd> {
    let fd = unsafe { libc::accept(listen_fd, std::ptr::null_mut(), std::ptr::null_mut()) };
    if fd < 0 {
        return Err(errno());
    }
    set_cloexec(fd)?;
    Ok(fd)
}

// ---------------------------------------------------------------------------
// Processes

pub fn fork() -> io::Result<libc::pid_t> {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(errno());
    }
    Ok(pid)
}

pub fn setsid() -> io::Result<()> {
    if unsafe { libc::setsid() } < 0 {
        return Err(errno());
    }
    Ok(())
}

/// Redirect stdin/stdout/stderr to /dev/null (stderr optionally to a log file).
pub fn detach_stdio(log: Option<&str>) {
    let devnull = CString::new("/dev/null").unwrap();
    unsafe {
        let fd = libc::open(devnull.as_ptr(), libc::O_RDWR);
        if fd >= 0 {
            libc::dup2(fd, 0);
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
            if fd > 2 {
                libc::close(fd);
            }
        }
        if let Some(c) = log.and_then(|p| CString::new(p).ok()) {
            let lfd = libc::open(c.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND, 0o600);
            if lfd >= 0 {
                libc::dup2(lfd, 2);
                libc::close(lfd);
            }
        }
    }
}

pub fn getuid() -> u32 {
    unsafe { libc::getuid() }
}
