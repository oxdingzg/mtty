# SSH language servers / SSH 远程语言服务器

Configure the SSH target used by the remote editor in `config.toml`:

```toml
[lsp.remote."<ssh-target>"]
# Optional absolute remote workspace root. Omit to discover language root markers.
root = "/project"

[lsp.remote."<ssh-target>".rust]
command = ["rust-analyzer"]

[lsp.remote."<ssh-target>".python]
command = ["pyright-langserver", "--stdio"]
```

The target is an SSH alias or `[user@]host[:port]`, matching the remote editor's
host. SSH authentication, proxy settings and connection reuse follow the same
connection arguments as remote file operations. Language servers execute on that
host and operate on its filesystem; their executable/arguments can be overridden
per language. The remote command adds the standard user Cargo/local bin directories
to PATH. An explicit `~/bin/server` executable also expands the remote home prefix.

Opening a supported remote editor resolves its canonical path and workspace
markers once on a worker, then initializes a server for that target/language/root.
Diagnostics, hover, completions and definition jumps use target-scoped document
identities, so equal paths on different hosts or the local machine remain distinct.
Remote definitions open in the embedded remote editor and retain dirty buffers.
Position encodings follow the server's UTF-8/UTF-16 negotiation.

The Info panel shows remote path resolution, startup/ready state or an error.
**Restart Language Servers** reloads the LSP configuration, clears transient
popups/diagnostics and reconnects open documents. A closed document sends
`didClose`, successful saves send `didSave`, and failed servers clear stale
requests/diagnostics. Full-text changes coalesce while transport output is busy.
Remote initialization uses a null process ID and remote file URIs.

## 中文说明

在 `[lsp.remote."<ssh-target>"]` 配置远程目标；名称与远程编辑器的主机一致。
`root` 可指定远程绝对工作区目录，省略时通过语言的根标记发现。Rust、Python 等语言的
`command` 在对应远端执行；认证、代理和连接复用沿用远程文件连接配置。

打开远程文件后，在后台解析规范路径并连接语言服务器。诊断、悬停、补全、定义跳转均按
目标主机隔离；同名本地文件与其他主机文件独立。Info 显示解析、启动、就绪或失败状态。
命令面板“重启语言服务器”重新加载配置，并同步已打开文档。
