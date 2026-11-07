# 统一应用：mtty

[English](APP-IDENTITY.md)

## 名称与实现

应用名 **mtty**(官网 `mtty.dev`),主命令 `mtty`,配套 CLI 为 `mtty-cli`。仓库与可嵌入终端引擎都叫
**mtty**。v0.0.5 及之前应用名为 **miaotty**;更名及兼容规则见
[ADR 0032](decisions/0032-rename-mtty.zh-CN.md)。

`mtty-app/src/main.rs` 直接启动 `mtty-widget` 原生实现;旧 eframe 应用和独立的
`miaotty-native` 二进制已退役(取代了 ADR 0030/0031 的双 host 阶段)。`mtty --version` 输出
`mtty <版本> (native)`,让校验可以确认实际实现,而非根据文件名猜测。

每份正式发布产物只包含一个 GUI 应用及其 CLI:

- macOS:一个 `mtty.app`,执行文件 `mtty`,bundle ID `dev.mtty.terminal`。
- Linux:tar/DEB/AppImage 中为 `mtty` 和 `mtty-cli`(DEB 另装 `miaotty` / `miaotty-cli`
  兼容别名,并替换旧的 `miaotty` 包)。
- Windows:zip/MSI 中为 `mtty.exe` 和 `mtty-cli.exe`(MSI 原地升级已安装的 miaotty)。

四个 runner 的构建和打包仍是发布门禁,本地 macOS 校验不替代 Linux/Windows 门禁。

## 从 miaotty 升级

| 方面 | 行为 |
|---|---|
| 配置目录 | 首次启动时,若 `$XDG_CONFIG_HOME/mtty` 不存在,则复制 `$XDG_CONFIG_HOME/miaotty`。旧目录保留,旧版本仍可使用。 |
| 环境变量 | 先读 `MTTY_*`,再读 `MIAOTTY_*`。pane 中两套都导出,`MTTY_CLI` / `MIAOTTY_CLI` 指向包内 CLI。 |
| Agent hook、miao | 已安装的 hook 脚本与 miao 读取 `MIAOTTY_PANE_ID` / `MIAOTTY_CLI`,这些变量仍会导出,无需修改即可继续上报状态;新安装的 hook 使用 `MTTY_*`。 |
| CLI 与 socket | socket 为 `mtty.sock`,并为旧客户端建立 `miaotty.sock` 链接;`mtty-cli` 找不到新 socket 时回退到旧版宿主的 socket 或命名管道。 |
| 链接 | `mtty://` 与 `miaotty://` 都会打开 mtty。 |
| macOS | `scripts/install-macos.sh` 按 bundle 身份归档 `miaotty.app` 并安装 `mtty.app`;bundle ID 变化后,系统会重新询问通知权限。 |

## 保存的状态

`session.json`、`queue.json`、`window` 位于配置目录。读取优先级:

1. 配置目录中的状态文件。
2. 原 native 的 `native-session.json`、`native-queue.json`、`native-window`。
3. 没有 native 会话时,读取已退役 eframe 应用的 `$XDG_DATA_HOME/miaotty/session.json`
   (默认 `~/.local/share/miaotty/session.json`)。

旧文件保留。eframe 的索引式布局和焦点转换为 native pane ID,保留分屏方向/比例、cwd、标题、
标记、分组、活动标签和最近文件;Recipes 也经过同样转换。恢复的是布局并创建新 shell,
不是保住仍在运行的进程。

## 构建、安装与核验

```sh
cargo build --release -p mtty-app -p mtty-cli
scripts/package-macos.sh
python3 scripts/check-macos-bundle.py dist/mtty.app
python3 scripts/smoke-hosts.py --bundle dist/mtty.app
bash scripts/install-macos.sh
```

桌面冒烟检查的是**实际打包的主程序和包内 CLI**:复制旧 miaotty 配置目录、分屏会话迁移、
pane 聚焦/关闭、真实 shell 命令、pane 中两套环境变量、更名前的 hook 脚本、旧 socket 路径、
文件读写、历史、Agent 状态、view/edit 派发及新鲜 GPU 截图。
`MTTY_SHOT_AFTER=<秒数>` 写 `/tmp/mtty_shot.ppm` 并退出。

`UI-AUDIT.zh-CN.md` 中的双 host 审计属于历史证据。应用保留 native 已有的版本检查界面;
旧 eframe 的自动下载/安装界面不属于当前应用功能。
