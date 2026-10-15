# mtty 产品需求与路线图

[English](PRODUCT.md)

> 基线:`v0.1.3` 加 [验收记录](ACCEPTANCE.zh-CN.md) 中的后续修复，日期 2026-10-04。
> 本文是功能需求的唯一出处:README 只描述"现在能用的",本文还记录"要做什么、按什么顺序、怎样算完成"。

## 1. 定位

**mtty** 是一个跨平台(macOS / Linux / Windows)的 **AI 原生终端与编辑器,本地与远程同样顺手**:终端、文件、
远程主机与 AI 编程 agent 共用一个快速的原生窗口,以及同一套标签与分屏。它立在三根支柱上——终端与远程、编辑器、Agent
工作台——把它们连在一起的是受性能门约束的 Rust 与 GPU 渲染,以及不需要账号、不强制上云:mtty 托管你选择的 agent,自己从不
调用模型。

| 方向 | 用户价值 |
|---|---|
| 终端与远程 | 本地 shell 与已保存主机共用标签和分屏；SSH、文件传输、端口转发、串口、Telnet、TCP 都在同一工作台 |
| 编辑器 | 在 shell 旁打开代码，使用高亮与语言服务器，处理大文件、多光标及实时预览 |
| 代理工作台 | 看见代理何时需要输入，准备后续提示，发送上下文并审阅拟议改动 |
| miao 集成 | 无需额外 hook 的状态上报、会话恢复与 MTP 控制 |

一句话:**终端要快而稳,编辑器要好用到不想离开,agent 要看得见、管得住、改动可审阅,远程主机要像本地目录一样好用。**

### 1.1 命名

- 正式产品名:**mtty**。仓库与可嵌入引擎都叫 **mtty**。
- 可执行文件 `mtty` / `mtty-cli`,bundle `mtty.app`(`dev.mtty.terminal`),配置目录
  `~/.config/mtty`,环境变量 `MTTY_*`,URL scheme `mtty://`。v0.0.5 及之前名为 `miaotty`,
  兼容规则见 [ADR 0032](decisions/0032-rename-mtty.zh-CN.md)。

### 1.2 目标用户与核心场景

1. **开发者本地终端**:多项目、多标签、分屏,中文路径和中文输出正常显示与查找。
2. **AI 编码 agent 用户**:同时跑 codex / claude / opencode / miao,知道谁在忙、谁在等我、
   谁出错了,能把提示排队交给它们。
3. **运维/全栈**:管理几十台主机,SSH 登录、传文件、转发端口、批量执行常用命令,
   凭证不离开本机;也能经串口与 Telnet 连接设备(M6)。
4. **编辑代码与文本**:在终端旁打开任何文件,大文件也快,有高亮和语言服务器的帮助(M5);保留 agent 的改动之前先审阅(M7)。

## 2. 交互设计原则

这些原则是验收标准的一部分,新功能评审时逐条核对。

1. **终端优先**:终端区域永远是主角;面板、对话框不能抢走终端输入,
   也不能让按键同时作用于两处(文本框有焦点时 ⌘C/⌘V/⌘A 作用于文本框)。
2. **键盘优先、一个入口**:每个功能都能从命令面板(`⌘K`)找到;常用功能有快捷键,
   并且快捷键在菜单、面板、文档三处一致。
3. **失败必须可见**:任何用户发起的操作失败(保存、打开、连接、写配置)都要在界面上看到原因;
   不能只写 stderr,不能假装成功。
4. **不丢数据**:未保存的修改关闭前要确认;保存失败保持"已修改";写配置不覆盖解析失败的文件。
5. **不替用户改配置**:不改 shell 点文件、agent 配置、`~/.ssh/config`;需要接入时给出片段让用户自己粘贴。
6. **远端零安装**:SSH 能力不要求远端安装任何东西。
7. **凭证留在本机**:优先使用 ssh-agent 与系统钥匙串;不明文保存密码;云同步(若做)必须端到端加密且可完全关闭。
8. **UI 线程不阻塞**:网络、外部进程、大目录读取都在后台线程,界面显示进度并可取消。
9. **中英双语**:界面与文档都提供英文和简体中文。

## 3. 功能基线(当前版本)

状态含义:

- **可用**:用户入口可达、实现完整,有自动化测试或真实窗口冒烟证据。
- **部分**:能用,但有明确缺口(列在备注里,并进入 M0)。
- **未提供**:曾被文档宣称或用户期望,但当前 native 应用没有;已从 README 移除,进入路线图。

验证证据:`cargo test --workspace`(183 项)、`cargo clippy -D warnings`、
`scripts/smoke-hosts.py`(真实窗口 + 真实 shell + GPU 截图),以及本次对 native 代码的逐项核对。
2026-10-02 按代码重新逐项核对并更新本表(此时 282 项测试),M0–M3 已修复的问题不再列出。

### 3.1 终端内核

| 功能 | 状态 | 备注 |
|---|---|---|
| PTY + VT 解析(alacritty_terminal),GPU 字形网格渲染 | 可用 | 性能门见 PERFORMANCE |
| 回滚、选区、复制粘贴、中日韩宽字符与字体回退 | 可用 | 复制在选区任一边界都保留完整宽字符;拖拽与双击一致 |
| 查找 `⌘F`(高亮 + 计数) | 可用 | 本次修复:中文查询不匹配、含中文行高亮错位 |
| 键盘编码:Ctrl/Alt/修饰键导航、F1–F12、Insert | 可用 | 本次修复:F1–F12 与 Insert 此前完全不发送 |
| kitty keyboard 协议 | 可用 | 五个 flag 全支持:消歧义转义(1)、按键事件类型(含释放,2)、替代键(4)、全键转义(8)、关联文本(16) |
| 输入法(IME)内联预编辑 | 可用 | 候选窗定位到光标(`set_ime_cursor_area`);macOS 上的位置待桌面人工确认 |
| 终端内联图片 Sixel / Kitty / iTerm2 | 可用 | Kitty z-index 生效(负值绘制在文字网格之下);会话恢复会把图片按回滚行锚点带回,文本重排后宁可省略也不错位 |
| 鼠标上报(SGR / X10) | 可用 | |

