//! Screen state kept by the server so a reattaching client can be repainted.
//!
//! With the `ghostty` feature, libghostty-vt emulates a full terminal and we ask
//! it to serialize the current screen as VT sequences. Without it, we fall back
//! to replaying the most recent output bytes, which is good enough for simple
//! programs but not for full-screen TUIs.

use crate::sys::WinSize;

pub trait Screen {
    /// Feed program output.
    fn write(&mut self, data: &[u8]);
    fn resize(&mut self, size: WinSize);
    /// Bytes to send to a freshly attached terminal to reproduce the current screen.
    fn snapshot(&mut self) -> Vec<u8>;
    /// Bytes the emulated terminal wants to send back to the program
    /// (answers to queries like DSR/DA). Empty for the fallback.
    fn take_pty_responses(&mut self) -> Vec<u8>;
}

// A session is shown on the alternate screen of the client's terminal, like
// tmux does: attaching switches to it, leaving switches back, and the user's
// terminal looks exactly as it did before. The program's own screen switches
// never reach the client's terminal; the server repaints instead.

/// Sent when a client attaches: switch to the alternate screen (this saves
/// the cursor) and clear it.
pub const ENTER_SCREEN: &[u8] = b"\x1b[?1049h\x1b[H\x1b[2J";

/// Reset every mode a program may have changed, without switching screens.
pub fn reset_modes() -> &'static [u8] {
    concat!(
        "\x1b[0m",                                                            // reset SGR
        "\x1b[?25h",                                                          // cursor visible
        "\x1b[?1l",                                                           // normal cursor keys
        "\x1b>",                                                              // normal keypad
        "\x1b[?7h",                                                           // autowrap on
        "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1016l", // mouse off
        "\x1b[?1004l",                                                        // focus events off
        "\x1b[?2004l",                                                        // bracketed paste off
        "\x1b[?2026l", // synchronized output off
        "\x1b[=0;1u",  // kitty keyboard flags off...
        "\x1b[<99u",   // ...and pop everything the program pushed
        "\x1b[>4m",    // xterm modifyOtherKeys off
        "\x1b[r",      // full-screen scroll region
    )
    .as_bytes()
}

/// Sent when a client leaves: undo the program's modes, then return to the
/// primary screen, which restores the user's screen and cursor as they were.
pub fn leave_screen() -> Vec<u8> {
    let mut v = reset_modes().to_vec();
    v.extend_from_slice(b"\x1b[?1049l");
    v
}

/// Removes screen switches (DEC modes 47, 1047, 1049 and RIS) from a stream
/// of program output, so they don't take the client's terminal off the
/// session's screen. Escape sequences split across chunks are held back until
/// complete.
#[derive(Default)]
pub struct ScreenSwitchFilter {
    held: Vec<u8>,
}

/// An unfinished escape sequence longer than this is passed on as is.
const MAX_HELD: usize = 64;

impl ScreenSwitchFilter {
    /// Returns the output to forward, and whether a screen switch was removed
    /// (the client's screen then needs a repaint).
    pub fn feed(&mut self, data: &[u8]) -> (Vec<u8>, bool) {
        let input: Vec<u8>;
        let data = if self.held.is_empty() {
            data
        } else {
            let mut v = std::mem::take(&mut self.held);
            v.extend_from_slice(data);
            input = v;
            &input
        };
        let mut out = Vec::with_capacity(data.len());
        let mut switched = false;
        let mut i = 0;
        while i < data.len() {
            let Some(off) = data[i..].iter().position(|&b| b == 0x1b) else {
                out.extend_from_slice(&data[i..]);
                break;
            };
            out.extend_from_slice(&data[i..i + off]);
            i += off;
            match parse_escape(&data[i..]) {
                Escape::Incomplete if data.len() - i <= MAX_HELD => {
                    self.held = data[i..].to_vec();
                    break;
                }
                Escape::Incomplete | Escape::Other => {
                    out.push(0x1b);
                    i += 1;
                }
                Escape::Reset => {
                    switched = true;
                    i += 2;
                }
                Escape::PrivateMode { len, params, set } => {
                    let kept: Vec<&[u8]> = params
                        .split(|&b| b == b';')
                        .filter(|p| !matches!(*p, b"47" | b"1047" | b"1049"))
                        .collect();
                    if kept.len() == params.split(|&b| b == b';').count() {
                        out.extend_from_slice(&data[i..i + len]);
                    } else {
                        switched = true;
                        if !kept.is_empty() {
                            out.extend_from_slice(b"\x1b[?");
                            out.extend_from_slice(&kept.join(&b';'));
                            out.push(if set { b'h' } else { b'l' });
                        }
                    }
                    i += len;
                }
            }
        }
        (out, switched)
    }
}

