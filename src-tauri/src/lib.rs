use std::fs::OpenOptions;
use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::Local;

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
/// 等待服务变为可达的最大时长。
///
/// 放宽到 10 分钟是因为修复流程会删除 npx 安装条目、下一次 npx 需要重新
/// 完整安装 dsh（约 1~3 分钟）；超时后回收子进程并清理可疑条目，避免
/// “装到一半被强杀”留下残缺缓存。
const STARTUP_TIMEOUT: Duration = Duration::from_secs(600);
/// 等待服务就绪过程中的轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(400);

/// 由 npx 拉起的服务子进程与“本实例是否拥有它”：
/// 自己启动的退出时一并关闭；附着到外部启动的服务则不动它。
#[derive(Default)]
struct ServerState {
    child: Mutex<Option<Child>>,
    owned: Mutex<bool>,
}

enum WaitOutcome {
    Ready,
    Exited,
    Timeout,
}

/// 关闭确认是否正在处理中，防止连点关闭按钮时弹出多个确认框。
static CLOSE_CONFIRM_PENDING: AtomicBool = AtomicBool::new(false);

/// “重试”是否正在执行，防止连点按钮时拉起多条启动流程。
static RETRY_PENDING: AtomicBool = AtomicBool::new(false);

/// 单次启动流程内是否已清理过 npx 安装条目。
///
/// 清理会触发一次耗时 1~3 分钟的重新安装，一轮流程只允许一次，
/// 避免“装了又删”的循环；`startup_inner` 每轮开头重置。
static NPX_ENTRY_CLEARED: AtomicBool = AtomicBool::new(false);

/// 本进程是否由“自动重启”拉起（见 relaunch_self）：
/// 重启后的实例带此标记，失败时不再二次重启，避免循环。
static SELF_RELAUNCHED: AtomicBool = AtomicBool::new(false);

/// 自动重启标记文件：旧实例重启前写入时间戳，新实例启动时消费。
/// 超过 MARKER_MAX_AGE_SECS 的标记视为残留（新实例没起来），按不存在处理，
/// 避免上次失败的标记毒化后续启动（跳过快速换谱系检查）。
const MARKER_MAX_AGE_SECS: i64 = 60;

fn relaunch_marker_path() -> PathBuf {
    config_dir().join(".auto-relaunch")
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn consume_relaunch_marker() -> bool {
    let path = relaunch_marker_path();
    let fresh = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| text.trim().parse::<i64>().ok())
        .map(|stamp| unix_now().saturating_sub(stamp) <= MARKER_MAX_AGE_SECS)
        .unwrap_or(false);
    let _ = std::fs::remove_file(&path);
    fresh
}

/// 查询父进程名（Windows），用于识别“由安装器直接启动”的实例——这类实例的
/// 子进程实测会持续出现模块解析失败，需要启动早期就切换为干净谱系。尽力而为。
///
/// 经 powershell 查询（约 1~2 秒）；此前尝试过 Toolhelp 原生实现但实测返回
/// 不稳定（总是查询失败），故回退到已验证可靠的方案。调用方放到后台线程，
/// 不阻塞启动画面。
#[cfg(target_os = "windows")]
fn parent_process_name() -> Option<String> {
    let script = format!(
        "$p=(Get-CimInstance Win32_Process -Filter 'ProcessId={}').ParentProcessId; \
         (Get-Process -Id $p -ErrorAction SilentlyContinue).ProcessName",
        std::process::id()
    );
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // 不闪黑色控制台窗口
    }
    let output = command.output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// 模块解析持续失败时的自愈：把用户“关闭再打开就好了”的操作自动化。
///
/// 经 WMI（Win32_Process.Create）拉起新实例：新进程由系统 WMI 服务创建，
/// 与旧实例的生死完全无关（此前 explorer.exe 转交未完成时旧实例退出会把它
/// 一并带走），父进程是 WmiPrvSE——与双击启动同样干净。
/// 不用任务计划程序的原因：创建计划任务重启自己会被 Defender 行为检测判定
/// 为持久化行为（Behavior:Win32/Persistence.A!ml）并删除程序文件。
#[cfg(target_os = "windows")]
fn relaunch_self(handle: &AppHandle) {
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => return,
    };
    // 写入标记（时间戳），让新实例知道自己是被自动重启拉起的（防止循环重启）。
    let _ = std::fs::write(relaunch_marker_path(), unix_now().to_string());
    let _ = log_line("启动失败原因为模块解析异常：将自动重启程序（等效于关闭后重新打开）");
    let _ = log_line(&format!("自动重启：目标程序 {}", exe.display()));
    set_status(handle, "环境异常，正在自动重启程序…");
    let script = format!(
        "$r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create \
         -Arguments @{{CommandLine='\"{}\"'}}; \
         if ($r -and $r.ReturnValue -eq 0) {{ exit 0 }} else {{ exit 1 }}",
        exe.display()
    );
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // 不闪黑色控制台窗口
    }
    let ok = command.status().map(|status| status.success()).unwrap_or(false);
    if !ok {
        let _ = log_line("自动重启失败：WMI 拉起新实例失败，保持后台自动重试");
        return;
    }
    // WMI 创建是异步的（由系统提供方完成），立即退出旧实例：旧实例先走，
    // 新实例随后启动并成为单实例服务端，避免竞争。
    let _ = log_line("自动重启：已请求 WMI 拉起新实例，本进程即将退出");
    handle.exit(0);
}

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
            pick_folder,
            retry_startup
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
            // 消费自动重启标记。放在 setup 里消费：单实例插件在 setup 之前
            // 已裁决，落败实例退出时不会误消费标记，下次启动仍可正常自愈。
            let relaunched = consume_relaunch_marker();
            SELF_RELAUNCHED.store(relaunched, Ordering::SeqCst);
            #[cfg(target_os = "windows")]
            {
                // 父进程检测（powershell，约 1~2 秒）放后台线程，不阻塞启动画面。
                // 由安装器直接拉起的实例（完成页“启动”按钮）会立即经 WMI 重启
                // 为干净实例；检测结果到达时启动流程仍在探测阶段，不影响决策。
                if !relaunched {
                    let handle = app.handle().clone();
                    std::thread::spawn(move || {
                        if let Some(parent) = parent_process_name() {
                            let _ = log_line(&format!("启动父进程：{parent}"));
                            if parent.eq_ignore_ascii_case("msiexec") {
                                let _ = log_line("检测到由安装器直接启动：立即重启为干净实例");
                                relaunch_self(&handle);
                            }
                        }
                    });
                }
            }
            // 托盘图标：打开主界面 / 设置… / 停止服务并退出 / 退出。
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
            // 拉起 npx + 等待就绪放后台线程，启动画面才能保持响应。
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

