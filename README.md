# pear

Keep a terminal program running independently of your SSH session.
Like [abduco](https://github.com/martanne/abduco), minus everything you don't need.

- Named sessions, attach/detach, list, kill. Nothing else.
- Exact screen repaint on reattach (vim, htop, ...) via [libghostty-vt](https://lib.ghostty.org/vt/).
- One static binary, ~1.5 MB, one dependency (`libc`), single-threaded `poll` loop.

## Install

On macOS or Linux with [Homebrew](https://brew.sh):

```sh
brew tap sander-ed/tap https://github.com/sander-ed/sander-ed-tap.git
brew install sander-ed/tap/pear
```

This installs a prebuilt binary with libghostty-vt linked in (about 1.6 MB, no
dependencies). No compiler or zig is needed.

Each [GitHub Release](https://github.com/sander-ed/pear/releases) also has the
archives for macOS (Apple Silicon, Intel) and Linux (x86_64, ARM64, static
musl), plus `SHA256SUMS`.

## Usage

```
pear                         start a new session named 1, 2, 3, ... and attach
pear NAME [CMD...]           attach to NAME, or create it if it doesn't exist
pear new [NAME] [CMD...]     create a session running CMD (default: $SHELL) and attach
pear new -d [NAME] [CMD...]  create without attaching
pear NAME -- CMD...          run CMD in NAME's shell (creating NAME if needed) and attach
pear NAME -d -- CMD...       run CMD in NAME's shell without attaching
pear attach NAME             attach to an existing session
pear ls                      list sessions
pear kill NAME               terminate the program running in NAME
```

Like tmux, a bare `pear` takes the lowest free number, so with sessions `1`
and `3` running the next one is `2`. When `pear` is not run in a terminal
(for example piped into another command), it lists sessions instead.

A session is shown on your terminal's alternate screen, like tmux. When you
detach, or the program exits, your terminal is back exactly as it was before
you attached: the session's output stays in the session.

`pear NAME -- CMD` types CMD into the session's shell as if you had, then
attaches, so after CMD finishes you are left in that shell. With `-d` it only
runs CMD, for example `pear build -d -- make test`; look at the result later with
`pear build`. If NAME doesn't exist yet, it is created running `$SHELL`, and CMD
is typed once the shell has printed its prompt. Compare `pear NAME CMD` (no
`--`), which creates NAME running CMD instead of a shell, so the session ends
when CMD does.

Detach with **Ctrl-a d** (default). Change it per invocation with
`-k`/`--detach-key`, or permanently in `~/.config/pear/config`:

```
detach-key = C-\    # comments are allowed after a value
```

Key syntax: `C-x` = Ctrl+x, `M-x` = Alt+x, plain characters are literal,
multiple tokens separated by spaces form a sequence (`C-a d`, `C-a C-d`).

The detach key always wins, like tmux's prefix. pear decodes your keystrokes
itself, so it still detaches when the program has switched your terminal to the
kitty keyboard protocol or xterm's modifyOtherKeys. GitHub Copilot CLI, Claude
Code, and other TUIs do this, and in those modes Ctrl-a is no longer the byte
`0x01`. Pasted text never triggers a detach. Ctrl-a followed by any other key
is passed through, so Ctrl-a still works as "beginning of line".

When the program exits, every attached `pear` exits with the same status and
the session disappears. If the SSH connection drops, the session keeps running;
just `pear NAME` again.

Sockets live in `$XDG_RUNTIME_DIR/pear/` or `/tmp/pear-$UID/` (`PEAR_DIR` overrides).
Inside a session, `PEAR` holds the session name and `PEAR_SOCKET` holds the
socket path. pear uses them to refuse attaching a session to itself.

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
DA) itself so programs don't hang. A client that falls more than 4 MB behind
(a slow SSH link during heavy output) skips the backlog and gets a fresh screen
snapshot instead of being disconnected.

Wire protocol: `[type u8][len u32 LE][payload]`. See `src/proto.rs`.

## Tests

```sh
PKG_CONFIG_PATH="$HOME/.local/lib/pkgconfig" cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
expect tests/e2e.exp target/release/pear   # end-to-end on a real pty
```

## Releasing

Bump `[package].version` in `Cargo.toml`, run `cargo update -p pear`, and
commit on `main`. Then, on a Mac with `zig` 0.17.1, `rustup`, and `gh`:

```sh
scripts/release.sh            # or --dry-run to only build and test into dist/
```

The script builds libghostty-vt with zig and pear for both macOS
architectures. It checks that each binary uses only system libraries and runs
the tests on the native one. Then it pushes `main` and creates the `vVERSION`
tag and GitHub Release with the macOS archives.

Publishing the release starts the **Release** workflow
(`.github/workflows/release.yml`). It builds and tests static Linux binaries
for x86_64 and ARM64 with libghostty-vt, then uploads them and `SHA256SUMS` to
the release. Finally it renders `packaging/homebrew/pear.rb` and commits it to
[`sander-ed/sander-ed-tap`](https://github.com/sander-ed/sander-ed-tap). To
retry, run the workflow manually with the tag.

The workflow pushes to the tap with the `HOMEBREW_TAP_DEPLOY_KEY` Actions
secret, which holds a write-enabled deploy key on the tap repository. To rotate
it, replace both the deploy key and the secret.

## License

[MIT](LICENSE)