### 3.2 窗口与工作区

| 功能 | 状态 | 备注 |
|---|---|---|
| 会话侧栏(占满高度)与标签栏(收起侧栏时):拖拽重排、`+`、`×`、右键菜单(重命名/前缀/标记/分组/复制/移动/关闭其他/关闭下方) | 可用 | 复制标签保留标记和分组;关闭其他/下方可重开 |
| 分屏树 `⌘D` / `⇧⌘D`、拖动分隔条、每个 pane 的关闭按钮 | 可用 | 本地新分屏/新标签继承当前目录；SSH 分屏复用保存的连接命令，普通 SSH 会话在启动命令符合 PTY 行长限制时继承已报告的远程目录，tmux/mosh 保留自身会话语义。pane 右键菜单也可向右/左/下/上分屏 |
| Pane 右键上下文菜单 | 可用 | 复制、粘贴、复制/粘贴为(ANSI、Shell 转义)、撰写、发送给 Agent、全选、搜索、关于本行、分屏(右/左/下/上)、清除回滚;复用命令面板的命令 |
| 快速终端 `⌘⇧T`、重开关闭的标签 `⌘⇧Z` | 可用 | 重开只恢复目录 |
| 全局快速终端热键 | 可用 | 本次修复:一次按键触发两个动作(切到 Quick 后又隐藏窗口) |
| 命令面板 `⌘K` | 可用 | “待处理事项”按窗格聚合等待输入、执行失败和待审阅结果；“下一个待处理任务”按紧急程度循环跳转。可打开终端、把上一条输出发到 Composer、打开 Agent 任务或标为已处理。重复状态不产生重复事项，继续运行或关闭窗格后移除旧事项。完成不代表验证通过 |
| Open Quickly `⌘⇧O` | 可用 | 标签、agent、片段、主机、当前目录的文件与目录、最近文件;并搜索该目录下文件内容与当前 pane 回滚中的文本(有界扫描,在 UI 线程外执行) |
| details 面板:Info / Agent / Outline / Git / Files / Ports / Queue | 可用 | Ports 覆盖 shell 及其子进程;非 git 目录如实显示;隐藏时只每 10 秒查一次 git(状态栏显示分支) |
| 会话恢复(布局、目录、标题、分组、终端内容) | 可用 | 每个终端最近 5000 行内容及其内联图片会恢复(保留颜色,文件仅本人可读,`restore-scrollback = false` 可关闭;图片仅在 pane 宽度不变时恢复,文本重排后宁可省略也不错位);退出时在运行的程序会提示"按回车重新运行",绝不自动执行。从 Dock、注销或 AppleScript 退出现在也会保存。运行中的程序通过每 pane 一个的 PTY 宿主挺过更新、重启和崩溃(ADR 0041,`pty-host`,默认开启);普通退出会结束它们,除非设置 `keep-sessions-on-quit` |
| Recipes 保存/打开工作区 | 可用 | 名称做文件名校验,写入失败会提示 |
| 画中画、Hint、只读模式 | 可用 | 只读模式拦截 MTP `pane.send/run` |
| View 规则 | 可用 | 别名、标题、图标、徽章;按目录/命令/agent/host/文件匹配(文件取前台程序打开的文件);`views.json` 热加载 |
| macOS 原生菜单栏 | 可用 | 本次修复:文本框有焦点时菜单的复制/粘贴/全选作用于终端 |
| 设置窗口 `⌘,` | 可用 | 本次修复:此前改动不落盘、主题名被强制为 Nord;配置语法错误时静默回退 |

### 3.3 查看器与编辑器

| 功能 | 状态 | 备注 |
|---|---|---|
| 只读查看、编辑、行号、语法高亮、极简 vim 模式 | 可用 | 本次修复:保存失败被吞掉且显示为已保存;关闭时不确认未保存修改 |
| Markdown 渲染 + Mermaid 子集 | 可用 | 相对路径图片按文档目录解析,表格单元格自动换行;外部 `mermaid-command` 在后台执行 |
| Edit in Tab | 可用 | 本地文件;运行配置项 `editor`,否则 `$EDITOR`,否则 `vi`(Windows 为记事本);有未保存修改时不可用 |
| 跳转行高亮、Open Externally | 可用 | 编辑器 pane 行定位及命令面板外部打开入口 |

### 3.4 Agent

| 功能 | 状态 | 备注 |
|---|---|---|
| 检测 claude / codex / opencode / miao,安装状态 hook,显示接入片段 | 可用 | 片段带"复制"按钮 |
| 状态徽章、需要关注标记、系统通知、防休眠 | 可用 | 本次修复:防休眠进程在应用退出后可能残留 |
| Composer `⌘⇧E`、提示队列(手动发送) | 可用 | |
| agent 空闲时自动投递队列 | 可用 | 队列项绑定目标 pane;旧版本保存的无 pane 项只能手动发送 |
| 从界面启动 agent | 可用 | 设置与命令面板 |
| 显示 hook 上报的会话 id | 可用 | Agent 面板显示 agent、状态、会话 id、tty |
| Resume、配额显示 | 可用 | [ADR 0042](decisions/0042-agent-resume-quota.zh-CN.md);恢复模板随 agent 而定,配额仅在 agent 上报时显示 |

