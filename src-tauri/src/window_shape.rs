//! Rounded window corners. The window has no native frame, so it would be
//! square: Windows 11 is asked for its native rounded corners here, and macOS
//! uses a transparent window (`tauri.macos.conf.json`) whose page draws the
//! radius (`frontend/src/lib/windowShape.ts`). Windows 10 has no rounded
//! corners for windows and stays square.

#[cfg(windows)]
pub fn round_corners(window: &tauri::WebviewWindow) {
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };
    let Ok(hwnd) = window.hwnd() else {
        return;
    };
    let preference = DWMWCP_ROUND;
    // Fails harmlessly on Windows 10, which does not know the attribute.
    unsafe {
        DwmSetWindowAttribute(
            hwnd.0 as _,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&preference as *const i32).cast(),
            std::mem::size_of_val(&preference) as u32,
        );
    }
}

#[cfg(not(windows))]
pub fn round_corners(_window: &tauri::WebviewWindow) {}
