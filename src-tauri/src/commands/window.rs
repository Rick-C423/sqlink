/// 切换窗口原生标题栏的深浅色（Windows 沉浸式标题栏）
#[tauri::command]
pub fn set_window_theme(theme: String, window: tauri::Window) -> Result<(), String> {
    let t = if theme == "dark" {
        tauri::Theme::Dark
    } else {
        tauri::Theme::Light
    };
    // tao 内部对 set_theme 同步处理：DWM 沉浸式深色属性已在此调用内生效
    window.set_theme(Some(t)).map_err(|e| e.to_string())?;

    // tao 用模拟 WM_NCACTIVATE 触发标题栏重绘，但首次切换经常不生效，
    // 表现为标题栏延迟到下一次交互（点一下窗口）才变色。
    // 这里显式要求非客户区重新计算布局并立即重绘，包括子窗口。
    #[cfg(windows)]
    if let Ok(hwnd) = window.hwnd() {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::Graphics::Gdi::{
            RedrawWindow, RDW_ALLCHILDREN, RDW_FRAME, RDW_INVALIDATE, RDW_UPDATENOW,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            SetWindowPos, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
        };
        let hwnd = HWND(hwnd.0);
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
            let _ = RedrawWindow(
                Some(hwnd),
                None,
                None,
                RDW_FRAME | RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN,
            );
        }
    }
    #[cfg(not(windows))]
    let _ = t;
    Ok(())
}
