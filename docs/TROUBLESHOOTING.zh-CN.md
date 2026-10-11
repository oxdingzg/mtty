# 排障

[English](TROUBLESHOOTING.md)

## 排查 CPU 或内存占用偏高

mtty 默认在启动时及每 30 秒记录一次进程资源，采样在后台线程执行。
启动前设置 `MTTY_MONITOR=0` 可关闭。需要运行包含此功能的新版本；
已经运行的旧版本不会自动获得记录功能。

| 平台 | 记录目录 |
|---|---|
| macOS | `~/Library/Logs/mtty/monitor/` |
| Linux | `$XDG_STATE_HOME/mtty/monitor/`，或 `~/.local/state/mtty/monitor/` |
| Windows | `%LOCALAPPDATA%\\mtty\\logs\\monitor\\` |

每次运行生成 `process-<timestamp>-<pid>.jsonl`。记录包括版本、平台、运行时间、
RSS 字节数、累计进程 CPU 微秒数、区间 CPU 百分比（100% 表示一个逻辑核心），
以及累计渲染调用次数和渲染墙钟耗时。渲染统计覆盖主窗口和画中画，包括提前返回
的调用，不代表 GPU 执行耗时。macOS/Linux 支持线程数，Linux 支持文件描述符数；
不支持或读取失败的指标记录为 `null`，而非零。统计只覆盖 mtty 本体，
不包含 PTY 后台、shell 或 agent 子进程。RSS 与 macOS 活动监视器的内存 footprint
不是同一口径。

记录 schema 2 另有 `mainPresentedFrames` 和 `pipPresentedFrames`，表示成功取得
surface 后向主窗口、画中画提交的累计帧数。相邻差值除以实际间隔秒数，可得到
呈现提交频率。`renderCalls` 仍包括被限帧推迟或隐藏窗口的调用，不能当作 FPS；
schema 1 还会在每次主窗口渲染后计入一次画中画调用，即使画中画没有打开。
`renderWallUs` 包含 CPU 准备与呈现等待时间，不是 GPU 忙碌时间；垂直同步可能
增加等待时间，同时减少 GPU 提交次数。

比较相邻记录：CPU 时间增长但渲染计数不变，提示非渲染工作；
空闲时渲染计数持续增加，提示不必要的刷新。应观察多轮相近任务及空闲期的内存
趋势，不要仅凭一个高数值判断泄漏。

单文件超过 4 MiB 前轮转，保留一份 `.previous.jsonl` 备份。保留最近 16 个已退出
进程的日志及其备份，同时受 **资源日志目录总量 64 MiB** 的限制。启动时清理多余
历史记录，每次写入也会按需清理旧备份或已退出进程日志。活跃进程的主日志保留；
如果它们占满预算，则丢弃新采样，不继续增加磁盘占用。跨进程写入锁避免多个实例
同时通过预算检查。采样或写入失败不会终止应用。
同级 `panic.log` 限制为 **单文件 1 MiB 加一份备份，总计 2 MiB**，
启动时也会清理超限的旧版崩溃日志。日志不包含终端内容、命令参数或工作目录。Rust panic 记录位于
同级 `panic.log`，资源日志可用于查看崩溃前的采样。

## 构建失败，或首次构建要好几分钟

| 要求 | 说明 |
|---|---|
| Rust | **stable** 工具链，锁定于 [`rust-toolchain.toml`](../rust-toolchain.toml);MSRV 1.80 |
| GPU | 支持 Metal(macOS)、Vulkan(Linux)或 DX12(Windows)的驱动 |
| Linux | 常见的 `winit`/`wgpu` 系统库(X11 或 Wayland 开发包) |

首次构建会编译 `wgpu` 与 `glyphon`，可能需要几分钟;之后的构建是增量的。

## macOS 提示应用来自未识别的开发者

`scripts/package-macos.sh` 产出的是 **ad-hoc 签名**的 bundle。只有在发布流程里配置了可选的签名
密钥，发布包才带 Developer ID 签名与公证;Windows MSI 签名同理。在此之前，macOS 会在首次打开时
要求确认，标准路径是「系统设置 → 隐私与安全性 → 仍要打开」。

可以确认当前用的是哪个构建:

```sh
mtty --version     # 输出 "mtty <版本号> (native)"
```

## macOS 每次重新构建后都再次询问文件夹访问权限

终端会继承 macOS 的文件夹保护:当窗格里的程序(或 shell 本身)读取
`~/Documents`、`~/Desktop` 或 `~/Downloads` 时,macOS 会向应用请求授权,并**按应用身份**
记住这次选择。以前 `scripts/package-macos.sh` 产出的是 ad-hoc 签名,每次构建都会生成新身份,
存下来的授权无法再匹配,于是每次重建都要再问一次。

现在脚本在存在稳定身份时会用它签名(`MTTY_CODESIGN_IDENTITY`,否则取
`security find-identity -p codesigning` 里第一个 `Apple Development` 身份)。用这样构建的
bundle 重新安装,授权一次后,后续构建会一直保留。如果提示针对整个主目录,或者你完全不想被询问,
可在「系统设置 → 隐私与安全性」里给应用授予**完全磁盘访问权限**。

## 配置没有被读取

配置就是一个文件:

| 平台 | 路径 |
|---|---|
| Linux / macOS | `~/.config/mtty/config.toml`，或 `$XDG_CONFIG_HOME/mtty/config.toml` |
| Windows | `%APPDATA%\mtty\config.toml` |

两个常见的坑:

- **Git Bash 下的 Windows。** 若 Git Bash 设置的 `HOME` 下已存在 `~/.config/mtty`，会继续使用它，
  而不是 `%APPDATA%`。
- **从 miaotty 升级过来。** 首次启动时，若 `$XDG_CONFIG_HOME/mtty` 不存在，会把
  `$XDG_CONFIG_HOME/miaotty` 复制过去。旧目录会保留，所以改它不会生效 —— 请把改动挪到新路径。

如果完全不存在 mtty 配置，会自动导入 ghostty 的 `config` 与 alacritty 的 `alacritty.toml`，
所以某项设置也可能是从那里来的。见[配置](CONFIG.zh-CN.md)。

## 窗格不上报目录或历史

那是 shell shim 的职责。shim 先加载你自己的启动文件，且绝不修改它们:

| Shell | shim 的加载方式 |
|---|---|
| zsh | 一个 `ZDOTDIR`,其 `.zshenv` 会恢复真实的 `ZDOTDIR` |
| bash | `--rcfile`,先 source `~/.bashrc`;bash 4.4+ 用 `PS0`,更老的 bash(macOS 3.2)用 DEBUG trap |
| fish | 经 `XDG_DATA_DIRS` 找到的 `vendor_conf.d` 脚本 |
| PowerShell | 在 profile 之后用 `-NoExit -Command` 加载;PSReadLine 历史需 PowerShell 7 |

如果某个启动文件在 shim 运行之后重置 `ZDOTDIR`、`XDG_DATA_DIRS` 或 `PS0`，上报就会失效。这些 shim
都在真实 PTY 中做过端到端测试(Linux 上的 zsh、bash 3.2/5.x、fish 3.7、PowerShell 7.5;
Windows 上的 PowerShell 由 CI 运行)。

## 恢复会话后，之前运行的东西没回来

更新、*重启 mtty(保留运行中的程序)* 和崩溃都不会结束正在运行的程序:每个 shell 运行在 PTY 宿主中(`pty-host`,默认开启),重启后的 mtty 会重新接上。普通退出会结束它们,除非设置:

```toml
keep-sessions-on-quit = true
```

`pty-host = false` 时打开的 pane,以及 SSH、串口、Telnet、TCP 标签,会重新开始:布局和内容会恢复,进程不会。

## `mtty-cli` 连不上

| 检查项 | 说明 |
|---|---|
| socket | `$XDG_RUNTIME_DIR/mtty.sock`(回退到 `$TMPDIR`);用 `--socket PATH` 或设置 `MTTY_SOCKET` 覆盖 |
| 更早的客户端 | `miaotty.sock` 链接到 `mtty.sock`;`mtty-cli` 也会回退到更早宿主的 socket 或管道 |
| 远程 TCP | 未设 `MTTY_MTP_TOKEN` 时 `remote-listen` 拒绝启动;设了之后每个请求都必须携带令牌 |
| 能力 | `MTTY_MTP_ALLOW` 会以 `forbidden` 拒绝白名单之外的请求 |

`mtty-cli ping` 会在 `allowed` 里报告宿主实际允许的能力。见[控制面](CLI.zh-CN.md)。

## 再次启动应用没有反应

这是有意的:第二次启动会经已有的控制 socket 转发给正在运行的实例，而不是再起一个进程。要再开一个
终端，请用 `⌘⇧T` / `Ctrl+Shift+Alt+T`(快速终端)或新建标签。

## 链接，以及从 miaotty 升级

| 项目 | 行为 |
|---|---|
| URL scheme | macOS 与 Linux 注册 `mtty://`、`ssh://` 与 `x-man-page://`;Windows MSI 只注册 `mtty://` |
| `mtty://` 与 `miaotty://` | 两者都会打开 mtty |
| agent 钩子 | 已安装的钩子脚本与 miao 的集成读取 `MIAOTTY_PANE_ID` / `MIAOTTY_CLI`，这些变量仍然导出，因此它们会继续上报状态;新安装的钩子使用 `MTTY_*` 名字 |
| macOS 通知 | 更名改变了 bundle ID,macOS 会重新询问通知权限 |

完整对照见[应用身份与迁移](APP-IDENTITY.zh-CN.md)。

## 编辑器没有补全、诊断或悬停提示

编辑器窗格会为**登录 shell 的 `PATH`** 上存在的 `rust-analyzer`、
`typescript-language-server`、`pyright-langserver`、`gopls` 或 `clangd` 启动语言服务器。检查两点:

- 语言服务器未安装，或不在登录 shell 的 `PATH` 上 —— 应用不会读取某个 shell 的交互式别名。
- 文件超过 2 MB，不启动语言服务器。

显式设置 `[lsp] enabled = false` 会全部关闭。见[配置](CONFIG.zh-CN.md)。

## 这里没有我遇到的问题

其余文档在仓库里:[安装](INSTALL.zh-CN.md)、[视图规则](VIEW-RULES.zh-CN.md)，以及带注释的
[`config.example.toml`](config.example.toml)。其他问题请在
[oxdingzg/mtty](https://github.com/oxdingzg/mtty/issues) 开 issue，或写信到
<contact@mtty.dev>。