### 3.5 远程与运维

| 功能 | 状态 | 备注 |
|---|---|---|
| 新建 SSH 会话(遵循 `~/.ssh/config`、ControlMaster 复用、零安装 terminfo) | 可用 | 不在 UI 线程执行 `ssh -G`;恢复的 SSH 标签提示已断开,按回车重新连接,设置 `ssh-auto-reconnect = true` 后启动时自动重连(默认关闭) |
| 远端文件查看/编辑(经 ssh) | 可用 | 主机取自当前 SSH 标签(其他情况手填);读写在后台执行,失败会提示 |
| 主机库、分组、密钥管理、SFTP、FTP、端口转发、Snippets | 可用 | 见 M3；真实 mosh SSH 引导及 UDP 会话已验收 |

### 3.6 系统集成与自动化

| 功能 | 状态 | 备注 |
|---|---|---|
| 配置文件 + ghostty / alacritty 导入 | 可用 | 本次修复:语法错误的 `config.toml` 会在状态栏提示原因 |
| shell 集成(OSC 7、OSC 133、命令历史) | 可用 | zsh、bash、fish、PowerShell。“命令输出历史”在当前会话为每个窗格保留最多 50 条已结束命令 / 8 MiB 输出，附命令文本、目录、退出码和耗时。可筛选失败项、浏览/复制/导出输出、发到 Composer 或 Agent，并编辑审阅后在已空闲且位于原已知目录的原窗格重跑。每条命令捕获最多 2 MiB，预览限制超长行，复制/导出保留捕获文本 |
| URL scheme 与单实例转发 | 可用 | 命令行与 macOS 浏览器/Finder 发来的 URL 事件均可 |
| MTP 控制面与 `mtty-cli` | 可用 | 真实窗口冒烟覆盖 |
| 版本检查与更新 | 可用 | 下载后程序内校验 SHA-256 与 minisign 签名(必需),macOS 安装并重启已端到端验证;Linux AppImage 签名更新已在实际窗口验证；Windows 桌面验收见 ACCEPTANCE |
| 中英界面 | 可用 | 少量示例/agent 串刻意保留英文 |

## 4. 路线图与执行批次

执行顺序:**M1 → M0 → M2 → M3 → M4**。M1(改名)按 2026-10-01 的决定提前,因为越晚改,
需要迁移的用户状态和外部集成越多。每个批次是一个可独立提交、独立验证的单元:

- 每批结束必须通过:`cargo fmt --check`、`cargo clippy -D warnings`、`cargo test --workspace`、
  `scripts/smoke-hosts.py`(涉及界面或打包时),以及该批列出的专项验收。
- 每批一个(或少数几个)提交,完成后立即推送,并在本节打勾、注明证据。
- 需要凭证、真机或所有者确认的步骤(签名、公证、发布、生产主机)标记为 **[所有者]**,不在批次内自动执行。

### M1 品牌统一:mtty(设计见 [ADR 0032](decisions/0032-rename-mtty.zh-CN.md))

- [x] **B1.1 运行时改名与兼容层** — `ee12822`:迁移/环境变量/socket 链接/scheme 单测;冒烟覆盖旧配置复制、两套 pane 变量、旧式 hook 上报、旧 socket 路径。
  - `mtty-config`:`config_dir()`、`migrate_legacy_config()`(复制一次、不覆盖、不删旧目录)、`env()`(`MTTY_*` 优先、回退 `MIAOTTY_*`)。
  - 所有路径改走 `config_dir()`:config、views、hooks、launch inbox、session/queue/window/recipes。
  - MTP:默认 socket `mtty.sock` + `miaotty.sock` 兼容链接;Windows 管道 `mtty`;令牌/能力环境变量双读。
  - pane 环境:同时导出 `MTTY_*` 与 `MIAOTTY_*`,`MTTY_CLI`/`MIAOTTY_CLI` 指向同目录 CLI 的绝对路径。
  - URL scheme `mtty://` 与 `miaotty://` 都接受;hook 脚本、shell shim、热键片段、ControlPath、界面文字改为 mtty。
  - 验收:迁移/环境变量/scheme 单元测试;旧 hook 脚本在新 pane 中上报状态成功(冒烟)。
- [x] **B1.2 构建与打包** — `ee12822`:`package-macos.sh` + `check-macos-bundle.py` + `smoke-hosts.py --bundle` 通过,`test-release-manifest.py` 9 项通过;release 工作流演练(run 36830274914)四个 runner 全部通过,含 MSI 安装/卸载与两个 scheme、deb 兼容别名。
  - 目录与包:`miaotty-app` → `mtty-app`(二进制 `mtty`),`miaotty-cli` → `mtty-cli`。
  - macOS:`mtty.app`、`dev.mtty.terminal`、URL scheme 注册;安装脚本归档并移除旧 `miaotty.app`。
  - Linux:deb 包 `mtty`(Replaces/Conflicts/Provides `miaotty`)、兼容符号链接、`.desktop`、AppImage。
  - Windows:MSI 产品名与目录 `mtty`,UpgradeCode 不变,注册 `mtty://` 与 `miaotty://`。
  - `release.yml`、`ci.yml`、更新清单脚本与其测试、冒烟/性能脚本、`windows-verify.ps1`。
  - 验收:`scripts/package-macos.sh` + `check-macos-bundle.py` + `smoke-hosts.py --bundle` 通过;
    `test-release-manifest.py` 通过;release 工作流手动演练 **[所有者]**。
