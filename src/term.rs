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

/// Put the client's terminal in a known, sane state before repainting / after detaching.
pub fn sane_terminal() -> &'static [u8] {
    concat!(
        "\x1b[0m",      // reset SGR
        "\x1b[?1049l",  // leave alternate screen
        "\x1b[?25h",    // cursor visible
        "\x1b[?1l",     // normal cursor keys
        "\x1b>",        // normal keypad
        "\x1b[?7h",     // autowrap on
        "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1016l", // mouse off
        "\x1b[?1004l",  // focus events off
        "\x1b[?2004l",  // bracketed paste off
        "\x1b[?2026l",  // synchronized output off
        "\x1b[<u",      // pop kitty keyboard flags (if any)
        "\x1b[r",       // reset scroll region
    )
    .as_bytes()
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
        RingScreen { buf: Vec::new(), cap: 256 * 1024 }
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
        let mut out = sane_terminal().to_vec();
        out.extend_from_slice(b"\x1b[H\x1b[2J");
        out.extend_from_slice(&self.buf);
        out
    }
    fn take_pty_responses(&mut self) -> Vec<u8> {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// libghostty-vt backed screen

#[cfg(feature = "ghostty")]
pub mod ghostty {
    use super::{sane_terminal, Screen};
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

    unsafe extern "C" fn write_pty_cb(_t: GhosttyTerminal, userdata: *mut c_void, data: *const u8, len: usize) {
        if userdata.is_null() || data.is_null() {
            return;
        }
        let pending = unsafe { &mut *(userdata as *mut Vec<u8>) };
        pending.extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
    }

    impl GhosttyScreen {
        pub fn new(size: WinSize) -> Result<Self, String> {
            let mut term: GhosttyTerminal = std::ptr::null_mut();
            let r = unsafe { ghostty_terminal_new(std::ptr::null(), &mut term, size.cols, size.rows) };
            if r != GHOSTTY_SUCCESS || term.is_null() {
                return Err(format!("ghostty_terminal_new: {r}"));
            }
            let mut s = GhosttyScreen { term, pending: Box::new(Vec::new()) };
            unsafe {
                let ud: *mut Vec<u8> = &mut *s.pending;
                ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_USERDATA, ud as *const c_void);
                ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_WRITE_PTY, write_pty_cb as *const c_void);
                // We only need the visible screen; keep memory small.
                let lines: usize = 0;
                ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES, &lines as *const _ as *const c_void);
            }
            Ok(s)
        }

        #[cfg(test)]
        fn alt_screen_active(&self) -> bool {
            let mut v: i32 = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
            unsafe { ghostty_terminal_get(self.term, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &mut v as *mut _ as *mut c_void) };
            v == GHOSTTY_TERMINAL_SCREEN_ALTERNATE
        }

        fn mode(&self, n: u16) -> Option<bool> {
            let mut cfg = GhosttyTerminalModeConfig { mode: dec_mode(n), value: false };
            let r = unsafe { ghostty_terminal_get(self.term, GHOSTTY_TERMINAL_DATA_MODE, &mut cfg as *mut _ as *mut c_void) };
            (r == GHOSTTY_SUCCESS).then_some(cfg.value)
        }

        fn format_vt(&self) -> Vec<u8> {
            let mut f: GhosttyFormatter = std::ptr::null_mut();
            let mut out = Vec::new();
            unsafe {
                if ghostty_formatter_terminal_new(std::ptr::null(), &mut f, self.term) != GHOSTTY_SUCCESS {
                    return out;
                }
                let emit = GHOSTTY_FORMATTER_FORMAT_VT;
                ghostty_formatter_set(f, GHOSTTY_FORMATTER_OPT_EMIT, &emit as *const _ as *const c_void);
                let trim = false;
                ghostty_formatter_set(f, GHOSTTY_FORMATTER_OPT_TRIM, &trim as *const _ as *const c_void);
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
                ghostty_formatter_set(f, GHOSTTY_FORMATTER_OPT_EXTRAS, &extras as *const _ as *const c_void);
                let mut ptr: *mut u8 = std::ptr::null_mut();
                let mut len: usize = 0;
                if ghostty_formatter_format_alloc(f, std::ptr::null(), &mut ptr, &mut len) == GHOSTTY_SUCCESS && !ptr.is_null() {
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
            let mut out = sane_terminal().to_vec();
            out.extend_from_slice(b"\x1b[H\x1b[2J");
            out.extend_from_slice(&self.format_vt());
            if self.mode(66) == Some(true) {
                out.extend_from_slice(b"\x1b="); // application keypad isn't a CSI mode
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
            assert!(snap.contains("ESC[?1049h"));
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
