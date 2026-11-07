# mtty 架构规格

> English (default): [`ARCHITECTURE.md`](ARCHITECTURE.md)

> 跨平台(macOS / Linux / Windows)终端**引擎** + `mtty` 应用的整体架构。
> 本文是**实现前的定稿设计**;所有"锁定"条目即为决策,变更须走 `docs/decisions/` 的 ADR。

## 0. 范围与一句话

一句话:**`portable-pty` + `alacritty_terminal` + `vte` 做内核,`winit` + `wgpu` 做窗口与绘制,终端网格自绘、周边 UI 用 egui;引擎(`mtty-*`)与应用(`mtty-app`)分离,控制面(MTP)与引擎解耦。**

## 1. 目标 · 非目标 · 约束

**目标**
- 三平台同一套代码,含 **Windows(ConPTY)**。
- 热路径性能对齐 Alacritty 量级:输入延迟 P95 ≤ 16ms(目标 ≤ 8ms)、首帧 ≤ 100ms、滚动不丢帧、空闲 CPU ≈ 0。
- 引擎可被第三方嵌入;`mtty-app` 是第一个消费者(仓库里唯一的示例是
  `crates/mtty-render/examples/pipeline_probe.rs`,一个渲染管线探针)。
- 复用现有控制面:`mtp` 类型、`mtty-cli`、插件、agent/shell hooks。

**非目标(现阶段)**
- 复刻 Ghostty 的渲染精细度与配置生态;macOS 专属集成(AppleScript/Sparkle)。
- 插件市场/"框架"级扩展;跨机同步。

**约束**
- 依赖只用宽松许可(Apache-2.0/MIT/BSD/ISC);**引擎内禁止 GPL/AGPL**(如 Pebrel 只能读、不能抄)。
- 每增加一个外部 crate 都要过性能与许可审查。
- 对外许可:**引擎与应用均为 Apache-2.0**。
- Rust stable,MSRV 见 `rust-toolchain.toml`。

## 2. 锁定决策(摘要)

| # | 决策 | 理由 |
|---|------|------|
| D1 | 内核用 `alacritty_terminal` + `vte` + `portable-pty` | 最成熟、跨平台(含 ConPTY)、许可宽松 |
| D2 | 渲染自绘于 `wgpu`(字形用 `glyphon`/`cosmic-text`/`swash`) | 性能可控;跨 Metal/Vulkan/DX12 |
| D3 | 周边 UI(面板/设置)用 `egui`,与终端**共用同一帧** | 开发速度 + 终端走自绘,两全 |
| D4 | **自研 tab/split 模型**(非 OS 原生标签) | 跨平台一致;可控 |
| D5 | 并发用 **Alacritty 同款的锁纪律**(`FairMutex<Term>` + `EventListener`) | 已验证,避免自造快照协议 |
| D6 | 平台差异只出现在 `core::pty` / `widget::platform` / `mtp::transport` | 收敛复杂度 |
| D7 | 控制面 `mtty-mtp` 与引擎解耦(Unix socket / Windows named pipe) | 引擎崩不拖垮 CLI;复用现有协议 |
| D8 | 先做 app、后抽库;扩展点分阶段 | 由真实需求驱动 API |
| D9 | 引擎 crate 采用 `Apache-2.0` | 宽松、便于被嵌 |
| D10 | 单一原生 `mtty` 主程序调用 `mtty-widget`（winit + wgpu），旧 eframe 宿主退役 | 统一产品身份与直接绘制；见 APP-IDENTITY.zh-CN.md |

## 3. 依赖分层(DAG)与规则

```
   mtty-graphics    mtty-config    mtty-mtp     (叶子 crate,无引擎依赖)
        │                │            │
        ▼                │            │
   mtty-core             │            │
        │                │            │
        ▼                │            │
   mtty-render           │            │
        │                │            │
        ▼                ▼            │
   mtty-ui ◄─────────────┘            │
        │                             │
        ▼                             │
   mtty-widget ◄──────────────────────┘   (原生宿主:winit + wgpu)
        ▲
        └── mtty-app   (原生主程序;依赖 mtty-widget)
   mtty-cli ──► mtty-mtp
```