enum Escape<'a> {
    Incomplete,
    /// ESC c: full reset, which also leaves the alternate screen.
    Reset,
    /// CSI ? params h/l (DECSET/DECRST): total length, params, set?
    PrivateMode {
        len: usize,
        params: &'a [u8],
        set: bool,
    },
    Other,
}

/// `s` starts with ESC.
fn parse_escape(s: &[u8]) -> Escape<'_> {
    match s.get(1) {
        None => Escape::Incomplete,
        Some(b'c') => Escape::Reset,
        Some(b'[') => {
            // CSI: parameter bytes 0x30-0x3F, intermediates 0x20-0x2F, final 0x40-0x7E.
            let mut j = 2;
            while s.get(j).is_some_and(|b| (0x30..=0x3f).contains(b)) {
                j += 1;
            }
            let params_end = j;
            while s.get(j).is_some_and(|b| (0x20..=0x2f).contains(b)) {
                j += 1;
            }
            match s.get(j) {
                None => Escape::Incomplete,
                Some(&f @ (b'h' | b'l')) if params_end == j && s.get(2) == Some(&b'?') => {
                    Escape::PrivateMode {
                        len: j + 1,
                        params: &s[3..params_end],
                        set: f == b'h',
                    }
                }
                Some(_) => Escape::Other,
            }
        }
        Some(_) => Escape::Other,
    }
}

/// `data` without screen switches (for snapshots, which are complete).
pub fn strip_screen_switches(data: &[u8]) -> Vec<u8> {
    let mut f = ScreenSwitchFilter::default();
    let (mut out, _) = f.feed(data);
    out.append(&mut f.held);
    out
}

pub fn new_screen(size: WinSize) -> Box<dyn Screen> {
    #[cfg(feature = "ghostty")]
    {
        match ghostty::GhosttyScreen::new(size) {
            Ok(s) => return Box::new(s),
            Err(e) => eprintln!("pear: libghostty-vt init failed ({e}), using raw replay"),
        }
    }
    Box::new(RingScreen::new(size))
}

// ---------------------------------------------------------------------------
// Fallback: ring buffer of raw output

pub struct RingScreen {
    buf: Vec<u8>,
    cap: usize,
}

impl RingScreen {
    pub fn new(_size: WinSize) -> Self {
        RingScreen {
            buf: Vec::new(),
            cap: 256 * 1024,
        }
    }
}

impl Screen for RingScreen {
    fn write(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        if self.buf.len() > self.cap {
            let excess = self.buf.len() - self.cap;
            self.buf.drain(..excess);
        }
    }
    fn resize(&mut self, _size: WinSize) {}
    fn snapshot(&mut self) -> Vec<u8> {
        let mut out = reset_modes().to_vec();
        out.extend_from_slice(b"\x1b[H\x1b[2J");
        out.extend_from_slice(&strip_screen_switches(&self.buf));
        out
    }
    fn take_pty_responses(&mut self) -> Vec<u8> {
        Vec::new()
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> (String, bool) {
        let mut f = ScreenSwitchFilter::default();
        let mut out = Vec::new();
        let mut any = false;
        for c in chunks {
            let (o, s) = f.feed(c);
            out.extend(o);
            any |= s;
        }
        (String::from_utf8_lossy(&out).replace('\x1b', "ESC"), any)
    }

    #[test]
    fn strips_screen_switches() {
        assert_eq!(run(&[b"a\x1b[?1049hb"]), ("ab".into(), true));
        assert_eq!(
            run(&[b"\x1b[?1049l\x1b[?47h\x1b[?1047l"]),
            ("".into(), true)
        );
        assert_eq!(run(&[b"x\x1bcy"]), ("xy".into(), true));
        // Combined modes keep the others.
        assert_eq!(run(&[b"\x1b[?1049;25h"]), ("ESC[?25h".into(), true));
    }

    #[test]
    fn keeps_everything_else() {
        let s: &[u8] =
            b"\x1b[?25l\x1b[2J\x1b[1;31mred\x1b[0m\x1b]0;title\x07\x1b7\x1b8\x1b[?10490h";
        let (out, sw) = run(&[s]);
        assert_eq!(
            out.as_bytes(),
            String::from_utf8_lossy(s).replace('\x1b', "ESC").as_bytes()
        );
        assert!(!sw);
    }

    #[test]
    fn split_sequences() {
        assert_eq!(run(&[b"a\x1b", b"[?10", b"49hb"]), ("ab".into(), true));
        assert_eq!(run(&[b"a\x1b[", b"31mb"]), ("aESC[31mb".into(), false));
        // An unterminated escape doesn't swallow output forever.
        let long = [b"\x1b[".as_slice(), &[b'1'; 100]].concat();
        let (out, _) = run(&[&long]);
        assert_eq!(out.len(), 3 + 1 + 100);
    }
}

// ---------------------------------------------------------------------------
// libghostty-vt backed screen

#[cfg(feature = "ghostty")]
pub mod ghostty {
    use super::{Screen, reset_modes, strip_screen_switches};
    use crate::ghostty::*;
    use crate::sys::WinSize;
    use std::ffi::c_void;

