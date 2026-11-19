//! The application menu, shared by the hosts (ADR 0031).
//!
//! macOS puts this in the system menu bar (`muda`) when the native host runs
//! from an app bundle — which is also where it gets the application icon the OS
//! menu bar wants; otherwise [`crate::chrome`] draws it inside the window. One
//! table keeps the two renderings from drifting apart.
//!
//! `shortcut` uses muda's accelerator syntax (`CmdOrCtrl+Shift+D`), which the
//! in-window renderer currently ignores.

use crate::chrome::MenuId;
use crate::i18n::{t, Lang};

/// One entry of a menu.
pub enum Entry {
    Item {
        label: String,
        id: MenuId,
        shortcut: Option<&'static str>,
    },
    Separator,
    Link {
        label: String,
        url: &'static str,
    },
}

/// The menus, in order, with the active language's labels.
pub fn menus(lang: Lang) -> Vec<(&'static str, Vec<Entry>)> {
    let item = |label: &str, id: MenuId, shortcut: Option<&'static str>| Entry::Item {
        label: label.to_string(),
        id,
        shortcut,
    };
    vec![
        (
            t(lang, "File", "文件"),
            vec![
                item(
                    t(lang, "New Tab", "新建标签"),
                    MenuId::NewTab,
                    Some("CmdOrCtrl+T"),
                ),
                item(
                    t(lang, "Duplicate Tab", "复制标签"),
                    MenuId::DuplicateTab,
                    None,
                ),
                item(
                    t(lang, "Reopen Last Closed", "重开最近关闭"),
                    MenuId::ReopenClosed,
                    Some("CmdOrCtrl+Shift+Z"),
                ),
                item(
                    t(lang, "Close Pane / Tab", "关闭 Pane/标签"),
                    MenuId::ClosePane,
                    Some("CmdOrCtrl+W"),
                ),
                Entry::Separator,
                item(
                    t(lang, "New SSH Session…", "新建 SSH 会话…"),
                    MenuId::NewSsh,
                    None,
                ),
                item(
                    t(
                        lang,
                        "New Serial/Telnet/TCP Session…",
                        "新建串口/Telnet/TCP 会话…",
                    ),
                    MenuId::NewTransport,
                    None,
                ),
                item(
                    t(lang, "Open Remote File…", "打开远端文件…"),
                    MenuId::OpenRemote,
                    None,
                ),
                Entry::Separator,
                item(
                    t(lang, "Save Recipe…", "保存配方…"),
                    MenuId::SaveRecipe,
                    None,
                ),
                item(
                    t(lang, "Open Recipe…", "打开配方…"),
                    MenuId::OpenRecipe,
                    None,
                ),
                Entry::Separator,
                item(t(lang, "Open File…", "打开文件…"), MenuId::OpenFile, None),
                item(t(lang, "Save", "保存"), MenuId::Save, Some("CmdOrCtrl+S")),
                Entry::Separator,
                item(t(lang, "Quit", "退出"), MenuId::Quit, Some("CmdOrCtrl+Q")),
            ],
        ),
        (
            t(lang, "Edit", "编辑"),
            vec![
                item(t(lang, "Copy", "复制"), MenuId::Copy, Some("CmdOrCtrl+C")),
                item(t(lang, "Paste", "粘贴"), MenuId::Paste, Some("CmdOrCtrl+V")),
                item(
                    t(lang, "Copy as ANSI", "复制为 ANSI"),
                    MenuId::CopyAnsi,
                    None,
                ),
                item(
                    t(lang, "Paste Escaped", "转义粘贴"),
                    MenuId::PasteEscaped,
                    None,
                ),
                Entry::Separator,
                item(
                    t(lang, "Select All", "全选"),
                    MenuId::SelectAll,
                    Some("CmdOrCtrl+A"),
                ),
                Entry::Separator,
                item(
                    t(lang, "Replace…", "替换…"),
                    MenuId::Replace,
                    Some("CmdOrCtrl+Alt+F"),
                ),
                // ⌃G stays the terminal's (BEL); the editor maps it itself.
                item(t(lang, "Go to Line…", "跳转到行…"), MenuId::GoToLine, None),
                item(
                    t(lang, "Go to Symbol in File…", "转到文件中的符号…"),
                    MenuId::GoToSymbol,
                    None,
                ),
                item(
                    t(lang, "Toggle Markdown Preview", "开关 Markdown 预览"),
                    MenuId::MarkdownPreview,
                    None,
                ),
            ],
        ),
        (
            t(lang, "View", "视图"),
            vec![
                item(
                    t(lang, "Toggle Sidebar", "开关侧栏"),
                    MenuId::ToggleSidebar,
                    Some("CmdOrCtrl+Shift+L"),
                ),
                item(
                    t(lang, "Toggle Details", "开关详情"),
                    MenuId::ToggleDetails,
                    Some("CmdOrCtrl+Shift+R"),
                ),
                Entry::Separator,
                item(
                    t(lang, "Increase Font Size", "增大字号"),
                    MenuId::FontUp,
                    Some("CmdOrCtrl+="),
                ),
                item(
                    t(lang, "Decrease Font Size", "减小字号"),
                    MenuId::FontDown,
                    Some("CmdOrCtrl+-"),
                ),
                item(
                    t(lang, "Reset Font Size", "重置字号"),
                    MenuId::FontReset,
                    Some("CmdOrCtrl+0"),
                ),
                Entry::Separator,
                item(
                    t(lang, "Settings", "设置"),
                    MenuId::Settings,
                    Some("CmdOrCtrl+,"),
                ),
                Entry::Separator,
                item(
                    t(lang, "Command Palette", "命令面板"),
                    MenuId::Palette,
                    Some("CmdOrCtrl+K"),
                ),
                item(t(lang, "Find…", "查找…"), MenuId::Find, Some("CmdOrCtrl+F")),
                item(
                    t(lang, "Find Next", "查找下一个"),
                    MenuId::FindNext,
                    Some("CmdOrCtrl+G"),
                ),
                item(
                    t(lang, "Find Previous", "查找上一个"),
                    MenuId::FindPrev,
                    Some("CmdOrCtrl+Shift+G"),
                ),
                item(
                    t(lang, "Find in All Tabs", "在所有标签中查找"),
                    MenuId::FindInAllTabs,
                    None,
                ),
                Entry::Separator,
                item(
                    t(lang, "Toggle Full Screen", "全屏切换"),
                    MenuId::Fullscreen,
                    Some("Ctrl+CmdOrCtrl+F"),
                ),
            ],
        ),
        (
            t(lang, "Shell", "终端"),
            vec![
                item(
                    t(lang, "Split Right", "向右分屏"),
                    MenuId::SplitRight,
                    Some("CmdOrCtrl+D"),
                ),
                item(
                    t(lang, "Split Down", "向下分屏"),
                    MenuId::SplitDown,
                    Some("CmdOrCtrl+Shift+D"),
                ),
                Entry::Separator,
                item(t(lang, "Clear Screen", "清屏"), MenuId::ClearScreen, None),
                item(
                    t(lang, "Clear Scrollback", "清除回滚"),
                    MenuId::ClearScrollback,
                    None,
                ),
                item(t(lang, "Read Only", "只读"), MenuId::ReadOnly, None),
                item(
                    t(lang, "Open Link (Hint Mode)", "打开链接（提示模式）"),
                    MenuId::HintMode,
                    Some("CmdOrCtrl+Shift+H"),
                ),
                item(t(lang, "Picture in Picture", "画中画"), MenuId::Pip, None),
                Entry::Separator,
                item(t(lang, "Copy Path", "复制路径"), MenuId::CopyPath, None),
                item(
                    t(lang, "Reveal in File Manager", "在文件管理器中显示"),
                    MenuId::RevealCwd,
                    None,
                ),
                item(
                    t(lang, "Open Externally", "用系统默认程序打开"),
                    MenuId::OpenExternally,
                    None,
                ),
            ],
        ),
        (
            t(lang, "Agent", "Agent"),
            vec![
                item("Composer", MenuId::Composer, Some("CmdOrCtrl+Shift+E")),
                item(
                    t(lang, "Quick Terminal", "快速终端"),
                    MenuId::QuickTerminal,
                    Some("CmdOrCtrl+Shift+T"),
                ),
                Entry::Separator,
                item(
                    t(lang, "Check for Updates", "检查更新"),
                    MenuId::CheckUpdates,
                    None,
                ),
            ],
        ),
        (
            t(lang, "Help", "帮助"),
            vec![Entry::Link {
                label: t(lang, "Documentation", "文档").to_string(),
                url: "https://github.com/mttydev/mtty#readme",
            }],
        ),
    ]
}

