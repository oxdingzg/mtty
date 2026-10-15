//! Project recipes append portable layouts and manage their own task terminals.
use super::*;
use mtty_ui::project::{self as model, ResolvedTask, Source};

pub(super) struct View {
    source: Source,
    variables: std::collections::BTreeMap<String, String>,
}
impl View {
    pub(super) fn new(source: Source) -> Self {
        Self {
            variables: source.variables.clone(),
            source,
        }
    }
}
#[derive(Clone, Copy)]
enum Phase {
    Starting,
    Running,
    Finished(Option<i32>),
    Stopped,
}
impl Phase {
    fn active(self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

pub(super) struct Run {
    root: std::path::PathBuf,
    plan: ResolvedTask,
    pane: String,
    phase: Phase,
    stop_requested: bool,
    output: String,
}

enum Action {
    Example,
    Reload,
    Edit,
    Recipe(String),
    Task(String),
    Stop(String),
    Open(String),
    Again(std::path::PathBuf, ResolvedTask),
}

impl State {
    pub(super) fn load_project_workspace(&mut self, start: bool) {
        let remote = self.tabs.get(self.active_tab).is_some_and(|tab| tab.ssh);
        let cwd = if remote {
            self.project_workspace
                .as_ref()
                .map(|view| view.source.root.clone())
                .or_else(|| {
                    self.project_runs
                        .iter()
                        .find(|run| {
                            self.tabs
                                .get(self.active_tab)
                                .is_some_and(|tab| tab.panes.iter().any(|pane| pane.id == run.pane))
                        })
                        .map(|run| run.root.clone())
                })
        } else {
            self.cwd()
        };
        if let Some(cwd) = cwd {
            self.spawn_job(move || JobDone::ProjectLoaded {
                result: model::load(&cwd),
                start,
            });
        } else {
            self.show_notice(
                mtty_ui::i18n::t(
                    self.lang,
                    "Open project workspaces from a local project pane",
                    "请从本地项目窗格打开项目工作区",
                )
                .into(),
            );
        }
    }

    pub(super) fn start_default_project_recipe(&mut self) {
        let name = self
            .project_workspace
            .as_ref()
            .and_then(|view| view.source.workspace.as_ref())
            .and_then(|workspace| workspace.default_recipe.clone());
        if let Some(name) = name {
            self.open_project_recipe(&name);
        } else {
            self.show_notice(
                mtty_ui::i18n::t(
                    self.lang,
                    "No default project recipe configured",
                    "尚未配置默认项目配方",
                )
                .into(),
            );
        }
    }

    fn open_project_recipe(&mut self, name: &str) {
        if self.read_only {
            return;
        }
        let Some(view) = &self.project_workspace else {
            return;
        };
        let Some(workspace) = &view.source.workspace else {
            return;
        };
        let Some(recipe) = workspace.recipes.iter().find(|recipe| recipe.name == name) else {
            return;
        };
        self.project_serial = self.project_serial.saturating_add(1);
        let session = model::instantiate(
            recipe,
            &view.variables,
            &format!("recipe-{}", self.project_serial),
        );
        let plans: Result<Vec<_>, String> = recipe
            .tasks
            .iter()
            .map(|name| {
                let task = workspace
                    .tasks
                    .iter()
                    .find(|task| &task.name == name)
                    .ok_or_else(|| format!("unknown task {name}"))?;
                model::resolve(&view.source.root, task, &view.variables)
            })
            .collect();
        let root = view.source.root.clone();
        let (session, plans) = match (session, plans) {
            (Ok(session), Ok(plans)) => (session, plans),
            (Err(error), _) | (_, Err(error)) => {
                self.show_notice(error);
                return;
            }
        };
        let offset = self.tabs.len();
        if let Some(session) = session {
            if !self.restore_from_value(&session) || self.tabs.len() == offset {
                self.show_notice(
                    mtty_ui::i18n::t(
                        self.lang,
                        "Could not open the project layout",
                        "无法打开项目布局",
                    )
                    .into(),
                );
                return;
            }
            // Opening a project is explicit; connect its SSH panes without
            // changing the independent automatic-reconnect startup setting.
            for tab in self.tabs.iter_mut().skip(offset).filter(|tab| tab.ssh) {
                for pane in &mut tab.panes {
                    if let Some(command) = pane.on_enter.take() {
                        pane.term.write(command.as_bytes());
                    }
                }
            }
            let active = session
                .get("active_tab")
                .and_then(|value| value.as_u64())
                .unwrap_or(0) as usize;
            self.active_tab = (offset + active).min(self.tabs.len() - 1);
            self.apply_restored_images();
        }
        let layout_tab = self.active_tab;
        for plan in plans {
            self.start_project_task(root.clone(), plan);
        }
        if self.tabs.len() > offset {
            self.active_tab = layout_tab.min(self.tabs.len() - 1);
        }
        self.publish_panes();
    }

    fn start_project_task(&mut self, root: std::path::PathBuf, plan: ResolvedTask) {
        if self.read_only {
            return;
        }
        let before = self.tabs.len();
        self.new_tab_in(Some(plan.cwd.clone()));
        if self.tabs.len() != before + 1 {
            return;
        }
        if let Some(tab) = self.tabs.last_mut() {
            tab.title = format!("task: {}", plan.name);
            tab.title_set = true;
            if let Some(host) = &plan.host {
                tab.ssh = true;
                tab.ssh_target = Some(host.clone());
            }
            if let Some(pane) = tab.panes.iter_mut().find(|pane| pane.id == tab.active) {
                pane.term.write(format!("{}\r", plan.command).as_bytes());
                self.project_runs.push(Run {
                    root,
                    plan,
                    pane: pane.id.clone(),
                    phase: Phase::Starting,
                    stop_requested: false,
                    output: String::new(),
                });
            }
        }
        while self.project_runs.len() > 50 {
            let Some(index) = self.project_runs.iter().position(|run| !run.phase.active()) else {
                break;
            };
            self.project_runs.remove(index);
        }
        self.publish_panes();
    }

    pub(super) fn poll_project_runs(&mut self) {
        let tabs = &self.tabs;
        let mut changed = false;
        for run in self
            .project_runs
            .iter_mut()
            .filter(|run| run.phase.active())
        {
            let pane = tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .find(|pane| pane.id == run.pane);
            if let Some(pane) = pane {
                if let Some(output) = pane.term.last_command_output() {
                    run.output = output.text.chars().take(64 * 1024).collect();
                    if output.truncated || run.output.len() < output.text.len() {
                        run.output.push_str("\n[output truncated]");
                    }
                    run.phase = if run.stop_requested {
                        Phase::Stopped
                    } else {
                        Phase::Finished(output.exit)
                    };
                    changed = true;
                } else if pane.term.command_running() && matches!(run.phase, Phase::Starting) {
                    run.phase = Phase::Running;
                    changed = true;
                }
            } else {
                run.phase = Phase::Stopped;
                changed = true;
            }
        }
        if changed {
            self.window.request_redraw();
        }
    }

    pub(super) fn project_workspace_window(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let Some(mut view) = self.project_workspace.take() else {
            return;
        };
        let lang = self.lang;
        let mut open = true;
        let mut action = None;
        app_window(t(lang, "Project Workspace", "项目工作区"), ctx)
            .open(&mut open)
            .default_size([720.0, 600.0])
            .show(ctx, |ui| {
                window_body(ui, |ui| {
                    ui.label(egui::RichText::new(view.source.root.display().to_string()).strong());
                    ui.horizontal(|ui| {
                        if ui.button(t(lang, "Reload", "重新加载")).clicked() {
                            action = Some(Action::Reload);
                        }
                        if view.source.workspace.is_some()
                            && ui
                                .button(t(lang, "Edit workspace.json", "编辑 workspace.json"))
                                .clicked()
                        {
                            action = Some(Action::Edit);
                        }
                    });
                    let Some(workspace) = &view.source.workspace else {
                        ui.label(t(
                            lang,
                            "No .mtty/workspace.json in this project.",
                            "项目中尚无 .mtty/workspace.json。",
                        ));
                        if ui
                            .add_enabled(
                                !self.read_only,
                                egui::Button::new(t(
                                    lang,
                                    "Create portable example",
                                    "创建可移植示例",
                                )),
                            )
                            .clicked()
                        {
                            action = Some(Action::Example);
                        }
                        return;
                    };
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for (name, value) in view
                            .variables
                            .iter_mut()
                            .filter(|(name, _)| name.as_str() != "project")
                        {
                            ui.horizontal(|ui| {
                                ui.label(name);
                                ui.text_edit_singleline(value);
                            });
                        }
                        ui.separator();
                        for recipe in &workspace.recipes {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(&recipe.name).strong());
                                if ui
                                    .add_enabled(
                                        !self.read_only,
                                        egui::Button::new(format!(
                                            "{} · {} {}",
                                            t(lang, "Open layout + run", "打开布局并运行"),
                                            recipe.tasks.len(),
                                            t(lang, "tasks", "个任务")
                                        )),
                                    )
                                    .clicked()
                                {
                                    action = Some(Action::Recipe(recipe.name.clone()));
                                }
                            });
                            if let Some(tabs) = recipe
                                .session
                                .as_ref()
                                .and_then(|value| value.get("tabs"))
                                .and_then(serde_json::Value::as_array)
                            {
                                for tab in tabs {
                                    if let Some(host) =
                                        tab.get("ssh_target").and_then(serde_json::Value::as_str)
                                    {
                                        ui.label(format!(
                                            "SSH: {}",
                                            model::expand(host, &view.variables, None)
                                                .unwrap_or_else(|error| error)
                                        ));
                                    }
                                }
                            }
                            if !recipe.tasks.is_empty() {
                                ui.label(recipe.tasks.join(", "));
                            }
                        }
                        ui.separator();
                        for task in &workspace.tasks {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(&task.name).strong());
                                if ui
                                    .add_enabled(
                                        !self.read_only,
                                        egui::Button::new(t(lang, "Run", "运行")),
                                    )
                                    .clicked()
                                {
                                    action = Some(Action::Task(task.name.clone()));
                                }
                            });
                            let syntax = if task.host.is_some() {
                                mtty_ui::ssh::Syntax::Posix
                            } else {
                                mtty_ui::ssh::Syntax::local()
                            };
                            ui.label(
                                egui::RichText::new(
                                    model::expand(&task.command, &view.variables, Some(syntax))
                                        .unwrap_or_else(|error| error),
                                )
                                .monospace(),
                            );
                        }
                        ui.separator();
                        ui.label(t(
                            lang,
                            "Task terminals · status requires shell integration",
                            "任务终端 · 状态捕获需要 shell 集成",
                        ));
                        for run in self
                            .project_runs
                            .iter()
                            .rev()
                            .filter(|run| run.root == view.source.root)
                        {
                            let phase = match run.phase {
                                Phase::Starting => t(
                                    lang,
                                    "Starting / awaiting shell report",
                                    "正在启动／等待 shell 报告",
                                )
                                .to_owned(),
                                Phase::Running => t(lang, "Running", "运行中").to_owned(),
                                Phase::Stopped => t(lang, "Stopped", "已停止").to_owned(),
                                Phase::Finished(exit) => format!(
                                    "{} {}",
                                    t(lang, "Exit", "退出码"),
                                    exit.map_or_else(|| "?".into(), |exit| exit.to_string())
                                ),
                            };
                            ui.horizontal_wrapped(|ui| {
                                ui.label(format!("{} · {phase}", run.plan.name));
                                if ui.button(t(lang, "Open terminal", "打开终端")).clicked() {
                                    action = Some(Action::Open(run.pane.clone()));
                                }
                                if ui
                                    .add_enabled(
                                        !self.read_only && run.phase.active(),
                                        egui::Button::new(t(lang, "Stop", "停止")),
                                    )
                                    .clicked()
                                {
                                    action = Some(Action::Stop(run.pane.clone()));
                                }
                                if ui
                                    .add_enabled(
                                        !self.read_only,
                                        egui::Button::new(t(lang, "Run again", "再次运行")),
                                    )
                                    .clicked()
                                {
                                    action =
                                        Some(Action::Again(run.root.clone(), run.plan.clone()));
                                }
                            });
                            ui.label(format!(
                                "{} · {}",
                                run.plan.host.as_deref().unwrap_or(t(lang, "Local", "本地")),
                                run.plan.directory
                            ));
                            if !run.output.is_empty() {
                                egui::CollapsingHeader::new(t(
                                    lang,
                                    "Captured output",
                                    "捕获的输出",
                                ))
                                .id_salt(("project-output", &run.pane))
                                .show(ui, |ui| {
                                    let lines: Vec<_> = run.output.lines().collect();
                                    let height = ui.text_style_height(&egui::TextStyle::Monospace);
                                    egui::ScrollArea::both()
                                        .id_salt(("project-log", &run.pane))
                                        .max_height(120.0)
                                        .show_rows(ui, height, lines.len(), |ui, rows| {
                                            for row in rows {
                                                ui.add(
                                                    egui::Label::new(
                                                        egui::RichText::new(
                                                            lines[row]
                                                                .chars()
                                                                .take(4096)
                                                                .collect::<String>(),
                                                        )
                                                        .monospace(),
                                                    )
                                                    .wrap_mode(egui::TextWrapMode::Extend),
                                                );
                                            }
                                        });
                                });
                            }
                        }
                    });
                })
            });
        let root = view.source.root.clone();
        let mut recipe = None;
        match action {
            Some(Action::Reload) => self.spawn_job(move || JobDone::ProjectLoaded {
                result: model::load(&root),
                start: false,
            }),
            Some(Action::Example) => self.spawn_job(move || JobDone::ProjectLoaded {
                result: model::create_example(&root),
                start: false,
            }),
            Some(Action::Edit) => {
                self.open_editor_ro(model::file(&root), false);
                open = false;
            }
            Some(Action::Recipe(name)) => recipe = Some(name),
            Some(Action::Task(name)) => {
                if let Some(task) = view
                    .source
                    .workspace
                    .as_ref()
                    .and_then(|workspace| workspace.tasks.iter().find(|task| task.name == name))
                {
                    match model::resolve(&root, task, &view.variables) {
                        Ok(plan) => self.start_project_task(root, plan),
                        Err(error) => self.show_notice(error),
                    }
                }
            }
            Some(Action::Stop(id)) => {
                if !self.read_only {
                    if let Some(pane) = self
                        .tabs
                        .iter_mut()
                        .flat_map(|tab| &mut tab.panes)
                        .find(|pane| pane.id == id)
                    {
                        pane.term.write(b"\x03");
                    }
                    if let Some(run) = self.project_runs.iter_mut().find(|run| run.pane == id) {
                        run.stop_requested = true;
                    }
                }
            }
            Some(Action::Open(id)) => {
                open = false;
                if let Some(index) = self
                    .tabs
                    .iter()
                    .position(|tab| tab.panes.iter().any(|pane| pane.id == id))
                {
                    self.active_tab = index;
                    self.tabs[index].active = id;
                }
            }
            Some(Action::Again(root, plan)) => self.start_project_task(root, plan),
            None => {}
        }
        if open {
            self.project_workspace = Some(view);
        }
        if let Some(recipe) = recipe {
            self.open_project_recipe(&recipe);
        }
    }
}
