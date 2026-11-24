//! pear: keep a terminal program running independently of your SSH session.
//!
//! Like abduco, minus everything you don't need.

mod client;
mod config;
#[cfg(feature = "ghostty")]
mod ghostty;
mod proto;
mod pty;
mod server;
mod sys;
mod term;

use std::io;
use std::path::{Path, PathBuf};
use std::process::exit;

const USAGE: &str = "\
pear - keep a terminal program running independently of your SSH session

Usage:
  pear                         list sessions
  pear NAME [CMD...]           attach to NAME, or create it if it doesn't exist
  pear new NAME [CMD...]       create a session running CMD (default: $SHELL) and attach
  pear attach NAME             attach to an existing session
  pear ls                      list sessions
  pear kill NAME               terminate the program running in NAME

Options:
  -k, --detach-key KEY         key sequence that detaches (default: C-a d)
  -d, --detached               with `new`: create the session without attaching
  -h, --help                   show this help
  -V, --version                show version

Keys: C-x = Ctrl+x, M-x = Alt+x, plain characters are literal.
      e.g. \"C-a d\", \"C-\\\\\", \"C-q\", \"M-d\".

Config: ~/.config/pear/config, e.g.
  detach-key = C-\\\\

Sockets live in $XDG_RUNTIME_DIR/pear or /tmp/pear-UID (override with PEAR_DIR).
";

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("pear: {msg}");
    exit(1)
}

fn main() {
    let cfg = config::load().unwrap_or_else(|e| die(e));
    let mut detach_key = cfg.detach_key;
    let mut detached = false;
    let mut positional: Vec<String> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            "-V" | "--version" => {
                println!("pear {}{}", env!("CARGO_PKG_VERSION"), if cfg!(feature = "ghostty") { " (libghostty-vt)" } else { "" });
                return;
            }
            "-d" | "--detached" => detached = true,
            "-k" | "--detach-key" => {
                let v = args.next().unwrap_or_else(|| die("--detach-key needs a value"));
                detach_key = config::parse_key(&v).unwrap_or_else(|e| die(e));
            }
            "--" => {
                positional.extend(args.by_ref());
                break;
            }
            s if s.starts_with('-') && positional.is_empty() => die(format!("unknown option {s}\n\n{USAGE}")),
            _ => {
                positional.push(a);
                // Everything after the command word is the command: stop option parsing.
                positional.extend(args.by_ref());
                break;
            }
        }
    }

    let dir = config::session_dir();
    let code = match positional.first().map(String::as_str) {
        None | Some("ls") | Some("list") => cmd_ls(&dir),
        Some("new") | Some("n") | Some("create") => {
            let name = positional.get(1).unwrap_or_else(|| die("new: missing session name"));
            cmd_new(&dir, name, &positional[2..], &detach_key, detached)
        }
        Some("attach") | Some("a") => {
            let name = positional.get(1).unwrap_or_else(|| die("attach: missing session name"));
            cmd_attach(&dir, name, &detach_key)
        }
        Some("kill") | Some("k") => {
            let name = positional.get(1).unwrap_or_else(|| die("kill: missing session name"));
            cmd_kill(&dir, name)
        }
        Some(name) => {
            // `pear NAME [CMD...]`: attach or create.
            if session_alive(&sock_path(&dir, name)) {
                cmd_attach(&dir, name, &detach_key)
            } else {
                cmd_new(&dir, name, &positional[1..], &detach_key, detached)
            }
        }
    };
    exit(code);
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && name != "." && name != ".."
}

fn sock_path(dir: &Path, name: &str) -> String {
    dir.join(name).to_string_lossy().into_owned()
}

/// Is there a live server behind this socket? Removes stale sockets.
fn session_alive(path: &str) -> bool {
    match sys::unix_connect(path) {
        Ok(fd) => {
            sys::close(fd);
            true
        }
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            let _ = std::fs::remove_file(path);
            false
        }
        Err(_) => false,
    }
}

fn ensure_dir(dir: &Path) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        die(format!("cannot create {}: {e}", dir.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
}

fn cmd_ls(dir: &Path) -> i32 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        println!("no sessions");
        return 0;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    let mut any = false;
    for name in names {
        let path = sock_path(dir, &name);
        if !session_alive(&path) {
            continue;
        }
        any = true;
        match client::control(&path, proto::C_INFO) {
            Ok(Some(info)) => println!("{info}"),
            _ => println!("{name}\t(no response)"),
        }
    }
    if !any {
        println!("no sessions");
    }
    0
}

