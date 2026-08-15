use std::fs::OpenOptions;
use std::io::Write;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindow};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

/// dsh Web 服务默认绑定的回环主机地址。
const HOST: &str = "127.0.0.1";
/// dsh Web 服务默认监听的端口（对应 `dsh --profile web`）。
const PORT: u16 = 3080;
/// Web GUI 的本地规范访问地址。
const WEB_URL: &str = "http://127.0.0.1:3080";
/// 等待服务变为可达的最大时长，超时则判定启动失败。
const STARTUP_TIMEOUT: Duration = Duration::from_secs(180);
/// 等待服务就绪过程中的轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(400);

/// 由 npx 拉起的服务子进程，以及“本次应用实例是否拥有它”的标记。
///
/// 如果服务是本应用自己启动的，退出时会一并把它关掉；如果只是附着到一个
/// 已经在运行的服务（例如在终端里手动启动的），则不去动它。
#[derive(Default)]
struct ServerState {
    child: Mutex<Option<Child>>,
    owned: Mutex<bool>,
}

/// 等待服务就绪的三种可能结果。
enum WaitOutcome {
    /// 服务已就绪。
    Ready,
    /// 服务进程提前退出。
    Exited,
    /// 等待超时。
    Timeout,
}

/// 关闭确认是否正在处理中，防止用户连点关闭按钮时弹出多个确认框。
static CLOSE_CONFIRM_PENDING: AtomicBool = AtomicBool::new(false);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // 单实例插件：重复双击只会把已打开的窗口调到前台。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            focus_main(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(ServerState::default())
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            pick_folder
        ])
        .on_window_event(|window, event| {
            // 只有主窗口需要“会话进行中”的关闭确认；设置窗口直接关闭。
            if window.label() != "main" {
                return;
            }
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // 先阻止关闭，再异步判断是否有会话仍在进行，决定是否弹窗确认。
                api.prevent_close();
                if let Some(webview) = window.app_handle().get_webview_window(window.label()) {
                    confirm_close_if_busy(webview);
                }
            }
        })
        .setup(|app| {
            // 托盘图标：提供“打开主界面 / 设置… / 退出”入口。
            let open_item = MenuItem::with_id(app, "open", "打开主界面", true, None::<&str>)?;
            let settings_item = MenuItem::with_id(app, "settings", "设置…", true, None::<&str>)?;
            let stop_item = MenuItem::with_id(app, "stop-server", "停止服务并退出", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_item, &settings_item, &stop_item, &quit_item])?;
            if let Some(icon) = app.default_window_icon().cloned() {
                TrayIconBuilder::new()
                    .icon(icon)
                    .tooltip("DeepSeek Harness")
                    .menu(&menu)
                    .on_menu_event(|app, event| match event.id().as_ref() {
                        "open" => focus_main(app),
                        "settings" => open_settings(app),
                        "stop-server" => stop_server_and_exit(app),
                        "quit" => app.exit(0),
                        _ => {}
                    })
                    .build(app)?;
            }
            let handle = app.handle().clone();
            // “拉起 npx + 等待服务就绪”这段耗时逻辑放到主线程之外执行，
            // 这样启动画面才能保持响应、不卡死。
            std::thread::spawn(move || startup(handle));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("dsh-desktop 应用构建失败")
        .run(|app_handle, event| {
            if let RunEvent::Exit = event {
                cleanup(app_handle);
            }
        });
}

/// 首次启动允许的最大尝试次数。
///
/// dsh 首次拉起时，其 profile 模块软链接（`~/.dsh/profiles/node_modules`）
/// 可能因 npx 缓存目录被替换等原因短暂失效，表现为日志里的
/// “plugin tree failed to load / Cannot find package …”。dsh 每次启动都会
/// 自愈这些链接，因此失败后自动重试即可，无需用户手动关闭重开。
const MAX_STARTUP_ATTEMPTS: u32 = 3;