/// 快速重试的最大次数。
///
/// 每次插件树失败都会清理模块链接目录与 npx 安装条目再重试；快速重试只
/// 覆盖秒级瞬态，持久故障由后台无限重试兜底。
const MAX_STARTUP_ATTEMPTS: u32 = 5;

/// 环境就绪探测总时长：探测通过才正式拉起，把“环境未就绪”的时段消化在
/// 启动画面里；之后若仍失败则触发“自动重启新实例”自愈。
const PROBE_TIMEOUT: Duration = Duration::from_secs(90);
/// 单次探测的最长等待（防止探测进程挂起）。
const PROBE_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_INTERVAL: Duration = Duration::from_millis(500);

fn startup(handle: AppHandle) {
    startup_inner(handle, true);
}

/// 完整启动流程；`interactive = false` 用于后台延时重试（只记日志、不弹画面）。
fn startup_inner(handle: AppHandle, interactive: bool) {
    let state = handle.state::<ServerState>();
    let config = load_config();
    // 新一轮启动流程：允许本轮再清一次 npx 安装条目。
    NPX_ENTRY_CLEARED.store(false, Ordering::SeqCst);

    for attempt in 1..=MAX_STARTUP_ATTEMPTS {
        // 如果端口还没人在监听，就自己拉起 npx 服务。
        if !is_up() {
            // 第一次拉起前先探测模块解析环境是否就绪，把等待消化在启动画面上。
            if attempt == 1 {
                set_status(&handle, "正在准备运行环境（安装后首次启动可能需要十几秒），请稍候…");
                wait_for_resolution(&config);
            }
            match spawn_server() {
                Ok(child) => {
                    let mut guard = state.child.lock().unwrap();
                    if let Some(mut old) = guard.take() {
                        kill_tree(&mut old); // 重试场景：先回收上次的子进程
                    }
                    *guard = Some(child);
                    *state.owned.lock().unwrap() = true;
                }
                Err(error) => {
                    if interactive {
                        show_failure(
                            &handle,
                            &format!("无法启动 npx @deepseek-ai/dsh web：\n{error}"),
                        );
                        // 拉不起进程也可能是瞬态：同样转入后台自动重试。
                        schedule_background_retries(handle.clone());
                    } else {
                        let _ = log_line(&format!("后台重试：无法启动 npx：{error}"));
                    }
                    return;
                }
            }
        }

        set_status(&handle, "DeepSeek Harness 正在启动，请稍候…");

        match wait_until_ready(&state) {
            WaitOutcome::Ready => {
                navigate_main(&handle);
                return;
            }
            WaitOutcome::Exited => {
                if attempt >= MAX_STARTUP_ATTEMPTS {
                    if interactive {
                        // 全部尝试都失败：跑一次 --port 0 交叉诊断，帮助区分
                        // “端口问题”与“环境/时机问题”，然后修复并转入后台重试。
                        set_status(&handle, "启动失败，正在运行额外诊断（--port 0）…");
                        let diagnostic = run_port0_diagnostic(&config);
                        repair_plugin_tree(&config);
                        let message = format!(
                            "dsh web 进程多次启动失败，Web UI 未能启动。\n\
                             程序已自动重试多次并完成额外诊断：\n{diagnostic}\n\
                             程序将在后台持续自动重试（间隔递增，最长约 30 分钟一轮），\
                             期间会自动清理损坏的安装缓存；恢复后会自动进入界面。\n\
                             详见下方日志文件。"
                        );
                        show_failure(&handle, &message);
                        schedule_background_retries(handle.clone());
                    } else {
                        set_status(&handle, "后台重试未成功，等待下一轮…");
                        let _ = log_line("后台重试未成功，等待下一轮…");
                    }
                    return;
                }
                let mut guard = state.child.lock().unwrap();
                if let Some(mut old) = guard.take() {
                    kill_tree(&mut old);
                }
                // 第一次失败时把 Node/npm 相关环境变量写进日志，便于对比排查。
                if attempt == 1 {
                    dump_env_diagnostics();
                }
                // 插件树/模块解析失败时清理两层缓存：派生的模块链接目录
                // （dsh 下次启动重建）与 npx 的 dsh 安装条目（下次重新完整
                // 安装）。npx 只按“目录存在”判定缓存可用，被中断的安装留下
                // 的残缺条目会被静默复用。
                if plugin_tree_broken() {
                    // 交互式流程里连续失败时，经 WMI 重启一个干净谱系的新实例
                    // （用户实测“关闭再打开”即恢复）；标记保证只做一次，
                    // 后台重试轮次继续走“修复 + 重试”路径。
                    #[cfg(target_os = "windows")]
                    if interactive && attempt >= 2 && !SELF_RELAUNCHED.load(Ordering::SeqCst) {
                        relaunch_self(&handle);
                        return;
                    }
                    repair_plugin_tree(&config);
                }
                set_status(
                    &handle,
                    &format!("启动未成功，正在自动重试（{attempt}/{MAX_STARTUP_ATTEMPTS}）…"),
                );
                // 指数退避 2s/4s/8s/16s，给瞬态条件留出恢复时间。
                std::thread::sleep(Duration::from_secs(1u64 << attempt.min(4)));
            }
            WaitOutcome::Timeout => {
                // 迟迟没有监听端口：可能在安装（npx 重新安装约 1~3 分钟）也可能
                // 卡死。回收子进程并清理可疑条目——被强杀的安装会留下残缺缓存。
                let mut guard = state.child.lock().unwrap();
                if let Some(mut child) = guard.take() {
                    kill_tree(&mut child);
                }
                repair_plugin_tree(&config);
                if interactive {
                    show_failure(
                        &handle,
                        "等待 Web UI 启动超时（10 分钟）。\n程序将继续在后台自动重试。\n详见下方日志文件。",
                    );
                    schedule_background_retries(handle.clone());
                } else {
                    let _ = log_line("后台重试：等待 Web UI 启动超时，已回收进程并清理安装缓存");
                }
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

/// 应用配置目录（Windows：%LOCALAPPDATA%\dsh-desktop；macOS/Linux：XDG 或 ~/.local/share）。
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

fn log_path() -> PathBuf {
    config_dir().join("dsh-web.log")
}

fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

/// 本地配置（%LOCALAPPDATA%\dsh-desktop\config.json，手动编辑即可）。
#[derive(Default)]
struct AppConfig {
    /// 覆盖 dsh 数据目录（等价于环境变量 DSH_HOME），用于把 .dsh 迁移到其他盘。
    dsh_home: Option<PathBuf>,
    workspace: Option<PathBuf>,
}

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

/// 解析 dsh 的工作目录（workspace）。dsh 按启动时的 cwd 给会话分组，双击启动
/// 默认以安装目录为 cwd，会导致终端里创建的会话在界面上看不到。
/// 优先级：环境变量 DSH_DESKTOP_WORKSPACE > 配置 workspace > 用户主目录。
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

/// 日志大小上限。单次失败约写 250KB 堆栈，限制在 2MB（外加一份 .old 备份），
/// 保证日志不无限膨胀，同时保留最近一轮完整失败记录。
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// 日志超过上限时轮转：当前日志改名 dsh-web.log.old（覆盖旧备份）。
fn rotate_log_if_large() {
    let path = log_path();
    let Ok(meta) = std::fs::metadata(&path) else {
        return;
    };
    if meta.len() <= MAX_LOG_BYTES {
        return;
    }
    let old = path.with_extension("log.old");
    let _ = std::fs::remove_file(&old);
    if std::fs::rename(&path, &old).is_ok() {
        // 直接写新文件记一行轮转说明，避免经 log_line 再触发轮转检查。
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(
                file,
                "dsh-desktop: [{}] 日志超过 2MB，旧日志已轮转为 dsh-web.log.old",
                now_stamp()
            );
        }
    }
}

/// 打开日志、写入本次启动的诊断行，并返回重定向用的 stdout/stderr。
fn log_streams(config: &AppConfig, workspace: &Option<PathBuf>) -> std::io::Result<(Stdio, Stdio)> {
    rotate_log_if_large();
    let log = log_path();
    let file = OpenOptions::new().create(true).append(true).open(&log)?;
    // 记录本次使用的数据目录与工作目录，方便排查“会话看不到”的问题。
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
        "dsh-desktop: [{}] DSH_HOME={home_note} workspace={workspace_note}",
        now_stamp()
    );
    Ok((Stdio::from(file.try_clone()?), Stdio::from(file)))
}

fn now_stamp() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

/// 向服务日志追加一行诊断（带时间戳，失败不影响主流程）。
fn log_line(text: &str) -> std::io::Result<()> {
    rotate_log_if_large();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())?;
    writeln!(file, "dsh-desktop: [{}] {text}", now_stamp())
}

fn read_log_tail(max_bytes: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(log_path()) else {
        return String::new();
    };
    let Ok(size) = file.metadata().map(|meta| meta.len()) else {
        return String::new();
    };
    if file
        .seek(SeekFrom::Start(size.saturating_sub(max_bytes)))
        .is_err()
    {
        return String::new();
    }
    let mut tail = Vec::new();
    if file.take(max_bytes).read_to_end(&mut tail).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&tail).into_owned()
}