fn cmd_attach(dir: &Path, name: &str, detach_key: &[u8]) -> i32 {
    if !valid_name(name) {
        die(format!("invalid session name `{name}`"));
    }
    let path = sock_path(dir, name);
    if !session_alive(&path) {
        die(format!("no session named `{name}`"));
    }
    run_attach(&path, name, detach_key)
}

fn run_attach(path: &str, name: &str, detach_key: &[u8]) -> i32 {
    match client::attach(path, detach_key) {
        Ok(client::Outcome::Detached) => {
            eprintln!("[detached from {name}]");
            0
        }
        Ok(client::Outcome::Exited(status)) => {
            eprintln!("[{name} exited with status {status}]");
            status
        }
        Ok(client::Outcome::ServerGone) => {
            eprintln!("[{name} is gone]");
            1
        }
        Err(e) => die(format!("attach: {e}")),
    }
}

fn cmd_kill(dir: &Path, name: &str) -> i32 {
    if !valid_name(name) {
        die(format!("invalid session name `{name}`"));
    }
    let path = sock_path(dir, name);
    if !session_alive(&path) {
        die(format!("no session named `{name}`"));
    }
    match client::control(&path, proto::C_KILL) {
        Ok(_) => {
            eprintln!("[killed {name}]");
            0
        }
        Err(e) => die(format!("kill: {e}")),
    }
}

fn cmd_new(dir: &Path, name: &str, cmd: &[String], detach_key: &[u8], detached: bool) -> i32 {
    if !valid_name(name) {
        die(format!("invalid session name `{name}`"));
    }
    ensure_dir(dir);
    let path = sock_path(dir, name);
    if session_alive(&path) {
        die(format!("session `{name}` already exists (use `pear attach {name}`)"));
    }

    let cmd: Vec<String> = if cmd.is_empty() {
        vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())]
    } else {
        cmd.to_vec()
    };

    let size = sys::get_winsize(1).unwrap_or_default();

    // Daemonize: fork, child becomes the server. A pipe tells us when it's ready.
    let (ready_r, ready_w) = sys::pipe().unwrap_or_else(|e| die(e));
    let pid = sys::fork().unwrap_or_else(|e| die(e));
    if pid == 0 {
        sys::close(ready_r);
        // Second fork so the server is not a child of the user's shell at all.
        match sys::fork() {
            Ok(0) => {}
            Ok(_) => unsafe { libc::_exit(0) },
            Err(_) => unsafe { libc::_exit(1) },
        }
        let _ = sys::setsid();
        if let Ok(cwd) = std::env::current_dir() {
            let _ = std::env::set_current_dir(cwd);
        }
        match server::Server::start(name, &path, &cmd, size) {
            Ok(server) => {
                let _ = sys::write_all(ready_w, b"\0");
                sys::close(ready_w);
                sys::detach_stdio(log_path(dir, name).as_deref());
                let status = server.run();
                unsafe { libc::_exit(status.clamp(0, 255)) };
            }
            Err(e) => {
                let _ = sys::write_all(ready_w, format!("{e}").as_bytes());
                sys::close(ready_w);
                unsafe { libc::_exit(1) };
            }
        }
    }
    sys::close(ready_w);
    // Reap the intermediate child.
    unsafe {
        let mut st = 0;
        libc::waitpid(pid, &mut st, 0);
    }
    let mut msg = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        match sys::read(ready_r, &mut buf) {
            Ok(0) => break,
            Ok(n) => msg.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    sys::close(ready_r);
    if msg.is_empty() {
        die("server failed to start");
    }
    if msg[0] != 0 {
        die(format!("cannot start session: {}", String::from_utf8_lossy(&msg)));
    }

    if detached {
        eprintln!("[created {name}]");
        return 0;
    }
    eprintln!("[attached to {name}; detach with {}]", config::describe_key(detach_key));
    run_attach(&path, name, detach_key)
}

fn log_path(dir: &Path, name: &str) -> Option<String> {
    let p: PathBuf = dir.join(format!(".{name}.log"));
    Some(p.to_string_lossy().into_owned())
}
