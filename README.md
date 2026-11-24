# pear

Keep a terminal program running independently of your SSH session.
Like [abduco](https://github.com/martanne/abduco), minus everything you don't need.

- Named sessions, attach/detach, list, kill. Nothing else.
- Exact screen repaint on reattach (vim, htop, ...) via [libghostty-vt](https://lib.ghostty.org/vt/).
- One static binary, ~1.5 MB, one dependency (`libc`), single-threaded `poll` loop.

## Usage

```
pear                         list sessions
pear NAME [CMD...]           attach to NAME, or create it if it doesn't exist
pear new NAME [CMD...]       create a session running CMD (default: $SHELL) and attach
pear new -d NAME [CMD...]    create without attaching
pear attach NAME             attach to an existing session
pear ls                      list sessions
pear kill NAME               terminate the program running in NAME
```

Detach with **Ctrl-a d** (default). Change it per invocation with
`-k`/`--detach-key`, or permanently in `~/.config/pear/config`:

```
detach-key = C-\
```

Key syntax: `C-x` = Ctrl+x, `M-x` = Alt+x, plain characters are literal,
multiple tokens separated by spaces form a sequence (`C-a d`, `C-a C-d`).

When the program exits, every attached `pear` exits with the same status and
the session disappears. If the SSH connection drops, the session keeps running;
just `pear NAME` again.

Sockets live in `$XDG_RUNTIME_DIR/pear/` or `/tmp/pear-$UID/` (`PEAR_DIR` overrides).
The environment variable `PEAR` is set to the session name inside the program.

## Building

Needs [zig](https://ziglang.org) 0.17 once, to build libghostty-vt:

```sh
curl -LO https://release.files.ghostty.org/libghostty-vt/1.0.1/libghostty-vt-1.0.1.tar.gz
tar xzf libghostty-vt-1.0.1.tar.gz && cd libghostty-vt-1.0.1
zig build -Doptimize=ReleaseFast --prefix "$HOME/.local"
cd ..

PKG_CONFIG_PATH="$HOME/.local/lib/pkgconfig" cargo build --release
# or: GHOSTTY_VT_DIR="$HOME/.local" cargo build --release
```

The library is linked statically; the resulting `target/release/pear` has no runtime dependencies.

Without zig/libghostty-vt, `cargo build --release --no-default-features` builds a
fallback that replays the last 256 KB of raw output on reattach. Fine for shells and
log tails, not for full-screen programs.

Linux and macOS are both supported.

## How it works

`pear new` forks a small server that owns a pty and runs your program on it. The
server feeds every byte the program writes into a libghostty-vt terminal
(scrollback disabled, so memory is just the visible screen) and forwards it to any
attached client over a unix socket. On attach, the server asks libghostty-vt to
serialize the current screen (contents, styles, cursor, alternate screen, mouse and
paste modes, ...) as VT sequences and sends that first. Resizes are propagated with
`TIOCSWINSZ`. While nobody is attached the server answers terminal queries (DSR,
DA) itself so programs don't hang.

Wire protocol: `[type u8][len u32 LE][payload]`. See `src/proto.rs`.

## Tests

```sh
PKG_CONFIG_PATH="$HOME/.local/lib/pkgconfig" cargo test
expect tests/e2e.exp target/release/pear   # end-to-end on a real pty
```