/// 清理子进程环境：移除会干扰 Node/npm 解析的变量（NODE_OPTIONS、NODE_PATH，
/// 以及除代理/registry 之外的 npm_config_*），其余保持原样。
fn sanitize_child_env(command: &mut Command) {
    command.env_remove("NODE_OPTIONS");
    command.env_remove("NODE_PATH");
    let suspects: Vec<std::ffi::OsString> = command
        .get_envs()
        .filter_map(|(key, _value)| key.to_str().map(str::to_owned))
        .filter(|name| {
            let lower = name.to_ascii_lowercase();
            lower.starts_with("npm_config_")
                && !lower.contains("proxy")
                && !lower.contains("registry")
        })
        .map(std::ffi::OsString::from)
        .collect();
    for key in suspects {
        command.env_remove(key);
    }
}

/// 失败时把 Node/npm 相关环境变量写进日志，便于对比排查；
/// npm_config_* 只记名字（值可能是 token）。
fn dump_env_diagnostics() {
    let mut lines = vec!["dsh-desktop: 环境诊断：".to_string()];
    for (key, value) in std::env::vars_os() {
        let Some(name) = key.to_str() else { continue };
        let lower = name.to_ascii_lowercase();
        let interesting = lower.starts_with("npm_config_")
            || lower.starts_with("node_")
            || lower.starts_with("electron")
            || lower.starts_with("dsh_")
            || lower == "path"
            || lower == "pnpm_home"
            || lower == "nvm_symlink";
        if !interesting {
            continue;
        }
        if lower.starts_with("npm_config_") {
            lines.push(format!("  {name}（值省略）"));
        } else {
            lines.push(format!("  {name}={}", value.to_string_lossy()));
        }
    }
    if lines.len() > 1 {
        let _ = log_line(&lines.join("\n"));
    }
}

