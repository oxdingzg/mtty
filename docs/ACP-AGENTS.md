# ACP agents / ACP Agent 接入

Open **ACP Agent…** to inspect configured profiles and local adapters. Discovery
checks PATH (including a profile's PATH override and Windows executable suffixes)
on a worker thread. The launcher supplies copyable TOML and installation commands
for `claude-agent-acp`, `codex-acp` and the built-in Gemini ACP entry point.

Adapters are launched as an executable plus argument vector. For paths or
arguments containing spaces, enter a JSON array in the command field:

```json
["/path with space/adapter", "--acp", "argument with spaces"]
```

A persistent profile uses the same argument-vector form in `config.toml`:

```toml
[[acp.agent]]
name = "Codex ACP"
command = ["codex-acp"]
```

`Start Configured ACP Agent` starts the first configured profile. The launcher
shows an absolute local working directory and an optional session ID; clearing
that field starts a new conversation. Reconnect uses `session/load` when the
agent advertises that capability. Startup and transport failures appear in the
session's diagnostics, including bounded adapter stderr and process exit status.
Diagnostics remain local to the running application window.

## Negotiated controls

The client negotiates ACP protocol version 1. It displays the identity, version,
resume support and prompt capabilities returned by `initialize`. Selectors are
built from the session's advertised `configOptions` (models, modes, reasoning
levels or custom select options), with grouped choices and full-state updates.
Legacy `modes` are used when `configOptions` are absent. Configuration values are
confirmed by the agent's response or notification. Prompt submission waits for
the previous prompt/configuration request to finish; cancellation keeps the turn
pending until its response or transport closure.

The process and callback generation are scoped together: late messages from an
old adapter cannot alter the next adapter's session or file-review state. Closing
or losing a connection releases pending review replies and managed terminals.

## 中文使用说明

从命令面板 **ACP Agent…** 打开启动器，可查看配置项及本机找到的适配器，复制配置模板
和安装命令。命令支持 JSON 参数数组，可正确保留路径和参数中的空格。选择本地绝对工作目录，
按需填写恢复会话 ID，再启动；模型、模式、推理级别等选择器来自 Agent 实际返回的配置。

会话窗口显示协商能力、会话 ID、诊断和断线状态。需要恢复时使用支持 `session/load` 的 Agent；
不支持恢复的重启操作会明确标为新会话。适配器的 stderr 以有界文本展示，协议错误与退出状态
也会出现在诊断中。文件改动沿用编辑器的提案审阅流程。

配置可使用 `[acp.agent.env]` 私有环境变量及 `auth-method`。复制的配置模板包含名称和命令，
启动器中的认证方式则按 Agent 握手返回的 `authMethods` 提供。

## Native workflow QA

The screenshot driver accepts timed palette actions:

```text
MTTY_QA_COMMANDS=2:Start Configured ACP Agent;6:Accept Agent Edit
```

Times are seconds after startup. Use isolated HOME/config/data/runtime paths and
a fake local adapter for deterministic file-review and reconnect tests. The
existing `MTTY_QA_COMMAND` and `MTTY_QA_AFTER` flags continue to work.

Protocol references: [ACP v1 config options](https://agentclientprotocol.com/protocol/v1/session-config-options),
[adapter registry](https://agentclientprotocol.com/).
