# 配置

[English](CONFIG.md)

mtty 的配置项全部可选。只有一行的文件就是一份合法配置，文件不存在等同于空配置。

## 配置文件的位置

| 平台 | 路径 |
|---|---|
| Linux / macOS | `~/.config/mtty/config.toml`，或 `$XDG_CONFIG_HOME/mtty/config.toml` |
| Windows | `%APPDATA%\mtty\config.toml` |

保存状态(会话、队列、窗口位置)放在同一目录;Windows 上是 `%LOCALAPPDATA%\mtty`。文档其他地方提到的
其他 `~/.config/mtty/...` 文件也都在这个目录里。

v0.0.5 及之前应用名为 `miaotty`。首次启动时，若 `$XDG_CONFIG_HOME/mtty` 不存在，会把
`$XDG_CONFIG_HOME/miaotty` 复制过去，并保留旧目录，使更早的版本仍可使用。见
[应用身份与迁移](APP-IDENTITY.zh-CN.md)。

## 最小配置

```toml
font-size   = 13                 # 默认 13
font-family = "JetBrains Mono"   # 默认;回退到系统等宽字体
theme       = "nord"             # nord | dracula | gruvbox | mtty | solarized | tokyo-night

[colors]                          # 显式配色会覆盖命名主题
background = "#2e3440"
foreground = "#d8dee9"
palette    = ["#3b4252", "#bf616a", "#a3be8c", "#ebcb8b",
              "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0",
              "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b",
              "#81a1c1", "#b48ead", "#8fbcbb", "#eceff4"]
```

若不存在 mtty 配置，会自动导入 ghostty 的 `config` 与 alacritty 的 `alacritty.toml`。

## 配置项