- [x] **B1.3 文档与官网**(2026-10-02 核对:仓库内 `miaotty` 只出现在兼容说明与历史记录中,mtty.dev 主分支已改为 mtty,`check-privacy.sh` 通过)
  - README(中英)、`docs/*`、AGENTS.md、配置示例改为 mtty;APP-IDENTITY 改写为迁移说明;历史 ADR 不改。
  - mtty.dev 站点的安装、配置、CLI 示例改为 mtty(独立仓库,单独提交)。
  - 验收:`check-privacy.sh` 通过;仓库内除兼容说明与历史记录外不再出现 `miaotty`。

### M0 稳定化:把"部分"变成"可用"

- [x] **B0.1 UI 线程不阻塞**(另修:SSH 会话改用用户输入的别名连接,`Host 别名` 下的配置此前不生效):`ssh -G`、远端读写、`mermaid-command`、Files 目录读取改为后台任务 +
  加载状态;details 面板只在可见时轮询;Ports 覆盖子进程;非 git 目录如实显示。
  验收:后台任务单元测试;冒烟中打开远端/大目录时界面帧不停顿(日志计时)。
- [x] **B0.2 工作区正确性**(只读模式下 MTP `pane.send/run` 返回 `read_only` 错误;URL 打开新标签运行命令属用户显式操作,不拦截;新分屏 cwd 由单测覆盖,MTP 无分屏接口):新分屏/标签继承目录;复制标签保留标记与分组;关闭其他/下方可重开;
  只读模式拦截 MTP/URL 输入;Recipes 名称校验与读写失败提示;GPU 初始化失败给出错误而非 panic。
  验收:对应纯函数单测 + 冒烟检查新分屏 cwd。
- [x] **B0.3 系统集成**(`0417c1b` 及后续;`open -a dist/mtty.app mtty://quick` 冷启动与运行中均实测生效;另修:复制 SSH 标签会重新连接;IME 候选窗定位待桌面人工确认):macOS URL Apple Event(浏览器/Finder 打开链接);恢复的 SSH 标签显示
  "已断开 — 回车重连"并可重连;IME 候选窗定位到光标。
  验收:`open mtty://quick` 在已安装应用上生效;重连有单测;IME 需桌面人工核对 **[所有者]**。
- [x] **B0.4 View 规则与入口**(`d52d898` 及后续;真实窗口截图核对图标、徽章、命令匹配与热加载;另修:重命名标签此前不生效、深链接 Focus 未切到 pane、符号链接目录不匹配):规则图标/徽章生效,支持按命令/host 匹配,`views.json` 热加载;
  Open Quickly 加入文件与 agent;details 面板剩余英文串国际化。验收:规则引擎单测、截图核对。
- [x] **B0.5 可自动验收的 UI**(标签栏导出每个标签的矩形作为测试目标;回放测试覆盖标签拖拽重排、分隔条拖动、重命名回车提交/Esc 取消、焦点文本框接收粘贴;回放即发现:8 个输入框按回车无效,已修复;编辑器保存由单测覆盖):语义化控件目标 + 无窗口事件回放,覆盖重命名/取消、标签重排、
  分隔条拖动、编辑器保存、剪贴板焦点。验收:新增回放测试在 CI 中运行。

### M2 Agent 工作台

- [x] **B2.1 启动与接入**(codex 片段按其 `hooks.json` 格式生成,需 `[features] hooks = true`;钩子脚本端到端测试覆盖 `--stdin` 取 session、pane 过滤、不阻塞;未改动用户真实的 agent 配置):设置与命令面板中"启动 agent"(codex/claude/opencode/miao,可选目录);
  接入片段带"复制"按钮;codex hook 片段与验证脚本。验收:启动命令单测;冒烟用假 agent 上报状态。
- [x] **B2.2 队列**(另修:"立即发送"不移出队列导致重复投递;状态按帧采样会漏掉快速的 processing→idle,改为 MTP 按序记录的状态变化;冒烟背靠背发送状态连续 8 次通过):队列项绑定目标 pane,agent 转为 idle 时自动投递,持久化到 `queue.json`(兼容旧格式)。
  验收:状态转换与投递单测(含重复事件不重复投递)。
- [x] **B2.3 命令边界**(按字节流捕获 C 与 D 之间的输出,不受回滚上限导致的行号平移影响;真实 zsh 端到端测试;冒烟经 MTP `pane.output` 读回):OSC 133(A/B/C/D)解析,zsh shim 发送;"复制/发送上一条命令输出"。
  验收:解析单测;冒烟中运行命令后取回其输出。
- [x] **B2.4 Worktree 任务**(临时仓库集成测试 4 项;任务窗口与新建对话框经 `MTTY_QA_COMMAND` 真实窗口截图核对):新建任务 = `git worktree add` + 分支 + agent pane;任务列表、diff 查看、
  合并或丢弃(破坏性操作需确认)。验收:在临时仓库上的集成测试。
- [x] **B2.5 注意力与 miao**(osascript 通知不支持点击回调,改为:通知后两分钟内激活 mtty 即跳到对应 pane;后台标签 `!`/✓/• 标记经真实窗口截图核对;miao 插件读取 `MTTY_*`,miao 仓库 `f596c592a`):通知点击跳转 pane、后台标签未读/完成标记;miao 插件读取 `MTTY_*`
  (miao 仓库,单独提交)。验收:状态转换单测;miao 插件测试。

### M3 远程运维

