//! ACP launcher discovery and negotiated-session controls.
use super::*;
use mtty_ui::acp_adapters as adapters;

impl State {
    pub(super) fn open_acp_launcher(&mut self) {
        let cwd = self
            .cwd()
            .filter(|_| !self.tabs.get(self.active_tab).is_some_and(|tab| tab.ssh))
            .or_else(mtty_config::home_dir)
            .unwrap_or_default();
        let directory = mtty_ui::file_link::local_path(".", cwd.to_str())
            .display()
            .to_string();
        self.acp_start = Some(AcpStart {
            directory,
            ..Default::default()
        });
        let agents = self.acp_agents.clone();
        self.spawn_job(move || JobDone::AcpInventory(adapters::discover(&agents)));
    }

    pub(super) fn draw_acp_launcher(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let Some(start) = &mut self.acp_start else {
            return;
        };
        let lang = self.lang;
        let mut open = true;
        let mut go = None;
        app_window(t(lang, "ACP Launcher", "ACP 启动器"), ctx)
            .open(&mut open)
            .default_size([660.0, 450.0])
            .show(ctx, |ui| {
                window_body(ui, |ui| {
                    if !start.scanned {
                        ui.label(t(
                            lang,
                            "Checking configured agents and local adapters…",
                            "正在检查已配置 Agent 和本机适配器…",
                        ));
                    }
                    egui::ComboBox::from_id_salt("acp-discovered")
                        .width(500.0)
                        .selected_text(
                            start
                                .inventory
                                .get(start.index)
                                .map(|entry| entry.agent.name.as_str())
                                .unwrap_or(t(lang, "Custom command", "自定义命令")),
                        )
                        .show_ui(ui, |ui| {
                            for (index, entry) in start.inventory.iter().enumerate() {
                                let found = if entry.found {
                                    t(lang, "found on PATH", "已在 PATH 找到")
                                } else {
                                    t(lang, "not found on PATH", "未在 PATH 找到")
                                };
                                if ui
                                    .selectable_value(
                                        &mut start.index,
                                        index,
                                        format!("{} · {found}", entry.agent.name),
                                    )
                                    .changed()
                                {
                                    start.command.clear();
                                    start.error = None;
                                    start.session_id =
                                        entry.agent.session_id.clone().unwrap_or_default();
                                }
                            }
                        });
                    if let Some(entry) = start.inventory.get(start.index) {
                        ui.label(
                            egui::RichText::new(
                                serde_json::to_string(&entry.agent.command).unwrap_or_default(),
                            )
                            .monospace(),
                        );
                        if entry.configured {
                            ui.label(t(lang, "Configured profile", "已配置的配置项"));
                        }
                        ui.horizontal(|ui| {
                            if ui
                                .button(t(lang, "Copy config template", "复制配置模板"))
                                .clicked()
                            {
                                ctx.copy_text(adapters::config_template(&entry.agent));
                            }
                            if let Some(install) = &entry.install {
                                if ui
                                    .button(t(lang, "Copy install command", "复制安装命令"))
                                    .clicked()
                                {
                                    ctx.copy_text(install.clone());
                                }
                            }
                        });
                        if let Some(install) = &entry.install {
                            if !entry.found {
                                ui.label(egui::RichText::new(install).monospace());
                            }
                        }
                    }
                    ui.separator();
                    ui.label(t(
                        lang,
                        "Optional command override (JSON argv preserves spaces)",
                        "可选的命令覆盖（JSON 参数数组可保留空格）",
                    ));
                    ui.add(
                        egui::TextEdit::singleline(&mut start.command)
                            .hint_text("[\"adapter\", \"--acp\"]")
                            .desired_width(f32::INFINITY),
                    );
                    ui.label(t(lang, "Local working directory", "本地工作目录"));
                    ui.add(
                        egui::TextEdit::singleline(&mut start.directory)
                            .desired_width(f32::INFINITY),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut start.session_id)
                            .hint_text(t(
                                lang,
                                "Session ID to resume (negotiated support required)",
                                "要恢复的会话 ID（需要协商得到恢复支持）",
                            ))
                            .desired_width(f32::INFINITY),
                    );
                    ui.label(t(
                        lang,
                        "Models, modes and resume support are shown from the agent's handshake.",
                        "模型、模式与恢复支持会根据 Agent 的实际握手结果展示。",
                    ));
                    if let Some(error) = &start.error {
                        ui.colored_label(chrome_rgb(self.theme.chrome().negative), error);
                    }
                    if ui
                        .add_enabled(
                            start.scanned || !start.command.trim().is_empty(),
                            egui::Button::new(t(lang, "Start", "启动")),
                        )
                        .clicked()
                    {
                        let selected = start
                            .inventory
                            .get(start.index)
                            .map(|entry| entry.agent.clone());
                        if start.command.trim().is_empty() {
                            go = selected;
                        } else {
                            match adapters::parse_command(&start.command) {
                                Ok(command) => {
                                    go = Some(mtty_config::AcpAgent {
                                        name: command[0].clone(),
                                        command,
                                        ..Default::default()
                                    })
                                }
                                Err(error) => start.error = Some(error),
                            }
                        }
                    }
                })
            });
        if let Some(agent) = go {
            self.start_acp(agent);
        } else if !open {
            self.acp_start = None;
        }
    }
}