图里没有画全所有边:`mtty-widget` 还依赖 `mtty-core`/`mtty-render`,`mtty-app` 调用 `mtty-widget`。
工作区共九个成员。

**规则**
- 依赖只能"由外向内":`widget → render → core`。唯一的例外是
  `core → graphics`(内联图形扫描/解码器);`core` 仍不依赖任何
  GPU/窗口/egui crate。
- `core` 不依赖 `wgpu`/`winit`/`egui`(**可无 GPU 编译**);`render` 不依赖 `winit`。
- `config`/`mtp` 不依赖渲染与窗口。
- 应用层依赖引擎;引擎**绝不**依赖应用层。
- CI 编译整个工作区(`cargo check --workspace`),并运行 `cargo fmt --check` 与
  `cargo clippy --workspace --all-targets -- -D warnings`;没有 `cargo-deny` job,也没有
  `deny.toml`。

## 4. crate / 模块职责矩阵

| crate | 职责(做什么) | 明确不做 |
|-------|--------------|----------|
| `mtty-graphics` | 内联图形扫描器 + Sixel/Kitty/iTerm2 解码器 | 不碰渲染/GPU/窗口 |
| `mtty-core` | PTY、vte 解析、网格/回滚/光标/模式、选区/查找、OSC/CSI 语义、键鼠→字节编码、事件;持有 `mtty-graphics` 的扫描器 | 不碰 GPU/窗口/配置/业务 |
| `mtty-render` | 字形加载/shaping/图集、网格实例化、绘制 pass、damage 增量 | 不管事件循环/输入 |
| `mtty-ui` | 无宿主 UI:主题、输入编码、选区、分屏布局、egui chrome、调色板、hints、vim、markdown、ssh、update、agent 集成 | 不含窗口/事件循环 |
| `mtty-widget` | 原生 host 库（主程序由 mtty-app 提供）:winit 事件循环、wgpu surface、输入/IME/剪贴板/拖放、直接自绘网格(ADR 0030) | 不重复实现 PTY/parser |
| `mtty-editor` | 编辑器 pane 的编辑内核:rope、事务与撤销、多选区、光标移动、查找替换(ADR 0034) | 不依赖 UI 与 GPU |
| `mtty-config` | 配置模型、主题、ghostty/alacritty 导入 | 不依赖 UI |
| `mtty-mtp` | 协议信封、传输、server/client、agent/history 注册表、revision + `core.wait` 长轮询 | 不依赖引擎 |
| `mtty-app` | 原生 `mtty` 入口、命令 help/version 与平台安装包元数据 | 不重复实现终端内核 |
| `mtty-cli` | 供脚本/agent 使用的 MTP 客户端 | 不依赖引擎 |

## 5. 核心类型与 trait(设计草图 —— 未采用)

初稿里画的这些 trait/类型(`Damage`、`TermEvent`、`EventSink`、`Pty`、`InputEncoder`、
`GlyphAtlas`、`Renderer`、`Host`)**并未采用**。实际交付的 API 是:

- `mtty-core`:`Terminal`(PTY + 解析器 + 网格)与 `ATerm`(`alacritty_terminal` 屏幕模型),
  位于 `crates/mtty-core`。
- `mtty-render`:`TermRenderer`、`QuadRenderer`、`ImageRenderer`。
- 应用：`mtty-app` 启动 `mtty-widget`，由后者管理事件循环，
  经 `mtty-render` 直接绘制网格并合成 egui 外壳。

## 6. 线程模型与锁纪律

采用 Alacritty 验证过的模型(`FairMutex<Term>` + `EventListener`),不自定义快照协议。

```
┌─ PTY 读线程 ───────────────────────────┐     ┌─ 主线程(winit 事件循环) ─────────────┐
│ loop read():                           │     │ winit event:                          │
│   lock(term) { vte.process(chunk) }    │     │   Keyboard/IME → core::input → pty    │
│   标 Damage; sink.send(Wakeup)         │     │   RedrawRequested:                    │
│   （锁仅覆盖解析,不含 I/O 与 GPU）      │     │     lock(term){ renderer.update }     │
└────────────────────────────────────────┘     │     renderer.render(frame); present   │
                                               └───────────────────────────────────────┘
```

