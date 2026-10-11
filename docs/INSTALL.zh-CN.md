# 安装 / 构建 mtty

[English](INSTALL.md)

mtty(原名 miaotty)是原生 winit/wgpu 应用，只提供一个 GUI 主程序和一个 CLI。
命名与迁移见 [APP-IDENTITY.zh-CN.md](APP-IDENTITY.zh-CN.md)。

## 安装发布版

从 [GitHub Releases](https://github.com/oxdingzg/mtty/releases/latest) 选择最新安装包。

| 平台 | 安装与首次启动 |
|---|---|
| macOS | Apple 芯片选 arm64 zip，Intel 选 x86_64 zip；解压后把 `mtty.app` 拖进 Applications，再打开 |
| Linux | Debian/Ubuntu 可安装 `.deb`；AppImage 添加执行权限后运行；tar 压缩包也提供可执行文件 |
| Windows | 运行 MSI 安装器，或解压 zip 后打开 `mtty.exe`；MSI 还会将 `mtty-cli` 加入 PATH |

当前签名状态与下载提示见发布说明和 [Windows 下载指引](https://mtty.dev/zh/docs/about/windows-downloads/)。
`.sig` 与 `minisign.pub` 用于手动验签，不是要打开的应用。

## 第一个工作区

1. 启动后使用默认本地终端，或通过 **New SSH Session…** 打开远程 shell。
2. 用 `⌘K`（其他平台 `Ctrl+Shift+K`）打开命令面板；快速打开是 `⌘⇧O` / `Ctrl+Shift+Alt+O`。
3. 在窗格里运行已安装的代理 CLI，例如 `miao`。miao 自动上报状态；其他受支持代理有状态 hook 设置入口。
4. 在终端旁打开文件，用详情面板（`⌘⇧R` / `Ctrl+Shift+Alt+R`）查看文件、Git、代理状态与后续提示队列。

后续操作见[快捷键](SHORTCUTS.zh-CN.md)、[配置](CONFIG.zh-CN.md)、[编辑器](EDITOR.zh-CN.md)与[远程连接](REMOTE.zh-CN.md)。

## 从源码运行

```sh
cargo run --release -p mtty-app
cargo build --release -p mtty-app -p mtty-cli
./target/release/mtty --version
```

需要 Rust stable 和对应平台的 wgpu/窗口系统库；首次编译 wgpu/glyphon 可能需要数分钟。

## macOS

```sh
scripts/package-macos.sh
python3 scripts/smoke-hosts.py --bundle dist/mtty.app
bash scripts/install-macos.sh
```

只生成一个 `dist/mtty.app`，包内有 `mtty`、`mtty-cli`、图标及 URL schemes;存在本地签名身份
时用它签名(`Apple Development`,或 `MTTY_CODESIGN_IDENTITY`),否则退回 ad-hoc。
安装时压缩备份身份匹配的旧应用，安装 mtty 并移除旧 native 入口，保留用户配置。
`PROFILE=debug scripts/package-macos.sh` 可生成开发包。
安装的 app 使用 macOS 系统菜单栏；裸跑二进制使用窗口内菜单。

## 发布包

[release.yml](../.github/workflows/release.yml) 要求 Apple Silicon macOS、Intel macOS、
Linux、Windows 四个 runner 成功。`v*` 标签触发发布，手动 dispatch 演练打包而不发布。

- macOS：zip 只包含 `mtty.app`。
- Linux：tar、DEB、AppImage，包含 `mtty` 和 `mtty-cli`。
- Windows：zip、MSI，包含 `mtty.exe` 和 `mtty-cli.exe`。

Apple Developer ID 签名/公证、Windows MSI 签名和 minisign 产物签名使用
[RELEASE.zh-CN.md](RELEASE.zh-CN.md) 说明的可选 secrets。
`dist-workspace.toml` 仍为 cargo-dist 脚手架，不是当前发布流水线。


## 配置与深链接

配置位于 `~/.config/mtty/config.toml` 或 `$XDG_CONFIG_HOME/mtty/config.toml`;Windows 上为 `%APPDATA%\mtty\config.toml`(保存的状态在 `%LOCALAPPDATA%\mtty`;若在 Git Bash 设置的 `HOME` 下已有 `~/.config/mtty`,则继续使用它)。
Ghostty/Alacritty 配置导入及 zsh ZDOTDIR 集成保持原有行为，旧 native/eframe 会话格式
通过兼容迁移读取。

macOS/Linux 注册 `mtty://`、`ssh://`、`x-man-page://`；Windows MSI 只注册 `mtty://`。
后续启动通过已有的控制 socket/inbox 转发到正在运行的实例，`mtty://` 身份保持不变。