/// 启动流程：必要时拉起服务，就绪后把窗口跳转到 Web GUI；失败自动重试。
fn startup(handle: AppHandle) {
    let state = handle.state::<ServerState>();

    for attempt in 1..=MAX_STARTUP_ATTEMPTS {
        // 如果端口还没人在监听，就自己拉起 npx 服务。
        if !is_up() {
            match spawn_server() {
                Ok(child) => {
                    let mut guard = state.child.lock().unwrap();
                    // 重试场景：先回收上一次已经退出的子进程，再记录新子进程。
                    if let Some(mut old) = guard.take() {
                        kill_tree(&mut old);
                    }
                    *guard = Some(child);
                    *state.owned.lock().unwrap() = true;
                }
                Err(error) => {
                    show_failure(
                        &handle,
                        &format!("无法启动 npx @deepseek-ai/dsh web：\n{error}"),
                    );
                    return;
                }
            }
        }

        set_status(&handle, "DeepSeek Harness 正在启动，请稍候…");

        match wait_until_ready(&state) {
            WaitOutcome::Ready => {
                let h = handle.clone();
                // 窗口操作必须在主线程上执行。
                let _ = handle.run_on_main_thread(move || {
                    if let Some(window) = h.get_webview_window("main") {
                        if let Ok(url) = url::Url::parse(WEB_URL) {
                            let _ = window.navigate(url);
                        }
                    }
                });
                return;
            }
            WaitOutcome::Exited => {
                if attempt >= MAX_STARTUP_ATTEMPTS {
                    show_failure(
                        &handle,
                        "dsh web 进程多次启动失败，Web UI 未能启动。\n详见下方日志文件。",
                    );
                    return;
                }
                // 回收已退出的子进程，稍等片刻后自动重试：第二次启动会
                // 修复 npx 缓存替换造成的 profile 软链接失效等问题。
                let mut guard = state.child.lock().unwrap();
                if let Some(mut old) = guard.take() {
                    kill_tree(&mut old);
                }
                set_status(
                    &handle,
                    &format!("启动未成功，正在自动重试（{attempt}/{MAX_STARTUP_ATTEMPTS}）…"),
                );
                std::thread::sleep(Duration::from_secs(2));
            }
            WaitOutcome::Timeout => {
                show_failure(
                    &handle,
                    "等待 Web UI 启动超时（3 分钟）。\n详见下方日志文件。",
                );
                return;
            }
        }
    }
}

/// 探测 127.0.0.1:3080 是否已经在监听（即服务是否已在运行）。
fn is_up() -> bool {
    let addr: std::net::SocketAddr = format!("{HOST}:{PORT}").parse().expect("地址解析失败");
    TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok()
}

/// 应用配置目录（Windows：%LOCALAPPDATA%\dsh-desktop；
/// macOS/Linux：$XDG_DATA_HOME 或 ~/.local/share 下的 dsh-desktop），与日志同一目录。
fn config_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("share"))
        })
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("dsh-desktop");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 服务日志文件的路径（位于 %LOCALAPPDATA%\dsh-desktop\dsh-web.log）。
fn log_path() -> PathBuf {
    config_dir().join("dsh-web.log")
}

/// 应用配置文件的路径（位于 %LOCALAPPDATA%\dsh-desktop\config.json）。
fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

/// 桌面程序的本地配置（%LOCALAPPDATA%\dsh-desktop\config.json，手动编辑即可）。
#[derive(Default)]
struct AppConfig {
    /// 覆盖 dsh 的数据目录（等价于设置环境变量 DSH_HOME）。
    /// 用于把 .dsh 迁移到其他盘（如 D 盘）的场景。
    dsh_home: Option<PathBuf>,
    /// 覆盖 dsh 的工作目录（workspace）。
    workspace: Option<PathBuf>,
}

/// 读取配置文件；文件不存在或内容非法时返回全空的默认值。
fn load_config() -> AppConfig {
    let path = config_dir().join("config.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return AppConfig::default();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return AppConfig::default();
    };
    let as_dir = |value: Option<&serde_json::Value>| {
        value
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
    };
    AppConfig {
        dsh_home: as_dir(json.get("dshHome")),
        workspace: as_dir(json.get("workspace")),
    }
}

/// 解析 dsh 应使用的工作目录（workspace）。
///
/// dsh 的会话是按"启动时的当前目录（cwd）"分组的。双击启动的进程默认以安装
/// 目录为当前目录，会新建一个空的 workspace，导致终端里创建的会话在界面上
/// 看不到。优先级：环境变量 `DSH_DESKTOP_WORKSPACE` > 配置文件 `workspace`
/// > 用户主目录（等价于"打开终端后直接跑 npx"）。
fn workspace_dir(config: &AppConfig) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("DSH_DESKTOP_WORKSPACE") {
        let path = PathBuf::from(dir);
        if path.is_dir() {
            return Some(path);
        }
    }
    if let Some(workspace) = &config.workspace {
        return Some(workspace.clone());
    }
    // Windows 用 USERPROFILE，macOS/Linux 用 HOME。
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
}

