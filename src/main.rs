//! pear: keep a terminal program running independently of your SSH session.
//!
//! Like abduco, minus everything you don't need.

mod client;
mod config;
#[cfg(feature = "ghostty")]
mod ghostty;
mod keys;
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
  pear                         start a new session named 1, 2, 3, ... and attach
  pear NAME [CMD...]           attach to NAME, or create it if it doesn't exist
  pear new [-d] [NAME] [CMD...]
                               create a session running CMD (default: $SHELL);
                               without NAME, use the next free number
  pear [new] [-d] -- CMD...    run CMD in a new numbered session
  pear NAME [-d] -- CMD...     type CMD into NAME's shell (creating NAME
                               if needed), then attach unless -d
  pear attach NAME             attach to an existing session
  pear ls                      list sessions
  pear kill NAME               terminate the program running in NAME

Options:
  -k, --detach-key KEY         key sequence that detaches (default: C-a d)
  -d, --detached               create the session without attaching
  -h, --help                   show this help
  -V, --version                show version

Keys: C-x = Ctrl+x, M-x = Alt+x, plain characters are literal.
      e.g. \"C-a d\", \"C-\\\", \"C-q\", \"M-d\".
      The detach key works in any keyboard mode a program enables.

Config: ~/.config/pear/config, e.g.
  detach-key = C-\\

Sockets live in $XDG_RUNTIME_DIR/pear or /tmp/pear-UID (override with PEAR_DIR).
";

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("pear: {msg}");
    exit(1)
}

#[derive(Default)]
struct Args {
    detach_key: Option<Vec<u8>>,
    detached: bool,
    positional: Vec<String>,
    /// How many positionals came before `--`, if it was given.
    dashdash: Option<usize>,
}

/// How many leading positionals a command takes before its CMD... (options
/// are accepted anywhere among those).
fn leading(first: &str) -> usize {
    match first {
        "new" | "n" | "create" | "attach" | "a" | "kill" | "k" => 2,
        _ => 1,
    }
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Args {
    let mut a = Args::default();
    while let Some(arg) = args.next() {
        // Options go before the command's CMD...: `pear new x -d sleep 1` but
        // `pear new x sleep -d`, `pear work -d -- ls` but `pear work ls -la`.
        let in_options = a.positional.first().is_none_or(|first| {
            let n = leading(first);
            a.positional.len() < n || (a.positional.len() == n && arg.starts_with('-'))
        });
        if !in_options {
            a.positional.push(arg);
            a.positional.extend(args.by_ref());
            break;
        }
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                exit(0);
            }
            "-V" | "--version" => {
                let ghostty = if cfg!(feature = "ghostty") {
                    " (libghostty-vt)"
                } else {
                    ""
                };
                println!("pear {}{ghostty}", env!("CARGO_PKG_VERSION"));
                exit(0);
            }
            "-d" | "--detached" => a.detached = true,
            "-k" | "--detach-key" => {
                let v = args
                    .next()
                    .unwrap_or_else(|| die("--detach-key needs a value"));
                a.detach_key = Some(
                    config::parse_key(&v).unwrap_or_else(|e| die(format!("--detach-key: {e}"))),
                );
            }
            "--" => {
                a.dashdash = Some(a.positional.len());
                a.positional.extend(args.by_ref());
                break;
            }
            s if s.starts_with('-') && s.len() > 1 => die(format!("unknown option {s}\n\n{USAGE}")),
            _ => a.positional.push(arg),
        }
    }
    a
}