/// 解析 dsh 数据目录（与 dsh 自身顺序一致）：DSH_HOME > 配置 dshHome > ~/.dsh。
fn dsh_home_dir(config: &AppConfig) -> PathBuf {
    if let Some(home) = std::env::var_os("DSH_HOME") {
        return PathBuf::from(home);
    }
    if let Some(home) = &config.dsh_home {
        return home.clone();
    }
    let base = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    match base {
        Some(base) => PathBuf::from(base).join(".dsh"),
        None => PathBuf::from(".dsh"),
    }
}

/// 删除 profile 模块链接目录（纯派生数据，dsh 下次启动会用
/// healProfilesModuleFallback 基于当前安装重建全部链接）。
fn clear_module_fallback(config: &AppConfig) {
    let fallback = dsh_home_dir(config).join("profiles").join("node_modules");
    match std::fs::remove_dir_all(&fallback) {
        Ok(()) => {
            let _ = log_line(&format!(
                "dsh-desktop: 已删除模块链接目录 {}（下一次启动 dsh 会重建）",
                fallback.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            let _ = log_line(&format!(
                "dsh-desktop: 删除模块链接目录失败 {}：{error}",
                fallback.display()
            ));
        }
    }
}

/// 从日志尾部解析 npx 安装条目目录（`<npm 缓存>/_npx/<hash>`）。
/// 失败堆栈带 `file:///…/_npx/<hash>/node_modules/…`，无需猜 npm 缓存位置；
/// 只返回确认装着 @deepseek-ai/dsh 的候选。
fn npx_entry_dir_from_log_tail() -> Option<PathBuf> {
    let text = read_log_tail(64 * 1024);
    let needle = "_npx";
    let mut offset = 0;
    while let Some(hit) = text[offset..].find(needle) {
        let index = offset + hit;
        let rest = &text[index + needle.len()..];
        // 跳过条目名与哈希之间的路径分隔符，取十六进制哈希（通常 16 位）。
        let after_sep = rest.strip_prefix(['/', '\\']).unwrap_or(rest);
        let hash_len = after_sep
            .chars()
            .take_while(|ch| ch.is_ascii_hexdigit())
            .count();
        if hash_len >= 8 {
            // 路径起点：本行开头到 `_npx` 之间，去掉可选的 `file:///` 前缀。
            let line_start = text[..index]
                .rfind(['\n', '\r'])
                .map(|pos| pos + 1)
                .unwrap_or(0);
            let line = &text[line_start..index];
            let dir = match line.rfind("file:///") {
                Some(pos) => line[pos + "file:///".len()..]
                    .trim_end_matches(['/', '\\'])
                    .to_string(),
                None => line.trim_end_matches(['/', '\\']).to_string(),
            };
            if dir.len() >= 3 {
                let entry = PathBuf::from(&dir)
                    .join("_npx")
                    .join(&after_sep[..hash_len]);
                let dsh_pkg = entry.join("node_modules").join("@deepseek-ai").join("dsh");
                if dsh_pkg.is_dir() {
                    return Some(entry);
                }
            }
        }
        offset = index + needle.len();
    }
    None
}

/// 删除 npx 的 dsh 安装条目。npx 只按“条目目录存在”判定缓存可用，被中断的
/// 安装留下的残缺条目会被静默复用、导致插件全部无法解析；删除后下次重新安装。
fn clear_npx_dsh_entry() {
    // 每轮启动流程只清一次，防止反复删除刚装好的条目。
    if NPX_ENTRY_CLEARED.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(entry) = npx_entry_dir_from_log_tail() else {
        let _ = log_line("dsh-desktop: 未能在日志中找到 npx 安装条目路径，跳过条目清理");
        return;
    };
    // 抽查安装闭包里的关键包：删除后只能靠联网重装，仅在确认残缺时才删。
    let dsh_scope = entry.join("node_modules").join("@deepseek-ai");
    let complete = ["dsh-llm", "cordis-plugin-timer", "dsh-web-app"]
        .iter()
        .all(|name| dsh_scope.join(name).join("package.json").is_file());
    if complete {
        let _ = log_line(&format!(
            "dsh-desktop: npx 条目 {} 完整，保留（本次失败按瞬态问题处理），跳过条目清理",
            entry.display()
        ));
        return;
    }
    match std::fs::remove_dir_all(&entry) {
        Ok(()) => {
            let _ = log_line(&format!(
                "dsh-desktop: 已删除 npx 安装条目 {}（下一次 npx 会重新完整安装）",
                entry.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            let _ = log_line(&format!(
                "dsh-desktop: 删除 npx 安装条目失败 {}：{error}",
                entry.display()
            ));
        }
    }
}

/// 插件树加载失败后的统一修复：清理模块链接目录与 npx 安装条目。
fn repair_plugin_tree(config: &AppConfig) {
    clear_module_fallback(config);
    clear_npx_dsh_entry();
}

/// 日志尾部是否包含插件树/模块解析失败的特征（只读末尾 64 KiB）。
fn plugin_tree_broken() -> bool {
    let text = read_log_tail(64 * 1024);
    text.contains("Cannot find package") || text.contains("plugin tree failed to load")
}

/// ==================== Node 运行环境（系统 → 内置便携包 → 在线下载） ====================
/// 便携 Node 的固定版本；升级时改这里并同步 CI 的下载步骤。
const NODE_VERSION: &str = "24.12.0";

/// Node 来源：System=系统 PATH 里的 node；Local=应用数据目录里的便携 Node；
/// Missing=系统没有且便携 Node 下载失败（启动时给出明确错误）。
#[derive(Clone)]
enum NodeSource {
    System,
    Local(PathBuf),
    Missing,
}

fn node_arch_suffix() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "x86",
        _ => "x64",
    }
}

/// 官方发行包里的平台名（win / darwin / linux）。
fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win",
        "macos" => "darwin",
        _ => "linux",
    }
}