pub(super) fn session_details(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    lang: mtty_ui::i18n::Lang,
    acp: &AcpSession,
) -> Option<(mtty_acp::Setting, String)> {
    use mtty_ui::i18n::t;
    let name = acp
        .advertised
        .pointer("/agentInfo/name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&acp.profile.name);
    let version = acp
        .advertised
        .pointer("/agentInfo/version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    ui.label(format!("{name} {version}"));
    ui.small(&acp.cwd);
    if let Some(session) = &acp.session {
        ui.horizontal(|ui| {
            ui.label(format!("Session: {session}"));
            if ui.small_button(t(lang, "Copy ID", "复制 ID")).clicked() {
                ctx.copy_text(session.clone());
            }
        });
    }
    if acp
        .advertised
        .get("protocolVersion")
        .and_then(serde_json::Value::as_u64)
        == Some(1)
    {
        ui.label(t(lang, "Agent-reported support", "Agent 报告的支持能力"));
        ui.horizontal_wrapped(|ui| {
            let yes = t(lang, "yes", "是");
            let no = t(lang, "no", "否");
            ui.label(format!(
                "{}: {}",
                t(lang, "Resume", "恢复"),
                if acp.load_supported { yes } else { no }
            ));
            for (label, path) in [
                (
                    t(lang, "Images", "图片"),
                    "/agentCapabilities/promptCapabilities/image",
                ),
                (
                    t(lang, "Audio", "音频"),
                    "/agentCapabilities/promptCapabilities/audio",
                ),
                (
                    t(lang, "Embedded context", "嵌入上下文"),
                    "/agentCapabilities/promptCapabilities/embeddedContext",
                ),
            ] {
                ui.label(format!(
                    "{label}: {}",
                    if acp
                        .advertised
                        .pointer(path)
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false)
                    {
                        yes
                    } else {
                        no
                    }
                ));
            }
        });
    } else {
        ui.label(t(
            lang,
            "Capabilities have not been negotiated",
            "尚未完成能力协商",
        ));
    }
    let mut change = None;
    for setting in &acp.settings {
        ui.horizontal(|ui| {
            ui.label(&setting.name);
            let mut value = setting.current.clone();
            ui.add_enabled_ui(acp.connected && acp.settings_pending.is_none(), |ui| {
                egui::ComboBox::from_id_salt(("acp-setting", acp.epoch, &setting.id))
                    .selected_text(
                        setting
                            .choices
                            .iter()
                            .find(|(id, _)| id == &value)
                            .map(|(_, label)| label.as_str())
                            .unwrap_or(&value),
                    )
                    .show_ui(ui, |ui| {
                        for (id, label) in &setting.choices {
                            ui.selectable_value(&mut value, id.clone(), label);
                        }
                    });
            });
            if value != setting.current {
                change = Some((setting.clone(), value));
            }
        });
    }
    if acp.settings_pending.is_some() {
        ui.spinner();
    }
    if !acp.log.is_empty() {
        let draw = |ui: &mut egui::Ui| {
            let lines: Vec<_> = acp.log.lines().collect();
            let height = ui.text_style_height(&egui::TextStyle::Monospace);
            egui::ScrollArea::both()
                .id_salt(("acp-diagnostics", acp.epoch))
                .max_height(120.0)
                .show_rows(ui, height, lines.len(), |ui, rows| {
                    for row in rows {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(
                                    lines[row].chars().take(4096).collect::<String>(),
                                )
                                .monospace(),
                            )
                            .wrap_mode(egui::TextWrapMode::Extend),
                        );
                    }
                });
        };
        if acp.connected {
            ui.collapsing(t(lang, "Adapter diagnostics", "适配器诊断"), draw);
        } else {
            ui.label(
                egui::RichText::new(t(
                    lang,
                    "Adapter disconnected · diagnostics",
                    "适配器已断开 · 诊断",
                ))
                .strong(),
            );
            draw(ui);
        }
        if ui
            .small_button(t(lang, "Copy diagnostics", "复制诊断"))
            .clicked()
        {
            ctx.copy_text(acp.log.clone());
        }
    }
    ui.collapsing(t(lang, "Negotiated capabilities", "协商能力"), |ui| {
        let text = serde_json::to_string_pretty(&acp.advertised).unwrap_or_default();
        egui::ScrollArea::vertical()
            .max_height(120.0)
            .show(ui, |ui| {
                ui.label(egui::RichText::new(text).monospace());
            });
    });
    change
}