/// Stable string ids for the OS menu bar, one per command. `documentation` has
/// no command: the handler opens its URL directly.
pub const IDS: &[(&str, MenuId)] = &[
    ("new-tab", MenuId::NewTab),
    ("duplicate-tab", MenuId::DuplicateTab),
    ("reopen-closed", MenuId::ReopenClosed),
    ("close-pane", MenuId::ClosePane),
    ("new-ssh", MenuId::NewSsh),
    ("new-transport", MenuId::NewTransport),
    ("open-remote", MenuId::OpenRemote),
    ("save-recipe", MenuId::SaveRecipe),
    ("open-recipe", MenuId::OpenRecipe),
    ("open-file", MenuId::OpenFile),
    ("save", MenuId::Save),
    ("quit", MenuId::Quit),
    ("copy", MenuId::Copy),
    ("paste", MenuId::Paste),
    ("copy-ansi", MenuId::CopyAnsi),
    ("paste-escaped", MenuId::PasteEscaped),
    ("select-all", MenuId::SelectAll),
    ("toggle-sidebar", MenuId::ToggleSidebar),
    ("toggle-details", MenuId::ToggleDetails),
    ("font-up", MenuId::FontUp),
    ("font-down", MenuId::FontDown),
    ("font-reset", MenuId::FontReset),
    ("settings", MenuId::Settings),
    ("palette", MenuId::Palette),
    ("find", MenuId::Find),
    ("replace", MenuId::Replace),
    ("go-to-line", MenuId::GoToLine),
    ("go-to-symbol", MenuId::GoToSymbol),
    ("markdown-preview", MenuId::MarkdownPreview),
    ("find-next", MenuId::FindNext),
    ("find-prev", MenuId::FindPrev),
    ("find-all-tabs", MenuId::FindInAllTabs),
    ("fullscreen", MenuId::Fullscreen),
    ("split-right", MenuId::SplitRight),
    ("split-down", MenuId::SplitDown),
    ("clear-screen", MenuId::ClearScreen),
    ("clear-scrollback", MenuId::ClearScrollback),
    ("read-only", MenuId::ReadOnly),
    ("hint-mode", MenuId::HintMode),
    ("pip", MenuId::Pip),
    ("copy-path", MenuId::CopyPath),
    ("reveal-cwd", MenuId::RevealCwd),
    ("open-externally", MenuId::OpenExternally),
    ("composer", MenuId::Composer),
    ("quick-terminal", MenuId::QuickTerminal),
    ("check-updates", MenuId::CheckUpdates),
];