- [x] **B3.1 主机库**(一台真实 Linux 主机端到端:导入的别名经 `~/.ssh/config` 的 ProxyJump 连接,经 `mtty://host/<名称>` 打开并在远端执行命令核对;侧栏 HOSTS 截图核对):`hosts.toml`(名称、地址、用户、端口、分组、标签、跳板机)、从 `~/.ssh/config`
  导入、侧栏主机列表、命令面板搜索、双击连接。验收:解析/导入单测,冒烟连接本机 sshd 或容器 **[需测试主机]**。
- [x] **B3.2 安全连接**(交互式 ssh 由 ssh 自身确认主机密钥;主机库提供后台检查:已知/未知(显示指纹,核对后信任)/已变化(拒绝);真实主机验证"已知";密钥生成与 ssh-copy-id 在终端标签中进行,mtty 不经手口令):known_hosts 首次连接/指纹变化确认界面;ssh-agent 状态与密钥生成;不保存明文密码。
- [x] **B3.3 端口转发**:L/R/D 规则随主机保存;每条规则一个由 mtty 管理的 `ssh -N`(`ExitOnForwardFailure`),状态可见、随 mtty 退出而结束(不复用 ControlMaster:其 60 秒 ControlPersist 会让转发随之消失)。真实主机端到端:经本地端口读到远端 sshd 握手,停止后端口关闭。
- [x] **B3.4 SFTP**(用系统 OpenSSH sftp 批处理,保留 ssh 配置/跳板/agent/ControlMaster;真实主机往返测试;Linux Wayland 真实桌面截图核对;下载进度读取本地文件大小,上传为无进度百分比的忙碌状态):双栏文件浏览、上传下载、拖放、进度、重命名/权限;远端编辑复用 pane 连接。
- [x] **B3.5 Snippets 与广播**(片段在多台主机上各开一个标签执行 `ssh -t 主机 '命令'`,引号经 sh 实际拆分验证;广播输入已通过 Linux 实际键盘事件验收，两个 pane 的命令输出一致):命令片段库,在当前 pane 或多台主机执行;多 pane 广播输入。
- [x] **B3.6 FTP/FTPS 与持久会话**:FTP/FTPS 共用文件浏览(明文提示);可选 tmux/mosh 重连。(FTP 经系统 curl,口令仅在内存并经 stdin 传入,明文 FTP 与显式 FTPS 均对真实服务器做往返测试;FTP 支持整个文件夹的递归上传/下载(在进程内按深度优先遍历,因为 curl 自身无法递归)。tmux 经真实 ssh 验证 detach 后重连回到同一 shell;mosh 已对真实 mosh-server 验证 SSH 引导及 UDP 会话(见 ACCEPTANCE)。)
  验收(M3 整体):对测试主机的自动化冒烟;凭证不进日志、会话文件或 MTP 响应。

### M4 跨平台交付

- [x] **B4.1** Windows/Linux 真实桌面验收(IME、热键、拖放、菜单)**[所有者 + 真机]**。
  Linux,2026-10-01(Ubuntu 24.04、GNOME 46、Wayland,经 uinput 注入输入):键盘输入、菜单及菜单动作正常;拼音输入法
  的预编辑、候选框位置与上屏正常;Quick Terminal 快捷键可通过 GNOME 自定义快捷键执行 `mtty --quick` 实现(GNOME 46
  没有 GlobalShortcuts portal)。**发现并已修复:** 原生 Wayland 下拖放文件无反应(winit 0.30 只为 X11 实现了拖放),
  现由我们自己的 `wl_data_device` 处理;由于 GNOME 每个客户端只认一个 data device,剪贴板读取也改由它完成;应用快捷键
  原用 Super(被桌面占用),现改为以 Ctrl+Shift 为主(见 README 表格);终端里按 Tab 会让 egui 焦点跳到 File 菜单并吞掉
  后续输入。
  Windows,2026-10-01(Windows 11 22H2,在已登录会话中经 SendInput 注入输入):键盘输入、Tab 补全、菜单、微软拼音(预编辑、
  候选框位置、上屏)、文件拖放、剪贴板(Ctrl+Shift+C/V)与 Quick Terminal 热键均正常。**发现并已修复:** 启动即退出(egui
  没有 CJK 字体);切换输入语言后窗口永久卡死(wgpu 的 OpenGL 探测留下一个隐藏窗口从不处理消息的线程 —— 通过带符号的
  minidump 定位);输入到 cmd.exe 的所有 ssh 命令都失败(POSIX 引号、`clear;`、ControlMaster);config.toml、主机、片段与
  会话都被忽略(Windows 没有 `HOME`;现为 `%APPDATA%\mtty`);启动时多出控制台窗口;聚焦时仍按着的键(热键的 Y)被输入
  到 pane;拖放路径用了单引号。
- [ ] **B4.2** Apple 公证、Windows MSI 签名 **[所有者 + 凭证]**。
- [x] **B4.3** 自动更新:下载、`update-pubkey` 签名校验、替换安装。(内置发布公钥;Mac mini 上用打包 app + 测试密钥签名的发布包验证安装并重启,并验证篡改的包因校验和或签名被拒;已发布的 v0.0.5 包用仓库公钥验签通过。2026-10-04 已验证 Linux AppImage 签名下载、替换、重启、错误摘要/签名拒绝及缺失下载回退；Windows 结果见 ACCEPTANCE。)
- [x] **B4.4** bash / fish / PowerShell 的 shell 集成。(真实 PTY 端到端测试:macOS 上的 zsh、bash 3.2/5.3,Linux 上的 bash 5.2、fish 3.7、PowerShell 7.5;Windows 上的 PowerShell 由 CI 运行。Windows PowerShell 5.1 也记录历史:它没有 `ProcessStartInfo.ArgumentList`,故 shim 按 `CommandLineToArgvW` 的规则引用命令行。该路径已在真实 5.1 主机上核对(shim 可解析、引用能经 `CommandLineToArgvW` 正确还原、回退路径被选中且能启动进程);在应用内完整捕获 5.1 历史仍需人工核对。)
- [x] **B4.5** 可选的端到端加密同步,默认关闭。(ADR 0033:主机与片段经用户已在同步的文件夹传递,XChaCha20-Poly1305 加密,密钥不进入该文件夹,每台设备一个文件,按条目合并并记录删除。在 Mac mini 上用两个应用实例经共享文件夹完成汇合,文件夹中无明文。)