    pub struct GhosttyScreen {
        term: GhosttyTerminal,
        // Boxed so the pointer handed to libghostty as userdata stays stable
        // even though `GhosttyScreen` itself gets moved around.
        #[allow(clippy::box_collection)]
        pending: Box<Vec<u8>>,
    }

    // The terminal is only ever touched from the server's single thread.
    unsafe impl Send for GhosttyScreen {}

    unsafe extern "C" fn write_pty_cb(
        _t: GhosttyTerminal,
        userdata: *mut c_void,
        data: *const u8,
        len: usize,
    ) {
        if userdata.is_null() || data.is_null() {
            return;
        }
        let pending = unsafe { &mut *userdata.cast::<Vec<u8>>() };
        pending.extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
    }

    impl GhosttyScreen {
        pub fn new(size: WinSize) -> Result<Self, String> {
            let mut term: GhosttyTerminal = std::ptr::null_mut();
            let r = unsafe {
                ghostty_terminal_new(std::ptr::null(), &raw mut term, size.cols, size.rows)
            };
            if r != GHOSTTY_SUCCESS || term.is_null() {
                return Err(format!("ghostty_terminal_new: {r}"));
            }
            let mut s = GhosttyScreen {
                term,
                pending: Box::new(Vec::new()),
            };
            unsafe {
                let ud: *mut Vec<u8> = &mut *s.pending;
                ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_USERDATA, ud.cast_const().cast());
                ghostty_terminal_set(
                    term,
                    GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                    write_pty_cb as *const c_void,
                );
                // We only need the visible screen; keep memory small.
                let lines: usize = 0;
                ghostty_terminal_set(
                    term,
                    GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES,
                    (&raw const lines).cast(),
                );
            }
            Ok(s)
        }

        #[cfg(test)]
        fn alt_screen_active(&self) -> bool {
            let mut v: i32 = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
            unsafe {
                ghostty_terminal_get(
                    self.term,
                    GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN,
                    (&raw mut v).cast(),
                )
            };
            v == GHOSTTY_TERMINAL_SCREEN_ALTERNATE
        }

        fn mode(&self, n: u16) -> Option<bool> {
            let mut cfg = GhosttyTerminalModeConfig {
                mode: dec_mode(n),
                value: false,
            };
            let r = unsafe {
                ghostty_terminal_get(self.term, GHOSTTY_TERMINAL_DATA_MODE, (&raw mut cfg).cast())
            };
            (r == GHOSTTY_SUCCESS).then_some(cfg.value)
        }

