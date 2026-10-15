//! Host-aware navigation and explicit process/session continuity information.
use super::*;
use mtty_ui::file_link::{self, FileLink};

#[derive(Clone, Copy, Default)]
pub(super) enum Recovery {
    #[default]
    New,
    History,
    Reattached,
    Reconnect,
}

impl State {
    pub(super) fn open_file_reference(&mut self, pane_id: &str, link: &FileLink) {
        let Some((tab, pane)) = self.tabs.iter().find_map(|tab| {
            tab.panes
                .iter()
                .find(|pane| pane.id == pane_id)
                .map(|pane| (tab, pane))
        }) else {
            return;
        };
        if tab.ssh || pane.term.remote_cwd().is_some() {
            let Some(dest) = tab.ssh_target.clone() else {
                self.show_notice(
                    mtty_ui::i18n::t(
                        self.lang,
                        "SSH target unavailable for this pane",
                        "此窗格的 SSH 目标不可用",
                    )
                    .into(),
                );
                return;
            };
            let path = match file_link::remote_path(&link.path, pane.term.remote_cwd()) {
                Ok(path) => path,
                Err(error) => {
                    self.show_notice(error);
                    return;
                }
            };
            let existing = self.tabs.iter().enumerate().find_map(|(index, tab)| {
                tab.editors
                    .iter()
                    .find(|ed| {
                        ed.remote
                            .as_ref()
                            .is_some_and(|remote| remote.dest == dest && remote.path == path)
                    })
                    .map(|ed| (index, ed.id.clone()))
            });
            if let Some((index, id)) = existing {
                self.active_tab = index;
                self.tabs[index].active = id;
                self.go_active_editor_to(
                    Some(link.line),
                    link.column.map(|column| column.saturating_sub(1)),
                );
            } else {
                let line = link.line;
                let column = link.column;
                self.spawn_job(move || JobDone::RemoteLinkRead {
                    result: mtty_ui::ssh::read_remote(&dest, &path),
                    dest,
                    path,
                    line,
                    column,
                });
            }
        } else {
            let path = file_link::local_path(&link.path, pane.term.cwd());
            if self.open_editor_ro(path, false) {
                self.go_active_editor_to(
                    Some(link.line),
                    link.column.map(|column| column.saturating_sub(1)),
                );
            }
        }
        self.window.request_redraw();
    }

    pub(super) fn pane_context_rows(&self, tab: &Tab, pane: &Pane) -> Vec<(String, String)> {
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let recovery = match pane.recovery {
            Recovery::New => t(lang, "New session", "新建会话"),
            Recovery::History => t(
                lang,
                "History restored; a new process was started",
                "已恢复历史，运行的是新进程",
            ),
            Recovery::Reattached => t(lang, "Reattached to the persistent PTY", "已重连持久 PTY"),
            Recovery::Reconnect => t(
                lang,
                "New SSH connection; remote continuity depends on tmux/mosh",
                "新的 SSH 连接；远程进程连续性取决于 tmux/mosh",
            ),
        };
        let mut rows = vec![(t(lang, "Recovery", "恢复方式").into(), recovery.into())];
        if pane.term.is_hosted() {
            rows.push((
                t(lang, "PTY state", "PTY 状态").into(),
                if pane.term.exited() {
                    t(lang, "Exited", "已退出")
                } else {
                    t(lang, "Live persistent host", "持久宿主仍在线")
                }
                .into(),
            ));
        }
        if let Some(command) = pane.term.foreground_command() {
            rows.push((t(lang, "Foreground process", "前台进程").into(), command));
        }
        if tab.ssh {
            rows.push((
                t(lang, "Host", "主机").into(),
                tab.ssh_target.clone().unwrap_or_else(|| "—".into()),
            ));
            let status = if pane.on_enter.is_some() {
                t(
                    lang,
                    "Disconnected; Enter starts a new SSH connection",
                    "已断开；回车会启动新的 SSH 连接",
                )
            } else if pane.term.remote_cwd().is_some() {
                t(lang, "Remote directory confirmed", "已确认远程目录")
            } else {
                t(lang, "Remote directory not reported", "尚未报告远程目录")
            };
            rows.push((
                t(lang, "Remote context", "远程上下文").into(),
                status.into(),
            ));
        }
        rows
    }
}