**不变量**
1. `Term` 的锁**只**覆盖"解析一段字节"或"构建渲染实例",**绝不**跨越 `read()`/`write()`/GPU submit。
2. PTY 读线程**独占** reader;写走独立句柄(`Pty::write`),避免读写互相阻塞(Windows ConPTY 尤其)。
3. 空闲时无轮询;只有 `Wakeup`/输入/定时器(光标眨眼)才请求重绘。
4. resize:主线程算 `cols/rows` → `Pty::resize`;`Term` 同步 resize。
5. 退出码以 **OSC 133;D** 为准(ConPTY/包装进程下进程码不可信)。

## 7. 数据流

- **输出**:PTY → 读线程 → `vte` → `Term`(+OSC→`TermEvent`)→ `Wakeup` → 主线程渲染。
- **输入**:winit 键/鼠/IME → `core::input` 编码 → `Pty::write`;选区/粘贴走 bracketed paste 模式。
- **控制**:`mtty-mtp` server 独立线程;进程内 UI 直连注册表,外部 CLI/插件走 socket/pipe。
- **元数据**:cwd(OSC 7)、标题、agent 状态、命令历史 → 事件/注册表 → 面板订阅。

## 8. 渲染架构(mtty-render)

1. **字形**:`cosmic-text` 解析/回退,`swash` 光栅化 → **R8 图集**(shelf packing,LRU;键 `(glyph_id, style, px, subpixel)`).
2. **实例化**:每个 Cell → quads(bg / glyph / underline / strike / cursor / selection);实例缓冲按脏行增量更新。
3. **Pass**:`bg`(纯色/主题)→ `glyph`(图集采样)→ `cursor/decoration`;透明度/背景图后置。
4. **Present**:优先 `Mailbox`(低延迟),掉帧回退 `Fifo`;光标杆眨眼用定时器。
5. **与 egui 组合**:egui 驱动整帧,终端通过 `egui-wgpu` 的 **`PaintCallback`** 在自己的矩形内绘制(独立 pipeline,复用同一 `wgpu::Device/Queue/Surface`)。
6. **优化顺序**:全量重建 → 脏行 → 图集命中 → 去 per-frame 分配;用 `tracy`/`puffin` 标注热点。

## 9. 终端模型(mtty-core)

- 网格:Cell{char + 组合 + fg/bg/attrs + underline 样式};回滚环形缓冲;reflow(宽窗口)。
- 模式:应用光标键、bracketed paste、鼠标上报、alternate screen、kitty keyboard(CSI u)、焦点上报。
- 语义:标题、超链接(OSC 8)、cwd(OSC 7)、进度(OSC 9;4)、shell 集成标记(OSC 133 A/B/C/D)。
- 选区/查找:行/块选区、词边界(CJK/grapheme 用 `unicode-width` + grapheme 边界)、搜索高亮。
- 图形协议(kitty graphics / sixel / iTerm2)已由 `mtty-graphics` 实现,
  经 `mtty-core`/`mtty-render` 接入,默认开启(`graphics = true`)。

## 10. 平台抽象层

| 关注点 | 抽象 | macOS/Linux | Windows |
|--------|------|-------------|---------|
| PTY | `trait Pty` | `forkpty` | **ConPTY** |
| 传输 | `mtp::transport` | Unix socket | `\\.\pipe\mtty` |
| 剪贴板 | `trait Clipboard` | NSPasteboard / X11-Wayland | Win32 clipboard |
| 字体 | `mtty-render::font` | CoreText / fontconfig | DirectWrite(`font-kit`) |
| IME | `widget::input` | 原生 | **TSF**(风险最高,见 §19) |

`#[cfg(...)]` **只允许**出现在上表对应模块内;其余代码保持平台无关。

## 11. 配置 / 主题

- `mtty-config`:自有 TOML;键名对齐 ghostty/alacritty 以便导入。
- 默认观感为 Nord(背景 `#2e3440`、字号 13),可覆盖。
- 内置主题:`nord`(默认)、`dracula`、`gruvbox`/`gruvbox-dark`、
  `solarized`/`solarized-dark`、`tokyo-night`/`tokyonight`;并支持自定义调色板。
  背景透明度已接入,背景图后置。