fn main() {
    let args = parse_args(std::env::args().skip(1));
    let detach_key = || match &args.detach_key {
        Some(k) => k.clone(),
        None => config::load().unwrap_or_else(|e| die(e)).detach_key,
    };
    let pos = &args.positional;
    let name_arg = |cmd: &str| {
        pos.get(1)
            .unwrap_or_else(|| die(format!("{cmd}: missing session name")))
    };
    let no_extra = |n: usize| {
        if let Some(extra) = pos.get(n) {
            die(format!("{}: unexpected argument `{extra}`", pos[0]));
        }
    };

    let dir = config::session_dir();
    let code = match pos.first().map(String::as_str) {
        // `pear -- CMD...`: new numbered session running CMD.
        Some(_) if args.dashdash == Some(0) => {
            cmd_new(&dir, None, pos, &detach_key(), args.detached)
        }
        None => {
            if !args.detached && !(sys::isatty(0) && sys::isatty(1)) {
                // Not interactive (a pipe or script): just list.
                cmd_ls(&dir)
            } else {
                cmd_new(&dir, None, &[], &detach_key(), args.detached)
            }
        }
        Some("ls" | "list") => {
            no_extra(1);
            cmd_ls(&dir)
        }
        Some("new" | "n" | "create") => {
            // `pear new -- CMD...` runs CMD in a numbered session.
            let (name, cmd) = if args.dashdash == Some(1) {
                (None, &pos[1..])
            } else {
                (
                    pos.get(1).map(String::as_str),
                    pos.get(2..).unwrap_or_default(),
                )
            };
            cmd_new(&dir, name, cmd, &detach_key(), args.detached)
        }
        Some(cmd @ ("attach" | "a")) => {
            let name = name_arg(cmd);
            no_extra(2);
            cmd_attach(&dir, name, &detach_key())
        }
        Some(cmd @ ("kill" | "k")) => {
            let name = name_arg(cmd);
            no_extra(2);
            cmd_kill(&dir, name)
        }
        Some(name) if args.dashdash == Some(1) => {
            // `pear NAME [-d] -- CMD...`: type CMD into NAME's shell.
            if pos.len() == 1 {
                die("missing command after `--`");
            }
            cmd_send(&dir, name, &pos[1..], &detach_key(), args.detached)
        }
        Some(name) => {
            // `pear NAME [CMD...]`: attach or create.
            check_name(name);
            if session_alive(&sock_path(&dir, name)) {
                if pos.len() > 1 {
                    die(format!(
                        "session `{name}` already exists; attach with `pear {name}`"
                    ));
                }
                if args.detached {
                    die(format!("session `{name}` already exists"));
                }
                cmd_attach(&dir, name, &detach_key())
            } else {
                cmd_new(&dir, Some(name), &pos[1..], &detach_key(), args.detached)
            }
        }
    };
    exit(code);
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['-', '.'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn check_name(name: &str) {
    if !valid_name(name) {
        die(format!(
            "invalid session name `{name}` (use letters, digits, `-`, `_`, `.`; up to 64 characters, not starting with `-` or `.`)"
        ));
    }
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

/// Names of live sessions, sorted (numbers numerically, before other names).
fn live_sessions(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| valid_name(n) && session_alive(&sock_path(dir, n)))
        .collect();
    names.sort_by_key(|n| (n.parse::<u64>().unwrap_or(u64::MAX), n.clone()));
    names
}

/// The lowest positive number not used by a session (or any other file) in `dir`.
fn next_free_number(dir: &Path) -> String {
    (1u64..)
        .map(|n| n.to_string())
        .find(|n| {
            let path = sock_path(dir, n);
            !session_alive(&path) && std::fs::symlink_metadata(&path).is_err()
        })
        .expect("a free session number")
}

fn ensure_dir(dir: &Path) {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let created = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir);
    if let Err(e) = created {
        die(format!("cannot create {}: {e}", dir.display()));
    }
    match std::fs::metadata(dir) {
        Ok(m) if m.uid() != sys::getuid() => {
            die(format!("{} is owned by another user", dir.display()))
        }
        Ok(_) => {}
        Err(e) => die(format!("cannot use {}: {e}", dir.display())),
    }
}

fn cmd_ls(dir: &Path) -> i32 {
    let mut any = false;
    for name in live_sessions(dir) {
        any = true;
        match client::control(&sock_path(dir, &name), proto::C_INFO) {
            Ok(Some(info)) => println!("{info}"),
            _ => println!("{name}\t(no response)"),
        }
    }
    if !any {
        println!("no sessions");
    }
    0
}

fn check_tty() {
    if !sys::isatty(0) || !sys::isatty(1) {
        die("stdin/stdout must be a terminal to attach (use -d to create without attaching)");
    }
}

fn cmd_attach(dir: &Path, name: &str, detach_key: &[u8]) -> i32 {
    check_name(name);
    let path = sock_path(dir, name);
    if !session_alive(&path) {
        die(format!("no session named `{name}`"));
    }
    if std::env::var("PEAR_SOCKET").is_ok_and(|cur| same_file(&cur, &path)) {
        die(format!("already inside session `{name}`"));
    }
    check_tty();
    run_attach(&path, name, detach_key)
}

fn same_file(a: &str, b: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
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
    check_name(name);
    let path = sock_path(dir, name);
    if !session_alive(&path) {
        die(format!("no session named `{name}`"));
    }
    match client::control(&path, proto::C_KILL) {
        Ok(_) => {
            eprintln!("[killed {name}]");
            0
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
            die(format!("kill: session `{name}` is not responding"))
        }
        Err(e) => die(format!("kill: {e}")),
    }
}

/// `pear NAME [-d] -- CMD...`: run CMD in NAME's shell as if typed there
/// (creating NAME with a shell first if needed), then attach unless detached.
fn cmd_send(dir: &Path, name: &str, cmd: &[String], detach_key: &[u8], detached: bool) -> i32 {
    check_name(name);
    let path = sock_path(dir, name);
    let created = if session_alive(&path) {
        false
    } else {
        if !detached {
            check_tty();
        }
        ensure_dir(dir);
        match start_server(dir, name, &[default_shell()]) {
            Ok(()) => true,
            Err(StartError::Exists) => false, // someone just created it
            Err(StartError::Failed(msg)) => die(format!("cannot start session: {msg}")),
        }
    };
    if !detached {
        if std::env::var("PEAR_SOCKET").is_ok_and(|cur| same_file(&cur, &path)) {
            die(format!("already inside session `{name}`"));
        }
        check_tty();
    }

    let mut line = cmd.join(" ").into_bytes();
    line.push(b'\r');
    // When attaching, the user sees the command run; when detached, wait
    // until it has been typed so errors are reported.
    if let Err(e) = client::send(&path, &line, detached) {
        die(format!("cannot send to `{name}`: {e}"));
    }
    if detached {
        let verb = if created { "created" } else { "sent to" };
        eprintln!("[{verb} {name}: {}]", cmd.join(" "));
        return 0;
    }
    run_attach(&path, name, detach_key)
}

fn default_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".into())
}

