//! External-browser automation, captured in owned task terminals.
use super::*;
use mtty_ui::browser::{self as model, Context, Plan, Report};

pub(super) struct View {
    pub epoch: u64,
    pub context: Option<Context>,
    pub loading: bool,
    pub error: Option<String>,
    url: String,
    command: String,
    notes: String,
}
pub(super) struct Run {
    pub id: u64,
    plan: Plan,
    pane: String,
    collecting: bool,
    pub result: Option<Result<Report, String>>,
}

impl State {
    pub(super) fn open_browser_verification(&mut self, run: bool) {
        self.browser_epoch = self.browser_epoch.saturating_add(1);
        let epoch = self.browser_epoch;
        let local = !self.tabs.get(self.active_tab).is_some_and(|tab| tab.ssh)
            && !self
                .active_editor()
                .is_some_and(|editor| editor.remote.is_some());
        let cwd = if local {
            self.cwd().unwrap_or_else(std::env::temp_dir)
        } else {
            std::env::temp_dir()
        };
        self.browser_view = Some(View {
            epoch,
            context: None,
            loading: true,
            error: None,
            url: self.browser_url.clone(),
            command: self.browser_command.clone().unwrap_or_default(),
            notes: String::new(),
        });
        self.spawn_job(move || JobDone::BrowserContextLoaded {
            epoch,
            run,
            result: model::context(&cwd),
        });
    }
    pub(super) fn prepare_browser_run(&mut self) {
        if self.read_only {
            return;
        }
        let Some(view) = &mut self.browser_view else {
            return;
        };
        let Some(context) = view.context.clone() else {
            return;
        };
        let (epoch, url, command, notes) = (
            view.epoch,
            view.url.clone(),
            view.command.clone(),
            view.notes.clone(),
        );
        view.loading = true;
        view.error = None;
        self.spawn_job(move || JobDone::BrowserPlanned {
            epoch,
            result: model::plan(context, &url, &command, &notes),
        });
    }
    pub(super) fn start_browser_run(&mut self, plan: Plan) {
        if self.read_only {
            return;
        }
        let before = self.tabs.len();
        self.new_tab_in(Some(plan.context.cwd.clone()));
        if self.tabs.len() != before + 1 {
            return;
        }
        self.browser_epoch = self.browser_epoch.saturating_add(1);
        let id = self.browser_epoch;
        if let Some(tab) = self.tabs.last_mut() {
            tab.title = "Browser verification".into();
            tab.title_set = true;
            if let Some(pane) = tab.panes.iter_mut().find(|pane| pane.id == tab.active) {
                pane.term.write(format!("{}\r", plan.command).as_bytes());
                self.browser_runs.push(Run {
                    id,
                    plan,
                    pane: pane.id.clone(),
                    collecting: false,
                    result: None,
                });
            }
        }
        self.publish_panes();
    }
    pub(super) fn poll_browser_runs(&mut self) {
        let mut ready = Vec::new();
        let tabs = &self.tabs;
        for run in self
            .browser_runs
            .iter_mut()
            .filter(|run| !run.collecting && run.result.is_none())
        {
            let pane = tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .find(|pane| pane.id == run.pane);
            if let Some(pane) = pane {
                if let Some(output) = pane.term.last_command_output() {
                    run.collecting = true;
                    ready.push((
                        run.id,
                        run.plan.clone(),
                        output.exit,
                        output.text.chars().take(64 * 1024).collect(),
                    ));
                }
            } else {
                run.collecting = true;
                ready.push((
                    run.id,
                    run.plan.clone(),
                    None,
                    "Verification terminal was closed before an exit status was captured.".into(),
                ));
            }
        }
        for (id, plan, exit, output) in ready {
            self.spawn_job(move || JobDone::BrowserCollected {
                id,
                result: model::finish(plan, exit, output),
            });
        }
    }
    pub(super) fn browser_verification_window(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let Some(mut view) = self.browser_view.take() else {
            return;
        };
        let lang = self.lang;
        let mut open = true;
        let mut run = false;
        let mut open_url = false;
        let mut screenshot = None;
        let mut copy = None;
        app_window(t(lang,"Browser Verification","浏览器验证"),ctx).open(&mut open).default_size([780.0,620.0]).show(ctx,|ui|window_body(ui,|ui|{
            if let Some(context)=&view.context{ui.label(context.cwd.display().to_string());if let Some(task)=&context.task{ui.label(format!("Task: {} · revision {}",task.name,context.revision));}}
            ui.label(t(lang,"Local or forwarded service URL","本地或已转发的服务 URL"));ui.add(egui::TextEdit::singleline(&mut view.url).desired_width(f32::INFINITY));
            if ui.button(t(lang,"Open in browser","在浏览器中打开")).clicked(){open_url=true;}
            ui.separator();ui.label(t(lang,"Automation command · ${url} ${screenshot} ${project}","自动化命令 · ${url} ${screenshot} ${project}"));
            ui.add(egui::TextEdit::multiline(&mut view.command).desired_rows(2).font(egui::TextStyle::Monospace).hint_text("playwright screenshot ${url} ${screenshot}"));
            ui.add(egui::TextEdit::multiline(&mut view.notes).desired_rows(2).hint_text(t(lang,"Verification notes","验证备注")));
            ui.horizontal(|ui|{if ui.add_enabled(!self.read_only&&!view.loading&&!view.command.trim().is_empty(),egui::Button::new(t(lang,"Run automation + capture evidence","运行自动化并收集证据"))).clicked(){run=true;}
                if view.loading{ui.spinner();}});
            if let Some(error)=&view.error{ui.colored_label(chrome_rgb(self.theme.chrome().negative),error);}
            ui.small(t(lang,"Each run gets a fresh PNG path in task Git metadata or application evidence storage.","每次运行在任务 Git 元数据或应用证据目录中分配新的 PNG 路径。"));
            egui::ScrollArea::vertical().max_height(300.0).show(ui,|ui|{
                for result in self.browser_runs.iter().rev(){
                    if let Some(context)=&view.context{if result.plan.context.cwd!=context.cwd{continue;}}
                    ui.separator();ui.label(&result.plan.url);
                    match &result.result{
                        None=>{ui.label(t(lang,"Running in a dedicated terminal…","正在独立终端中运行…"));}
                        Some(Err(error))=>{ui.colored_label(chrome_rgb(self.theme.chrome().negative),error);}
                        Some(Ok(report))=>{
                            ui.label(format!("{} {:?} · PNG {} · {}×{}",t(lang,"Automation exit","自动化退出码"),report.exit,report.evidence.valid,report.evidence.width,report.evidence.height));
                            ui.label(&report.evidence.screenshot);
                            if let Some(error)=&report.evidence.error{ui.label(error);}
                            ui.horizontal(|ui|{
                                if ui.add_enabled(report.evidence.valid,egui::Button::new(t(lang,"Open screenshot","打开截图"))).clicked(){screenshot=Some(report.evidence.screenshot.clone());}
                                if ui.button(t(lang,"Evidence to Composer","证据发到 Composer")).clicked(){copy=Some(format!("URL: {}\nScreenshot: {}\nAutomation exit: {:?}\nPNG valid: {}\nNotes: {}\n",report.evidence.url,report.evidence.screenshot,report.exit,report.evidence.valid,report.evidence.notes));}
                            });
                        }
                    }
                }
            });
        }));
        if open_url {
            match model::validate_url(&view.url) {
                Ok(url) => open_external(&url),
                Err(error) => view.error = Some(error),
            }
        }
        if let Some(path) = screenshot {
            open_external(&mtty_lsp::position::path_to_uri(std::path::Path::new(
                &path,
            )));
        }
        if let Some(context) = copy {
            let draft = self.composer.take().unwrap_or_default();
            self.composer = Some(format!("{draft}\n\n{context}"));
        }
        if open {
            self.browser_view = Some(view);
        }
        if run {
            self.prepare_browser_run();
        }
    }
}