## 12. 控制面(MTP)

- `mtty-mtp` 实现 server;本地传输:Unix socket(`$XDG_RUNTIME_DIR/mtty.sock`,回退到
  `$TMPDIR/mtty.sock`)或 Windows named pipe。
- 远程访问:`remote-listen = addr:port` 会额外用 TCP 提供控制面;它**要求**设置
  `MTTY_MTP_TOKEN`,客户端每个请求都要带上该令牌。
- 复用现有 `mtp` 报文与 `mtty-cli`;**进程内 UI 直连注册表**,外部走 socket/pipe。
- 方法面:`core.ping/health/wait/subscribe`、`agent.state.*`、`history.*`、
  `pane.list/send/run/focus/close`、`app.view/edit`(在查看器/编辑器中打开文件)、
  `file.read/write`(offset/length、base64、上限 2 MB;可选 `MTTY_MTP_TOKEN` 令牌)。
- 变更通知有两种:**revision 计数**配 `core.wait` 长轮询(一旦 revision 超过调用方给的值即返回),
  以及 `core.subscribe` 的**服务端推送**——把连接升级为 `{"kind":"event", …}` 行流,覆盖
  `agent.state`、`panes`、`history` 三个 topic(`mtty-cli events`)。不发送 `cwd.changed`。
- 传输:`interprocess`(本地 socket / named pipe);`remote-listen` 走 TCP。

## 13. 应用层(mtty-app)

- 模型:`Window → Tab[] → SplitTree<Surface>`;surface = 一个终端实例(core+render 视图)。
- 窗口:一个 OS 窗口承载一个 tab 集;分屏是 tab 内的树(自研,非 OS 标签)。
- 面板(egui):**左 Tabs 侧栏**(标题/⌘N/标记/分隔线/右键菜单)、**右 Details**(Info/Outline/Git/Files)、徽章、设置、命令面板。
- 统一外框观感:面板与终端同背景、无分隔线、hover 高亮。
- 系统集成:hook 安装、URL 打开、通知、防休眠(平台分支)。

## 14. 扩展点(分阶段,别提前)

- **现在**:引擎 API(`Terminal`/`ATerm`、`mtty-render`)+ MTP(够用)。
- **R4+**:MTP `provider.*`(Details 自定义组件,kind=tui/web)。
- **以后(真有人要)**:渲染器/面板插件、主题包 —— 才考虑"框架"化。

## 15. 错误处理与安全

- 库用 `thiserror`、应用用 `anyhow`;**热路径不 panic**;可恢复错误转为事件/日志。
- 安全:粘贴确认、OSC 注入面、URL scheme 白名单、剪贴板策略、可选 secure input。
- 崩溃:捕获并写日志(panic hook),尽量保 PTY/子进程清理。

## 16. 测试 / CI / 性能门

- **一致性**:尚未接入 `vttest`/`esctest`/`cargo-fuzz`;解析行为由各 crate 自身的单元测试覆盖。
- **集成**:真起 shell,喂字节序列,断言网格/事件。
- **性能门(仅 ubuntu-latest)**:`cargo test --release -p mtty-core -p mtty-app -- --ignored`
  加 `scripts/check-perf-baseline.py`。预算:输入延迟 P95 ≤ 16ms、首帧 ≤ 100ms、
  `cat` 大文件不丢帧、空闲 CPU ≈ 0。
- **CI**(`.github/workflows/ci.yml`):jobs 为 `changes`、`privacy`、`lint`、`check-linux`(`cargo check --workspace`)、
  `test-linux`、`macos`、`windows`、`render-linux`、`perf`。`lint`
  (`cargo fmt --check` 与 `cargo clippy --workspace --all-targets -- -D warnings`)和 `check-linux`
  在 push 时跑(macOS 只做编译检查);test/perf/render/windows 在 PR、夜间 `17 4 * * *` 定时和
  `workflow_dispatch` 时跑。
- **渲染**:`render-linux` 装 Mesa lavapipe(软件 Vulkan),以
  `MIAO_REQUIRE_GPU=1 WGPU_BACKEND=vulkan` 跑无头冒烟测试。
- 没有 `cargo-deny` job,也没有 `deny.toml`。

## 17. 打包 / 发布 / 版本

