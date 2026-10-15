//! Portable, explicit project workspaces at `.mtty/workspace.json`.
use crate::ssh::{self, Syntax};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workspace {
    pub version: u32,
    #[serde(default)]
    pub default_recipe: Option<String>,
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
    #[serde(default)]
    pub recipes: Vec<Recipe>,
    #[serde(default)]
    pub tasks: Vec<Task>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Recipe {
    pub name: String,
    #[serde(default)]
    pub session: Option<Value>,
    #[serde(default)]
    pub tasks: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
}
#[derive(Clone, Debug)]
pub struct Source {
    pub root: PathBuf,
    pub workspace: Option<Workspace>,
    pub variables: BTreeMap<String, String>,
}
#[derive(Clone, Debug)]
pub struct ResolvedTask {
    pub name: String,
    pub command: String,
    pub cwd: PathBuf,
    pub directory: String,
    pub host: Option<String>,
}

pub fn file(root: &Path) -> PathBuf {
    root.join(".mtty").join("workspace.json")
}

pub fn load(cwd: &Path) -> Result<Source, String> {
    #[cfg(windows)]
    let normalized = cwd
        .to_str()
        .filter(|text| {
            text.starts_with('/')
                && text.as_bytes().get(1).is_some_and(u8::is_ascii_alphabetic)
                && text.as_bytes().get(2) == Some(&b':')
        })
        .map(|text| PathBuf::from(&text[1..]));
    #[cfg(windows)]
    let cwd = normalized.as_deref().unwrap_or(cwd);
    let cwd = std::fs::canonicalize(cwd).map_err(|e| e.to_string())?;
    let root = cwd
        .ancestors()
        .find(|path| file(path).is_file() || path.join(".git").exists())
        .unwrap_or(&cwd)
        .to_path_buf();
    let workspace = match std::fs::File::open(file(&root)) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(256 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            if bytes.len() > 256 * 1024 {
                return Err("project workspace exceeds 256 KiB".into());
            }
            let workspace: Workspace = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            validate(&workspace)?;
            Some(workspace)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    let mut variables = workspace
        .as_ref()
        .map(|workspace| workspace.parameters.clone())
        .unwrap_or_default();
    variables.insert("project".into(), root.to_string_lossy().into_owned());
    if !variables.contains_key("branch") {
        let branch = mtty_platform::background_command("git")
            .arg("-C")
            .arg(&root)
            .args(["branch", "--show-current"])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .filter(|branch| !branch.is_empty())
            .unwrap_or_else(|| "HEAD".into());
        variables.insert("branch".into(), branch);
    }
    Ok(Source {
        root,
        workspace,
        variables,
    })
}

pub fn validate(workspace: &Workspace) -> Result<(), String> {
    if workspace.version != 1 {
        return Err("unsupported project workspace version".into());
    }
    if workspace.parameters.contains_key("project") {
        return Err("project is a built-in parameter".into());
    }
    if workspace.parameters.contains_key("project") {
        return Err("project is a built-in parameter".into());
    }
    let mut tasks = BTreeSet::new();
    for task in &workspace.tasks {
        if task.name.trim().is_empty()
            || task.command.trim().is_empty()
            || !tasks.insert(&task.name)
        {
            return Err("project tasks need unique names and nonempty commands".into());
        }
    }
    let mut recipes = BTreeSet::new();
    for recipe in &workspace.recipes {
        if recipe.name.trim().is_empty() || !recipes.insert(&recipe.name) {
            return Err("project recipes need unique nonempty names".into());
        }
        for task in &recipe.tasks {
            if !tasks.contains(task) {
                return Err(format!(
                    "recipe {} references unknown task {task}",
                    recipe.name
                ));
            }
        }
    }
    if workspace
        .default_recipe
        .as_ref()
        .is_some_and(|name| !recipes.contains(name))
    {
        return Err("default_recipe does not name a recipe".into());
    }
    Ok(())
}

/// Command values are quoted as standalone shell fragments. Reject ambiguous
/// substitutions inside existing quotes instead of guessing nested shell syntax.
pub fn expand(
    text: &str,
    variables: &BTreeMap<String, String>,
    shell: Option<Syntax>,
) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    let mut quote = None;
    while let Some(ch) = chars.next() {
        let escape = match shell {
            Some(Syntax::PowerShell) => '`',
            Some(Syntax::Cmd) => '^',
            _ => '\\',
        };
        if shell.is_some() && ch == escape {
            out.push(ch);
            if let Some(next) = chars.next() {
                out.push(next);
            }
            continue;
        }
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next();
            let mut name = String::new();
            let mut closed = false;
            for ch in chars.by_ref() {
                if ch == '}' {
                    closed = true;
                    break;
                }
                name.push(ch);
            }
            if !closed {
                return Err("unclosed workspace parameter".into());
            }
            let value = variables
                .get(&name)
                .ok_or_else(|| format!("unknown workspace parameter: {name}"))?;
            if shell.is_some() && quote.is_some() {
                return Err("put workspace parameters outside shell quotes; values are quoted automatically".into());
            }
            out.push_str(&shell.map_or_else(|| value.clone(), |syntax| syntax.quote(value)));
        } else {
            if shell.is_some() && matches!(ch, '\'' | '"' | '`') {
                if quote == Some(ch) {
                    quote = None;
                } else if quote.is_none() {
                    quote = Some(ch);
                }
            }
            out.push(ch);
        }
    }
    Ok(out)
}