/// 各平台的归档扩展名与 tar 解压参数。
fn node_archive_parts() -> (&'static str, &'static [&'static str]) {
    match std::env::consts::OS {
        "windows" => ("zip", &["-xf"]),
        "macos" => ("tar.gz", &["-xzf"]),
        _ => ("tar.xz", &["-xJf"]),
    }
}

/// Node 可执行文件名（Windows 为 node.exe，其余为 node）。
fn node_bin_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "node.exe"
    } else {
        "node"
    }
}

fn local_node_dir() -> PathBuf {
    config_dir().join("node").join(format!(
        "node-v{NODE_VERSION}-{}-{}",
        node_platform(),
        node_arch_suffix()
    ))
}

/// 解析（并按进程缓存）Node 来源：系统 node 可用则优先用系统；
/// 否则确保便携 Node 就绪（缺失时从国内镜像下载解压到应用数据目录）。
fn node_source() -> NodeSource {
    use std::sync::OnceLock;
    static CACHED: OnceLock<NodeSource> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            let mut probe = Command::new("node");
            probe
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(target_os = "windows")]
            {
                use std::os::windows::process::CommandExt;
                probe.creation_flags(0x0800_0000); // 不闪黑色控制台窗口
            }
            let system_ok = probe
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if system_ok {
                return NodeSource::System;
            }
            match ensure_local_node() {
                Some(dir) => NodeSource::Local(dir),
                None => NodeSource::Missing,
            }
        })
        .clone()
}

/// 安装包内置的便携 Node 归档可能所在的位置（随 Tauri resources 打包）：
/// Windows/Linux 在可执行文件同级的 runtime 目录，macOS 在 .app 的
/// Contents/Resources/runtime。
fn bundled_node_archive_candidates() -> Vec<PathBuf> {
    let (ext, _) = node_archive_parts();
    let name = format!(
        "node-v{NODE_VERSION}-{}-{}.{ext}",
        node_platform(),
        node_arch_suffix()
    );
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("runtime").join(&name));
            // macOS：Contents/MacOS/.. → Contents/Resources/runtime
            candidates.push(dir.join("..").join("Resources").join("runtime").join(&name));
        }
    }
    candidates
}

/// 解压便携 Node 归档到应用数据目录，成功返回解压后的 Node 目录。
fn extract_node_archive(archive: &Path) -> Option<PathBuf> {
    let dir = local_node_dir();
    let target = config_dir().join("node");
    let _ = std::fs::create_dir_all(&target);
    let (_ext, tar_flags) = node_archive_parts();
    let mut command = Command::new("tar");
    command.args(tar_flags).arg(archive).arg("-C").arg(&target);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let ok = command
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if ok && dir.join(node_bin_name()).is_file() {
        let _ = log_line(&format!("便携 Node 已就绪：{}", dir.display()));
        Some(dir)
    } else {
        let _ = log_line("便携 Node 解压失败");
        None
    }
}

