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

pub fn spawn(cmd: &[String], size: WinSize, env: &[(String, String)]) -> io::Result<Child> {
    if cmd.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty command"));
    }

    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let mut ws = libc::winsize { ws_row: size.rows, ws_col: size.cols, ws_xpixel: 0, ws_ypixel: 0 };
    let r = unsafe {
        libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws)
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
        .map(|(k, v)| CString::new(format!("{k}={v}")).unwrap())
        .collect();

    let pid = sys::fork()?;
    if pid == 0 {
        // ---- child ----
        unsafe {
            libc::close(master);
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            if slave > 2 {
                libc::close(slave);
            }
            for sig in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP, libc::SIGPIPE, libc::SIGCHLD] {
                libc::signal(sig, libc::SIG_DFL);
            }
            // Unblock all signals.
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            for e in &env_c {
                libc::putenv(e.as_ptr() as *mut libc::c_char);
            }
            libc::execvp(argv_ptrs[0], argv_ptrs.as_ptr());
            // exec failed
            let msg = b"pear: exec failed\r\n";
            libc::write(2, msg.as_ptr() as *const _, msg.len());
            libc::_exit(127);
        }
    }

    sys::close(slave);
    Ok(Child { pid, master })
}

/// Non-blocking reap. Returns Some(exit status) once the child has exited.
pub fn try_wait(pid: libc::pid_t) -> Option<i32> {
    let mut status: libc::c_int = 0;
    let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if r == pid {
        Some(decode_status(status))
    } else {
        None
    }
}

/// Blocking wait.
pub fn wait(pid: libc::pid_t) -> i32 {
    let mut status: libc::c_int = 0;
    loop {
        let r = unsafe { libc::waitpid(pid, &mut status, 0) };
        if r == pid {
            return decode_status(status);
        }
        if r < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
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