/// 打开日志文件、写入本次启动的诊断行，并返回重定向用的 stdout/stderr。
fn log_streams(config: &AppConfig, workspace: &Option<PathBuf>) -> std::io::Result<(Stdio, Stdio)> {
    let log = log_path();
    let file = OpenOptions::new().create(true).append(true).open(&log)?;
    // 把本次使用的数据目录和工作目录写进日志，方便排查"会话看不到"的问题。
    let home_note = config
        .dsh_home
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "(默认 ~/.dsh)".to_string());
    let workspace_note = workspace
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "(继承进程目录)".to_string());
    let _ = writeln!(
        &file,
        "dsh-desktop: DSH_HOME={home_note} workspace={workspace_note}"
    );
    Ok((Stdio::from(file.try_clone()?), Stdio::from(file)))
}

/// 通过 npx 执行 `npx @deepseek-ai/dsh web`，并把输出重定向到日志文件。
///
/// Windows 实现：用 `cmd /C` 启动，并设置 CREATE_NO_WINDOW 避免闪出黑色控制台窗口。
#[cfg(target_os = "windows")]
fn spawn_server() -> std::io::Result<Child> {
    let config = load_config();
    let workspace = workspace_dir(&config);
    let (stdout, stderr) = log_streams(&config, &workspace)?;

    let mut command = Command::new("cmd");
    command
        .args(["/C", "npx", "@deepseek-ai/dsh", "web"])
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    // 若配置文件指定了 dshHome（例如把 .dsh 迁移到了 D 盘），把它作为
    // DSH_HOME 传给 dsh，让它去正确的数据目录读会话。
    if let Some(home) = &config.dsh_home {
        command.env("DSH_HOME", home);
    }
    // 关键：固定 dsh 的工作目录（workspace）。否则双击启动时进程的当前目录
    // 是安装目录，会话会被归到另一个 workspace，侧边栏里就看不到之前的会话。
    if let Some(dir) = workspace {
        command.current_dir(dir);
    }
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：避免拉起服务时闪出一个黑色控制台窗口。
        command.creation_flags(0x0800_0000);
    }
    command.spawn()
}

/// macOS / Linux 实现：直接执行 `npx`（npm 附带的可执行脚本）。
#[cfg(not(target_os = "windows"))]
fn spawn_server() -> std::io::Result<Child> {
    let config = load_config();
    let workspace = workspace_dir(&config);
    let (stdout, stderr) = log_streams(&config, &workspace)?;

    let mut command = Command::new("npx");
    command
        .args(["@deepseek-ai/dsh", "web"])
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    if let Some(home) = &config.dsh_home {
        command.env("DSH_HOME", home);
    }
    if let Some(dir) = workspace {
        command.current_dir(dir);
    }
    command.spawn()
}