- 引擎 crate `Apache-2.0`,先内部用;**尚未发布 crates.io**(API 未稳定)。
- 应用产物(`.github/workflows/release.yml`):macOS `.app` zip;Linux tar.gz + `.AppImage` + `.deb`;
  Windows zip + MSI(WiX,走 `cargo-wix`)。
- 签名均为可选:macOS `codesign`/`notarize`,Windows `signtool` 配可选 PFX,外加可选的
  产物 `minisign` 签名。
- `wgpu` DX12 需随包 `dxcompiler.dll` 或静态 `static-dxc`。

## 18. 里程碑

R0 最小闭环(pty→vt→grid→render→input,量延迟) → **R0.5 IME 专项** → R1 可用终端 → R2 三平台+打包 → R3 `mtty-mtp` → R4 面板 → R5 打磨/签名。每阶段退出须过对应性能门。

## 19. 风险

| 风险 | 缓解 |
|------|------|
| 自绘渲染达不到 Alacritty 级 | glyphon 起步→damage/vsync/图集优化;参考 Alacritty/Rio |
| **IME/CJK(Windows 最险)** | R0.5 专项;winit `Ime` + `set_ime_cursor_area`;字宽/grapheme 测试集 |
| ConPTY 性能/退出码 | 读写并发 + OSC 133;D + 独立基准 |
| egui 与自绘共帧的兼容/性能 | `PaintCallback` 方式;必要时退化为手动 viewport |
| 早期过度抽象 | 引擎先"够用";扩展点分阶段 |
| 上游 `alacritty_terminal` API 变动 | pin 版本;封装 `mtty-core` 适配层 |

## 20. 性能影响分析(相对 Ghostty / Alacritty)

**结论:选型本身没有根本性性能损失**(内核就是 Alacritty 那套,渲染 GPU 化)。风险集中在四处:
① egui 立即模式若不加门控会每帧空转;② 自绘渲染器初期不如 Ghostty 多年打磨的 Metal compute;
③ wgpu 抽象 + macOS present 模式;④ Windows ConPTY 的平台成本(任何方案都有)。

| 维度 | 对比基线 | 影响 | 缓解 |
|------|----------|------|------|
| 输入延迟 | Alacritty 同档 | 无本质损失 | 事件驱动(无轮询)+ 优先 Mailbox/Immediate;R0 硬门控 |
| 输出吞吐 | Alacritty 同档 | **锁竞争**是主要风险 | 分段解析、短临界区;必要时渲染读快照 |
| 渲染 | vs Ghostty Metal | 初期弱(图集/pass 不极致) | glyphon 起步 → 自绘 damage/持久实例缓冲 → 必要时 compute |
| **egui 共帧** | 新增开销 | 立即模式可能每帧重建 UI | **仅 chrome 脏/需要时跑 egui**;终端由 damage 驱动重绘;禁用持续 repaint |
| wgpu 抽象 | vs 直接 Metal | 极小(同一 Metal 后端) | 保留专用路径可能;实测对比 |
| present/vsync | macOS 模式受限 | 若被迫 Fifo 会增延迟 | 实测 Mailbox/Immediate,动态选择 |
| 字体/首帧 | — | 加载 + shaping 拖慢首帧 | 并行初始化 + 磁盘缓存 |
| 内存 | parity | 图集/实例缓冲 | LRU 图集、复用缓冲、按图集脏区上传 |
| 空闲 CPU | parity(≈0) | 眨眼/动画导致非零 | **仅焦点时眨眼**;无定时轮询 |
| Windows | ConPTY 平台成本 | 固有变慢(ConHost+VT 重编码) | 读写并发;Windows 单列基线 |
| macOS 观感 | 丢子像素/emoji/模糊 | **观感**损失,非性能 | 后置专项 |

**红线**:R0 未过"输入延迟 P95 ≤ 16ms / 大文件不丢帧 / 空闲 CPU≈0"三项,不进入后续里程碑;
egui 每帧空转、present 模式、图集上传是三大重点实测项。

## 21. ADR(`docs/decisions/`)

完整索引:[`docs/decisions/README.zh-CN.md`](decisions/README.zh-CN.md)(ADR 0001–0030)。
