// 在 Windows 上双击 release 版本时不弹出控制台窗口；debug 版本保留控制台以便查看日志。
// （该属性仅对 Windows 生效，macOS/Linux 上不应用）
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

fn main() {
    dsh_desktop_lib::run();
}
