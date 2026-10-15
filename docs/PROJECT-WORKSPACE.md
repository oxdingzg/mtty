# Project workspaces / 项目工作区

`Project Workspace…` / `项目工作区…` reads `.mtty/workspace.json` from the
current local project or one of its parent directories, stopping at the Git
root. `Create portable example` creates a starting file, and `Edit workspace.json`
opens it in the embedded editor.

`Start Project Workspace` / `启动项目工作区` opens the configured `default_recipe`
with its default parameters. The workspace window lets you override parameters,
open a named recipe, or run an individual task. Opening a project recipe appends
its tabs to your current workspace and starts the tasks listed by that recipe.

```json
{
  "version": 1,
  "default_recipe": "work",
  "parameters": { "port": "3000" },
  "recipes": [{
    "name": "work",
    "session": {
      "active_tab": 0,
      "tabs": [{
        "title": "Project",
        "title_set": true,
        "panes": [{ "id": "shell", "cwd": "${project}" }],
        "active": "shell",
        "layout": { "leaf": "shell" }
      }]
    },
    "tasks": ["status"]
  }],
  "tasks": [{
    "name": "status",
    "command": "git status --short",
    "cwd": "${project}"
  }]
}
```

## Parameters and commands / 参数与命令

- `${project}` is the detected local project directory. `${branch}` defaults to
  the current Git branch; other parameters come from `parameters` and the form.
- Paths and layout strings expand parameters literally. Command substitutions
  are quoted for the task's shell. Put `${port}` directly in a command rather
  than inside additional shell quotes: `npm run dev -- --port ${port}`.
- Task names are unique. A recipe's `tasks` lists the tasks to start after its
  layout. `session` is optional for recipes that only start commands.
- A task's optional `host` expands to an SSH alias or `[user@]host[:port]`.
  Its `cwd` is then a remote path (default `~`), and its command uses a POSIX
  shell over SSH. SSH uses your existing authentication and connection config.
- Layouts use the same `tabs`, `panes`, `editors`, `previews` and split-tree data
  as saved recipes. Each pane needs a unique string `id` within its tab; `active`
  names that tab's active pane. Runtime pane ids are generated for each opening.

`${project}` 是本地项目根目录，`${branch}` 默认取当前 Git 分支，其他参数来自
`parameters` 和界面输入。路径按字面值展开，命令参数由对应 shell 自动引用；命令中的
`${port}` 直接作为参数使用，例如 `npm run dev -- --port ${port}`。

任务的 `host` 可以填写 SSH 别名或 `[user@]host[:port]`，此时 `cwd` 表示远程目录，
默认 `~`，通过 SSH 上的 POSIX shell 执行命令。项目配置可以用参数描述主机和端口。

## Task lifecycle / 任务生命周期

Each task has its own terminal, working directory and resolved parameters.
The workspace shows starting/running/exit status, captured output, and controls
to open its terminal, interrupt it with Ctrl+C, or run it again using the recorded
parameters. Long-running development servers should stay in the foreground.
Exit status is captured through the shell integration. Closed terminals are
shown as stopped; completed task output remains available in the workspace.
The terminal view and copied/exported command history provide the full captured
output when the workspace preview is truncated.

每个任务使用独立终端、工作目录和已解析参数。工作区显示启动、运行、退出状态及输出，
可以打开终端、用 Ctrl+C 中断或使用同样的参数再次运行。开发服务应保持前台运行。
退出状态通过 shell 集成捕获；终端关闭后标记停止，已完成的输出仍可在工作区查看。
项目模板和任务配置使用可提交的相对路径与参数，运行参数在当前窗口会话中维护。
