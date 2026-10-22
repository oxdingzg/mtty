# mtty

**mtty —— 用 Rust 编写的 AI 原生终端与编辑器,本地与远程同样顺手。**

[![CI](https://github.com/oxdingzg/mtty/actions/workflows/ci.yml/badge.svg)](https://github.com/oxdingzg/mtty/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](Cargo.toml)
[![Platforms](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey.svg)](#环境要求)

**官网:** [mtty.dev/mtty](https://mtty.dev/zh/mtty) · **文档:** [mtty.dev/docs/mtty](https://mtty.dev/zh/docs/mtty) · [版本发布](https://github.com/oxdingzg/mtty/releases) · [相关项目](#相关项目)

[English](README.md) · **简体中文**

---

## 概述

**mtty** 把终端、文件、远程主机与 AI 编程 agent 放进同一个快速的原生窗口。它有三根支柱:

| 方向 | 当前可用 |
|---|---|
| **终端与远程** | GPU 渲染的标签与分屏、会话恢复、主机库、SSH 密钥、SFTP/FTP、端口转发、跳板机、片段与广播输入；串口、Telnet、原始 TCP 与加密 `.ppk` 密钥导入 |
| **编辑器** | 原生编辑器窗格、tree-sitter 高亮、大文件查看模式、多光标、正则查找替换、Markdown 与 Mermaid 预览、LSP、可选 vim、折叠与大纲 |
| **代理工作台** | 代理状态徽章与通知、可操作的待处理事项、提示队列、逐任务 git worktree、可撤销的行内编辑提案、ACP 代理、上下文发送与 MTP 控制面 |

安装 miao 后，右上角的猫形按钮可在当前终端启动 `miao`，保留当前目录与环境；该终端运行 miao 时隐藏，回到 shell 后恢复。

把它们连在一起的是:Rust 与 GPU 渲染,并由性能门把关;终端、编辑器、远程主机与 agent 位于同一套标签与分屏;不强制账号,
不强制上云——mtty 托管你选择的 agent,自己从不调用模型。

本仓库包含:

- **终端引擎** —— `mtty-core` 负责从 PTY 到屏幕的热路径,不依赖任何窗口或 GPU 代码;
- **编辑器内核** —— `mtty-editor`,同样不含任何界面代码;
- **`mtty`** —— 构建在它们之上的应用,具备标签、分屏、侧边面板、设置窗口、shell 集成以及可脚本化的控制面。

**应用统一为一个 `mtty`**，使用 `mtty-widget` 中的原生 `winit` + `wgpu`
窗口与渲染循环，在同一帧合成 egui 界面。`mtty-app` 提供主程序及安装包元数据。
包含画中画、Hint、只读模式和每个 pane 的关闭按钮。

> **由 miaotty 更名而来。** v0.0.5 及之前应用名为 `miaotty`。mtty 首次启动会复制
> `~/.config/miaotty`,读取 `MIAOTTY_*` 环境变量,继续响应 `miaotty://` 链接与旧 socket 路径,
> 并在 pane 中导出旧变量,已安装的 hook 与 miao 无需修改 —— 见
> [ADR 0032](docs/decisions/0032-rename-mtty.zh-CN.md) 与 [APP-IDENTITY.zh-CN.md](docs/APP-IDENTITY.zh-CN.md)。

引擎与应用保持解耦，供第三方嵌入。
完整设计见 [`docs/ARCHITECTURE.zh-CN.md`](docs/ARCHITECTURE.zh-CN.md)。

## 截图

终端、文件、远程主机与代理，同在一个窗口。Markdown 编辑器、Mermaid 预览与终端
共用同一套标签和递归分屏布局：

![当前 mtty 工作区：会话侧栏、Markdown 编辑器、Mermaid 预览和分屏终端](docs/images/mtty.png?v=20261004)

采集于 2026-10-04，使用 v0.1.3 开发构建与独立示例项目。代理状态事件与修改提案
通过真实 MTP 控制面驱动；主机地址与排队提示均为示例。

| 代理状态，实时变化 | 编辑器与实时预览 |
|---|---|
| ![控制面事件让徽章和 Agent 面板依次显示 processing、awaiting、error 与 idle](docs/images/mtty-states.gif?v=20261004) | ![Markdown 改动实时更新旁边的预览，包含 Mermaid、表格和代码块](docs/images/mtty-editor.gif?v=20261004) |

| 命令面板 | 详情面板 |
|---|---|
| ![在工作区中打开当前命令面板](docs/images/mtty-palette.gif?v=20261004) | ![当前信息、Agent、大纲、Git、文件、端口和队列面板的截图序列](docs/images/mtty-panels.gif?v=20261004) |

| 行内改动审阅 | 本地与远程主机 |
|---|---|
| ![拟议改动在编辑器中行内展示，再通过命令面板接受](docs/images/mtty-review.gif?v=20261004) | ![分组 SSH 主机库，提供文件入口、跳板配置与端口转发；地址为示例](docs/images/mtty-hosts.png?v=20261004) |

| 后续提示队列 | 代理状态徽章 |
|---|---|
| ![处于 processing 的代理窗格保留两条后续提示](docs/images/mtty-queue.png?v=20261004) | ![processing、awaiting 与 idle 会话徽章，旁边是终端输出与 Git 详情](docs/images/mtty-workspace.png?v=20261004) |

递归分屏把 Git 改动与实际运行的本地 HTTP 服务并排放在同一个标签里：

![左侧是 Git 改动，右侧是实际运行的本地 HTTP 服务](docs/images/mtty-splits.png?v=20261004)

更多可暂停的短演示：**[mtty.dev 上的 mtty](https://mtty.dev/zh/mtty#screens)**。

> **项目状态 —— 预发布。** API 尚未稳定，最新版本见[发布页](https://github.com/oxdingzg/mtty/releases/latest)。macOS 是主要平台;
> Windows 已在真实硬件上构建、测试并经 MTP 驱动(见 [`docs/WINDOWS-DEV.zh-CN.md`](docs/WINDOWS-DEV.zh-CN.md));
> Linux 在 CI 中构建并通过测试,并已在真实的 GNOME/Wayland 桌面上验收(输入法、菜单、文件拖放、剪贴板、快捷键)。

---


## 相关项目

| 项目 | 是什么 | 链接 |
|---|---|---|
| **mtty**(本仓库) | AI 原生的终端与编辑器,以及其背后可嵌入的引擎 | [mtty.dev/mtty](https://mtty.dev/zh/mtty) · [oxdingzg/mtty](https://github.com/oxdingzg/mtty) |
| **miao** | 在终端里运行的开源 AI 编程代理 | [mtty.dev/miao](https://mtty.dev/zh/miao) · [oxdingzg/miao](https://github.com/oxdingzg/miao) |
| **mtty.dev** | 两者的官网与文档站 | [mtty.dev](https://mtty.dev/zh/) |

mtty 与 miao 是两个独立项目,任意一个都可以单独使用。在 mtty 的窗格里,miao 通过控制面上报自己的状态(工作中、
等待你、已完成、出错);mtty 据此给窗格加徽章、在代理需要你时通知你、在代理工作时阻止电脑休眠,并在它空闲时发出你
排队的提示。mtty 也为其他代理 CLI(Claude Code、Codex、OpenCode)提供状态钩子。

有两个都叫 *miaotty* 的东西:本仓库的应用在 v0.0.5 及之前名为 `miaotty`(见上文的更名说明);而
[oxdingzg/miaotty](https://github.com/oxdingzg/miaotty) 是另一个更早的个人 macOS 原型(基于 Ghostty 分叉),
已不再开发,请改用 mtty。

## 功能

**终端内核**
- 基于 PTY 的 shell,经 `alacritty_terminal` 做 VT 解析。
- GPU 字形网格渲染(`wgpu` + `glyphon`),复用 egui 的 device、queue 与 surface。
- 回滚缓冲与滚动条指示、拖拽/双击选区、复制粘贴,以及宽字符 / 中日韩排版与系统 CJK 字体回退。
- 把文件拖到终端上会插入转义好的路径;拖到终端底部的条带上(或按住 Option/Alt)则改为打开:文件在编辑器中打开,文件夹在该目录开一个新终端。拖动时两个落点都会显示出来。
- 查找(`⌘F`),带匹配高亮与结果计数,支持中文。
- 当程序请求时,支持
  [kitty keyboard 协议](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)(CSI-u)
  (五个增强 flag 全支持)与带修饰键的光标移动;F1–F12 / Insert 按 xterm 序列发送。

**窗口与工作区**
- 满高窗口:会话侧栏占满窗口高度(图标、Agent 徽章、`+`、拖拽重排);侧栏标题旁的一行显示当前
  标签标题,面板与字号按钮在右上角。macOS 上标题栏透明,红绿灯位于侧栏顶部,这一行可拖动窗口(双击缩放)。
  收起侧栏时这一行改为显示内联标签栏(`+`、`×`、拖拽重排)。右键标签或会话行弹出
  同一套菜单:重命名标签…、前缀…、标记…、分组…、移出分组(仅在分组内时出现)、复制标签、
  上移/下移、新建标签、关闭标签、关闭其他标签、关闭下方标签。标记会附加在标题后,分组随会话
  一起持久化,重启后仍在。应用提供这套菜单,会话列表与标签栏在分组变化处显示分隔线。
  “关闭标签”、标签的 `×` 与会话行中键关闭整个标签(含全部分屏),但保留最后一个标签。
  `⌘W` / `Ctrl+W` 在分屏标签中仍只关闭当前窗格。
- 左右两侧面板可拖动边缘调整宽度,宽度随窗口尺寸一起保存。
- 递归分屏树:`⌘D` 向右分屏,`⇧⌘D` 向下分屏,分隔条可拖拽调整比例,每个 pane 都有
  关闭按钮。右键 pane 弹出上下文菜单(复制、粘贴、复制/粘贴为 ANSI 或 Shell 转义、撰写、
  发送给 Agent、全选、搜索、关于本行、向右/左/下/上分屏、清除回滚)。`⌘⇧T` 切换临时快速终端。
- details 面板 Files 分页中的**文件列表**(单击用查看器打开)与 **View 规则**:把 pane 的目录、
  前台命令、agent 或 SSH 主机映射为别名、图标、标签标题与徽章,`views.json` 修改后自动生效 ——
  见 [`docs/VIEW-RULES.zh-CN.md`](docs/VIEW-RULES.zh-CN.md)。
- **命令面板**(`⌘K`)与 **Open Quickly**(`⌘⇧O`):覆盖标签、agent、当前目录的文件与子目录、最近文件,并搜索这些文件内容与当前 pane 回滚中的文本(有界扫描,在 UI 线程外执行)。
- 右侧 details 面板含分页:**Info / Agent / Outline / Git / Files / Ports / Queue**
  (git 状态、目录列表、监听端口、提示队列)。
- **Composer**(`⌘E`;Linux/Windows 上 `Ctrl+Shift+E`)与**提示队列**:队列项绑定入队时的 pane,该 pane 的 agent 每次转为空闲时
  投递一条(重启后保留;"立即发送"会将其移出队列);由 agent 状态驱动的**通知**与**防休眠**。后台标签会
  标出 agent 需要输入或出错(`!`)、已完成(✓)或有新输出(•);收到通知后不久切回 mtty,会直接显示该
  通知对应的 pane。
- **查看器/编辑器**:只读预览带行号,编辑态带行号栏与**可选的极简 vim 模式**(`editor-vim`)。
  保存失败会提示且保持"已修改";有未保存修改时需再次关闭才会丢弃。CommonMark 渲染
  (`egui_commonmark`):标题、列表、引用、表格、代码、链接、远程图片,外加 `graph`/`flowchart`、`sequenceDiagram`、`stateDiagram`、`classDiagram`、
  `erDiagram` 与 `pie` 的 Mermaid 子集(或经 `mermaid-command` 全量渲染)。
- **终端内联图片**:Sixel / Kitty / iTerm2 图片由 mtty 直接画在字符网格上——随内容滚动、
  裁剪在 pane 内;Kitty 位置遵循协议的 z-index:负值绘制在文字之下,默认层与正值之上。
  用 `graphics` 开关(默认开)。
- **Agent 任务**:*新建 Agent 任务…* 创建 git worktree 与分支(`<仓库>/.worktrees/<名称>`、
  `mtty/<名称>`),在独立标签中打开并可直接启动 agent;*Agent 任务…* 列出任务,可打开、查看改动
  (含未提交内容)、合并回基线分支或丢弃(二者均需确认)。git 操作在后台执行,不改动仓库中被跟踪的文件。
- **Recipes**:保存并回放整个工作区。
- 设置窗口(`⌘,`):字号/字体族、透明度、行高、光标样式、主题、内联图片、通知、防休眠与
  agent 钩子安装。主题选择器提供 Nord(默认)、Dracula、Gruvbox 与 mtty;mtty 是
  航海蓝(navy)工作区配色,其窗口、卡片与侧栏会跟随该预设,其余预设保持中性的深色外壳。
  关闭窗口时把改动过的值写回 `config.toml`,保留注释与其他键;无法解析的
  `config.toml` 会在状态栏提示,且不会被覆盖。

**配置与集成**
- 配置位于 `~/.config/mtty/config.toml`:字号、字体族、透明度、行高、光标样式、主题、配色、
  `language`(英文或简体中文)、`editor`、agent 开关、`quick-terminal-hotkey`、
  `update-check-url` / `update-pubkey` —— 见 [`docs/config.example.toml`](docs/config.example.toml)。
- 当不存在 mtty 配置时,自动导入 ghostty 的 `config` 与 alacritty 的 `alacritty.toml`。
- zsh、bash、fish 与 PowerShell 的 shell 集成(经 OSC 7 上报 cwd、命令历史、OSC 133 命令边界),
  每种 shell 各用一个 shim 安装 —— 不修改用户点文件。有了它,*复制上一条命令的输出* 与 *把上一条命令的输出发到 Composer* 只取上一条
  命令的输出(含退出码)。
- **URL scheme**:`mtty://`、`ssh://`、`x-man-page://` 会用对应命令新开标签;二次启动会转发给
  正在运行的实例(单实例,含"聚焦 pane""quick"意图),无参数的二次启动会把窗口带到前台。
  URL 目前经命令行参数传入;macOS 从浏览器/Finder 发来的 URL 事件尚未处理。
- **macOS 原生菜单栏**:安装的 `.app` 会把 文件/编辑/视图/终端/Agent/帮助 放进系统菜单栏
  (含 About/Services/Hide/Quit),窗口内不再有菜单条,与其它 macOS 终端一致(ADR 0031);
  裸跑 `mtty` 仍用窗口内菜单。窗口本身请求深色外观,标题栏与界面一致,不再是一条
  浅色条。
- **全局快速终端热键**:macOS/Windows 用 `global-hotkey`,Linux 使用 sway/hyprland/GNOME 等 compositor 绑定。
- **Agent 集成**:检测 claude/codex/opencode/miao,并可在新标签中启动(设置或命令面板);
  安装状态上报 hook 脚本,并复制可直接合并的配置 —— Claude Code(`~/.claude/settings.json`)
  与 codex(`~/.codex/hooks.json`)的 `hooks` JSON、opencode 的插件文件。不替用户修改 agent 配置;
  hook 只为运行在 mtty pane 内的 agent 上报。`miao` 通过内置集成自动上报状态,无需接线 hook。
- **更新**:mtty 启动时静默检查一次版本清单(设 `update-auto-check = false` 可关闭),有新版本时在状态栏提示;
  也可随时 *检查更新*;*下载更新* 获取本平台安装包,在程序内校验 SHA-256 与 minisign 签名
  (使用内置的发布公钥,或 `update-pubkey`);未签名或校验不符的下载会被删除,绝不安装。*安装并重启*(或命令面板
  的 *更新并重启*,一步完成)在 macOS 上替换 app 并在失败时回滚,替换正在运行的 AppImage,或运行 Windows MSI;
  deb 与 tarball 安装则打开已校验的下载交给包管理器。
- **主机库**:保存在 `~/.config/mtty/hosts.toml` 的主机(名称、地址、用户、端口、分组、标签、跳板机)
  显示在侧栏与 Open Quickly 中;*主机…* 可搜索、添加、删除(需确认),并导入 `~/.ssh/config` 中的具体
  `Host` 条目 —— 导入的主机按别名连接,ssh 对该条目的所有配置都会生效。`mtty://host/<名称>` 可从脚本或
  启动器直接连接。不保存密码或密钥。*主机…* 还显示 ssh-agent 中的密钥,检查主机密钥是否与 `known_hosts`
  一致(未知密钥显示指纹供核对后信任;已变化的密钥会被拒绝),并在终端标签中运行 `ssh-keygen` / `ssh-copy-id`。每台主机可保存端口转发(`-L`、`-R`、`-D` SOCKS),
  在窗口中启停,显示运行状态或 ssh 放弃的原因;状态栏显示数量。
- **SSH 会话与远端 view/edit**:*新建 SSH 会话…* 是一个完整的主机编辑器。快速连接接收
  `[user@]host[:port]` 并直接连接,不保存;保存主机字段包括名称、可选的 `~/.ssh/config` 别名、
  主机、用户、端口、分组与标签,高级区还可设置跳板机、持久 tmux 会话、mosh 与端口转发。
  *连接* 直接打开目标而不保存,*保存并连接* 会先保存主机。它遵循 `~/.ssh/config`,复用
  ControlMaster 连接,远端零安装引导 terminfo;*查看/编辑远端文件…* 经该连接读写(自动带入
  当前 SSH 标签的主机)。
- **串口、Telnet 与裸 TCP**:*新建串口/Telnet/TCP 会话…* 把串口控制台(设备、波特率、数据位、校验、停止位、流控)、
  Telnet 连接或裸 TCP socket 作为 pane 打开。已保存主机带 `kind`,也按同样方式打开;Telnet 与裸 TCP 标注为未加密(ADR 0037)。
- **.ppk 密钥**:主机窗口中的 .ppk 密钥导入操作 读取 `.ppk`(v2 或 v3,Ed25519/RSA/ECDSA),
  写出加密的 OpenSSH 密钥到 `~/.ssh`,绝不明文保存(ADR 0038)。
- **ACP agent**:任何支持 Agent Client Protocol 的 agent(Codex、Gemini CLI 等)都可从
  *ACP Agent…* 运行:转写窗口流式显示回复、发送 prompt、把其 diff 变成可审阅的编辑器
  提案,并以“允许/拒绝”询问权限。agent 在 `config.toml` 的 `[acp]` 中配置(ADR 0040)。
- **FTP/FTPS**:*连接 FTP/FTPS…* 经系统 `curl` 打开同一个双栏文件浏览器(默认显式 TLS;明文 FTP 会标注未加密)。
  口令只保存在内存中;留空则使用 `~/.netrc` 或匿名登录。上传与下载均可递归处理整个文件夹。
- **持久会话**:在已保存主机上勾选 *在 tmux 中保持 shell*,重连后回到原会话;*使用 mosh 连接* 可跨休眠和网络切换
  (两端都需安装 mosh;本机没有 mosh 时退回 ssh,Windows 没有原生 mosh 客户端,始终使用 ssh)。
- **加密同步(可选,默认关闭)**:*同步主机与片段…* 把主机与片段加密后写入你已在同步的文件夹(iCloud Drive、
  Dropbox、Syncthing)。无需账号、没有服务器;密钥保存在 `~/.config/mtty/sync.key`,第二台设备用配对码加入
  (ADR 0033)。
- **命令片段与广播**:*命令片段…* 把命令保存在 `~/.config/mtty/snippets.toml`(名称、命令、标签),
  可在当前 pane 执行,或在多台已保存主机上执行(各开一个标签);Open Quickly 也能搜到。*向本标签所有分屏
  广播输入* 让输入同时进入所有分屏(状态栏显示 BROADCAST)。
- **SFTP**:双栏文件浏览(本机 | 主机),可从"主机…"、命令面板(当前 SSH 标签)或 `mtty://sftp/<名称>` 打开:
  浏览、上传(也可把文件拖到窗口上)、带进度的下载、重命名、chmod、新建文件夹与删除(需确认)。底层调用系统
  `sftp`,ssh 配置、跳板机、agent 与共享连接均照常生效。

**自动化**
- **MTP 控制面**,经 per-user Unix socket(Windows 为命名管道):
  `core.ping/health`、`agent.state.*`、`history.*`、`pane.list/send/run/focus/close`、
  `pane.output`(上一条命令的输出,能力 `pane.read`),以及 `app.view/edit`(在应用中打开文件)与
  `file.read/write`(上限 2 MB)。
- **`mtty-cli`**,跨平台的控制面客户端。

---

## 架构

引擎按层组织,依赖只向内指向(`host → render → core`),引擎之上提供可复用的 UI helper。

| Crate | 职责 |
|-------|------|
| [`mtty-core`](crates/mtty-core) | PTY、VT 解析、网格/回滚、选区、查找、OSC、输入编码。不含 GPU 与窗口。 |
| [`mtty-graphics`](crates/mtty-graphics) | 内联图片流扫描器与解码器(Sixel、Kitty、iTerm2)。 |
| [`mtty-render`](crates/mtty-render) | `wgpu` + `glyphon` 字形网格渲染器,含 quad 与图像管线。 |
| [`mtty-ui`](crates/mtty-ui) | 与 host 无关的 UI:主题、输入编码、选区、分屏布局、egui 外壳、命令面板、hint、vim、markdown、ssh、更新与 agent 集成等 helper。 |
| [`mtty-editor`](crates/mtty-editor) | 编辑器 pane 的编辑内核:rope 缓冲区、事务与撤销、多选区、光标移动、查找替换(ADR 0034)。不含界面代码。 |
| [`mtty-lsp`](crates/mtty-lsp) | 编辑器 pane 的语言服务器(LSP)客户端:按工作区在后台线程运行服务器,同步文档,提供诊断、悬停、补全与跳转定义(ADR 0034)。不含界面代码。 |
| [`mtty-config`](crates/mtty-config) | 配置与主题,ghostty/alacritty 导入,以及 View 规则引擎。 |
| [`mtty-mtp`](crates/mtty-mtp) | MTP 协议、host/client 与传输(Unix socket、Windows 命名管道、TCP)。 |
| [`mtty-widget`](crates/mtty-widget) | mtty 原生 host 库:`winit` + `wgpu` 渲染循环,直接绘制网格并合成 egui 外壳。 |
| [`mtty-app`](mtty-app) | `mtty` 原生主程序及平台安装包元数据。 |
| [`mtty-cli`](mtty-cli) | `mtty-cli` 控制客户端。 |

终端路径 —— `pty → vt → grid → renderer` —— 使用有界输出队列与损伤驱动重绘。行构建仍分配 span 缓冲区，吞吐与分配预算见[性能文档](docs/PERFORMANCE.md)。平台差异只出现在负责
相应关注点的 crate 中少量 `#[cfg]` 守卫的代码块里:`mtty-core` 的 PTY 派生
(`src/term.rs`)、`mtty-widget` 的窗口/事件循环(`src/lib.rs`),以及 `mtty-mtp` 的
socket/命名管道传输(`src/lib.rs`)。

### 原则

1. 引擎与 host 分离;hosts 是引擎的第一批消费者。
2. 输出队列与保留图像有界，减少复制和重绘，并通过实测预算验证性能。
3. 先做应用、后抽库,API 由真实需求驱动。
4. 平台差异只出现在 `mtty-core`、`mtty-widget` 与 `mtty-mtp` 中 `#[cfg]` 守卫的代码块里。
5. 控制面(MTP / CLI)与引擎解耦。

---

## 环境要求

- Rust **stable** 工具链(锁定于 [`rust-toolchain.toml`](rust-toolchain.toml);MSRV 1.80)。
- 支持 Metal(macOS)、Vulkan(Linux)或 DX12(Windows)的 GPU 与驱动。
- 在 Linux 上,需要常见的 `winit`/`wgpu` 系统库(X11 或 Wayland 开发包)。

---

## 快速开始

```sh
git clone https://github.com/oxdingzg/mtty.git
cd mtty

# 构建并运行统一的原生终端
cargo run --release -p mtty-app
```

首次构建会编译 `wgpu`/`glyphon`,可能需要几分钟。

### 测试与静态检查

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all -- --check
```

CI([`.github/workflows/ci.yml`](.github/workflows/ci.yml))在每次 push、PR 与 nightly 上运行。
push 时执行 `cargo fmt --check`、`cargo clippy --workspace --all-targets -D warnings`、
Linux 与 macOS 的 `cargo check` 以及隐私扫描;完整的三平台
`cargo test --workspace`、Linux 软件 Vulkan 渲染测试、Windows job 与 release 模式性能门,
则在 pull request、nightly 计划与手动触发时运行。仅文档变更的 push 会跳过编译与 lint job。

### 打包

```sh
scripts/package-macos.sh          # -> dist/mtty.app(ad-hoc 签名)
```

发布构建由 [`.github/workflows/release.yml`](.github/workflows/release.yml) 在
`v*` 标签上生成:`mtty` 与 `mtty-cli`、macOS app bundle、
Linux `.deb`/AppImage 以及 Windows MSI。[`dist-workspace.toml`](dist-workspace.toml) 是一份
[cargo-dist](https://opensource.axo.dev/cargo-dist/) 脚手架。见
[`docs/INSTALL.zh-CN.md`](docs/INSTALL.zh-CN.md) 与 [`docs/RELEASE.zh-CN.md`](docs/RELEASE.zh-CN.md)。


---

## 配置

mtty 读取 `~/.config/mtty/config.toml`(或
`$XDG_CONFIG_HOME/mtty/config.toml`;Windows 上为 `%APPDATA%\mtty\config.toml`)。所有键均可选;
若不存在 mtty 配置，会自动导入 ghostty 的 `config` 与 alacritty 的 `alacritty.toml`。

```toml
font-size = 13
theme     = "nord"   # nord | dracula | gruvbox | mtty
```

[`docs/CONFIG.zh-CN.md`](docs/CONFIG.zh-CN.md) 里有各配置项、编辑器与 ACP 部分，以及 shell 集成;
[`docs/config.example.toml`](docs/config.example.toml) 则把每个键及其默认值放在一个文件里。

---

## 控制面

**MTP**(mtty terminal protocol)控制面在 `$XDG_RUNTIME_DIR/mtty.sock`
(回退到 `$TMPDIR`)上使用换行分隔的 JSON,且 socket 以仅属主可访问的权限创建。每个响应都带状态
`revision`,客户端可以阻塞等待它变化，或订阅事件流，而不必轮询。`mtty-cli` 是参考客户端:

```sh
mtty-cli pane list
mtty-cli pane run --pane ID --data "echo hello"
mtty-cli events --topic agent.state
mtty-cli wait --since 42
mtty-cli state claude --state processing --pane ID
```

[`docs/CLI.zh-CN.md`](docs/CLI.zh-CN.md) 里有完整命令集、状态版本号与事件模型、经 TCP 的远程访问，
以及令牌与能力白名单。

---

## 快捷键

`⌘T` 新建标签，`⌘D` / `⇧⌘D` 分屏，`⇧⌘O` 是 Open Quickly，`⌘K` 是命令面板，`⌘F` 是查找。
在 Linux 与 Windows 上，单独的 `Ctrl` 组合键(`Ctrl+C`、`Ctrl+W`、`Ctrl+D`……)始终交给 shell，
`Super`/`Win` 组合留给桌面。

[`docs/SHORTCUTS.zh-CN.md`](docs/SHORTCUTS.zh-CN.md) 里有完整的两张表，包含编辑器窗格自己的按键。

---

## 文档

- [安装与首次上手](docs/INSTALL.zh-CN.md) · [编辑器](docs/EDITOR.zh-CN.md) · [远程连接](docs/REMOTE.zh-CN.md)

文档默认英文,并配套同步维护的简体中文版(`*.zh-CN.md`)。

| 文档 | English | 简体中文 |
|------|---------|---------|
| 架构与设计 | [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | [`docs/ARCHITECTURE.zh-CN.md`](docs/ARCHITECTURE.zh-CN.md) |
| 架构决策记录 | [`docs/decisions/`](docs/decisions/README.md) | [`docs/decisions/README.zh-CN.md`](docs/decisions/README.zh-CN.md) |
| 安装 | [`docs/INSTALL.md`](docs/INSTALL.md) | [`docs/INSTALL.zh-CN.md`](docs/INSTALL.zh-CN.md) |
| 配置 | [`docs/CONFIG.md`](docs/CONFIG.md) | [`docs/CONFIG.zh-CN.md`](docs/CONFIG.zh-CN.md) |
| `mtty-cli` 控制面 | [`docs/CLI.md`](docs/CLI.md) | [`docs/CLI.zh-CN.md`](docs/CLI.zh-CN.md) |
| 快捷键 | [`docs/SHORTCUTS.md`](docs/SHORTCUTS.md) | [`docs/SHORTCUTS.zh-CN.md`](docs/SHORTCUTS.zh-CN.md) |
| 排障 | [`docs/TROUBLESHOOTING.md`](docs/TROUBLESHOOTING.md) | [`docs/TROUBLESHOOTING.zh-CN.md`](docs/TROUBLESHOOTING.zh-CN.md) |
| View 规则(标题/图标/徽章) | [`docs/VIEW-RULES.md`](docs/VIEW-RULES.md) | [`docs/VIEW-RULES.zh-CN.md`](docs/VIEW-RULES.zh-CN.md) |
| 性能预算与门 | [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) | [`docs/PERFORMANCE.zh-CN.md`](docs/PERFORMANCE.zh-CN.md) |
| Windows 开发/验证 | [`docs/WINDOWS-DEV.md`](docs/WINDOWS-DEV.md) | [`docs/WINDOWS-DEV.zh-CN.md`](docs/WINDOWS-DEV.zh-CN.md) |
| 产品需求与路线图 | [`docs/PRODUCT.md`](docs/PRODUCT.md) | [`docs/PRODUCT.zh-CN.md`](docs/PRODUCT.zh-CN.md) |
| 发布与更新 | [`docs/RELEASE.md`](docs/RELEASE.md) | [`docs/RELEASE.zh-CN.md`](docs/RELEASE.zh-CN.md) |
| 工作约定(隐私、检查) | [`AGENTS.md`](AGENTS.md) | —— |
| 示例配置 | [`docs/config.example.toml`](docs/config.example.toml) | —— |

---

## 路线图

**mtty** 的产品方向、
已验证的功能基线与里程碑计划见 [`docs/PRODUCT.zh-CN.md`](docs/PRODUCT.zh-CN.md)。

已完成的里程碑:

- **M5 编辑器 pane**：rope 文档、tree-sitter 高亮、多光标、查找替换、LSP、vim、折叠与 Markdown 预览。
- **M6 远程(连接)**：串口、Telnet、原始 TCP 与加密 `.ppk` 导入；SSH 按 ADR 0039 继续使用系统 OpenSSH。
- **M7 Agent 工作台**：可撤销的修改提案、ACP 会话和上下文交接。ACP 支持认证、加载会话及终端请求；
  文件写入在用户审阅并保存后才报告成功。

近期已完成:Windows 命名管道传输与 ConPTY 路径(真实硬件验证)、会话恢复、View 规则、
Open Quickly、details 面板、agent 闭环(通知、防休眠、提示队列)、Recipes、经 ssh 的远端
view/edit、版本检查、URL scheme、全局快速终端热键、应用的内联 IME 拼写与
终端内联图片、Mermaid 时序/状态/类/ER/饼图、MTP 事件流、i18n,以及性能门。
CI 性能基线已持久化于 `benches/perf-baseline.json`,由 nightly/手动运行刷新;
比较仅报告,绝对预算作为门控。

仍待完成:

- **发布签名**：Apple 公证与 Windows MSI 签名仍需要所有者提供凭证；发布包已有 minisign 签名和更新清单。
- **验收记录**：本轮桌面更新、输入法、剪贴板、传输与 Agent 验收结果及边界见
  [ACCEPTANCE.zh-CN.md](docs/ACCEPTANCE.zh-CN.md)。
- **终端内联图片**:回滚容量内锚定精确,超出后为近似(alacritty 不暴露滚动计数,除非打补丁)。
  会话恢复会把图像按回滚行锚点一并带回:宽度不变时精确;宽度变化导致文本重排时宁可省略,
  也不画在无关文本上(动画恢复为静止帧)。
- **Mermaid**:内置子集覆盖 `graph`/`flowchart`、`sequenceDiagram`、`stateDiagram`、
  `classDiagram`、`erDiagram` 与 `pie`;gantt、journey、git graph 等仍回退到
  `mermaid-command` 或占位符。
- **i18n**:外壳、命令面板、设置与对话框已覆盖;少量示例/agent 串刻意保留英文。

---

## 参与贡献

欢迎贡献。提交 pull request 前:

1. 运行 `cargo fmt --all`、`cargo clippy --workspace --all-targets` 与
   `cargo test --workspace`。
2. 保持文档双语:修改某个文档时,请在同一改动中更新其 `*.zh-CN.md` 对应版本(例如
   `README.md` / `README.zh-CN.md`,或 `docs/RELEASE.md` / `docs/RELEASE.zh-CN.md`)。
3. 若要变更某条**锁定**的设计决策,请在 [`docs/decisions/`](docs/decisions/README.md)
   下新增一份 ADR,而不是就地改写记录。

参与贡献即表示你同意你的贡献按本项目的许可(Apache-2.0)授权。

---

## 许可

基于 [Apache License, Version 2.0](LICENSE) 授权。