/// 确保便携 Node 就绪，返回其目录；失败返回 None。
/// 顺序：已有解压结果 → 安装包内置归档（离线可用）→ 在线下载（国内镜像优先）。
fn ensure_local_node() -> Option<PathBuf> {
    let dir = local_node_dir();
    if dir.join(node_bin_name()).is_file() {
        return Some(dir);
    }
    for archive in bundled_node_archive_candidates() {
        if archive.is_file() {
            let _ = log_line(&format!("发现内置便携 Node 归档：{}", archive.display()));
            if let Some(dir) = extract_node_archive(&archive) {
                return Some(dir);
            }
            // 内置归档损坏等解压失败场景：回退在线下载。
            let _ = log_line("内置归档解压失败，回退在线下载…");
            break;
        }
    }
    let _ = log_line("未检测到系统 Node.js，准备下载便携 Node（国内镜像）…");
    let (ext, _tar_flags) = node_archive_parts();
    let name = format!(
        "node-v{NODE_VERSION}-{}-{}.{ext}",
        node_platform(),
        node_arch_suffix()
    );
    let urls = [
        format!("https://registry.npmmirror.com/-/binary/node/v{NODE_VERSION}/{name}"),
        format!("https://nodejs.org/dist/v{NODE_VERSION}/{name}"),
    ];
    let archive = config_dir().join(&name);
    let _ = std::fs::remove_file(&archive);
    let mut downloaded = false;
    for url in &urls {
        let mut command = Command::new("curl");
        command.args(["-L", "--fail", "--silent", "--show-error", "-o"]);
        command.arg(&archive).arg(url);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let ok = command
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if ok {
            downloaded = true;
            break;
        }
        // 非 Windows 且 curl 不可用时，用 wget 再试一次当前源。
        #[cfg(not(target_os = "windows"))]
        if !downloaded {
            let mut fallback = Command::new("wget");
            fallback.args(["-q", "-O"]).arg(&archive).arg(url);
            if fallback
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
            {
                downloaded = true;
                break;
            }
        }
        let _ = log_line(&format!("便携 Node 下载失败（{url}），尝试下一个源…"));
    }
    if !downloaded {
        let _ = log_line("便携 Node 下载失败：所有源均不可用");
        return None;
    }
    let result = extract_node_archive(&archive);
    let _ = std::fs::remove_file(&archive);
    result
}

/// 在 profile 目录做一次模块解析探测：能 import dsh-llm 说明链接目录可被
/// Node 正常解析。Ok(true)=成功；Ok(false)=失败；Err=无法执行探测（按就绪处理）。
fn probe_module_resolution(config: &AppConfig) -> std::io::Result<bool> {
    let profile = dsh_home_dir(config).join("profiles").join("web");
    if !profile.is_dir() {
        return Ok(true); // 全新环境：交给 dsh 启动时自建
    }
    let mut command = match node_source() {
        NodeSource::System => Command::new("node"),
        NodeSource::Local(dir) => Command::new(dir.join(node_bin_name())),
        NodeSource::Missing => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "未检测到 Node.js 环境，且便携 Node 自动下载失败",
            ))
        }
    };
    command
        .arg("--input-type=module")
        .arg("-e")
        .arg("import('@deepseek-ai/dsh-llm').then(()=>process.exit(0),()=>process.exit(1))")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir(&profile);
    sanitize_child_env(&mut command);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：探测进程不闪黑色控制台窗口。
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn()?;
    let deadline = Instant::now() + PROBE_ATTEMPT_TIMEOUT;
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(status.success()),
            None => {
                if Instant::now() >= deadline {
                    kill_tree(&mut child);
                    return Ok(false);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 正式拉起前等待模块解析环境就绪（最多 PROBE_TIMEOUT）。只在链接目录已
/// 存在时探测——目录不存在说明尚未初始化，dsh 启动时会自行重建。
fn wait_for_resolution(config: &AppConfig) {
    let fallback = dsh_home_dir(config).join("profiles").join("node_modules");
    if !fallback.is_dir() {
        return;
    }
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match probe_module_resolution(config) {
            Ok(true) => return,
            Ok(false) => {}
            Err(_) => return, // node 不可用等：直接进入正式启动流程
        }
        if Instant::now() >= deadline {
            let _ = log_line("dsh-desktop: 环境就绪探测超时，直接进入启动流程");
            return;
        }
        std::thread::sleep(PROBE_INTERVAL);
    }
}

/// 构造并拉起 `npx @deepseek-ai/dsh web [extra_args]`，输出重定向到日志。
fn dsh_child(config: &AppConfig, extra_args: &[&str]) -> std::io::Result<Child> {
    let workspace = workspace_dir(config);
    let (stdout, stderr) = log_streams(config, &workspace)?;

    let mut command = match node_source() {
        NodeSource::System => {
            #[cfg(target_os = "windows")]
            {
                let mut command = Command::new("cmd");
                // -y：全新机器无缓存时跳过 npx 的安装确认（标准输入为空会卡死）。
                command.args(["/C", "npx", "-y", "@deepseek-ai/dsh", "web"]);
                command
            }
            #[cfg(not(target_os = "windows"))]
            {
                let mut command = Command::new("npx");
                command.args(["-y", "@deepseek-ai/dsh", "web"]);
                command
            }
        }
        NodeSource::Local(dir) => {
            // 便携 Node：直接以 node 运行 npm 的 npx-cli.js（Windows 上还能
            // 避免 cmd 引号问题），并把 npm 源指向国内镜像，保证国内网络下
            // 安装 dsh 顺畅。-y 跳过安装确认，避免空标准输入下卡死。
            let mut command = Command::new(dir.join(node_bin_name()));
            command.arg(dir.join("node_modules").join("npm").join("bin").join("npx-cli.js"));
            command.args(["-y", "@deepseek-ai/dsh", "web"]);
            command.env("npm_config_registry", "https://registry.npmmirror.com");
            command
        }
        NodeSource::Missing => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "未检测到 Node.js 环境，且便携 Node 自动下载失败；请手动安装 Node.js 后重试",
            ))
        }
    };
    command.args(extra_args);
    command.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
    sanitize_child_env(&mut command);
    if let Some(home) = &config.dsh_home {
        command.env("DSH_HOME", home);
    }
    // 固定 dsh 的工作目录：否则双击启动时 cwd 是安装目录，会话会被归到
    // 另一个 workspace，侧边栏里看不到之前的会话。
    if let Some(dir) = workspace {
        command.current_dir(dir);
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // 不闪黑色控制台窗口
    }
    command.spawn()
}