pub fn resolve(
    root: &Path,
    task: &Task,
    variables: &BTreeMap<String, String>,
) -> Result<ResolvedTask, String> {
    let host = task
        .host
        .as_deref()
        .map(|host| expand(host, variables, None))
        .transpose()?;
    let syntax = if host.is_some() {
        Syntax::Posix
    } else {
        Syntax::local()
    };
    let command = expand(&task.command, variables, Some(syntax))?;
    let default = if host.is_some() { "~" } else { "${project}" };
    let directory = expand(task.cwd.as_deref().unwrap_or(default), variables, None)?;
    if let Some(host) = &host {
        let target = ssh::Target::parse(host).ok_or("invalid project SSH target")?;
        let cwd = if directory == "~" {
            "\"$HOME\"".into()
        } else if let Some(path) = directory.strip_prefix("~/") {
            format!("\"$HOME\"/{}", Syntax::Posix.quote(path))
        } else {
            Syntax::Posix.quote(&directory)
        };
        let encoded = command
            .replace('%', "%25")
            .replace('\x1b', "%1B")
            .replace('\x07', "%07")
            .replace('\n', "%0A")
            .replace('\r', "%0D");
        let script = format!("export TERM=xterm-256color\ncd {cwd} || exit\nprintf '\\033]7;file://mtty-remote%s\\033\\\\' \"$(printf %s \"$PWD\" | sed \"s/%/%25/g\")\"\nprintf '\\033]133;E;%s\\007' {}\nprintf '\\033]133;C\\007'\nsh -lc {}\nstatus=$?\nprintf '\\033]133;D;%s\\007' \"$status\"\nexit \"$status\"", Syntax::Posix.quote(&encoded), Syntax::Posix.quote(&command));
        let remote = format!("sh -lc {}", Syntax::Posix.quote(&script));
        Ok(ResolvedTask {
            name: task.name.clone(),
            command: ssh::command(&target, &remote),
            cwd: root.to_path_buf(),
            directory,
            host: Some(host.clone()),
        })
    } else {
        let cwd = PathBuf::from(&directory);
        let cwd = if cwd.is_absolute() {
            cwd
        } else {
            root.join(cwd)
        };
        if !cwd.is_dir() {
            return Err(format!("task directory does not exist: {}", cwd.display()));
        }
        let command = match syntax {
            Syntax::Posix => format!("sh -lc {}", syntax.quote(&command)),
            Syntax::PowerShell => format!("& {{ {command} }}"),
            Syntax::Cmd => format!(
                "cmd /D /S /C {}",
                syntax.quote(&command.replace("\r\n", "\n").replace('\n', " & "))
            ),
        };
        Ok(ResolvedTask {
            name: task.name.clone(),
            command,
            directory: cwd.display().to_string(),
            cwd,
            host: None,
        })
    }
}

