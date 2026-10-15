//! The actionable, per-pane attention inbox.

use super::*;

impl State {
    pub(super) fn focus_attention_pane(&mut self, id: &str) -> bool {
        let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.panes.iter().any(|p| p.id == id))
        else {
            return false;
        };
        self.active_tab = index;
        self.tabs[index].active = id.to_owned();
        self.selection = None;
        self.window.request_redraw();
        true
    }

    pub(super) fn attention_window(&mut self, ctx: &egui::Context) {
        if !self.attention_open {
            return;
        }
        use mtty_ui::attention::Reason;
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let rows: Vec<_> = self
            .attention_inbox
            .items()
            .iter()
            .filter_map(|item| {
                let tab = self
                    .tabs
                    .iter()
                    .find(|tab| tab.panes.iter().any(|pane| pane.id == item.pane))?;
                Some((item.clone(), self.title_of(tab)))
            })
            .collect();
        let mut open = true;
        let mut action = None;
        app_window(t(lang, "Attention Inbox", "待处理事项"), ctx)
            .open(&mut open)
            .default_size([600.0, 400.0])
            .show(ctx, |ui| window_body(ui, |ui| {
                ui.label(t(lang, "Pending actions are grouped by pane. Completion does not mean verification passed.", "待处理事项按窗格聚合。任务完成不代表验证通过。"));
                if rows.is_empty() {
                    ui.label(t(lang, "No pending tasks", "没有待处理任务"));
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (item, title) in &rows {
                        ui.separator();
                        let reason = match item.reason {
                            Reason::Failed => t(lang, "Execution failed", "执行失败"),
                            Reason::Input => t(lang, "Waiting for your input", "等待你的输入"),
                            Reason::Review => t(lang, "Finished · review results", "已结束 · 待审阅结果"),
                        };
                        ui.label(egui::RichText::new(title).strong());
                        ui.label(format!("{} · {reason}", item.agent));
                        ui.horizontal_wrapped(|ui| {
                            for (kind, label) in [
                                (0, t(lang, "Open terminal", "打开终端")),
                                (1, t(lang, "Output to Composer", "输出发到 Composer")),
                                (2, t(lang, "Agent Tasks…", "Agent 任务…")),
                                (3, t(lang, "Dismiss", "标为已处理")),
                            ] {
                                if ui.button(label).clicked() {
                                    action = Some((item.pane.clone(), kind));
                                }
                            }
                        });
                    }
                });
            }));
        self.attention_open = open;
        if let Some((pane, kind)) = action {
            if kind == 3 {
                self.attention_inbox.dismiss(&pane);
            } else if self.focus_attention_pane(&pane) {
                match kind {
                    1 => self.run_command(Cmd::SendLastOutput),
                    2 => self.run_command(Cmd::Tasks),
                    _ => self.attention_open = false,
                }
            }
        }
    }
}