enum StartError {
    Exists,
    Failed(String),
}

fn cmd_new(
    dir: &Path,
    name: Option<&str>,
    cmd: &[String],
    detach_key: &[u8],
    detached: bool,
) -> i32 {
    if let Some(name) = name {
        check_name(name);
    }
    if !detached {
        check_tty();
    }
    ensure_dir(dir);

    let cmd: Vec<String> = if cmd.is_empty() {
        vec![default_shell()]
    } else {
        cmd.to_vec()
    };

    let mut attempts = 0;
    let name = loop {
        attempts += 1;
        let candidate = name.map_or_else(|| next_free_number(dir), str::to_string);
        match start_server(dir, &candidate, &cmd) {
            Ok(()) => break candidate,
            // Someone else just took this number: try the next one.
            Err(StartError::Exists) if name.is_none() && attempts < 100 => {}
            Err(StartError::Exists) => die(format!(
                "session `{candidate}` already exists (use `pear attach {candidate}`)"
            )),
            Err(StartError::Failed(msg)) => die(format!("cannot start session: {msg}")),
        }
    };

    if detached {
        eprintln!(
            "[created {name}; attach with `pear {name}`, detach with {}]",
            config::describe_key(detach_key)
        );
        return 0;
    }
    run_attach(&sock_path(dir, &name), &name, detach_key)
}

