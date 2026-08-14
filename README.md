# DeepSeek Harness Desktop

> 一个基于 **Tauri 2.0** 的 DeepSeek Harness 桌面启动器（社区项目，非官方出品）。

双击程序即会执行 `npx @deepseek-ai/dsh web`，等 Web UI 就绪后在本机原生窗口里打开 `http://127.0.0.1:3080`。它并没有重新实现 dsh，而是直接复用你一直在用的 `npx @deepseek-ai/dsh web` 命令，因此**官方发新版后仍然会自动跟随 `latest`**，和你手动在终端里跑 npx 的行为一致。

**支持平台**：Windows（x64 / x86 / arm64）、macOS（Intel / Apple Silicon）、Linux（amd64 / arm64）。

**特性**：

- 双击即开，自动拉起服务（服务已在运行则直接附着，不会重复启动）
- 内置设置界面（托盘 → 设置）：可视化配置 `DSH_HOME`（迁移过的 .dsh 目录）与工作目录，无需改配置文件和终端
- 关闭窗口时若会话仍在进行会弹窗确认，避免误关
- 单实例运行；关闭时自动回收由本程序启动的服务进程
- Windows 安装器（NSIS / MSI）为自定义 DeepSeek 风格界面（浅色科技风，全中文）

## 使用

- 直接双击编译产物：`src-tauri/target/release/dsh-desktop.exe`
- 第一次启动如果端口 3080 还没被占用，程序会后台拉起 `npx @deepseek-ai/dsh web`（日志写到 `%LOCALAPPDATA%\dsh-desktop\dsh-web.log`），随后打开窗口。
- 如果 3080 已经在运行（比如你在终端里手动跑过 npx），程序会直接“附着”到现有服务，不会重复启动。
- 关闭窗口时，程序会终止**由它自己**启动的服务进程；外部启动的服务不受影响。
- 关闭窗口时若检测到仍有会话在进行，会先弹窗确认，避免误关。
- 程序是单实例的：重复双击只会把已打开的窗口调到前台。

## 配置（内置设置界面）

点击**托盘图标 → 设置…**即可打开设置窗口，无需编辑任何文件或使用终端：

- `DSH_HOME（.dsh 数据目录）`：把 `.dsh` 从 C 盘迁移到其他盘后，点“浏览…”选择迁移后的 `.dsh` 目录（等价于给 dsh 设置 `DSH_HOME`），否则桌面程序读不到终端里的会话。
- `工作目录（workspace）`：dsh 按“启动时所在目录”分组会话，默认用户主目录；平时在某个固定目录里跑 npx 就选那个目录，历史会话会显示在同一个工作区。
- 保存后，若服务由本程序启动，会自动重启服务并刷新主界面（进行中的会话会中断）；若服务由外部（终端）启动，提示重启程序后生效。

设置保存在 `%LOCALAPPDATA%\dsh-desktop\config.json`（与日志同目录，也可手动编辑）。每次启动会把实际使用的 `DSH_HOME` 和 `workspace` 写进 `dsh-web.log`，方便排查“会话看不到”的问题。

## 构建

前置条件：Rust（MSVC toolchain）+ 已安装 WebView2（Win10/11 自带）。

```sh
# 生成图标（需要 Node，仅首次）
node scripts/make-icon.mjs
# 生成 MSI 安装器位图（需要 Node，仅首次）
node scripts/make-installer-images.mjs

# 构建 release 可执行文件
cargo build --release --manifest-path src-tauri/Cargo.toml
# 或
npm run build:exe

# 打包 NSIS（exe）+ MSI 两个安装程序
npm run build
```

产物在 `src-tauri/target/release/bundle/` 下的 `nsis/` 与 `msi/` 目录。

## 自动发布（GitHub Actions）

`.github/workflows/release.yml` 会在推送 `v*` 标签时自动构建 Windows 安装包并发布到 Releases：

```sh
# 1. 先改 src-tauri/tauri.conf.json 里的 "version"
# 2. 打标签并推送
git tag v0.1.1
git push origin v0.1.1
# 3. 构建完成后到 GitHub Releases 确认草稿并发布
```

产物：NSIS（`*_x64-setup.exe`）+ MSI（`*_x64_zh-CN.msi`），自动上传到对应 Release。

## 目录结构

```
dsh-desktop/
├── dist/index.html        # 启动时的加载/启动画面
├── scripts/make-icon.mjs  # 无依赖生成应用图标
├── scripts/make-installer-images.mjs  # 无依赖生成 MSI 安装器位图
└── src-tauri/             # Tauri 2 后端（Rust）
    ├── src/lib.rs         # 核心：拉起 npx dsh web、探测就绪、跳转、关闭确认、退出清理
    ├── tauri.conf.json    # 含 wix 自定义模板/语言配置（bundle.windows.wix）
    ├── wix/main.wxs       # 自定义 MSI 安装界面（欢迎/选目录/进度/完成四个自定义对话框）
    ├── wix/zh-CN.wxl      # MSI 中文界面文案（基于 WiX 官方 zh-CN 翻译 + 自定义字符串）
    └── icons/
```

> 自定义 MSI 界面说明：`wix/main.wxs` 只改动了 `<UI>` 部分（自定义 4 个主流程对话框 + 页面流转），其余（组件、功能、WebView2 处理等）与 Tauri 官方默认模板保持一致；`UIRef WixUI_Common` 提供错误/取消/浏览文件夹等系统对话框。