### M5 编辑器 pane(设计见 [ADR 0034](decisions/0034-editor-pane.zh-CN.md))

- [x] **E1** `mtty-editor` 内核:rope、事务、撤销、选区、搜索。(`crates/mtty-editor`:37 项单元测试,覆盖事务与位置映射、合并撤销与保存状态、多光标的输入/粘贴/删除、⌘D、缩进、字素簇与按词移动、搜索与正则替换、BOM/CRLF 往返。100 MB 文件上的性能门,Mac mini release 构建:打开 62 ms,在中间位置按键 0.4 µs,撤销 1000 次按键 0.3 ms,全文查找 120 ms,保存 3.6 ms。)
- [x] **E2** 编辑器 pane MVP:pane 类型、带样式文字段与裁剪、键盘、鼠标、输入法、剪贴板、打开/保存/关闭、会话恢复。(已完成:本地非 Markdown 文件在编辑器 pane 中打开;行号栏、当前行、选区、光标与超长行裁剪均经终端渲染器绘制;macOS 式键位,编辑器中 ⌘D/⇧⌘Z 优先;单击、Shift 单击、双击/三击、⌥ 单击加光标、拖选、滚轮;剪贴板与剪切;输入法上屏与候选窗定位;保留权限的原子保存;关闭、关闭其他/下方与退出时的未保存保护;会话恢复;MTP 类型、发送与关闭。证据:`editor_pane.rs` 中 9 项单元测试,100 MB 文件每屏 0.10 ms 的性能门,以及在 Mac mini 真实窗口中经 MTP 插入文本后的截图;已在桌面上人工核对键盘输入、输入法与 ⌘S,带样式文字段随 E3 一起提供。)
- [x] **E3** tree-sitter 高亮(80 种语言,更多语言由 Sublime 语法兜底)、大文件模式、远端文件。(已完成:内置 80 种 tree-sitter 语法,增量重新解析,只查询可见范围;没有语法认领的文件退回到 syntect 的 Sublime 语法,另从 bat 引入宽松许可的语法(Julia、nginx、VHDL、Org 等),许可见 `docs/third-party/SYNTAXES.md`;语言名显示在状态栏。依据:每种语法的查询编译、文件识别与高亮测试,所有 Sublime 语法在纯 Rust 正则引擎下都能解析的测试,以及 Julia 文件的真实窗口截图。Mac mini release 构建的性能门:1 MB 首次解析 64 ms,按键后重新解析 3.5 ms,高亮一屏 0.09 ms。大文件:超过 512 KB 时在后台线程解析(8 MB 文件:0.5 s 后显示颜色,每次按键 UI 线程耗时 0.009 ms,重新解析 127 ms 完成);语法树占用的内存约为文件的 25–35 倍,因此 tree-sitter 高亮止于 8 MB,Sublime 兜底止于 1 MB。大文件模式:超过 64 MB 的文件以只读查看模式打开,按行窗口从磁盘读取,后台建立稀疏行索引,文件多大内存都很小;查找在后台线程扫描文件。打字、粘贴或"切换为可编辑…"会提示加载为可编辑,并说明需要的内存(约为文件大小的 2.2 倍)。Mac mini release 构建,1 GB 文件:首屏 1 ms,建索引 0.3 s,读最后一屏 0.03 ms,全文查找 0.14 s(系统缓存已热);打包后的应用打开 1 GB 日志用时 0.44 s,内存只多约 70 MB。远端文件同样在编辑器 pane 中打开:字节在后台任务里经 ssh 读取,`⌘S` 以同样方式写回,若写入期间又发生编辑则 pane 保持已修改状态。远端 pane 同样会轮询文件:后台线程约每秒经 ssh 探测远端文件的长度与修改时间,若时间戳变化且 pane 没有未保存的编辑,则沿用本地外部变更的同一条路径就地重新加载;若变化发生时 pane 有未保存编辑,则静默保留本地文本,探测失败也只是静默无操作,不弹提示。它不启用语言服务器与查看模式,标题显示 `主机:文件名`;会话保存目标与路径,恢复时重新从主机读取。依据:编辑器 pane 中加载字节、写入与编辑竞争、以及恢复后由字节填充的单元测试,轮询判定仅在 pane 未修改且时间戳更新时重新加载的纯逻辑测试,以及远端重新加载会替换文本并清除脏标记的测试;ssh 路径本身即 B3.4 中在真实主机上验证过的同一路径。)
- [x] **E4** 多光标、查找替换、跳转到行、Markdown 预览 pane。(已完成:⇧⌘L 选中所有相同项(光标未选中文字时按全字匹配取当前词),⇧⌥I 在所选各行末尾放置光标,⌥⌘↑/⌥⌘↓ 添加光标(此前会被 ⌘↑/⌘↓ 抢走),其他平台为 Ctrl+Alt+↑/↓;编辑器中的查找增加区分大小写、全字匹配与正则,屏幕上的所有匹配都高亮,支持替换(随后跳到下一个匹配)、全部替换(一个撤销步骤)与选中全部匹配(⌥↩);跳转到行(⌃G,支持 `行:列`)。macOS 应用中,菜单栏不再从编辑器手里抢走 ⌘D、⇧⌘Z 与 ⇧⌘L。证据:经键位表回放按键的回放测试,内核中关于相同项、行尾光标与单次替换的测试,以及 Mac mini 上的真实窗口截图;性能门:100 MB 文件中选中全部相同项(150 万个光标)耗时 196 ms,每屏绘制 0.12 ms。Markdown 预览 pane:本地 Markdown 文件在编辑器中打开,右侧附带随输入更新的预览(命令面板与“视图”菜单中的“开关 Markdown 预览”可打开或关闭);预览随编辑器一起关闭,并保存在会话中;浮窗编辑器只保留给只读的 `app.view` 与远端文件。证据:预览的会话测试,以及 Mac mini 上的真实窗口截图(其中一张在经 MTP 插入文本之后)。并在打包应用中人工核对:视图菜单与命令面板可开关预览,且 ⌘D、⇧⌘Z、⇧⌘L 交给编辑器而非菜单栏。)
- [x] **E5** LSP:诊断、悬停、补全、跳转定义。(`crates/mtty-lsp`:JSON-RPC 分帧;在后台线程启动、初始化并读取服务器的客户端;UTF-8/UTF-16 位置与文件 URI;根目录探测;片段转文本。每个语言组与工作区根目录一个服务器,在登录 shell 的 PATH 中查找;全文同步,最大 2 MB。编辑器中:诊断下划线与状态栏中的 ✖/⚠ 计数,指针停留后显示悬停信息,补全在触发字符、输入单词与 Ctrl+Space 时打开、在客户端过滤、支持片段与 import 附加编辑,F12 或 ⌘ 单击跳转到定义,F8 跳到下一个问题;`config.toml` 中的 `[lsp]`。证据:`mtty-lsp` 中 15 项单元测试,以及对 rust-analyzer 1.94 与 typescript-language-server 5.3 的验收测试(诊断、悬停、跳转定义、补全、编辑后重新检查;rust-analyzer 冷启动后 10.4 s 给出首批诊断,TypeScript 0.4 s),补全排序与写入的单元测试,以及 Mac mini 上诊断下划线、悬停、补全与跳转定义的真实窗口截图。注意:rust-analyzer 需要 `rust-src` 组件才能补全标准库成员。)
- [x] **E6** pane 中的 vim 模式、折叠与大纲、外部修改重新加载。(已完成。外部修改重新加载:打开的 pane 记住文件长度与修改时间,约每秒重新检查一次;没有未保存修改时原地重载为一个可撤销事务,只替换有差异的片段并把光标映射过去,面板恢复为“未修改”;有未保存修改时询问是从磁盘重载还是保留本地版本;文件在面板下被删除时提示一次;超过 64 MB 的只读查看 pane 跳过。折叠:依据语法树(任何跨行的命名节点,按起始行取最外层),没有语法树时退回按缩进;行号栏显示 ▸/▾,折叠后的首行末尾显示 ⋯,上下移动跳过被隐藏的行,⌥⌘[ / ⌥⌘] 折叠/展开,命令面板提供“全部折叠/全部展开/切换折叠”。大纲:列出定义型节点(函数、类、结构体、枚举、trait/接口、impl、模块、常量、类型、变量、宏),取节点的 `name` 字段并按层级缩进;⌘R 打开可过滤的选择器。vim:作用于 `mtty-editor` 文档的状态机——NORMAL/INSERT/VISUAL/VISUAL LINE,计数,`h j k l w b e 0 ^ $ gg G`,`i a I A o O`,`x`,`d`/`c`/`y` 配合动作(`dd`/`cc`/`yy`、`dw`、`d$`),`p`/`P` 配内部寄存器,`u` 与 Ctrl-r,`J`;`/` 打开查找,`:` 运行 `w`/`q`/`wq`/行号;`za` 系列操作折叠;状态栏显示模式;由 `editor-vim` 开启。证据:内核中语法树/缩进折叠、大纲提取与 vim 命令的测试;编辑器 pane 的折叠、vim 普通/插入模式与外部重载测试;Mac mini 上的 `cargo test`/clippy。已在打包的 Linux 构建(Xvfb)上人工核对:vim 的 NORMAL/INSERT/VISUAL 与“全部折叠”生效,折叠后函数体收起、隐藏行被跳过。)