/// Fork the session server and wait until it is listening.
fn start_server(dir: &Path, name: &str, cmd: &[String]) -> Result<(), StartError> {
    let path = sock_path(dir, name);
    if session_alive(&path) {
        return Err(StartError::Exists);
    }
    let size = sys::get_winsize(1)
        .or_else(|| sys::get_winsize(0))
        .unwrap_or_default();

    // Daemonize: fork, child becomes the server. A pipe tells us when it's ready:
    // 0 = ok, 1 = name taken, 2 + message = error.
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
        sys::set_umask(0o077);
        let log = log_path(dir, name);
        match server::Server::start(name, &path, cmd, size) {
            Ok(mut server) => {
                let _ = sys::write_all(ready_w, b"\0");
                sys::close(ready_w);
                sys::detach_stdio(Some(&log));
                server.set_log_path(Some(log));
                let status = server.run();
                unsafe { libc::_exit(status.clamp(0, 255)) };
            }
            Err(e) => {
                let taken = matches!(
                    e.kind(),
                    io::ErrorKind::AddrInUse | io::ErrorKind::AlreadyExists
                );
                let msg = if taken {
                    vec![1]
                } else {
                    format!("\x02{e}").into_bytes()
                };
                let _ = sys::write_all(ready_w, &msg);
                sys::close(ready_w);
                unsafe { libc::_exit(1) };
            }
        }
    }
    sys::close(ready_w);
    // Reap the intermediate child.
    unsafe {
        let mut st = 0;
        libc::waitpid(pid, &raw mut st, 0);
    }
    let mut msg = Vec::new();
    let mut buf = [0u8; 256];
    while let Ok(n @ 1..) = sys::read(ready_r, &mut buf) {
        msg.extend_from_slice(&buf[..n]);
    }
    sys::close(ready_r);
    match msg.first() {
        Some(0) => Ok(()),
        Some(1) => Err(StartError::Exists),
        Some(_) => Err(StartError::Failed(
            String::from_utf8_lossy(&msg[1..]).into_owned(),
        )),
        None => Err(StartError::Failed("server failed to start".into())),
    }
}

fn log_path(dir: &Path, name: &str) -> String {
    let p: PathBuf = dir.join(format!(".{name}.log"));
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        parse_args(args.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn options_after_subcommand() {
        let a = parse(&["new", "-d", "x", "sleep", "-d"]);
        assert!(a.detached);
        assert_eq!(a.positional, ["new", "x", "sleep", "-d"]);
        let a = parse(&["-d", "new", "x"]);
        assert!(a.detached);
        assert_eq!(a.positional, ["new", "x"]);
        let a = parse(&["work", "ls", "-la"]);
        assert!(!a.detached);
        assert_eq!(a.positional, ["work", "ls", "-la"]);
        let a = parse(&["coco", "-d", "--", "ls", "-d"]);
        assert!(a.detached);
        assert_eq!(a.positional, ["coco", "ls", "-d"]);
        assert_eq!(a.dashdash, Some(1));
        let a = parse(&["coco", "--", "cortex"]);
        assert!(!a.detached);
        assert_eq!(a.dashdash, Some(1));
        let a = parse(&["new", "--", "-x"]);
        assert_eq!(a.positional, ["new", "-x"]);
        assert_eq!(a.dashdash, Some(1));
    }

    #[test]
    fn names() {
        assert!(valid_name("1"));
        assert!(valid_name("my-session.2"));
        assert!(!valid_name("-d"));
        assert!(!valid_name(".x.log"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name(&"x".repeat(65)));
    }
}