        fn format_vt(&self) -> Vec<u8> {
            let mut f: GhosttyFormatter = std::ptr::null_mut();
            let mut out = Vec::new();
            unsafe {
                if ghostty_formatter_terminal_new(std::ptr::null(), &raw mut f, self.term)
                    != GHOSTTY_SUCCESS
                {
                    return out;
                }
                let emit = GHOSTTY_FORMATTER_FORMAT_VT;
                ghostty_formatter_set(f, GHOSTTY_FORMATTER_OPT_EMIT, (&raw const emit).cast());
                let trim = false;
                ghostty_formatter_set(f, GHOSTTY_FORMATTER_OPT_TRIM, (&raw const trim).cast());
                // Not PALETTE: that would overwrite the user's terminal colors with ghostty's
                // defaults. Not PWD: OSC 7 would make the client's terminal think it changed dir.
                let extras: u32 = EXTRA_MODES
                    | EXTRA_SCROLLING_REGION
                    | EXTRA_TABSTOPS
                    | EXTRA_KEYBOARD
                    | EXTRA_CURSOR
                    | EXTRA_STYLE
                    | EXTRA_HYPERLINK
                    | EXTRA_PROTECTION
                    | EXTRA_KITTY_KEYBOARD
                    | EXTRA_CHARSETS;
                ghostty_formatter_set(f, GHOSTTY_FORMATTER_OPT_EXTRAS, (&raw const extras).cast());
                let mut ptr: *mut u8 = std::ptr::null_mut();
                let mut len: usize = 0;
                if ghostty_formatter_format_alloc(f, std::ptr::null(), &raw mut ptr, &raw mut len)
                    == GHOSTTY_SUCCESS
                    && !ptr.is_null()
                {
                    out.extend_from_slice(std::slice::from_raw_parts(ptr, len));
                    ghostty_free(std::ptr::null(), ptr, len);
                }
                ghostty_formatter_free(f);
            }
            out
        }
    }

    impl Screen for GhosttyScreen {
        fn write(&mut self, data: &[u8]) {
            unsafe { ghostty_terminal_vt_write(self.term, data.as_ptr(), data.len()) };
        }

        fn resize(&mut self, size: WinSize) {
            unsafe { ghostty_terminal_resize(self.term, size.cols, size.rows, 0, 0) };
        }

        fn snapshot(&mut self) -> Vec<u8> {
            // Reset the client's terminal, clear, then let the formatter emit the
            // screen plus every mode that differs from default (alt screen,
            // cursor visibility, mouse, bracketed paste, cursor position, ...).
            // The client shows the session on its alternate screen already, so
            // the program's own screen switch is left out.
            let mut out = reset_modes().to_vec();
            out.extend_from_slice(b"\x1b[?2026h"); // synchronized update: no flicker
            out.extend_from_slice(b"\x1b[H\x1b[2J");
            out.extend_from_slice(&strip_screen_switches(&self.format_vt()));
            if self.mode(66) == Some(true) {
                out.extend_from_slice(b"\x1b="); // application keypad isn't a CSI mode
            }
            if self.mode(2026) != Some(true) {
                out.extend_from_slice(b"\x1b[?2026l");
            }
            out
        }

        fn take_pty_responses(&mut self) -> Vec<u8> {
            std::mem::take(&mut *self.pending)
        }
    }

    impl Drop for GhosttyScreen {
        fn drop(&mut self) {
            unsafe { ghostty_terminal_free(self.term) };
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn esc(b: &[u8]) -> String {
            String::from_utf8_lossy(b).replace('\x1b', "ESC")
        }

        #[test]
        fn snapshot_alt_screen_and_modes() {
            let mut s = GhosttyScreen::new(WinSize { cols: 20, rows: 4 }).unwrap();
            s.write(b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?2004h\x1b[2;3H\x1b[1;32mhi\x1b[0m");
            let snap = esc(&s.snapshot());
            eprintln!("{snap}");
            // The client is already on its alternate screen.
            assert!(!snap.contains("ESC[?1049"));
            assert!(snap.contains("hi"));
            assert!(snap.contains("ESC[?25l"));
            assert!(snap.contains("ESC[?1000h"));
            assert!(snap.contains("ESC[?2004h"));
            assert!(s.alt_screen_active());
        }

        #[test]
        fn pty_responses_are_collected() {
            let mut s = GhosttyScreen::new(WinSize { cols: 20, rows: 4 }).unwrap();
            s.write(b"\x1b[6n"); // DSR cursor position
            let r = s.take_pty_responses();
            assert_eq!(esc(&r), "ESC[1;1R");
        }
    }
}
