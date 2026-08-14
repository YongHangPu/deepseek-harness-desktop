# DeepSeek Harness Desktop

> 一个基于 **Tauri 2.0** 的 DeepSeek Harness 桌面启动器（社区项目，非官方出品）。

双击程序即会执行 `npx @deepseek-ai/dsh web`，等 Web UI 就绪后在本机原生窗口里打开 `http://127.0.0.1:3080`。它没有重新实现 dsh，而是直接复用同一条命令，因此**官方发新版后仍然会自动跟随 `latest`**，和手动在终端里跑 npx 的行为一致。

**支持平台**：Windows（x64 / x86 / arm64）、macOS（Intel / Apple Silicon）、Linux（amd64 / arm64）。

**特性**：

- 双击即开，自动拉起服务（服务已在运行则直接附着，不会重复启动）
- 内置设置界面（托盘 → 设置）：可视化配置 `DSH_HOME`（迁移过的 .dsh 目录）与工作目录，无需改配置文件和终端
- 关闭窗口时若会话仍在进行会弹窗确认，避免误关
- 单实例运行；关闭时自动回收由本程序启动的服务进程
- Windows 安装器（NSIS / MSI）为自定义 DeepSeek 风格界面（浅色科技风，全中文）

## 使用

- 从 [Releases](../../releases) 下载对应平台的安装包安装，或自行构建后运行。
- 首次启动时，如果 3080 端口空闲，程序会后台拉起 `npx @deepseek-ai/dsh web`（日志：Windows 在 `%LOCALAPPDATA%\dsh-desktop\dsh-web.log`，macOS/Linux 在 `~/.local/share/dsh-desktop/dsh-web.log`）；如果已有 dsh 在运行则直接附着，不会重复启动。
- 点击系统托盘（菜单栏）图标 → **设置…** 即可配置数据目录与工作目录（见下文）。

## 配置（内置设置界面）

点击**托盘图标 → 设置…**即可打开设置窗口，无需编辑任何文件或使用终端：

- `DSH_HOME（.dsh 数据目录）`：把 `.dsh` 从 C 盘迁移到其他盘后，点“浏览…”选择迁移后的 `.dsh` 目录（等价于给 dsh 设置 `DSH_HOME`），否则桌面程序读不到终端里的会话。
- `工作目录（workspace）`：dsh 按“启动时所在目录”分组会话，默认用户主目录；平时在某个固定目录里跑 npx 就选那个目录，历史会话会显示在同一个工作区。
- 保存后，若服务由本程序启动，会自动重启服务并刷新主界面（进行中的会话会中断）；若服务由外部（终端）启动，提示重启程序后生效。

设置保存在配置目录下的 `config.json`（与日志同目录，也可手动编辑）。每次启动会把实际使用的 `DSH_HOME` 和 `workspace` 写进 `dsh-web.log`，方便排查“会话看不到”的问题。

## 构建

前置条件：Rust（MSVC toolchain）+ 已安装 WebView2（Win10/11 自带）。

```sh
# 生成图标与 MSI 安装器位图（需要 Node，仅首次）
node scripts/make-icon.mjs
node scripts/make-installer-images.mjs

# 统一更新各文件版本号（package.json / tauri.conf.json / Cargo.toml 等）
npm run bump-version -- 0.1.1

# 构建 release 可执行文件
npm run build:exe

# 打包当前平台的全部安装程序
npm run build
```

产物在 `src-tauri/target/release/bundle/` 下（Windows：`nsis/` 与 `msi/`）。

## 目录结构

```
dsh-desktop/
├── .github/workflows/    # GitHub Actions 自动构建发布
├── dist/                 # 启动画面与设置页（纯静态，无构建步骤）
├── scripts/              # 图标/位图生成、版本号统一更新脚本
└── src-tauri/            # Tauri 2 后端（Rust）
    ├── src/lib.rs        # 核心：拉起 npx dsh web、探测就绪、关闭确认、托盘设置
    ├── tauri.conf.json   # 窗口/图标/打包配置（含 wix 自定义模板配置）
    ├── wix/              # 自定义 MSI 安装界面（main.wxs + 中文文案 + 附加位图）
    ├── capabilities/     # IPC 权限清单
    └── icons/            # 应用与安装器图标
```

> 自定义 MSI 界面说明：`wix/main.wxs` 只改动了 `<UI>` 部分（自定义欢迎/选目录/进度/完成四个对话框 + 页面流转），其余（组件、功能、WebView2 处理等）与 Tauri 官方默认模板保持一致；`UIRef WixUI_Common` 提供错误/取消/浏览文件夹等系统对话框。