| 键 | 默认值 | 作用 |
|---|---|---|
| `font-size` | `13` | 字号 |
| `font-family` | `JetBrains Mono` | 任意已安装字体;回退到系统等宽字体 |
| `line-height` | `1.25` | 字号的倍数 |
| `cursor-style` | `block` | `block`、`bar` 或 `underline` |
| `background-opacity` | `1.0` | `0.1`–`1.0`;小于 1 需要支持合成的窗口管理器 |
| `notifications` | `true` | agent 需要你时发系统通知 |
| `prevent-sleep` | `true` | agent 工作时保持系统不休眠 |
| `restore-scrollback` | `true` | 退出时保存终端内容，重启后显示 |
| `ssh-auto-reconnect` | `false` | 启动时自动重连恢复的 SSH 标签，无需按回车 |
| `pty-host` | `true` | 每个 shell 运行在 PTY 宿主中,更新、重启或崩溃都不会结束 pane 里正在运行的程序 |
| `keep-sessions-on-quit` | `false` | 退出时程序也继续运行,下次启动时接回(类似 tmux) |
| `detached-timeout` | `"24h"` | 程序等待 mtty 的时长:`90s`、`30m`、`24h`、`7d` 或秒数 |
| `quick-terminal-hotkey` | — | 全局快速终端热键，如 `cmd+shift+t` |
| `editor-vim` | `false` | 内置编辑器启用极简 vim 模式 |
| `editor` | — | 「在标签中编辑」执行的命令，如 `code --wait` |
| `mermaid-command` | — | 用 mermaid-cli 渲染 ` ```mermaid ` 块;不设则用内置子集 |
| `graphics` | `true` | 内联终端图像(Sixel / Kitty / iTerm2) |
| `remote-listen` | — | 用 TCP 暴露 MTP 控制面，如 `127.0.0.1:7273`(需 `MTTY_MTP_TOKEN`) |
| `language` | — | 界面语言，`en` 或 `zh`;也会读取 `$LANG` |
| `update-auto-check` | `true` | 启动时检查一次更新；设为 `false` 则在你主动查之前不发任何请求 |
| `update-pubkey` | — | minisign 公钥;启用签名校验 |
| `update-check-url` | 项目自己的清单 | 更新检查去哪里取；见下文 |
| `theme` | — | 内置命名主题，会被显式的 `[colors]` 覆盖 |

### 主题

`theme` 按名称选择内置调色板，不区分大小写。**Nord** 仍为默认值。

```toml
theme = "mtty"   # nord | dracula | gruvbox | mtty
```

预设为 Nord、Dracula、Gruvbox 与 mtty。mtty 是航海蓝(navy)工作区配色;
与前三个不同，它会连带周围的界面外壳——窗口、卡片与侧栏——一并跟随该预设，
而非保持中性的深色外壳。此外也接受命名主题 `solarized`/`solarized-dark` 与
`tokyo-night`/`tokyonight`，显式的 `[colors]` 块会覆盖所选主题。

### 标签徽章

`[badges]` 决定哪些 agent 状态在标签上显示:状态标记——空闲为空心圆圈,执行中为旋转的圆弧,等待你时为带实心圆心的圆环,完成(绿)或失败(红)为实心圆——以及 `!` 或完成标记。关闭某个状态后,处于该状态的标签显示普通终端图标,也不加标记。四项默认全开,系统通知不受影响:

```toml
[badges]
processing = true
idle = true
awaiting = true
error = true
```

### 语言服务器

编辑器窗格会为登录 shell 的 `PATH` 上已安装的 `rust-analyzer`、
`typescript-language-server`、`pyright-langserver`、`gopls` 或 `clangd` 启动语言服务器;超过
2 MB 的文件不启动。每项的命令可以是用空格切分的字符串，也可以是数组;`root-markers` 用于识别
工作区根目录。

```toml
# [lsp]
# enabled = false                  # 全部关闭
# [lsp.rust]                       # rust | typescript | python | go | c
# command = "rust-analyzer"
# root-markers = ["Cargo.toml"]    # 最近的含该文件的目录即工作区
# [lsp.python]
# command = ["pylsp"]
# [lsp.go]
# enabled = false
```

鼠标停在代码上会显示类型、文档与问题。

### ACP agent

每个 `[acp]` 条目都可以从命令面板的「ACP Agent…」启动，并打开一个会话窗口。`command` 可以是用
空格切分的字符串，也可以是数组。

```toml
# [acp]
# [[acp.agent]]
# name = "codex"
# command = "codex acp"
# [[acp.agent]]
# name = "gemini"
# command = ["gemini", "--experimental-acp"]
```


可选 `env` 会传给 Agent 进程。`auth-method` 指定 Agent 公布的认证方式 ID，ACP 窗口也提供认证选择器。
`session-id` 在 Agent 声明支持 `loadSession` 时恢复已有会话；不支持时会显示错误。
启动对话框也可以输入会话 ID。

```toml
# env = { EXAMPLE_SETTING = "value" }
# auth-method = "<agent-auth-method-id>"
# session-id = "<agent-session-id>"
```

ACP 读文件会优先读取编辑器中的未保存内容。写文件会打开修改提案：“接受并保存”实际写入文件后才向 Agent
报告成功；“拒绝”保留磁盘原文。终端命令需要权限确认，输出在 ACP 窗口显示。

### 更新检查

**启动时检查一次，之后只在你主动查时检查。** mtty 启动时会静默检查一次(设
`update-auto-check = false` 可跳过，则完全不发请求);有新版本时在状态栏提示。菜单里的
「Check for Updates」以及更新对话框失败后的重试按钮按需检查。检查就是一条普通的 `curl`:

```sh
curl -fsSL --max-time 8 <update-check-url>
```

`update-check-url` 已经指向项目自己的发布清单，所以这个键只用于改到别处 —— 镜像站或内部主机:

```toml
# update-check-url = "https://example.com/mtty/latest.json"
#   JSON 清单: {"version":"0.2.0","artifacts":{"macos-aarch64":{"url":"…","sha256":"…"}}}
#   首行为版本号的纯文本文件同样可用。
```

下载下来的产物会与清单里声明的 `sha256` 核对。把 `update-pubkey` 设为 minisign 公钥可以额外要求
签名校验:

```toml
# update-pubkey = "RW…"
```

## Shell 集成

新 pane 中的 shell 会上报工作目录(OSC 7)、每条命令输出的起止与退出码(OSC 133)以及命令历史，
无需手动配置。shim 写入仅当前用户可访问的私有目录，并先加载用户自己的启动文件:

| Shell | shim 的加载方式 |
|---|---|
| zsh | 一个 `ZDOTDIR`,其 `.zshenv` 会恢复真实的 `ZDOTDIR` |
| bash | `--rcfile`,先 source `~/.bashrc`;bash 4.4+ 用 `PS0`,更老的 bash(macOS 3.2)用 DEBUG trap |
| fish | 经 `XDG_DATA_DIRS` 找到的 `vendor_conf.d` 脚本，并恢复原值 |
| PowerShell | 在 profile 之后用 `-NoExit -Command` 加载;包装 `prompt` 与 PSReadLine(历史需 PowerShell 7) |

你自己的启动文件绝不修改。每种 shim 都在真实 PTY 中做了端到端测试(Linux 上的 zsh、bash 3.2/5.x、
fish 3.7、PowerShell 7.5;Windows 上的 PowerShell 由 CI 运行)。

## 视图规则

窗格标题、图标与徽章来自同一目录下的 `views.json`。见[视图规则](VIEW-RULES.zh-CN.md)。

## 完整参考

[`config.example.toml`](config.example.toml) 是带注释的完整参考:上面每个键及其默认值都在一个文件里。
