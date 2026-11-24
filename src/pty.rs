//! Spawn a command on a fresh pseudo-terminal.

use crate::sys::{self, WinSize};
use std::ffi::CString;
use std::io;
use std::os::unix::io::RawFd;

pub struct Child {
    pub pid: libc::pid_t,
    /// Master side of the pty. Read program output here, write keystrokes here.
    pub master: RawFd,
}

/// Start `cmd` on a new pty. Fails if the command can't be executed.
pub fn spawn(cmd: &[String], size: WinSize, env: &[(String, String)]) -> io::Result<Child> {
    if cmd.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty command"));
    }

    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let mut ws = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let r = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut ws,
        )
    };
    if r < 0 {
        return Err(sys::errno());
    }
    sys::set_cloexec(master)?;

    // Prepare everything that allocates *before* fork.
    let argv: Vec<CString> = cmd
        .iter()
        .map(|s| CString::new(s.as_str()))
        .collect::<Result<_, _>>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in argument"))?;
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|c| c.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let env_c: Vec<CString> = env
        .iter()
        .map(|(k, v)| CString::new(format!("{k}={v}")))
        .collect::<Result<_, _>>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in environment"))?;
    // Close-on-exec pipe: closed without data if exec succeeds, errno otherwise.
    let (err_r, err_w) = sys::pipe()?;

    let pid = sys::fork()?;
    if pid == 0 {
        // ---- child ----
        unsafe {
            libc::close(master);
            libc::close(err_r);
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            if slave > 2 {
                libc::close(slave);
            }
            for sig in [
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGHUP,
                libc::SIGPIPE,
                libc::SIGCHLD,
            ] {
                libc::signal(sig, libc::SIG_DFL);
            }
            // Unblock all signals.
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&raw mut set);
            libc::sigprocmask(libc::SIG_SETMASK, &raw const set, std::ptr::null_mut());
            for e in &env_c {
                libc::putenv(e.as_ptr().cast_mut());
            }
            libc::execvp(argv_ptrs[0], argv_ptrs.as_ptr());
            let err = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            libc::write(err_w, (&raw const err).cast(), std::mem::size_of::<i32>());
            libc::_exit(127);
        }
    }

    sys::close(slave);
    sys::close(err_w);
    let mut buf = [0u8; 4];
    let n = sys::read(err_r, &mut buf);
    sys::close(err_r);
    if let Ok(4) = n {
        wait(pid);
        sys::close(master);
        let err = io::Error::from_raw_os_error(i32::from_ne_bytes(buf));
        return Err(io::Error::new(err.kind(), format!("{}: {err}", cmd[0])));
    }
    Ok(Child { pid, master })
}

/// Non-blocking reap. Returns Some(exit status) once the child has exited.
pub fn try_wait(pid: libc::pid_t) -> Option<i32> {
    let mut status: libc::c_int = 0;
    let r = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
    (r == pid).then(|| decode_status(status))
}

/// Blocking wait.
pub fn wait(pid: libc::pid_t) -> i32 {
    let mut status: libc::c_int = 0;
    loop {
        let r = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        if r == pid {
            return decode_status(status);
        }
        if r < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return -1;
        }
    }
}

fn decode_status(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        -1
    }
}

pub fn kill(pid: libc::pid_t, sig: libc::c_int) {
    unsafe {
        libc::kill(pid, sig);
    }
}