### M6 远程(连接,R1 见 [ADR 0037](decisions/0037-serial-telnet-tcp.md),R2 见 [ADR 0038](decisions/0038-ppk-import.md),R3 见 [ADR 0039](decisions/0039-ssh-stack.md))

- [x] **R1** 串口(波特率、校验、流控)与 Telnet 会话、原始 TCP;保存在主机库中。(ADR 0037。会话以字节管道(`Terminal::from_pipe`)在 pane 中打开,在后台线程拨号:裸 TCP 与内置 Telnet 编解码不新增依赖,串口使用 MIT 的 `serialport`(`default-features = false`,不含 libudev)。Shell 菜单与命令面板中有“新建串口/Telnet/TCP 会话…”,带配置表单;`hosts.toml` 增加 `kind`(默认 `ssh`,另有 `serial`/`telnet`/`tcp`)与 `[host.serial]`,侧栏、`mtty://host/<name>` 与命令面板都能打开这些类型。Telnet 与裸 TCP 标注为未加密,串口不标。恢复会话时重连,复制标签会重新拨号,连接断开即结束 pane;不适用 shell shim、OSC 7 工作目录与命令捕获。依据:管道终端与 Telnet 编解码单元测试、主机类型往返、transport target 的 JSON 测试,以及 macOS 与 Linux 上的工作区构建。)
- [x] **R2** 导入 `.ppk` 密钥(转为 OpenSSH 格式,不以明文保存)。(ADR 0038。不依赖 GPU 的 `mtty-keys` 解析 PPK v2(SHA-1 派生、HMAC-SHA-1)与 v3(Argon2id、HMAC-SHA-256),支持 Ed25519、RSA、ECDSA,先校验 MAC 再接触密钥,可解密 `none`/`aes256-cbc`,并经 `ssh-key` 以 bcrypt-pbkdf + aes256-ctr 和用户设置的口令重新加密。主机窗口中的 .ppk 密钥导入操作 读取文件后写出 `~/.ssh/<name>`(0600)与 `<name>.pub`,已存在时除非勾选“覆盖”否则拒绝;新口令为空会被拒绝,不存在未加密输出的路径。`chacha20-poly1305` 以明确错误拒绝(暂无向量)。依据:`puttygen` 0.81 生成的 12 个真实夹具——Ed25519/RSA/ECDSA × v2/v3 × 明文/加密——导入后与各自 `.pub` 比对,并有口令错误、文件被篡改与空口令的测试。)
- [x] **R3** 以 ADR 决定 SSH 是否从系统 OpenSSH 改为 Rust 原生实现(收益:Windows 无需 OpenSSH、进程内 SFTP 与转发;代价:重新实现 `~/.ssh/config`、ProxyJump 与 agent 转发)。(决定见 [ADR 0039](decisions/0039-ssh-stack.md):目前继续使用系统 OpenSSH;macOS、Linux 与 Windows 都已自带,若情况变化,ADR 0037 的传输层就是原生后端的接入点。无 OpenSSH 的平台、进程内 SFTP/转发或安全理由会触发重新评估。)