/// The stable id of a command, for the OS menu bar.
pub fn key(id: MenuId) -> &'static str {
    IDS.iter()
        .find(|(_, i)| *i == id)
        .map(|(k, _)| *k)
        .unwrap_or("")
}

/// The inverse of [`key`]; `documentation` maps to no command.
pub fn from_key(key: &str) -> Option<MenuId> {
    IDS.iter().find(|(k, _)| *k == key).map(|(_, i)| *i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_round_trip() {
        for (id_key, id) in IDS {
            assert_eq!(from_key(id_key), Some(*id), "{id_key} does not round-trip");
            assert_eq!(key(*id), *id_key, "{id:?} does not round-trip");
            assert!(
                IDS.iter().filter(|(k, _)| k == id_key).count() == 1,
                "duplicate key {id_key}"
            );
        }
    }

    #[test]
    fn menus_cover_the_table() {
        let menus = menus(Lang::En);
        let titles: Vec<&str> = menus.iter().map(|(t, _)| *t).collect();
        assert_eq!(
            titles,
            vec!["File", "Edit", "View", "Shell", "Agent", "Help"],
            "menu titles"
        );
        let ids: Vec<MenuId> = menus
            .iter()
            .flat_map(|(_, entries)| entries.iter())
            .filter_map(|e| match e {
                Entry::Item { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        for id in ids {
            assert!(!key(id).is_empty(), "{id:?} has no stable key");
        }
    }
}
