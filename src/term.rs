//! Screen state kept by the server so a reattaching client can be repainted.
//!
//! With the `ghostty` feature, libghostty-vt emulates a full terminal and we ask
//! it to serialize the current screen as VT sequences. It also keeps the
//! session's scrollback, which the mouse wheel scrolls through the way Ghostty
//! does. Without it, we fall back to replaying the most recent output bytes,
//! which is good enough for simple programs but not for full-screen TUIs.

use crate::sys::WinSize;

pub trait Screen {
    /// Feed program output. Returns what to send attached clients.
    fn write(&mut self, data: &[u8]) -> Update;
    /// Returns whether attached clients need a repaint.
    fn resize(&mut self, size: WinSize) -> bool;
    /// Bytes that repaint a client's terminal (on attach, or after `Update::Repaint`).
    fn snapshot(&mut self) -> Vec<u8>;
    /// Bytes the emulated terminal wants to send back to the program
    /// (answers to queries like DSR/DA). Empty for the fallback.
    fn take_pty_responses(&mut self) -> Vec<u8>;
    /// A client's terminal reported a mouse event. Returns the bytes for the
    /// program, and whether clients need a repaint (the view scrolled).
    fn mouse(&mut self, m: &MouseReport) -> (Vec<u8>, bool);
    /// The user typed: show the live screen again. Returns whether clients
    /// need a repaint (the view was scrolled back).
    fn scroll_to_bottom(&mut self) -> bool;
}

pub enum Update {
    /// Send these bytes (possibly none).
    Forward(Vec<u8>),
    /// Send a fresh snapshot instead.
    Repaint,
}

/// Sent when a client attaches: a session gets the terminal's alternate screen,
/// like tmux, so detaching leaves the user's screen as it was.
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

/// Removes from program output what would take the client's terminal off the
/// session's screen (DEC modes 47, 1047, 1049 and RIS) and, when pear owns the
/// mouse, the mouse modes. Escape sequences split across chunks are held back
/// until complete.
#[derive(Default)]
pub struct OutputFilter {
    held: Vec<u8>,
    mouse: bool,
}

/// An unfinished escape sequence longer than this is passed on as is.
const MAX_HELD: usize = 64;

impl OutputFilter {
    pub fn new(mouse: bool) -> Self {
        OutputFilter {
            held: Vec::new(),
            mouse,
        }
    }

    fn strips(&self, mode: &[u8]) -> bool {
        matches!(mode, b"47" | b"1047" | b"1049")
            || self.mouse
                && matches!(
                    mode,
                    b"9" | b"1000"
                        | b"1001"
                        | b"1002"
                        | b"1003"
                        | b"1005"
                        | b"1006"
                        | b"1015"
                        | b"1016"
                )
    }

