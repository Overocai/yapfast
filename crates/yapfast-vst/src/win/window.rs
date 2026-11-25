//! The windows plugins' editors open in, and the hidden window the plugins'
//! thread takes requests on: Win32, on that thread.

use std::{
    ptr::{null, null_mut},
    sync::Once,
};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
        AdjustWindowRectEx, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, GWL_EXSTYLE, GWL_STYLE,
        GetWindowLongPtrW, HWND_MESSAGE, IDC_ARROW, IsIconic, LoadCursorW, RegisterClassExW,
        SW_RESTORE, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetForegroundWindow,
        SetWindowPos, ShowWindow, WNDCLASSEXW, WS_CAPTION, WS_CLIPCHILDREN, WS_MINIMIZEBOX,
        WS_OVERLAPPED, WS_SYSMENU,
    },
};

/// The window classes' names.
const EDITOR: &str = "YapfastPluginEditor";
const MESSAGES: &str = "YapfastPluginMessages";

/// An editor window's style: a title bar with close and minimize, fixed
/// in size, as plugins draw their own.
const STYLE: u32 = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;

static REGISTERED: Once = Once::new();

type Procedure = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// Registers the window classes once, both taking their messages at
/// `procedure`.
pub fn register(procedure: Procedure) {
    REGISTERED.call_once(|| {
        // SAFETY: this module's own instance and classes, each named by a
        // null-terminated string that lives through the call.
        unsafe {
            let instance = GetModuleHandleW(null());
            for name in [EDITOR, MESSAGES] {
                let name = wide(name);
                let class = WNDCLASSEXW {
                    cbSize: size_of::<WNDCLASSEXW>() as u32,
                    style: 0,
                    lpfnWndProc: Some(procedure),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: instance,
                    hIcon: null_mut(),
                    hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                    hbrBackground: null_mut(),
                    lpszMenuName: null(),
                    lpszClassName: name.as_ptr(),
                    hIconSm: null_mut(),
                };
                RegisterClassExW(&class);
            }
        }
    });
}

/// The hidden window the plugins' thread takes requests on: a message to it
/// is dispatched even inside a plugin's own dialog, which a message to the
/// thread itself wouldn't be.
pub fn messages() -> HWND {
    let class = wide(MESSAGES);
    // SAFETY: a registered class, as a message-only window.
    unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        )
    }
}

/// A new editor window titled `title`, whose inside is `width` by
/// `height`, not yet shown.
pub fn editor(title: &str, width: i32, height: i32) -> HWND {
    let (class, title) = (wide(EDITOR), wide(title));
    let (outer_width, outer_height) = outer(STYLE, 0, width, height);
    // SAFETY: a registered class and null-terminated texts that live
    // through the call.
    unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            STYLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            outer_width,
            outer_height,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        )
    }
}

/// The window's size for an inside `width` by `height` in its `style`.
fn outer(style: u32, extended: u32, width: i32, height: i32) -> (i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    // SAFETY: a rectangle to adjust.
    unsafe { AdjustWindowRectEx(&mut rect, style, 0, extended) };
    (rect.right - rect.left, rect.bottom - rect.top)
}

/// Resizes `window` so its inside is `width` by `height`, as a plugin's
/// editor asks.
pub fn resize(window: HWND, width: i32, height: i32) {
    if window.is_null() || width <= 0 || height <= 0 {
        return;
    }
    // SAFETY: a window of this thread's, read and resized.
    unsafe {
        let style = GetWindowLongPtrW(window, GWL_STYLE) as u32;
        let extended = GetWindowLongPtrW(window, GWL_EXSTYLE) as u32;
        let (outer_width, outer_height) = outer(style, extended, width, height);
        SetWindowPos(
            window,
            null_mut(),
            0,
            0,
            outer_width,
            outer_height,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Shows `window` and brings it to the front.
pub fn show(window: HWND) {
    // SAFETY: a window of this thread's.
    unsafe {
        if IsIconic(window) != 0 {
            ShowWindow(window, SW_RESTORE);
        } else {
            ShowWindow(window, SW_SHOW);
        }
        SetForegroundWindow(window);
    }
}

/// What a window does with a message nobody else took.
///
/// # Safety
///
/// As for `DefWindowProcW`.
pub unsafe fn default(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: as the caller says.
    unsafe { DefWindowProcW(window, message, w, l) }
}
