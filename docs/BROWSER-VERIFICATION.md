# Browser verification / 浏览器验证

**Browser Verification…** opens a local or forwarded service URL in the system
browser and runs an external automation command in a dedicated terminal.
Configure defaults in `config.toml`, or enter a command in the window:

```toml
browser-url = "http://127.0.0.1:3000"
browser-command = "playwright screenshot --wait-for-selector body ${url} ${screenshot}"
```

**Run Browser Verification** loads the defaults and starts the command. Placeholders
`${url}`, `${screenshot}` and `${project}` are passed as quoted shell fragments;
put them outside additional shell quotes. Each run receives a fresh PNG path in
the task's Git metadata or application evidence directory. Use your installed
browser automation tool; its command can include functional assertions before
writing the screenshot.

The result records the automation's actual exit code, URL, notes, PNG path and
validated dimensions. PNG data is decoded on a worker. A task check is considered
passed only when the automation exits zero and its PNG evidence is valid. Task
review associates the record with the checked diff revision and identifies older
results. The window can open the screenshot and send evidence context to Composer.

In an SSH workflow, select a forwarded local service URL from the Ports workflow
or enter its local endpoint. Browser automation executes in the local project
context. The screenshot and command records stay in task/application metadata.

中文：从“浏览器验证…”输入本地或转发后的服务 URL，可以打开系统浏览器，并在独立终端
运行已安装的外部自动化工具。命令支持 `${url}`、`${screenshot}`、`${project}`，参数自动
引用；“运行浏览器验证”使用默认配置直接执行。每次分配独立 PNG 文件，收集真实退出码、
地址、备注和经过解码校验的图片尺寸。任务检查同时要求退出码为零和 PNG 有效，证据关联
被检查的改动版本。结果可打开图片或发到 Composer。