    /// Returns the output to forward, and whether anything was removed (the
    /// client's screen then needs a repaint).
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
        let mut stripped = false;
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
                    stripped = true;
                    i += 2;
                }
                Escape::PrivateMode { len, params, set } => {
                    let kept: Vec<&[u8]> = params
                        .split(|&b| b == b';')
                        .filter(|p| !self.strips(p))
                        .collect();
                    if kept.len() == params.split(|&b| b == b';').count() {
                        out.extend_from_slice(&data[i..i + len]);
                    } else {
                        stripped = true;
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
        (out, stripped)
    }

    /// `data` filtered (for snapshots, which are complete).
    pub fn strip(mouse: bool, data: &[u8]) -> Vec<u8> {
        let mut f = OutputFilter::new(mouse);
        let (mut out, _) = f.feed(data);
        out.append(&mut f.held);
        out
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

/// An SGR (mode 1006) mouse report from the client's terminal:
/// `ESC [ < button ; column ; row M|m`, with 1-based cells.
#[derive(Debug, PartialEq)]
pub struct MouseReport {
    pub button: u16,
    pub col: u16,
    pub row: u16,
    /// `M`: press or motion; `m`: release.
    pub press: bool,
}

impl MouseReport {
    /// -1 for wheel up, 1 for wheel down.
    #[cfg_attr(not(feature = "ghostty"), allow(dead_code))]
    pub fn wheel(&self) -> Option<isize> {
        match self.button & !(4 | 8 | 16) {
            64 => Some(-1),
            65 => Some(1),
            _ => None,
        }
    }

    fn encode(&self) -> Vec<u8> {
        let end = if self.press { 'M' } else { 'm' };
        format!("\x1b[<{};{};{}{end}", self.button, self.col, self.row).into_bytes()
    }
}

pub enum Input<'a> {
    Keys(&'a [u8]),
    Mouse(MouseReport),
}

/// Split client input into keys and SGR mouse reports. Returns the parts and
/// how many bytes at the end are an unfinished report to hold for the next read.
pub fn split_input(data: &[u8]) -> (Vec<Input<'_>>, usize) {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while let Some(off) = data[i..].windows(3).position(|w| w == b"\x1b[<") {
        i += off;
        let body = &data[i + 3..];
        let Some(end) = body.iter().position(|b| !matches!(b, b'0'..=b'9' | b';')) else {
            if body.len() <= 16 {
                if start < i {
                    parts.push(Input::Keys(&data[start..i]));
                }
                return (parts, data.len() - i);
            }
            break;
        };
        let nums: Vec<u16> = body[..end]
            .split(|&b| b == b';')
            .filter_map(|n| std::str::from_utf8(n).ok()?.parse().ok())
            .collect();
        match (nums.as_slice(), body[end]) {
            (&[button, col, row], f @ (b'M' | b'm')) => {
                if start < i {
                    parts.push(Input::Keys(&data[start..i]));
                }
                parts.push(Input::Mouse(MouseReport {
                    button,
                    col,
                    row,
                    press: f == b'M',
                }));
                i += 3 + end + 1;
                start = i;
            }
            _ => i += 3,
        }
    }
    if start < data.len() {
        parts.push(Input::Keys(&data[start..]));
    }
    (parts, 0)
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
// Fallback: ring buffer of raw output. The program's own mouse modes reach the
// client's terminal, and there is no scrollback.

pub struct RingScreen {
    buf: Vec<u8>,
    cap: usize,
    filter: OutputFilter,
}

impl RingScreen {
    pub fn new(_size: WinSize) -> Self {
        RingScreen {
            buf: Vec::new(),
            cap: 256 * 1024,
            filter: OutputFilter::new(false),
        }
    }
}

impl Screen for RingScreen {
    fn write(&mut self, data: &[u8]) -> Update {
        self.buf.extend_from_slice(data);
        if self.buf.len() > self.cap {
            let excess = self.buf.len() - self.cap;
            self.buf.drain(..excess);
        }
        match self.filter.feed(data) {
            (_, true) => Update::Repaint,
            (out, false) => Update::Forward(out),
        }
    }
    fn resize(&mut self, _size: WinSize) -> bool {
        false
    }
    fn snapshot(&mut self) -> Vec<u8> {
        let mut out = reset_modes().to_vec();
        out.extend_from_slice(b"\x1b[H\x1b[2J");
        out.extend_from_slice(&OutputFilter::strip(false, &self.buf));
        out
    }
    fn take_pty_responses(&mut self) -> Vec<u8> {
        Vec::new()
    }
    fn mouse(&mut self, m: &MouseReport) -> (Vec<u8>, bool) {
        (m.encode(), false)
    }
    fn scroll_to_bottom(&mut self) -> bool {
        false
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    fn run(mouse: bool, chunks: &[&[u8]]) -> (String, bool) {
        let mut f = OutputFilter::new(mouse);
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
        assert_eq!(run(false, &[b"a\x1b[?1049hb"]), ("ab".into(), true));
        assert_eq!(
            run(false, &[b"\x1b[?1049l\x1b[?47h\x1b[?1047l"]),
            ("".into(), true)
        );
        assert_eq!(run(false, &[b"x\x1bcy"]), ("xy".into(), true));
        // Combined modes keep the others.
        assert_eq!(run(false, &[b"\x1b[?1049;25h"]), ("ESC[?25h".into(), true));
    }

    #[test]
    fn strips_mouse_modes_when_owned() {
        let s: &[u8] = b"\x1b[?1000;1006h\x1b[?1003l";
        assert_eq!(run(true, &[s]), ("".into(), true));
        assert_eq!(
            run(false, &[s]),
            ("ESC[?1000;1006hESC[?1003l".into(), false)
        );
    }

    #[test]
    fn keeps_everything_else() {
        let s: &[u8] =
            b"\x1b[?25l\x1b[2J\x1b[1;31mred\x1b[0m\x1b]0;title\x07\x1b7\x1b8\x1b[?10490h\x1b[?1007h";
        let (out, sw) = run(true, &[s]);
        assert_eq!(
            out.as_bytes(),
            String::from_utf8_lossy(s).replace('\x1b', "ESC").as_bytes()
        );
        assert!(!sw);
    }

    #[test]
    fn split_sequences() {
        assert_eq!(
            run(false, &[b"a\x1b", b"[?10", b"49hb"]),
            ("ab".into(), true)
        );
        assert_eq!(
            run(false, &[b"a\x1b[", b"31mb"]),
            ("aESC[31mb".into(), false)
        );
        // An unterminated escape doesn't swallow output forever.
        let long = [b"\x1b[".as_slice(), &[b'1'; 100]].concat();
        let (out, _) = run(false, &[&long]);
        assert_eq!(out.len(), 3 + 1 + 100);
    }

    #[test]
    fn splits_mouse_reports_from_keys() {
        let (parts, held) = split_input(b"ab\x1b[<65;10;5Mc\x1b[<0;1;2m\x1b[<64;3");
        assert_eq!(held, 7);
        let got: Vec<String> = parts
            .iter()
            .map(|p| match p {
                Input::Keys(k) => String::from_utf8_lossy(k).into_owned(),
                Input::Mouse(m) => format!("{m:?}"),
            })
            .collect();
        assert_eq!(
            got,
            [
                "ab",
                "MouseReport { button: 65, col: 10, row: 5, press: true }",
                "c",
                "MouseReport { button: 0, col: 1, row: 2, press: false }",
            ]
        );
        // Other sequences, including malformed reports, are keys.
        let (parts, held) = split_input(b"\x1b[A\x1b[<1;2X");
        assert_eq!((parts.len(), held), (1, 0));
    }
}

// ---------------------------------------------------------------------------
// libghostty-vt backed screen

#[cfg(feature = "ghostty")]
pub mod ghostty {
    use super::{MouseReport, OutputFilter, Screen, Update, reset_modes};
    use crate::ghostty::*;
    use crate::sys::WinSize;
    use std::ffi::{c_int, c_void};

    pub struct GhosttyScreen {
        term: GhosttyTerminal,
        size: WinSize,
        // Boxed so the pointer handed to libghostty as userdata stays stable
        // even though `GhosttyScreen` itself gets moved around.
        #[allow(clippy::box_collection)]
        pending: Box<Vec<u8>>,
        filter: OutputFilter,
        encoder: GhosttyMouseEncoder,
        event: GhosttyMouseEvent,
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
            // From here on, Drop frees whatever was created.
            let mut s = GhosttyScreen {
                term,
                size,
                pending: Box::new(Vec::new()),
                filter: OutputFilter::new(true),
                encoder: std::ptr::null_mut(),
                event: std::ptr::null_mut(),
            };
            unsafe {
                if ghostty_mouse_encoder_new(std::ptr::null(), &raw mut s.encoder)
                    != GHOSTTY_SUCCESS
                    || ghostty_mouse_event_new(std::ptr::null(), &raw mut s.event)
                        != GHOSTTY_SUCCESS
                {
                    return Err("ghostty mouse encoder init failed".into());
                }
                let ud: *mut Vec<u8> = &mut *s.pending;
                ghostty_terminal_set(term, GHOSTTY_TERMINAL_OPT_USERDATA, ud.cast_const().cast());
                ghostty_terminal_set(
                    term,
                    GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                    write_pty_cb as *const c_void,
                );
                // Keep history like Ghostty does (its default scrollback-limit).
                let max_bytes: usize = 10_000_000;
                ghostty_terminal_set(
                    term,
                    GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
                    (&raw const max_bytes).cast(),
                );
            }
            Ok(s)
        }

        fn get<T: Default>(&self, data: c_int) -> Option<T> {
            let mut v = T::default();
            let r = unsafe { ghostty_terminal_get(self.term, data, (&raw mut v).cast()) };
            (r == GHOSTTY_SUCCESS).then_some(v)
        }

        fn alt_screen_active(&self) -> bool {
            self.get::<c_int>(GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN)
                == Some(GHOSTTY_TERMINAL_SCREEN_ALTERNATE)
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

        fn mouse_tracking(&self) -> bool {
            self.get(GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING)
                .unwrap_or(false)
        }

        /// Whether the view is on the live screen rather than scrolled back.
        fn at_bottom(&self) -> bool {
            self.get(GHOSTTY_TERMINAL_DATA_VIEWPORT_ACTIVE)
                .unwrap_or(true)
        }

        fn scrollbar(&self) -> GhosttyTerminalScrollbar {
            self.get(GHOSTTY_TERMINAL_DATA_SCROLLBAR)
                .unwrap_or_default()
        }

        fn scroll(&mut self, tag: c_int, delta: isize) {
            let behavior = GhosttyTerminalScrollViewport {
                tag,
                value: [delta, 0],
            };
            unsafe { ghostty_terminal_scroll_viewport(self.term, behavior) };
        }

        /// The mouse modes for the client's terminal: always on, so the wheel
        /// reaches us, and at least as much as the program asked for. Always
        /// SGR; `encode_mouse` converts to the program's format.
        fn outer_mouse(&self) -> &'static [u8] {
            if self.mode(1003) == Some(true) {
                b"\x1b[?1003h\x1b[?1006h"
            } else if self.mode(1002) == Some(true) {
                b"\x1b[?1002h\x1b[?1006h"
            } else {
                b"\x1b[?1000h\x1b[?1006h"
            }
        }

        /// The rows of `region` (the active area or the viewport) as VT.
        fn format_vt(&self, region: c_int, extras: u32) -> Vec<u8> {
            let mut f: GhosttyFormatter = std::ptr::null_mut();
            let mut out = Vec::new();
            let point = |x, y| GhosttyPoint {
                tag: region,
                value: GhosttyPointCoordinate { x, y, _padding: 0 },
            };
            let null_ref = GhosttyGridRef {
                node: std::ptr::null_mut(),
                x: 0,
                y: 0,
            };
            let mut sel = GhosttySelection {
                start: null_ref,
                end: null_ref,
                rectangle: false,
            };
            unsafe {
                if ghostty_terminal_grid_ref(self.term, point(0, 0), &raw mut sel.start)
                    != GHOSTTY_SUCCESS
                    || ghostty_terminal_grid_ref(
                        self.term,
                        point(self.size.cols - 1, u32::from(self.size.rows) - 1),
                        &raw mut sel.end,
                    ) != GHOSTTY_SUCCESS
                    || ghostty_formatter_terminal_new(std::ptr::null(), &raw mut f, self.term)
                        != GHOSTTY_SUCCESS
                {
                    return out;
                }
                ghostty_formatter_set(f, GHOSTTY_FORMATTER_OPT_SELECTION, (&raw const sel).cast());
                let emit = GHOSTTY_FORMATTER_FORMAT_VT;
                ghostty_formatter_set(f, GHOSTTY_FORMATTER_OPT_EMIT, (&raw const emit).cast());
                let trim = false;
                ghostty_formatter_set(f, GHOSTTY_FORMATTER_OPT_TRIM, (&raw const trim).cast());
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

        /// Repaint for a view scrolled back into history: just the rows, no
        /// cursor. The program's modes stay as they are on the client.
        fn viewport(&self) -> Vec<u8> {
            // Full-screen scroll region and absolute origin, so the rows land
            // where they belong; the snapshot restores both when back at the bottom.
            let mut out = b"\x1b[?2026h\x1b[r\x1b[?6l\x1b[H\x1b[2J\x1b[0m".to_vec();
            out.extend_from_slice(
                &self.format_vt(GHOSTTY_POINT_TAG_VIEWPORT, EXTRA_STYLE | EXTRA_HYPERLINK),
            );
            out.extend_from_slice(b"\x1b[0m\x1b[?25l\x1b[?2026l");
            out
        }

        /// Translate a report from the client's terminal (SGR, cells) into
        /// what the program asked for, with libghostty's mouse encoder.
        fn encode_mouse(&mut self, m: &MouseReport) -> Vec<u8> {
            let b = m.button;
            let mut mods = 0;
            for (bit, m) in [
                (4, GHOSTTY_MODS_SHIFT),
                (8, GHOSTTY_MODS_ALT),
                (16, GHOSTTY_MODS_CTRL),
            ] {
                if b & bit != 0 {
                    mods |= m;
                }
            }
            let motion = b & 32 != 0;
            let button = match b & !(4 | 8 | 16 | 32) {
                0 => Some(1), // left
                1 => Some(3), // middle
                2 => Some(2), // right
                n @ 64..=67 => Some(c_int::from(n) - 60),
                n @ 128..=131 => Some(c_int::from(n) - 120),
                _ => None,
            };
            let action = match (m.press, motion) {
                (false, _) => GHOSTTY_MOUSE_ACTION_RELEASE,
                (true, true) => GHOSTTY_MOUSE_ACTION_MOTION,
                (true, false) => GHOSTTY_MOUSE_ACTION_PRESS,
            };
            // Cells as pixels: one pixel per cell, positioned at the cell's middle.
            let size = GhosttyMouseEncoderSize {
                size: size_of::<GhosttyMouseEncoderSize>(),
                screen_width: u32::from(self.size.cols),
                screen_height: u32::from(self.size.rows),
                cell_width: 1,
                cell_height: 1,
                padding_top: 0,
                padding_bottom: 0,
                padding_right: 0,
                padding_left: 0,
            };
            let any_pressed = motion && button.is_some();
            let mut buf = [0u8; 128];
            let mut n = 0;
            unsafe {
                let (enc, ev) = (self.encoder, self.event);
                ghostty_mouse_encoder_set_from_terminal(enc, self.term);
                ghostty_mouse_encoder_set(
                    enc,
                    GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                    (&raw const size).cast(),
                );
                ghostty_mouse_encoder_set(
                    enc,
                    GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED,
                    (&raw const any_pressed).cast(),
                );
                ghostty_mouse_event_set_action(ev, action);
                match button {
                    Some(button) => ghostty_mouse_event_set_button(ev, button),
                    None => ghostty_mouse_event_clear_button(ev),
                };
                ghostty_mouse_event_set_mods(ev, mods);
                ghostty_mouse_event_set_position(
                    ev,
                    GhosttyMousePosition {
                        x: f32::from(m.col.max(1) - 1) + 0.5,
                        y: f32::from(m.row.max(1) - 1) + 0.5,
                    },
                );
                if ghostty_mouse_encoder_encode(enc, ev, buf.as_mut_ptr(), buf.len(), &raw mut n)
                    != GHOSTTY_SUCCESS
                {
                    n = 0;
                }
            }
            buf[..n].to_vec()
        }
    }

    impl Screen for GhosttyScreen {
        fn write(&mut self, data: &[u8]) -> Update {
            unsafe { ghostty_terminal_vt_write(self.term, data.as_ptr(), data.len()) };
            let (out, stripped) = self.filter.feed(data);
            if !self.at_bottom() {
                // Scrolled back: repaint only if the view shows part of the live screen.
                let sb = self.scrollbar();
                let live_top = sb.total.saturating_sub(u64::from(self.size.rows));
                return if sb.offset + sb.len > live_top {
                    Update::Repaint
                } else {
                    Update::Forward(Vec::new())
                };
            }
            if stripped {
                Update::Repaint
            } else {
                Update::Forward(out)
            }
        }

        fn resize(&mut self, size: WinSize) -> bool {
            self.size = size;
            unsafe { ghostty_terminal_resize(self.term, size.cols, size.rows, 0, 0) };
            !self.at_bottom()
        }

        fn snapshot(&mut self) -> Vec<u8> {
            if !self.at_bottom() {
                return self.viewport();
            }
            // Reset the client's terminal, clear, then let the formatter emit the
            // screen plus every mode that differs from default (cursor
            // visibility, bracketed paste, cursor position, ...).
            let mut out = reset_modes().to_vec();
            out.extend_from_slice(b"\x1b[?2026h"); // synchronized update: no flicker
            out.extend_from_slice(b"\x1b[H\x1b[2J");
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
            let vt = self.format_vt(GHOSTTY_POINT_TAG_ACTIVE, extras);
            out.extend_from_slice(&OutputFilter::strip(true, &vt));
            if self.mode(66) == Some(true) {
                out.extend_from_slice(b"\x1b="); // application keypad isn't a CSI mode
            }
            out.extend_from_slice(self.outer_mouse());
            if self.mode(2026) != Some(true) {
                out.extend_from_slice(b"\x1b[?2026l");
            }
            out
        }

        fn take_pty_responses(&mut self) -> Vec<u8> {
            std::mem::take(&mut *self.pending)
        }

        // Like Ghostty: a program that tracks the mouse gets the events; on
        // the alternate screen the wheel becomes arrow keys (mode 1007);
        // otherwise it scrolls through the scrollback.
        fn mouse(&mut self, m: &MouseReport) -> (Vec<u8>, bool) {
            if self.mouse_tracking() {
                return (self.encode_mouse(m), false);
            }
            let Some(delta) = m.wheel().filter(|_| m.press) else {
                return (Vec::new(), false);
            };
            if self.alt_screen_active() {
                if self.mode(1007) != Some(true) {
                    return (Vec::new(), false);
                }
                let arrow: &[u8] = match (delta < 0, self.mode(1) == Some(true)) {
                    (true, true) => b"\x1bOA",
                    (true, false) => b"\x1b[A",
                    (false, true) => b"\x1bOB",
                    (false, false) => b"\x1b[B",
                };
                return (arrow.to_vec(), false);
            }
            let before = self.scrollbar().offset;
            self.scroll(GHOSTTY_TERMINAL_SCROLL_VIEWPORT_DELTA, delta);
            (Vec::new(), self.scrollbar().offset != before)
        }

        fn scroll_to_bottom(&mut self) -> bool {
            if self.at_bottom() {
                return false;
            }
            self.scroll(GHOSTTY_TERMINAL_SCROLL_VIEWPORT_BOTTOM, 0);
            true
        }
    }

    impl Drop for GhosttyScreen {
        fn drop(&mut self) {
            unsafe {
                if !self.event.is_null() {
                    ghostty_mouse_event_free(self.event);
                }
                if !self.encoder.is_null() {
                    ghostty_mouse_encoder_free(self.encoder);
                }
                ghostty_terminal_free(self.term);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn esc(b: &[u8]) -> String {
            String::from_utf8_lossy(b).replace('\x1b', "ESC")
        }

        fn wheel(s: &mut GhosttyScreen, up: bool) -> (String, bool) {
            let m = MouseReport {
                button: if up { 64 } else { 65 },
                col: 1,
                row: 1,
                press: true,
            };
            let (bytes, moved) = s.mouse(&m);
            (esc(&bytes), moved)
        }

        #[test]
        fn snapshot_alt_screen_and_modes() {
            let mut s = GhosttyScreen::new(WinSize { cols: 20, rows: 4 }).unwrap();
            let _ =
                s.write(b"\x1b[?1049h\x1b[?25l\x1b[?1003h\x1b[?2004h\x1b[2;3H\x1b[1;32mhi\x1b[0m");
            let snap = esc(&s.snapshot());
            // The client stays on the screen pear gave it.
            assert!(!snap.contains("1049"));
            assert!(snap.contains("hi"));
            assert!(snap.contains("ESC[?25l"));
            assert!(snap.contains("ESC[?2004h"));
            assert!(snap.ends_with("ESC[?1003hESC[?1006hESC[?2026l"), "{snap}");
            assert!(s.alt_screen_active());
        }

        #[test]
        fn wheel_scrolls_scrollback() {
            let mut s = GhosttyScreen::new(WinSize { cols: 20, rows: 4 }).unwrap();
            for i in 0..100 {
                let _ = s.write(format!("line {i}\r\n").as_bytes());
            }
            let snap = esc(&s.snapshot());
            assert!(
                !snap.contains("line 0\r\n") && snap.contains("line 99"),
                "{snap}"
            );
            assert!(snap.ends_with("ESC[?1000hESC[?1006hESC[?2026l"), "{snap}");

            assert_eq!(wheel(&mut s, true), (String::new(), true));
            let snap = esc(&s.snapshot());
            assert!(
                snap.contains("line 96") && snap.contains("line 99"),
                "{snap}"
            );
            // Output while the live screen is still in view repaints...
            assert!(matches!(s.write(b"more\r\n"), Update::Repaint));
            // ...but not once it's scrolled out of view.
            for _ in 0..10 {
                wheel(&mut s, true);
            }
            assert!(matches!(s.write(b"more\r\n"), Update::Forward(v) if v.is_empty()));
            let snap = esc(&s.snapshot());
            assert!(snap.contains("line 86") && !snap.contains("more"), "{snap}");

            assert!(s.scroll_to_bottom());
            assert!(!s.scroll_to_bottom());
            let snap = esc(&s.snapshot());
            assert!(snap.contains("more") && !snap.contains("line 86"), "{snap}");
            assert_eq!(wheel(&mut s, false), (String::new(), false));
        }

        #[test]
        fn wheel_on_alt_screen_sends_arrows() {
            let mut s = GhosttyScreen::new(WinSize { cols: 20, rows: 4 }).unwrap();
            let _ = s.write(b"\x1b[?1049h");
            assert_eq!(wheel(&mut s, true), ("ESC[A".into(), false));
            let _ = s.write(b"\x1b[?1h");
            assert_eq!(wheel(&mut s, false), ("ESCOB".into(), false));
        }

        #[test]
        fn mouse_goes_to_programs_that_track_it() {
            let mut s = GhosttyScreen::new(WinSize { cols: 20, rows: 4 }).unwrap();
            let click = MouseReport {
                button: 0,
                col: 3,
                row: 2,
                press: true,
            };
            assert_eq!(s.mouse(&click), (Vec::new(), false));
            // X10 encoding: ESC [ M, then button, column, row, each + 32.
            let _ = s.write(b"\x1b[?1000h");
            assert_eq!(esc(&s.mouse(&click).0), "ESC[M #\"");
            let _ = s.write(b"\x1b[?1006h");
            assert_eq!(esc(&s.mouse(&click).0), "ESC[<0;3;2M");
            assert_eq!(wheel(&mut s, true), ("ESC[<64;1;1M".into(), false));
        }

        #[test]
        fn pty_responses_are_collected() {
            let mut s = GhosttyScreen::new(WinSize { cols: 20, rows: 4 }).unwrap();
            let _ = s.write(b"\x1b[6n"); // DSR cursor position
            let r = s.take_pty_responses();
            assert_eq!(esc(&r), "ESC[1;1R");
        }
    }
}
