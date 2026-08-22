/// 切换窗口原生标题栏的深浅色（Windows 沉浸式标题栏）
#[tauri::command]
pub fn set_window_theme(theme: String, window: tauri::Window) -> Result<(), String> {
    let t = if theme == "dark" {
        tauri::Theme::Dark
    } else {
        tauri::Theme::Light
    };
    window.set_theme(Some(t)).map_err(|e| e.to_string())
}