fn spawn_server() -> std::io::Result<Child> {
    dsh_child(&load_config(), &[])
}

/// 失败后的交叉诊断：以 `--port 0`（系统分配空闲端口）再启动一次 dsh。
/// 同样失败 → 与端口无关的环境问题；成功 → 3080 端口路径存在特殊问题。
fn run_port0_diagnostic(config: &AppConfig) -> String {
    let _ = log_line("额外诊断：以 --port 0 启动一次 dsh…");
    let mut child = match dsh_child(config, &["--port", "0"]) {
        Ok(child) => child,
        Err(error) => return format!("无法启动诊断进程：{error}"),
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                let tail = read_log_tail(8 * 1024);
                let summary = if tail.contains("EADDRINUSE") {
                    "失败：EADDRINUSE（端口冲突）".to_string()
                } else if tail.contains("Cannot find package") {
                    "失败：Cannot find package（与正式启动相同的模块解析错误）".to_string()
                } else {
                    let line = tail
                        .lines()
                        .rev()
                        .find(|line| line.contains("Error"))
                        .map(|line| line.trim().to_string())
                        .unwrap_or_else(|| "未知错误".to_string());
                    format!("失败：{line}")
                };
                let _ = log_line(&format!("额外诊断(--port 0) 结果：{summary}"));
                return summary;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_tree(&mut child);
                    let banner = read_log_tail(8 * 1024)
                        .lines()
                        .rev()
                        .find(|line| line.starts_with("dsh web: http"))
                        .map(|line| line.trim().to_string());
                    let summary = match banner {
                        Some(line) => format!("成功（{line}），诊断进程已关闭"),
                        None => "成功（服务持续运行 60 秒），诊断进程已关闭".to_string(),
                    };
                    let _ = log_line(&format!("额外诊断(--port 0) 结果：{summary}"));
                    return summary;
                }
            }
            Err(error) => {
                kill_tree(&mut child);
                let _ = log_line(&format!("额外诊断(--port 0) 状态检查失败：{error}"));
                return format!("诊断进程状态检查失败：{error}");
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// 轮询等待服务就绪；子进程提前退出或超时给出对应结果。
fn wait_until_ready(state: &ServerState) -> WaitOutcome {
    let start = Instant::now();
    loop {
        if is_up() {
            return WaitOutcome::Ready;
        }
        // 快速失败：子进程在服务起来之前就退出，立刻返回失败。
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

/// 应用退出时清理：若服务是本应用启动的，连同其进程树一起结束。
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

/// 托盘“停止服务并退出”：结束后台 dsh 服务（连同其子进程，如 MCP 服务器）
/// 再退出。附着模式下通过端口找到监听进程，用 taskkill /T 结束整棵进程树。
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

fn focus_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// 关闭窗口前检测是否有仍在进行的会话（页面里带 data-state="ongoing"/
/// "warning" 的会话行），有则弹确认框，否则直接关闭。
fn confirm_close_if_busy(window: WebviewWindow) {
    if CLOSE_CONFIRM_PENDING.swap(true, Ordering::SeqCst) {
        return; // 防止连点关闭按钮弹出多个确认框
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

fn eval_js(handle: &AppHandle, js: String) {
    let h = handle.clone();
    let _ = handle.run_on_main_thread(move || {
        if let Some(window) = h.get_webview_window("main") {
            let _ = window.eval(&js);
        }
    });
}

fn set_status(handle: &AppHandle, text: &str) {
    let js = format!(
        "document.getElementById('status').textContent = {};",
        serde_json::to_string(text).expect("字符串序列化失败")
    );
    eval_js(handle, js);
}

fn show_failure(handle: &AppHandle, message: &str) {
    let log = log_path().display().to_string();
    let js = format!(
        "document.getElementById('status').textContent = '启动失败'; \
         document.getElementById('spinner').style.display = 'none'; \
         var e = document.getElementById('error'); \
         e.textContent = {} + '\\n\\n日志文件：' + {}; \
         e.style.display = 'block'; \
         document.getElementById('retry').style.display = 'inline-block';",
        serde_json::to_string(message).expect("字符串序列化失败"),
        serde_json::to_string(&log).expect("字符串序列化失败")
    );
    eval_js(handle, js);
}

/// 失败画面上的“重试”按钮：重置启动画面后重新走一遍完整启动流程。
#[tauri::command]
fn retry_startup(app: AppHandle) {
    if RETRY_PENDING.swap(true, Ordering::SeqCst) {
        return;
    }
    let js = "document.getElementById('status').textContent = '正在重新启动 Web UI…'; \
              document.getElementById('spinner').style.display = ''; \
              document.getElementById('error').style.display = 'none'; \
              document.getElementById('retry').style.display = 'none';";
    eval_js(&app, js.to_string());
    let handle = app.clone();
    std::thread::spawn(move || {
        startup(handle);
        RETRY_PENDING.store(false, Ordering::SeqCst);
    });
}

fn navigate_main(app: &AppHandle) {
    let h = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(window) = h.get_webview_window("main") {
            if let Ok(url) = url::Url::parse(WEB_URL) {
                let _ = window.navigate(url);
            }
        }
    });
}

/// 后台延时重试是否已在运行（避免重复调度多条后台重试线程）。
static BACKGROUND_RETRY_RUNNING: AtomicBool = AtomicBool::new(false);

/// 快速重试全部失败后，先密后疏地持续后台重试，直到成功。
///
/// 失败可能持续数十分钟（npx 条目残缺、安全软件锁文件等）；驻留期间按递增
/// 间隔（最长 30 分钟一轮）一直重试，每轮先修复缓存，恢复后自动进入界面。
fn schedule_background_retries(app: AppHandle) {
    if BACKGROUND_RETRY_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let delays: [u64; 7] = [60, 120, 240, 300, 600, 900, 1800];
        let mut index = 0usize;
        loop {
            let delay = delays[index.min(delays.len() - 1)];
            std::thread::sleep(Duration::from_secs(delay));
            // 可能有外部服务恢复监听（例如其它终端拉起的 dsh），直接附着进入界面。
            if is_up() {
                let _ = log_line("后台重试：检测到服务已就绪，进入界面。");
                navigate_main(&app);
                BACKGROUND_RETRY_RUNNING.store(false, Ordering::SeqCst);
                return;
            }
            set_status(&app, &format!("正在后台自动重试（第 {} 轮）…", index + 1));
            let _ = log_line(&format!("后台自动重试（第 {} 轮）…", index + 1));
            startup_inner(app.clone(), false);
            if is_up() {
                navigate_main(&app);
                BACKGROUND_RETRY_RUNNING.store(false, Ordering::SeqCst);
                return;
            }
            index += 1;
        }
    });
}

// ==================== 内置设置（托盘 → 设置…） ====================

#[derive(serde::Serialize)]
struct SettingsView {
    #[serde(rename = "dshHome")]
    dsh_home: Option<String>,
    workspace: Option<String>,
}

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
    let mut guard = state.child.lock().unwrap();
    if let Some(mut child) = guard.take() {
        kill_tree(&mut child);
    }
    let Ok(child) = spawn_server() else {
        return false;
    };
    *guard = Some(child);
    // 等服务就绪后刷新主界面。
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 便携包目录名、归档名、二进制名与官方发行版命名一致。
    #[test]
    fn portable_node_naming() {
        let (ext, tar_flags) = node_archive_parts();
        let expected_platform = if cfg!(target_os = "windows") {
            "win"
        } else if cfg!(target_os = "macos") {
            "darwin"
        } else {
            "linux"
        };
        let expected_ext = match expected_platform {
            "win" => "zip",
            "darwin" => "tar.gz",
            _ => "tar.xz",
        };
        assert_eq!(node_platform(), expected_platform);
        assert_eq!(ext, expected_ext);
        assert!(!tar_flags.is_empty());
        assert_eq!(node_bin_name(), if cfg!(target_os = "windows") { "node.exe" } else { "node" });
        let dir = local_node_dir();
        let dir_name = dir.file_name().unwrap().to_string_lossy();
        assert_eq!(
            dir_name,
            format!(
                "node-v{NODE_VERSION}-{expected_platform}-{}",
                node_arch_suffix()
            )
        );
    }

    /// 下载 URL 必须同时指向国内镜像与官方源，且文件名一致。
    #[test]
    fn portable_node_download_urls() {
        let (ext, _) = node_archive_parts();
        let name = format!(
            "node-v{NODE_VERSION}-{}-{}.{ext}",
            node_platform(),
            node_arch_suffix()
        );
        for url in [
            format!("https://registry.npmmirror.com/-/binary/node/v{NODE_VERSION}/{name}"),
            format!("https://nodejs.org/dist/v{NODE_VERSION}/{name}"),
        ] {
            assert!(url.ends_with(&name), "URL 应以归档名结尾：{url}");
            assert!(url.contains(&format!("node-v{NODE_VERSION}")), "URL 版本号错误：{url}");
        }
    }

    /// 内置归档候选路径：文件名与官方命名一致，且都位于 runtime 目录内。
    #[test]
    fn bundled_node_archive_candidates_match_name() {
        let (ext, _) = node_archive_parts();
        let expected = format!(
            "node-v{NODE_VERSION}-{}-{}.{ext}",
            node_platform(),
            node_arch_suffix()
        );
        let candidates = bundled_node_archive_candidates();
        assert!(!candidates.is_empty(), "至少应有一个候选路径");
        for candidate in &candidates {
            assert_eq!(
                candidate.file_name().unwrap().to_string_lossy(),
                expected,
                "内置归档文件名与官方命名不一致：{}",
                candidate.display()
            );
            let path_text = candidate.to_string_lossy();
            assert!(
                path_text.contains("runtime"),
                "内置归档应位于 runtime 目录内：{}",
                candidate.display()
            );
        }
    }
}