/// 轮询等待服务就绪；子进程提前退出或超时都会给出对应的失败结果。
fn wait_until_ready(state: &ServerState) -> WaitOutcome {
    let start = Instant::now();
    loop {
        if is_up() {
            return WaitOutcome::Ready;
        }
        // 快速失败：如果子进程在服务还没起来之前就退出了，立刻返回失败。
        if *state.owned.lock().unwrap() {
            let mut guard = state.child.lock().unwrap();
            if let Some(child) = guard.as_mut() {
                if let Ok(Some(_status)) = child.try_wait() {
                    return WaitOutcome::Exited;
                }
            }
        }
        if start.elapsed() >= STARTUP_TIMEOUT {
            return WaitOutcome::Timeout;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// 应用退出时清理：若服务是本应用启动的，则连同其进程树一起结束。
fn cleanup(app: &AppHandle) {
    let state = app.state::<ServerState>();
    let mut guard = state.child.lock().unwrap();
    if let Some(mut child) = guard.take() {
        kill_tree(&mut child);
    }
}

/// Windows 下用 `taskkill /T` 结束整个进程树（npx → node → dsh 都在内）。
#[cfg(target_os = "windows")]
fn kill_tree(child: &mut Child) {
    let pid = child.id();
    let mut command = Command::new("taskkill");
    command.args(["/PID", &pid.to_string(), "/T", "/F"]);
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let _ = command.output();
    let _ = child.wait();
}

/// 非 Windows 平台退化为直接结束子进程。
#[cfg(not(target_os = "windows"))]
fn kill_tree(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// 托盘“停止服务并退出”：结束后台 dsh 服务（连同其子进程，如 MCP 服务器），
/// 然后退出程序。
///
/// 用于“附着模式”下用户想连后台服务一起关掉的场景：程序只负责关闭自己启动
/// 的服务，附着到外部服务时按设计不会自动关闭；这里通过端口找到监听进程，
/// 用 `taskkill /T` 结束整棵进程树，再正常退出应用。
fn stop_server_and_exit(app: &AppHandle) {
    let state = app.state::<ServerState>();
    {
        let mut guard = state.child.lock().unwrap();
        if let Some(mut child) = guard.take() {
            kill_tree(&mut child);
        }
    }
    #[cfg(target_os = "windows")]
    {
        // 附着模式下按端口找到监听进程（可能是其它实例遗留的服务），结束其进程树。
        if let Some(pid) = tcp_listener_pid(PORT) {
            let mut command = Command::new("taskkill");
            command.args(["/PID", &pid.to_string(), "/T", "/F"]);
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0800_0000);
            }
            let _ = command.output();
        }
    }
    app.exit(0);
}

/// 通过 `netstat` 找到监听指定 TCP 端口的进程 PID（Windows）。
#[cfg(target_os = "windows")]
fn tcp_listener_pid(port: u16) -> Option<u32> {
    let output = Command::new("netstat").args(["-ano", "-p", "tcp"]).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let needle = format!(":{} ", port);
    for line in text.lines() {
        if line.contains(&needle) && line.contains("LISTENING") {
            return line.split_whitespace().last()?.parse::<u32>().ok();
        }
    }
    None
}

/// 把主窗口取消最小化、显示并置为前台焦点。
fn focus_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// 关闭窗口前检测是否有仍在进行的会话，有则弹窗确认，避免误关。
///
/// dsh 界面上，运行中（`ongoing`）或等待操作（`warning`：等待审批/回答/计划
/// 审阅）的会话行都会渲染带 `data-state` 属性的状态点。这里用 `eval_with_callback`
/// 让页面把检测结果回传：存在这类会话就弹原生确认框，否则直接关闭窗口。
fn confirm_close_if_busy(window: WebviewWindow) {
    // 防止用户连点关闭按钮时弹出多个确认框。
    if CLOSE_CONFIRM_PENDING.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = window.app_handle().clone();
    let js = r#"(function(){ try { return document.querySelector('[data-state="ongoing"], [data-state="warning"]') !== null; } catch (e) { return false; } })()"#;
    let win = window.clone();
    let _ = window.eval_with_callback(js, move |result| {
        let busy = result.trim() == "true";
        if !busy {
            CLOSE_CONFIRM_PENDING.store(false, Ordering::SeqCst);
            let _ = win.destroy();
            return;
        }
        let win2 = win.clone();
        let app2 = app.clone();
        let _ = app.run_on_main_thread(move || {
            let should_close = app2
                .dialog()
                .message("当前有会话仍在进行中，关闭窗口会中断正在运行的任务。确定要关闭吗？")
                .title("会话仍在进行")
                .kind(MessageDialogKind::Warning)
                .buttons(MessageDialogButtons::OkCancelCustom(
                    "仍要关闭".to_string(),
                    "取消".to_string(),
                ))
                .blocking_show();
            CLOSE_CONFIRM_PENDING.store(false, Ordering::SeqCst);
            if should_close {
                let _ = win2.destroy();
            }
        });
    });
}

/// 在主线程上对窗口执行一段 JavaScript（用于更新启动画面）。
fn eval_js(handle: &AppHandle, js: String) {
    let h = handle.clone();
    let _ = handle.run_on_main_thread(move || {
        if let Some(window) = h.get_webview_window("main") {
            let _ = window.eval(&js);
        }
    });
}

/// 更新启动画面上的状态文字。
fn set_status(handle: &AppHandle, text: &str) {
    let js = format!(
        "document.getElementById('status').textContent = {};",
        serde_json::to_string(text).expect("字符串序列化失败")
    );
    eval_js(handle, js);
}

/// 在启动画面上显示启动失败信息，并附上日志文件路径。
fn show_failure(handle: &AppHandle, message: &str) {
    let log = log_path().display().to_string();
    let js = format!(
        "document.getElementById('status').textContent = '启动失败'; \
         document.getElementById('spinner').style.display = 'none'; \
         var e = document.getElementById('error'); \
         e.textContent = {} + '\\n\\n日志文件：' + {}; \
         e.style.display = 'block';",
        serde_json::to_string(message).expect("字符串序列化失败"),
        serde_json::to_string(&log).expect("字符串序列化失败")
    );
    eval_js(handle, js);
}

// ==================== 内置设置（托盘 → 设置…） ====================

/// 设置窗口回传给前端的配置视图。
#[derive(serde::Serialize)]
struct SettingsView {
    #[serde(rename = "dshHome")]
    dsh_home: Option<String>,
    workspace: Option<String>,
}

/// 读取当前配置（供设置窗口初始化表单）。
#[tauri::command]
fn get_settings() -> SettingsView {
    let config = load_config();
    SettingsView {
        dsh_home: config.dsh_home.map(|path| path.display().to_string()),
        workspace: config.workspace.map(|path| path.display().to_string()),
    }
}

/// 保存配置并应用：校验目录 → 写入 config.json → 若服务由本程序启动则重启。
#[tauri::command]
fn save_settings(app: AppHandle, dsh_home: Option<String>, workspace: Option<String>) -> String {
    let dsh_home = dsh_home.filter(|value| !value.trim().is_empty());
    let workspace = workspace.filter(|value| !value.trim().is_empty());
    if let Some(home) = &dsh_home {
        if !PathBuf::from(home).is_dir() {
            return format!("保存失败：DSH_HOME 目录不存在或不是文件夹：{home}");
        }
    }
    if let Some(dir) = &workspace {
        if !PathBuf::from(dir).is_dir() {
            return format!("保存失败：工作目录不存在或不是文件夹：{dir}");
        }
    }
    let mut json = serde_json::Map::new();
    if let Some(home) = &dsh_home {
        json.insert("dshHome".to_string(), serde_json::Value::String(home.clone()));
    }
    if let Some(dir) = &workspace {
        json.insert("workspace".to_string(), serde_json::Value::String(dir.clone()));
    }
    let path = config_path();
    let content = serde_json::to_string_pretty(&serde_json::Value::Object(json))
        .expect("配置序列化失败");
    if let Err(error) = std::fs::write(&path, content) {
        return format!("保存失败：无法写入 {}\n{error}", path.display());
    }
    if restart_server(&app) {
        "设置已保存，本地服务已按新设置重启，主界面已刷新。".to_string()
    } else {
        "设置已保存。当前服务由外部启动，新设置将在下次由本程序启动服务时生效。".to_string()
    }
}

/// 弹出原生文件夹选择框，返回所选目录路径（取消则返回 null）。
#[tauri::command]
fn pick_folder(app: AppHandle) -> Option<String> {
    app.dialog()
        .file()
        .blocking_pick_folder()
        .and_then(|path| path.into_path().ok())
        .map(|path| path.display().to_string())
}

/// 打开设置窗口（已存在则置前）。
fn open_settings(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let _ = tauri::WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("settings.html".into()))
        .title("DeepSeek Harness 设置")
        .inner_size(540.0, 600.0)
        .resizable(false)
        .center()
        .build();
}

/// 用新配置重启由本程序启动的本地服务，并刷新主界面；返回是否执行了重启。
fn restart_server(app: &AppHandle) -> bool {
    let state = app.state::<ServerState>();
    if !*state.owned.lock().unwrap() {
        return false;
    }
    // 结束旧服务，再用新配置拉起。
    let mut guard = state.child.lock().unwrap();
    if let Some(mut child) = guard.take() {
        kill_tree(&mut child);
    }
    let Ok(child) = spawn_server() else {
        return false;
    };
    *guard = Some(child);
    // 等服务就绪后刷新主界面（新服务约需 1~2 秒启动）。
    let app = app.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline && !is_up() {
            std::thread::sleep(POLL_INTERVAL);
        }
        if is_up() {
            let handle = app.clone();
            let _ = app.run_on_main_thread(move || {
                if let Some(window) = handle.get_webview_window("main") {
                    if let Ok(url) = url::Url::parse(WEB_URL) {
                        let _ = window.navigate(url);
                    }
                }
            });
        }
    });
    true
}
