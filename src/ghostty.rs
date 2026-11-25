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

#[repr(C)]
pub struct GhosttyMouseEncoderImpl {
    _p: [u8; 0],
}
#[repr(C)]
pub struct GhosttyMouseEventImpl {
    _p: [u8; 0],
}

pub type GhosttyTerminal = *mut GhosttyTerminalImpl;
pub type GhosttyFormatter = *mut GhosttyFormatterImpl;
pub type GhosttyMouseEncoder = *mut GhosttyMouseEncoderImpl;
pub type GhosttyMouseEvent = *mut GhosttyMouseEventImpl;

pub type GhosttyTerminalWritePtyFn =
    unsafe extern "C" fn(term: GhosttyTerminal, userdata: *mut c_void, data: *const u8, len: usize);

// GhosttyTerminalOption
pub const GHOSTTY_TERMINAL_OPT_USERDATA: c_int = 1;
pub const GHOSTTY_TERMINAL_OPT_WRITE_PTY: c_int = 2;
pub const GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES: c_int = 28;

// GhosttyTerminalData
pub const GHOSTTY_TERMINAL_DATA_CURSOR_VISIBLE: c_int = 7;
pub const GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN: c_int = 6;
pub const GHOSTTY_TERMINAL_DATA_SCROLLBAR: c_int = 9;
pub const GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING: c_int = 11;
pub const GHOSTTY_TERMINAL_DATA_VIEWPORT_ACTIVE: c_int = 32;
pub const GHOSTTY_TERMINAL_DATA_MODE: c_int = 37;

pub const GHOSTTY_TERMINAL_SCREEN_PRIMARY: c_int = 0;
pub const GHOSTTY_TERMINAL_SCREEN_ALTERNATE: c_int = 1;

#[repr(C)]
#[derive(Default)]
pub struct GhosttyTerminalScrollbar {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}

// GhosttyTerminalScrollViewportTag
pub const GHOSTTY_TERMINAL_SCROLL_VIEWPORT_BOTTOM: c_int = 1;
pub const GHOSTTY_TERMINAL_SCROLL_VIEWPORT_DELTA: c_int = 2;

#[repr(C)]
pub struct GhosttyTerminalScrollViewport {
    pub tag: c_int,
    /// Union; the delta (intptr_t) is the first member.
    pub value: [isize; 2],
}

// GhosttyPointTag
pub const GHOSTTY_POINT_TAG_ACTIVE: c_int = 0;
pub const GHOSTTY_POINT_TAG_VIEWPORT: c_int = 1;

#[repr(C)]
pub struct GhosttyPoint {
    pub tag: c_int,
    pub value: GhosttyPointCoordinate,
}

/// The GhosttyPointValue union: its coordinate member, padded to the union's size.
#[repr(C, align(8))]
pub struct GhosttyPointCoordinate {
    pub x: u16,
    pub y: u32,
    pub _padding: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyGridRef {
    pub node: *mut c_void,
    pub x: u16,
    pub y: u16,
}

#[repr(C)]
pub struct GhosttySelection {
    pub start: GhosttyGridRef,
    pub end: GhosttyGridRef,
    pub rectangle: bool,
}

// GhosttyMouseAction
pub const GHOSTTY_MOUSE_ACTION_PRESS: c_int = 0;
pub const GHOSTTY_MOUSE_ACTION_RELEASE: c_int = 1;
pub const GHOSTTY_MOUSE_ACTION_MOTION: c_int = 2;

// GhosttyMods
pub const GHOSTTY_MODS_SHIFT: u16 = 1 << 0;
pub const GHOSTTY_MODS_CTRL: u16 = 1 << 1;
pub const GHOSTTY_MODS_ALT: u16 = 1 << 2;

#[repr(C)]
pub struct GhosttyMousePosition {
    pub x: f32,
    pub y: f32,
}

/// Sized struct: `size` must be `size_of::<GhosttyMouseEncoderSize>()`.
#[repr(C)]
pub struct GhosttyMouseEncoderSize {
    pub size: usize,
    pub screen_width: u32,
    pub screen_height: u32,
    pub cell_width: u32,
    pub cell_height: u32,
    pub padding_top: u32,
    pub padding_bottom: u32,
    pub padding_right: u32,
    pub padding_left: u32,
}

// GhosttyMouseEncoderOption
pub const GHOSTTY_MOUSE_ENCODER_OPT_SIZE: c_int = 3;
pub const GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED: c_int = 4;

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
pub const GHOSTTY_FORMATTER_OPT_SELECTION: c_int = 4;
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

    pub fn ghostty_terminal_scroll_viewport(
        terminal: GhosttyTerminal,
        behavior: GhosttyTerminalScrollViewport,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_grid_ref(
        terminal: GhosttyTerminal,
        point: GhosttyPoint,
        out_ref: *mut GhosttyGridRef,
    ) -> GhosttyResult;

    pub fn ghostty_mouse_encoder_new(
        allocator: *const GhosttyAllocator,
        encoder: *mut GhosttyMouseEncoder,
    ) -> GhosttyResult;
    pub fn ghostty_mouse_encoder_free(encoder: GhosttyMouseEncoder);
    pub fn ghostty_mouse_encoder_set(
        encoder: GhosttyMouseEncoder,
        option: c_int,
        value: *const c_void,
    ) -> GhosttyResult;
    pub fn ghostty_mouse_encoder_set_from_terminal(
        encoder: GhosttyMouseEncoder,
        terminal: GhosttyTerminal,
    ) -> GhosttyResult;
    pub fn ghostty_mouse_encoder_encode(
        encoder: GhosttyMouseEncoder,
        event: GhosttyMouseEvent,
        buf: *mut u8,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;

    pub fn ghostty_mouse_event_new(
        allocator: *const GhosttyAllocator,
        event: *mut GhosttyMouseEvent,
    ) -> GhosttyResult;
    pub fn ghostty_mouse_event_free(event: GhosttyMouseEvent);
    pub fn ghostty_mouse_event_set_action(event: GhosttyMouseEvent, action: c_int)
    -> GhosttyResult;
    pub fn ghostty_mouse_event_set_button(event: GhosttyMouseEvent, button: c_int)
    -> GhosttyResult;
    pub fn ghostty_mouse_event_clear_button(event: GhosttyMouseEvent) -> GhosttyResult;
    pub fn ghostty_mouse_event_set_mods(event: GhosttyMouseEvent, mods: u16) -> GhosttyResult;
    pub fn ghostty_mouse_event_set_position(
        event: GhosttyMouseEvent,
        position: GhosttyMousePosition,
    ) -> GhosttyResult;

    pub fn ghostty_free(allocator: *const GhosttyAllocator, ptr: *mut u8, len: usize);
}