### M7 AI 原生工作台(设计见 [ADR 0040](decisions/0040-ai-native-workspace.md))

- [x] **A1** agent 的修改以编辑器事务的形式进入,行内显示为 diff,可接受或拒绝,也可撤销。(ADR 0040。MTP `editor.propose` 接受 `pane_id`/`path` 以及 `edits`(`{start,end,text}` 字符范围)或整文件 `text`;宿主把它们作为**一个事务**应用到编辑器 pane 的 `Document`(`apply_external`),把改动行标成绿色,并在状态栏显示。输入、保存或出现新修改即视为接受;*拒绝 Agent 修改* 是一步撤销回到原样。依据:编辑器 pane 的单事务应用/撤销、输入即接受与接受测试,以及该请求的 MTP 测试。)
- [x] **A2** ACP(Agent Client Protocol)客户端,任何 ACP agent 都能与编辑器 pane 协作。(ADR 0040。不依赖 GPU 的 `mtty-acp` crate 启动 agent,在其 stdio 上讲按行分隔的 JSON-RPC 2.0:`initialize`/`session/new`/`session/prompt`/`session/cancel`,应答 `fs/read_text_file`、`fs/write_text_file` 与 `session/request_permission`,并把 `session/update` 通知与响应交给 `Handler`。宿主可从 `config.toml` 的 `[acp]`(`[[acp.agent]]` 带 `name`/`command`)或通过 *ACP Agent…* 输入命令启动;ACP 窗口显示流式转写、发送 prompt、取消当前回合,把 agent diff 变成 A1 提案,并用“允许/拒绝”应答权限请求。依据:`mtty-acp` 对假 agent 的测试(initialize + 流式 update、fs 读取)、config 解析测试,以及工作区构建与测试。)
- [x] **A3** 一步把选区、诊断或命令输出交给 agent;agent 经 MTP 打开文件并定位到行。(ADR 0040。MTP `app.edit`/`app.view` 增加可选的 1 基 `line` 与 `column`;宿主打开文件并把编辑器 pane 移到该处。命令面板新增 *把选区发给 Agent*、*把诊断发给 Agent*、*把上一条命令的输出发给 Agent*:各自找到本标签的 agent pane,输入一段带围栏的简短 prompt 并回车。依据:`app.edit` 携带 `line`/`column` 的 MTP 测试,以及工作区构建与测试。)
- [x] **A4** Agent 会话恢复与配额显示([ADR 0042](decisions/0042-agent-resume-quota.zh-CN.md))。*恢复 Agent 会话…* 选择器列出 agent 经 MTP 上报的会话,并在其记录的 cwd 里重启所选会话;MTP/`mtty-cli` 新增 `agent.sessions` 与 `agent.resume`(新增 `agent.resume` 能力),hook 上报 `--cwd`,Agent 面板显示 agent 上报的配额行。依据:恢复模板的 token/转义测试、`agent.sessions` 排序与 `agent.resume` 入队的 MTP 测试、hook `--cwd` 测试,以及工作区测试与 clippy。

## 5. 不做的事

- 不做账号体系和强制登录;任何云能力都是可选的。
- 不内置 AI 模型调用;mtty 编排外部 agent,不替代它们。
- 不修改用户的 shell、agent、ssh 配置文件。
- 不在 UI 中保存明文密码。

## 6. 维护规则

- 功能状态变化时,在同一提交中更新 §3 的表格和 README(中英两份)。
- 路线图条目完成后打勾,并附验证证据(测试名、冒烟输出或截图说明)。
- 改变已锁定的设计决策时,在 `docs/decisions/` 新增 ADR。
