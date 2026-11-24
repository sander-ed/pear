//! Minimal FFI bindings to libghostty-vt (https://lib.ghostty.org/vt/).
//!
//! Only the handful of functions pear needs. The C ABI is stable from 1.0.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::c_void;
use std::os::raw::c_int;

pub type GhosttyResult = c_int;
pub const GHOSTTY_SUCCESS: GhosttyResult = 0;

#[repr(C)]
pub struct GhosttyTerminalImpl {
    _p: [u8; 0],
}
#[repr(C)]
pub struct GhosttyFormatterImpl {
    _p: [u8; 0],
}
#[repr(C)]
pub struct GhosttyAllocator {
    _p: [u8; 0],
}

pub type GhosttyTerminal = *mut GhosttyTerminalImpl;
pub type GhosttyFormatter = *mut GhosttyFormatterImpl;

pub type GhosttyTerminalWritePtyFn =
    unsafe extern "C" fn(term: GhosttyTerminal, userdata: *mut c_void, data: *const u8, len: usize);

// GhosttyTerminalOption
pub const GHOSTTY_TERMINAL_OPT_USERDATA: c_int = 1;
pub const GHOSTTY_TERMINAL_OPT_WRITE_PTY: c_int = 2;
pub const GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES: c_int = 29;

// GhosttyTerminalData
pub const GHOSTTY_TERMINAL_DATA_CURSOR_VISIBLE: c_int = 7;
pub const GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN: c_int = 6;
pub const GHOSTTY_TERMINAL_DATA_MODE: c_int = 37;

pub const GHOSTTY_TERMINAL_SCREEN_PRIMARY: c_int = 0;
pub const GHOSTTY_TERMINAL_SCREEN_ALTERNATE: c_int = 1;

/// GhosttyMode is a packed u16: bits 0-14 = number, bit 15 = ANSI (vs DEC private).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyTerminalModeConfig {
    pub mode: u16,
    pub value: bool,
}

pub const fn dec_mode(n: u16) -> u16 {
    n & 0x7FFF
}

// GhosttyFormatterOption
pub const GHOSTTY_FORMATTER_OPT_EMIT: c_int = 1;
pub const GHOSTTY_FORMATTER_OPT_TRIM: c_int = 3;
pub const GHOSTTY_FORMATTER_OPT_EXTRAS: c_int = 5;

// GhosttyFormatterFormat
pub const GHOSTTY_FORMATTER_FORMAT_VT: c_int = 1;

// GhosttyFormatterExtras bits
pub const EXTRA_PALETTE: u32 = 1 << 0;
pub const EXTRA_MODES: u32 = 1 << 1;
pub const EXTRA_SCROLLING_REGION: u32 = 1 << 2;
pub const EXTRA_TABSTOPS: u32 = 1 << 3;
pub const EXTRA_PWD: u32 = 1 << 4;
pub const EXTRA_KEYBOARD: u32 = 1 << 5;
pub const EXTRA_CURSOR: u32 = 1 << 6;
pub const EXTRA_STYLE: u32 = 1 << 7;
pub const EXTRA_HYPERLINK: u32 = 1 << 8;
pub const EXTRA_PROTECTION: u32 = 1 << 9;
pub const EXTRA_KITTY_KEYBOARD: u32 = 1 << 10;
pub const EXTRA_CHARSETS: u32 = 1 << 11;

unsafe extern "C" {
    pub fn ghostty_terminal_new(
        allocator: *const GhosttyAllocator,
        terminal: *mut GhosttyTerminal,
        cols: u16,
        rows: u16,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_free(terminal: GhosttyTerminal);
    pub fn ghostty_terminal_vt_write(terminal: GhosttyTerminal, data: *const u8, len: usize);
    pub fn ghostty_terminal_resize(
        terminal: GhosttyTerminal,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_set(
        terminal: GhosttyTerminal,
        option: c_int,
        value: *const c_void,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_get(
        terminal: GhosttyTerminal,
        data: c_int,
        out: *mut c_void,
    ) -> GhosttyResult;

    pub fn ghostty_formatter_terminal_new(
        allocator: *const GhosttyAllocator,
        formatter: *mut GhosttyFormatter,
        terminal: GhosttyTerminal,
    ) -> GhosttyResult;
    pub fn ghostty_formatter_set(
        formatter: GhosttyFormatter,
        option: c_int,
        value: *const c_void,
    ) -> GhosttyResult;
    pub fn ghostty_formatter_format_alloc(
        formatter: GhosttyFormatter,
        allocator: *const GhosttyAllocator,
        out_ptr: *mut *mut u8,
        out_len: *mut usize,
    ) -> GhosttyResult;
    pub fn ghostty_formatter_free(formatter: GhosttyFormatter);

    pub fn ghostty_free(allocator: *const GhosttyAllocator, ptr: *mut u8, len: usize);
}