pub fn instantiate(
    recipe: &Recipe,
    variables: &BTreeMap<String, String>,
    namespace: &str,
) -> Result<Option<Value>, String> {
    fn strings(value: &mut Value, variables: &BTreeMap<String, String>) -> Result<(), String> {
        match value {
            Value::String(text) => *text = expand(text, variables, None)?,
            Value::Array(values) => {
                for value in values {
                    strings(value, variables)?;
                }
            }
            Value::Object(values) => {
                for value in values.values_mut() {
                    strings(value, variables)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn ids(value: &mut Value, map: &BTreeMap<String, String>) {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    if key == "leaf" {
                        if let Some(id) = value.as_str().and_then(|id| map.get(id)) {
                            *value = Value::String(id.clone());
                        }
                    } else {
                        ids(value, map);
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    ids(value, map);
                }
            }
            _ => {}
        }
    }
    let Some(mut value) = recipe.session.clone() else {
        return Ok(None);
    };
    let tabs = value
        .get_mut("tabs")
        .and_then(Value::as_array_mut)
        .ok_or("recipe session needs tabs")?;
    for (index, tab) in tabs.iter_mut().enumerate() {
        let tab = tab.as_object_mut().ok_or("invalid recipe tab")?;
        tab.remove("ssh_cmd");
        tab.remove("transport");
        let mut map = BTreeMap::new();
        for group in ["panes", "editors", "previews"] {
            if let Some(entries) = tab.get_mut(group).and_then(Value::as_array_mut) {
                for entry in entries {
                    let object = entry.as_object_mut().ok_or("invalid recipe pane")?;
                    let old = object
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or("recipe panes need string ids")?
                        .to_owned();
                    let new = format!("{namespace}-{index}-{old}");
                    if map.insert(old, new.clone()).is_some() {
                        return Err("recipe pane ids must be unique within a tab".into());
                    }
                    object.insert("id".into(), Value::String(new));
                    for key in ["host", "program", "remote_cwd", "cwd_origin"] {
                        object.remove(key);
                    }
                }
            }
        }
        if let Some(active) = tab.get_mut("active") {
            if let Some(new) = active.as_str().and_then(|id| map.get(id)) {
                *active = Value::String(new.clone());
            }
        }
        if let Some(layout) = tab.get_mut("layout") {
            ids(layout, &map);
        }
        if let Some(previews) = tab.get_mut("previews").and_then(Value::as_array_mut) {
            for preview in previews {
                if let Some(new) = preview
                    .get("source")
                    .and_then(Value::as_str)
                    .and_then(|id| map.get(id))
                    .cloned()
                {
                    preview["source"] = Value::String(new);
                }
            }
        }
    }
    strings(&mut value, variables)?;
    if let Some(tabs) = value.get("tabs").and_then(Value::as_array) {
        for tab in tabs {
            if tab.get("ssh").and_then(Value::as_bool) == Some(true)
                && tab
                    .get("ssh_target")
                    .and_then(Value::as_str)
                    .and_then(ssh::Target::parse)
                    .is_none()
            {
                return Err("SSH recipes need a valid expanded ssh_target".into());
            }
        }
    }
    Ok(Some(value))
}

pub fn create_example(root: &Path) -> Result<Source, String> {
    use std::io::Write;
    let path = file(root);
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let example = serde_json::json!({
        "version":1, "default_recipe":"work", "parameters":{"port":"3000"},
        "recipes":[{"name":"work", "session":{"active_tab":0,"tabs":[{"title":"Project","title_set":true,"panes":[{"id":"shell","cwd":"${project}"}],"active":"shell","layout":{"leaf":"shell"}}]},"tasks":["status"]}],
        "tasks":[{"name":"status","command":"git status --short","cwd":"${project}"}]
    });
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    output
        .write_all(
            serde_json::to_string_pretty(&example)
                .map_err(|e| e.to_string())?
                .as_bytes(),
        )
        .map_err(|e| e.to_string())?;
    load(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parameters_are_literal_shell_arguments_and_unknown_or_quoted_ones_fail() {
        let variables = BTreeMap::from([("branch".into(), "feature/a; echo bad".into())]);
        assert_eq!(
            expand("git switch ${branch}", &variables, Some(Syntax::Posix)).unwrap(),
            "git switch 'feature/a; echo bad'"
        );
        assert!(expand("echo '${branch}'", &variables, Some(Syntax::Posix)).is_err());
        assert!(expand("${missing}", &variables, None).is_err());
    }
    #[test]
    #[cfg(unix)]
    fn resolved_commands_keep_parameters_literal_across_multiple_steps() {
        let root = std::env::temp_dir();
        let variables = BTreeMap::from([
            ("project".into(), root.display().to_string()),
            ("value".into(), "literal; not another command".into()),
        ]);
        let task = Task {
            name: "test".into(),
            command: "printf '%s' ${value}\nprintf '\\nsecond step'".into(),
            cwd: None,
            host: None,
        };
        let plan = resolve(&root, &task, &variables).unwrap();
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &plan.command])
            .current_dir(&plan.cwd)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "literal; not another command\nsecond step"
        );
    }

    #[test]
    fn templates_expand_paths_and_never_reuse_runtime_host_or_pane_ids() {
        let variables = BTreeMap::from([("project".into(), "/project with space".into())]);
        let recipe = Recipe {
            name: "test".into(),
            tasks: vec![],
            session: Some(
                serde_json::json!({"tabs":[{"ssh_cmd":"old command","panes":[{"id":"pane1","cwd":"${project}","host":{"id":"old"},"program":"old program"}],"active":"pane1","layout":{"leaf":"pane1"}}]}),
            ),
        };
        let value = instantiate(&recipe, &variables, "recipe-1")
            .unwrap()
            .unwrap();
        assert_eq!(value["tabs"][0]["panes"][0]["cwd"], "/project with space");
        assert_eq!(
            value["tabs"][0]["active"],
            value["tabs"][0]["layout"]["leaf"]
        );
        assert_ne!(value["tabs"][0]["active"], "pane1");
        assert!(value["tabs"][0]["panes"][0].get("host").is_none());
        assert!(value["tabs"][0].get("ssh_cmd").is_none());
    }
    #[test]
    fn example_creation_is_non_overwriting_and_discovered_from_subdirectories() {
        let root = std::env::temp_dir().join(format!(
            "mtty-project-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let source = create_example(&root).unwrap();
        assert_eq!(source.workspace.unwrap().tasks[0].name, "status");
        assert!(create_example(&root).is_err());
        let discovered = load(&root.join("src")).unwrap();
        assert_eq!(discovered.root, std::fs::canonicalize(&root).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }
}
