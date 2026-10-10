//! Reusable egui chrome widgets for mtty and embedders. They only take plain
//! data and return actions, so they are independent of any host's state type.

use crate::theme::{Chrome as ChromeColors, Rgb};

/// How a shortcut is written on this platform: macOS's ⌘ chord, or the
/// Linux/Windows one (see the keymap in mtty-widget).
pub fn shortcut_hint(mac: &'static str, pc: &'static str) -> &'static str {
    if cfg!(target_os = "macos") {
        mac
    } else {
        pc
    }
}

pub fn fg_color(t: &crate::theme::Theme) -> egui::Color32 {
    egui::Color32::from_rgb(t.fg.0, t.fg.1, t.fg.2)
}

pub fn bg_color(c: Rgb) -> egui::Color32 {
    egui::Color32::from_rgb(c.0, c.1, c.2)
}

pub fn section(ch: &ChromeColors, text: &str) -> egui::RichText {
    egui::RichText::new(text.to_uppercase())
        .size(10.0)
        .strong()
        .color(bg_color(ch.muted))
}

/// What the user did in the tab bar.
#[derive(Default)]
pub struct TabBarEvents {
    pub switch: Option<usize>,
    pub close: Option<usize>,
    pub rename: Option<usize>,
    pub mark: Option<usize>,
    pub group: Option<usize>,
    pub ungroup: Option<usize>,
    pub reorder: Option<(usize, usize)>,
    pub duplicate: Option<usize>,
    pub close_others: Option<usize>,
    pub close_below: Option<usize>,
    pub move_up: Option<usize>,
    pub move_down: Option<usize>,
    pub set_prefix: Option<usize>,
    pub new_tab: bool,
    /// Where each tab chip was drawn, in tab order: stable targets for
    /// replayed-input tests and accessibility tooling.
    pub tab_rects: Vec<egui::Rect>,
}

/// A row action from the right-click menu shared by the tab bar and the session
/// list (wording is kept consistent where the two overlap).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabMenuAction {
    Rename,
    Prefix,
    Mark,
    Group,
    Ungroup,
    Duplicate,
    MoveUp,
    MoveDown,
    NewTab,
    Close,
    CloseOthers,
    CloseBelow,
}

/// The context-menu entries in order; `None` is a separator.
pub fn tab_menu_items(lang: Lang) -> Vec<(&'static str, Option<TabMenuAction>)> {
    vec![
        (
            t(lang, "Rename Tab…", "重命名标签…"),
            Some(TabMenuAction::Rename),
        ),
        (t(lang, "Prefix…", "前缀…"), Some(TabMenuAction::Prefix)),
        (t(lang, "Mark…", "标记…"), Some(TabMenuAction::Mark)),
        (t(lang, "Group…", "分组…"), Some(TabMenuAction::Group)),
        (
            t(lang, "Remove from Group", "移出分组"),
            Some(TabMenuAction::Ungroup),
        ),
        (
            t(lang, "Duplicate Tab", "复制标签"),
            Some(TabMenuAction::Duplicate),
        ),
        ("", None),
        (t(lang, "Move Up", "上移"), Some(TabMenuAction::MoveUp)),
        (t(lang, "Move Down", "下移"), Some(TabMenuAction::MoveDown)),
        ("", None),
        (t(lang, "New Tab", "新建标签"), Some(TabMenuAction::NewTab)),
        ("", None),
        (t(lang, "Close Tab", "关闭标签"), Some(TabMenuAction::Close)),
        (
            t(lang, "Close Other Tabs", "关闭其他标签"),
            Some(TabMenuAction::CloseOthers),
        ),
        (
            t(lang, "Close Below", "关闭下方标签"),
            Some(TabMenuAction::CloseBelow),
        ),
    ]
}

/// Record the chosen action for tab `i`.
pub fn apply_tab_menu(ev: &mut TabBarEvents, i: usize, action: TabMenuAction) {
    match action {
        TabMenuAction::Rename => ev.rename = Some(i),
        TabMenuAction::Prefix => ev.set_prefix = Some(i),
        TabMenuAction::Mark => ev.mark = Some(i),
        TabMenuAction::Group => ev.group = Some(i),
        TabMenuAction::Ungroup => ev.ungroup = Some(i),
        TabMenuAction::Duplicate => ev.duplicate = Some(i),
        TabMenuAction::MoveUp => ev.move_up = Some(i),
        TabMenuAction::MoveDown => ev.move_down = Some(i),
        TabMenuAction::NewTab => ev.new_tab = true,
        TabMenuAction::Close => ev.close = Some(i),
        TabMenuAction::CloseOthers => ev.close_others = Some(i),
        TabMenuAction::CloseBelow => ev.close_below = Some(i),
    }
}

/// Attach the row menu to a tab/session row. `has_group` hides the
/// "Remove from Group" entry when the row is not in a group.
pub fn tab_row_menu(
    resp: &egui::Response,
    lang: Lang,
    i: usize,
    has_group: bool,
    ev: &mut TabBarEvents,
) {
    resp.context_menu(|ui| {
        for (label, action) in tab_menu_items(lang) {
            match action {
                None => {
                    ui.separator();
                }
                Some(action) if !tab_menu_action_visible(action, has_group) => {}
                Some(action) => {
                    if ui.button(label).clicked() {
                        apply_tab_menu(ev, i, action);
                        ui.close_menu();
                    }
                }
            }
        }
    });
}

/// Ungroup is only meaningful for a grouped tab.
pub fn tab_menu_action_visible(action: TabMenuAction, has_group: bool) -> bool {
    action != TabMenuAction::Ungroup || has_group
}

/// A divider belongs between adjacent tabs whose group differs, never before
/// the first tab. Missing metadata is treated as an ungrouped tab.
pub fn tab_group_boundary(groups: &[Option<String>], i: usize) -> bool {
    i > 0
        && groups.get(i - 1).and_then(|g| g.as_deref()) != groups.get(i).and_then(|g| g.as_deref())
}

/// The sidebar's saved hosts, grouped; a click asks to connect.
pub fn host_list(
    ui: &mut egui::Ui,
    ch: &ChromeColors,
    hosts: &[(String, Option<String>)],
    lang: Lang,
) -> Option<usize> {
    let mut picked = None;
    ui.add_space(10.0);
    ui.label(section(
        ch,
        &format!("{} ({})", t(lang, "Hosts", "主机"), hosts.len()),
    ));
    ui.separator();
    egui::ScrollArea::vertical()
        .id_salt("host_list")
        .max_height(240.0)
        .show(ui, |ui| {
            let mut last_group: Option<&Option<String>> = None;
            for (i, (label, group)) in hosts.iter().enumerate() {
                if last_group != Some(group) {
                    if let Some(name) = group {
                        ui.label(
                            egui::RichText::new(name)
                                .size(10.5)
                                .color(bg_color(ch.muted)),
                        );
                    }
                    last_group = Some(group);
                }
                ui.horizontal(|ui| {
                    let (irect, _) =
                        ui.allocate_exact_size(egui::Vec2::splat(14.0), egui::Sense::hover());
                    crate::icons::draw(
                        ui.painter(),
                        irect,
                        crate::icons::Icon::Server,
                        bg_color(ch.text),
                    );
                    if ui
                        .selectable_label(false, egui::RichText::new(label).size(12.0))
                        .on_hover_text(t(lang, "Connect", "连接"))
                        .clicked()
                    {
                        picked = Some(i);
                    }
                });
            }
        });
    picked
}

/// A horizontal tab bar: clickable, draggable labels, a close affordance, `+`.
pub fn tab_bar(
    ui: &mut egui::Ui,
    ch: &ChromeColors,
    titles: &[String],
    icons: &[crate::icons::TabIcon],
    groups: &[Option<String>],
    active: usize,
    lang: Lang,
) -> TabBarEvents {
    const DRAG_ID: &str = "miao_tab_drag";
    let mut ev = TabBarEvents::default();
    ui.visuals_mut().selection.bg_fill = bg_color(ch.active);
    ui.visuals_mut().override_text_color = Some(bg_color(ch.text));
    let font = egui::FontId::proportional(13.0);
    let text_color = bg_color(ch.text);
    let mut rects: Vec<egui::Rect> = Vec::with_capacity(titles.len());
    for (i, title) in titles.iter().enumerate() {
        if tab_group_boundary(groups, i) {
            let (divider, _) = ui.allocate_exact_size(egui::vec2(6.0, 22.0), egui::Sense::hover());
            ui.painter().line_segment(
                [
                    divider.center_top() + egui::vec2(0.0, 3.0),
                    divider.center_bottom() - egui::vec2(0.0, 3.0),
                ],
                egui::Stroke::new(1.0_f32, bg_color(ch.border)),
            );
        }
        let icon = icons
            .get(i)
            .cloned()
            .unwrap_or_else(|| crate::icons::Icon::Terminal.into());
        let galley = ui
            .painter()
            .layout_no_wrap(title.clone(), font.clone(), text_color);
        let closable = titles.len() > 1;
        let extra = if closable { 46.0 } else { 30.0 };
        let desired = egui::vec2(galley.size().x + extra, 22.0);
        let (rect, resp) = ui.allocate_exact_size(desired, egui::Sense::click_and_drag());
        let bg = if i == active {
            bg_color(ch.active)
        } else if resp.hovered() {
            bg_color(ch.hover)
        } else {
            egui::Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, 4.0, bg);
        let ir = egui::Rect::from_center_size(
            egui::pos2(rect.left() + 12.0, rect.center().y),
            egui::Vec2::splat(14.0),
        );
        crate::icons::draw_tab_icon(ui.painter(), ir, &icon, text_color);
        let resp = match icon.hint.as_deref() {
            Some(hint) => resp.on_hover_text(format!("{title}\n{hint}")),
            None => resp,
        };
        let pos = rect.min + egui::vec2(22.0, (rect.height() - galley.size().y) * 0.5);
        ui.painter().galley(pos, galley, text_color);
        // Close affordance, inside the chip.
        if closable {
            let xr = egui::Rect::from_center_size(
                egui::pos2(rect.right() - 11.0, rect.center().y),
                egui::Vec2::splat(14.0),
            );
            let x_resp = ui.interact(xr, ui.id().with(("tabclose", i)), egui::Sense::click());
            let ccol = if x_resp.hovered() {
                text_color
            } else {
                bg_color(ch.muted)
            };
            let (a, b) = (xr.shrink(4.0), xr.shrink(4.0));
            let stroke = egui::Stroke::new(1.4_f32, ccol);
            ui.painter()
                .line_segment([a.left_top(), b.right_bottom()], stroke);
            ui.painter()
                .line_segment([a.right_top(), b.left_bottom()], stroke);
            if x_resp.clicked() {
                ev.close = Some(i);
            }
        }
        if ev.close.is_none() && resp.clicked() {
            ev.switch = Some(i);
        }
        if closable && resp.clicked_by(egui::PointerButton::Middle) {
            ev.close = Some(i);
        }
        if resp.double_clicked() {
            ev.rename = Some(i);
        }
        tab_row_menu(
            &resp,
            lang,
            i,
            groups.get(i).is_some_and(|g| g.is_some()),
            &mut ev,
        );
        if resp.drag_started() {
            ui.ctx()
                .memory_mut(|m| m.data.insert_temp(egui::Id::new(DRAG_ID), i));
        }
        rects.push(rect);
        ui.add_space(2.0);
    }
    ev.tab_rects = rects.clone();
    // Move the dragged chip live, so the tabs visibly follow the pointer
    // instead of only snapping on release. `from` tracks the chip's current
    // index as the order changes, so the drag converges rather than oscillates.
    let pressed = ui.ctx().input(|i| i.pointer.any_down());
    let from = ui
        .ctx()
        .memory(|m| m.data.get_temp::<usize>(egui::Id::new(DRAG_ID)));
    if let (true, Some(from)) = (pressed, from) {
        if let Some(p) = ui.ctx().pointer_interact_pos() {
            let mut target = from;
            for (j, r) in rects.iter().enumerate() {
                if p.x >= r.left() && p.x <= r.right() {
                    target = j;
                    break;
                }
            }
            if target != from {
                ev.reorder = Some((from, target));
                ui.ctx()
                    .memory_mut(|m| m.data.insert_temp(egui::Id::new(DRAG_ID), target));
            }
        }
    } else if from.is_some() {
        ui.ctx()
            .memory_mut(|m| m.data.remove_temp::<usize>(egui::Id::new(DRAG_ID)));
    }
    if ui
        .button("+")
        .on_hover_text(t(lang, "New Tab", "新建标签"))
        .clicked()
    {
        ev.new_tab = true;
    }
    ev
}

/// A vertical session list for the sidebar. Returns what the user did; the
/// right-click menu mirrors the tab bar's ([`tab_menu_items`]).
#[allow(clippy::too_many_arguments)]
pub fn sidebar(
    ui: &mut egui::Ui,
    ch: &ChromeColors,
    titles: &[String],
    icons: &[crate::icons::TabIcon],
    badges: &[Option<Rgb>],
    metas: &[String],
    locations: &[String],
    groups: &[Option<String>],
    active: usize,
    heading: &str,
    lang: Lang,
) -> TabBarEvents {
    const DRAG_ID: &str = "miao_session_drag";
    let mut ev = TabBarEvents::default();
    ui.visuals_mut().selection.bg_fill = bg_color(ch.active);
    ui.visuals_mut().override_text_color = Some(bg_color(ch.text));
    ui.label(section(ch, &format!("{heading} ({})", titles.len())));
    ui.separator();
    let text_color = bg_color(ch.text);
    let font = egui::FontId::proportional(13.0);
    let meta_font = egui::FontId::proportional(10.5);
    let mut rects: Vec<egui::Rect> = Vec::with_capacity(titles.len());
    for (i, title) in titles.iter().enumerate() {
        // The tab bar's group divider, laid horizontally (ADR 0011).
        if tab_group_boundary(groups, i) {
            let (r, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 7.0), egui::Sense::hover());
            ui.painter().hline(
                r.x_range().shrink(4.0),
                r.center().y,
                egui::Stroke::new(1.0_f32, bg_color(ch.border)),
            );
        }
        // One painted row (icon, badge, title, shortcut) so the whole row can
        // be clicked and dragged, like the tab bar's chips.
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 24.0),
            egui::Sense::click_and_drag(),
        );
        let bg = if i == active {
            bg_color(ch.active)
        } else if resp.hovered() {
            bg_color(ch.hover)
        } else {
            egui::Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, 5.0, bg);
        let icon = icons
            .get(i)
            .cloned()
            .unwrap_or_else(|| crate::icons::Icon::Terminal.into());
        let ir = egui::Rect::from_center_size(
            egui::pos2(rect.left() + 12.0, rect.center().y),
            egui::Vec2::splat(14.0),
        );
        crate::icons::draw_tab_icon(ui.painter(), ir, &icon, text_color);
        if let Some(c) = badges.get(i).copied().flatten() {
            ui.painter().circle_filled(
                egui::pos2(rect.left() + 26.0, rect.center().y),
                3.0,
                bg_color(c),
            );
        }
        // Under the pointer the row's shortcut gives way to a close button
        // (closing the last session is left to the menu).
        let show_close = titles.len() > 1 && ui.rect_contains_pointer(rect);
        let meta = metas
            .get(i)
            .filter(|m| !m.is_empty() && !show_close)
            .map(|m| {
                ui.painter()
                    .layout_no_wrap(m.clone(), meta_font.clone(), bg_color(ch.muted))
            });
        let meta_w = if show_close {
            24.0
        } else {
            meta.as_ref().map_or(0.0, |g| g.size().x + 8.0)
        };
        let text_left = rect.left() + 34.0;
        let title_room = (rect.right() - 6.0 - meta_w - text_left).max(8.0);
        let mut job =
            egui::text::LayoutJob::simple_singleline(title.clone(), font.clone(), text_color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(title_room);
        let galley = ui.painter().layout_job(job);
        let truncated = galley.rows.first().is_some_and(|r| r.ends_with_newline)
            || ui
                .painter()
                .layout_no_wrap(title.clone(), font.clone(), text_color)
                .size()
                .x
                > title_room;
        ui.painter().galley(
            egui::pos2(text_left, rect.center().y - galley.size().y * 0.5),
            galley,
            text_color,
        );
        if let Some(g) = meta {
            ui.painter().galley(
                egui::pos2(
                    rect.right() - 6.0 - g.size().x,
                    rect.center().y - g.size().y * 0.5,
                ),
                g,
                text_color,
            );
        }
        if show_close {
            let xr = egui::Rect::from_center_size(
                egui::pos2(rect.right() - 14.0, rect.center().y),
                egui::Vec2::splat(18.0),
            );
            let x_resp = ui
                .interact(xr, ui.id().with(("session_close", i)), egui::Sense::click())
                .on_hover_text(t(lang, "Close Tab", "关闭标签"));
            if x_resp.hovered() {
                ui.painter().rect_filled(xr, 4.0, bg_color(ch.hover));
            }
            let color = if x_resp.hovered() {
                text_color
            } else {
                bg_color(ch.muted)
            };
            let x = xr.shrink(5.0);
            let stroke = egui::Stroke::new(1.4_f32, color);
            ui.painter()
                .line_segment([x.left_top(), x.right_bottom()], stroke);
            ui.painter()
                .line_segment([x.right_top(), x.left_bottom()], stroke);
            if x_resp.clicked() {
                ev.close = Some(i);
            }
        }
        // Hover shows where the session is (its folder, file or host),
        // and the title in full when the row cuts it off.
        let details = locations
            .get(i)
            .filter(|l| !l.is_empty())
            .map(String::as_str)
            .into_iter()
            .chain(icon.hint.as_deref())
            .collect::<Vec<_>>()
            .join("\n");
        let resp = match (details.is_empty(), truncated) {
            (false, true) => resp.on_hover_text(format!("{title}\n{details}")),
            (false, false) => resp.on_hover_text(details),
            (true, true) => resp.on_hover_text(title),
            (true, false) => resp,
        };
        if ev.close != Some(i) && resp.clicked() {
            ev.switch = Some(i);
        }
        if resp.double_clicked() {
            ev.rename = Some(i);
        }
        if titles.len() > 1 && resp.clicked_by(egui::PointerButton::Middle) {
            ev.close = Some(i);
        }
        tab_row_menu(
            &resp,
            lang,
            i,
            groups.get(i).is_some_and(|g| g.is_some()),
            &mut ev,
        );
        if resp.drag_started() {
            ui.ctx()
                .memory_mut(|m| m.data.insert_temp(egui::Id::new(DRAG_ID), i));
        }
        rects.push(rect);
    }
    ev.tab_rects = rects.clone();
    // Reorder live while a row is dragged (same idea as `tab_bar`).
    let pressed = ui.ctx().input(|i| i.pointer.any_down());
    let from = ui
        .ctx()
        .memory(|m| m.data.get_temp::<usize>(egui::Id::new(DRAG_ID)));
    if let (true, Some(from)) = (pressed, from) {
        if let Some(p) = ui.ctx().pointer_interact_pos() {
            let target = rects
                .iter()
                .position(|r| p.y >= r.top() && p.y <= r.bottom())
                .unwrap_or(from);
            if target != from {
                ev.reorder = Some((from, target));
                ui.ctx()
                    .memory_mut(|m| m.data.insert_temp(egui::Id::new(DRAG_ID), target));
            }
        }
    } else if from.is_some() {
        ui.ctx()
            .memory_mut(|m| m.data.remove_temp::<usize>(egui::Id::new(DRAG_ID)));
    }
    ev
}

/// A row of selectable details tabs; returns the newly selected index.
pub fn details_tabs(
    ui: &mut egui::Ui,
    ch: &ChromeColors,
    tabs: &[(crate::icons::Icon, &str)],
    active: usize,
) -> Option<usize> {
    let mut sel = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for (i, (icon, label)) in tabs.iter().enumerate() {
            // Icon-only tabs (the label is a tooltip), like the reference app.
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(26.0, 22.0), egui::Sense::click());
            let bg = if i == active {
                Some(bg_color(ch.active))
            } else if resp.hovered() {
                Some(bg_color(ch.hover))
            } else {
                None
            };
            if let Some(b) = bg {
                ui.painter().rect_filled(rect, egui::Rounding::same(5.0), b);
            }
            let ir = egui::Rect::from_center_size(rect.center(), egui::Vec2::splat(15.0));
            crate::icons::draw(ui.painter(), ir, *icon, bg_color(ch.text));
            if resp.on_hover_text(*label).clicked() {
                sel = Some(i);
            }
        }
    });
    sel
}

/// Actions from the prompt-queue widget.
#[derive(Default)]
pub struct QueueEvents {
    pub add: bool,
    pub send: Option<usize>,
    pub remove: Option<usize>,
    pub send_all: bool,
    pub clear: bool,
}

/// A minimal prompt queue: type a prompt, queue it, then send to the shell.
pub fn queue(
    ui: &mut egui::Ui,
    ch: &ChromeColors,
    items: &[String],
    input: &mut String,
    lang: Lang,
) -> QueueEvents {
    let mut ev = QueueEvents::default();
    ui.visuals_mut().override_text_color = Some(bg_color(ch.text));
    ui.label(section(
        ch,
        &format!("{} ({})", t(lang, "Queue", "队列"), items.len()),
    ));
    ui.horizontal_wrapped(|ui| {
        ui.add(
            egui::TextEdit::singleline(input)
                .hint_text(t(lang, "Prompt to run…", "要执行的提示…"))
                .desired_width(160.0),
        );
        if ui.button(t(lang, "Add", "添加")).clicked() {
            ev.add = true;
        }
        if ui.button(t(lang, "Send All", "全部发送")).clicked() {
            ev.send_all = true;
        }
        if ui.button(t(lang, "Clear", "清空")).clicked() {
            ev.clear = true;
        }
    });
    ui.separator();
    for (i, item) in items.iter().enumerate() {
        // Wrapping keeps a long prompt from widening the panel.
        ui.horizontal_wrapped(|ui| {
            if ui
                .small_button("\u{25b6}")
                .on_hover_text(t(lang, "Send", "发送"))
                .clicked()
            {
                ev.send = Some(i);
            }
            if ui.small_button("\u{00d7}").clicked() {
                ev.remove = Some(i);
            }
            ui.label(egui::RichText::new(item).monospace().size(12.0));
        });
    }
    ev
}

/// A two-column label/value info list (details panel).
pub fn info(ui: &mut egui::Ui, ch: &ChromeColors, title: &str, rows: &[(String, String)]) {
    ui.visuals_mut().override_text_color = Some(bg_color(ch.text));
    ui.label(section(ch, title));
    ui.add_space(4.0);
    for (k, v) in rows {
        ui.label(egui::RichText::new(k).color(bg_color(ch.muted)).size(11.0));
        ui.label(egui::RichText::new(v).monospace().size(12.0));
        ui.add_space(2.0);
    }
}

use crate::i18n::{t, Lang};

fn panel_frame(ch: &ChromeColors, margin: egui::Margin) -> egui::Frame {
    panel_frame_fill(ch, margin, ch.bg)
}

/// Like [`panel_frame`] but with an explicit surface colour, so the session
/// list and the details inspector can sit above/below the terminal card.
fn panel_frame_fill(ch: &ChromeColors, margin: egui::Margin, fill: Rgb) -> egui::Frame {
    panel_frame_stroke(margin, fill, ch.hover)
}

/// A panel frame with an explicit fill and stroke (the side panels use the
/// border colour so their edge reads as a separator, like `[sidebar]`).
fn panel_frame_stroke(margin: egui::Margin, fill: Rgb, stroke: Rgb) -> egui::Frame {
    egui::Frame::default()
        .fill(bg_color(fill))
        .stroke(egui::Stroke::new(1.0_f32, bg_color(stroke)))
        .inner_margin(margin)
}

/// One row in a details list (Files / Ports / Git / Outline): a single line
/// with an icon, a label and right-aligned meta.
pub struct ChromeItem {
    pub icon: crate::icons::TabIcon,
    pub label: String,
    pub meta: String,
    /// Label colour; falls back to the panel text colour.
    pub label_color: Option<Rgb>,
}

/// Render `items` as a compact one-line-per-row list.
pub fn list(ui: &mut egui::Ui, ch: &ChromeColors, items: &[ChromeItem]) {
    ui.visuals_mut().override_text_color = Some(bg_color(ch.text));
    let muted = bg_color(ch.muted);
    for it in items {
        ui.horizontal(|ui| {
            let (irect, _) = ui.allocate_exact_size(egui::Vec2::splat(14.0), egui::Sense::hover());
            crate::icons::draw_tab_icon(ui.painter(), irect, &it.icon, bg_color(ch.text));
            ui.add_space(2.0);
            // Lay the row out right-to-left so the meta hugs the edge, then the
            // label fills what is left and truncates. A long row must never ask
            // for more width than the panel already has.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                if !it.meta.is_empty() {
                    ui.label(egui::RichText::new(&it.meta).size(10.5).color(muted));
                    ui.add_space(6.0);
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let label = egui::RichText::new(&it.label).monospace().size(12.0);
                    let label = match it.label_color {
                        Some(c) => label.color(bg_color(c)),
                        None => label,
                    };
                    ui.label(label);
                });
            });
        });
    }
}

/// A menu command id; each host maps it to its own action.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuId {
    NewTab,
    ClosePane,
    OpenFile,
    Save,
    SaveRecipe,
    OpenRecipe,
    NewSsh,
    NewTransport,
    OpenRemote,
    Composer,
    QuickTerminal,
    CheckUpdates,
    Copy,
    Paste,
    SplitRight,
    SplitDown,
    ToggleSidebar,
    ToggleDetails,
    FontUp,
    FontDown,
    FontReset,
    Settings,
    Palette,
    Find,
    /// Find with the replace field (an editor pane).
    Replace,
    /// Go to a line in an editor pane.
    GoToLine,
    /// Go to a symbol in the active editor (ADR 0034, E6).
    GoToSymbol,
    /// Open or close the Markdown preview beside an editor pane.
    MarkdownPreview,
    DuplicateTab,
    ReopenClosed,
    ClearScrollback,
    SelectAll,
    CopyAnsi,
    PasteEscaped,
    FindNext,
    FindPrev,
    UseSelForFind,
    JumpToSel,
    FindInAllTabs,
    Fullscreen,
    ClearScreen,
    ReadOnly,
    HintMode,
    Pip,
    CopyPath,
    RevealCwd,
    OpenExternally,
    Quit,
}

/// One tab as the chrome needs it.
pub struct ChromeTab {
    pub title: String,
    pub badge: Option<Rgb>,
    pub icon: crate::icons::TabIcon,
    /// Where the tab is (a folder, a file, an ssh host), shown when its
    /// sidebar row is hovered; empty for none.
    pub location: String,
}

/// The host implements this; [`render`] draws the surrounding UI from it and
/// calls the `on_*` methods to report user actions.
#[allow(unused_variables)]
pub trait Chrome {
    fn lang(&self) -> Lang {
        Lang::En
    }
    fn tabs(&self) -> Vec<ChromeTab> {
        Vec::new()
    }
    fn active_tab(&self) -> usize {
        0
    }
    fn show_sidebar(&self) -> bool {
        true
    }
    fn show_details(&self) -> bool {
        true
    }
    fn details_tab(&self) -> usize {
        0
    }
    fn details_title(&self) -> String {
        String::new()
    }
    fn details_rows(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn details_is_queue(&self) -> bool {
        false
    }
    fn read_only(&self) -> bool {
        false
    }
    /// A short right-side status chip (running program / agent), if any.
    fn status_right(&self) -> String {
        String::new()
    }
    /// Whether the host draws the menu bar inside the window. macOS inside an
    /// app bundle returns false: the menu lives in the system menu bar there
    /// (ADR 0031).
    fn draws_menu_bar(&self) -> bool {
        true
    }
    /// Windows places its menu and caption controls in the tab row.
    fn window_controls(&self) -> bool {
        false
    }
    fn window_maximized(&self) -> bool {
        false
    }
    fn on_minimize_window(&mut self) {}
    fn on_maximize_window(&mut self) {}

    /// The host's active theme (colours for the whole chrome).
    fn theme(&self) -> crate::theme::Theme {
        crate::theme::Theme::default()
    }
    /// A single-line list view for list-like tabs; `None` falls back to k/v.
    fn details_list(&self) -> Option<Vec<ChromeItem>> {
        None
    }
    /// Let the host render the whole details body (e.g. a file tree). Return
    /// true if it drew something.
    fn details_body(&mut self, ui: &mut egui::Ui, lang: Lang) -> bool {
        let _ = (ui, lang);
        false
    }
    fn status(&self) -> String {
        String::new()
    }
    fn queue(&self) -> Vec<String> {
        Vec::new()
    }
    fn take_queue_input(&mut self) -> String {
        String::new()
    }
    fn set_queue_input(&mut self, input: String) {}

    fn on_new_tab(&mut self) {}
    fn on_switch_tab(&mut self, i: usize) {}
    fn on_close_tab(&mut self, i: usize) {}
    fn on_rename_tab(&mut self, i: usize) {}
    /// The group label of each tab, in order (`None` = ungrouped).
    fn tab_groups(&self) -> Vec<Option<String>> {
        Vec::new()
    }
    fn on_mark_tab(&mut self, i: usize) {}
    fn on_group_tab(&mut self, i: usize) {}
    fn on_ungroup_tab(&mut self, i: usize) {}
    fn on_reorder_tab(&mut self, from: usize, to: usize) {}
    fn on_duplicate_tab(&mut self, i: usize) {}
    fn on_close_others(&mut self, i: usize) {}
    fn on_close_below(&mut self, i: usize) {}
    fn on_move_tab(&mut self, i: usize, delta: i32) {}
    fn on_set_prefix(&mut self, i: usize) {}
    fn on_font_delta(&mut self, delta: f32) {}
    fn on_toggle_sidebar(&mut self) {}
    fn on_toggle_details(&mut self) {}
    fn on_details_tab(&mut self, i: usize) {}
    fn on_queue_add(&mut self) {}
    fn on_queue_send(&mut self, i: usize) {}
    fn on_queue_remove(&mut self, i: usize) {}
    fn on_queue_send_all(&mut self) {}
    fn on_queue_clear(&mut self) {}
    fn on_menu(&mut self, id: MenuId) {}

    /// Screen-space rect of every pane in the active tab that wants its own
    /// close button: all panes of a split tab, plus a lone editor pane. Empty
    /// when the tab's own close affordance covers every pane.
    fn pane_close_rects(&self) -> Vec<(String, egui::Rect)> {
        Vec::new()
    }
    fn on_close_pane(&mut self, id: &str) {
        let _ = id;
    }
    /// Starting widths of the side panels (restored from the last session).
    fn sidebar_width(&self) -> f32 {
        CHROME_SIDEBAR_W
    }
    fn details_width(&self) -> f32 {
        CHROME_DETAILS_W
    }
    /// The panels' actual widths after this frame (`None` when hidden).
    fn on_panel_widths(&mut self, sidebar: Option<f32>, details: Option<f32>) {}
    /// Saved SSH hosts for the sidebar, in display order: (label, group).
    fn hosts(&self) -> Vec<(String, Option<String>)> {
        Vec::new()
    }
    /// The user picked `hosts()[i]`.
    fn on_host_connect(&mut self, i: usize) {}
    /// Room the title row leaves at its leading edge for window controls the
    /// OS draws over it (macOS traffic lights on a transparent title bar).
    /// 0 when the OS draws its own title bar.
    fn titlebar_inset(&self) -> f32 {
        0.0
    }
    /// Whether the pointer rests on empty title-row space this frame, where a
    /// press moves the window (only meaningful with a [`Self::titlebar_inset`]).
    fn on_title_drag_hover(&mut self, hovered: bool) {}
}

pub const CHROME_MENU_H: f32 = 24.0;
/// The title row above the terminal, and the sidebar header beside it. On
/// macOS it shares the transparent title bar with the traffic lights.
pub const CHROME_TITLE_H: f32 = 30.0;
pub const CHROME_STATUS_H: f32 = 22.0;
pub const CHROME_SIDEBAR_W: f32 = 200.0;
pub const CHROME_DETAILS_W: f32 = 300.0;
/// How far the side panels can be dragged (their default is the width above).
pub const SIDEBAR_RANGE: std::ops::RangeInclusive<f32> = 140.0..=480.0;
pub const DETAILS_RANGE: std::ops::RangeInclusive<f32> = 200.0..=640.0;

/// Width reserved for the title row's right-hand controls (panel toggles and
/// font size), mirrored on the left so the title stays centred.
const TITLE_CONTROLS_W: f32 = 120.0;

/// Empty space that moves the window: a click-sensing backdrop over `rect`.
/// Widgets added after it sit on top, so it only reports hover where none is.
fn drag_region(ui: &mut egui::Ui, rect: egui::Rect, id: &str) -> bool {
    ui.interact(rect, ui.id().with(id), egui::Sense::click())
        .hovered()
}

/// Where the terminal grid starts: below the in-window menu (if drawn) and the
/// title row. [`render`] lays the panels out to match.
pub fn content_top(draws_menu_bar: bool) -> f32 {
    if draws_menu_bar {
        CHROME_MENU_H + CHROME_TITLE_H
    } else {
        CHROME_TITLE_H
    }
}

/// Whether egui shows a resize cursor: the pointer is on a panel or window
/// edge, so a press there belongs to the UI even outside the panel itself.
pub fn resize_cursor(icon: egui::CursorIcon) -> bool {
    use egui::CursorIcon::*;
    matches!(
        icon,
        ResizeHorizontal
            | ResizeColumn
            | ResizeEast
            | ResizeWest
            | ResizeVertical
            | ResizeRow
            | ResizeNorth
            | ResizeSouth
            | ResizeNeSw
            | ResizeNwSe
            | ResizeNorthEast
            | ResizeNorthWest
            | ResizeSouthEast
            | ResizeSouthWest
    )
}

/// The width a side panel actually has this frame (after the user dragged its
/// edge), so the host lays the terminal out beside it.
fn panel_width(ctx: &egui::Context, id: &str) -> Option<f32> {
    egui::containers::panel::PanelState::load(ctx, egui::Id::new(id)).map(|p| p.rect.width())
}

/// Draw the whole surrounding UI (menu, tabs, sidebar, details, status).
pub fn render(ctx: &egui::Context, host: &mut impl Chrome) {
    use crate::icons::{icon_button, Icon};
    let theme = host.theme();
    let ch = theme.chrome();
    let lang = host.lang();

    // Snapshot (owned), so nothing borrows the host while egui closures run.
    let tabs = host.tabs();
    let titles: Vec<String> = tabs.iter().map(|t| t.title.clone()).collect();
    let tab_groups = host.tab_groups();
    let badges: Vec<Option<Rgb>> = tabs.iter().map(|t| t.badge).collect();
    let icons: Vec<crate::icons::TabIcon> = tabs.iter().map(|t| t.icon.clone()).collect();
    let locations: Vec<String> = tabs.iter().map(|t| t.location.clone()).collect();
    let metas: Vec<String> = (0..titles.len())
        .map(|i| {
            if i < 9 {
                format!("{}{}", shortcut_hint("\u{2318}", "Alt+"), i + 1)
            } else {
                String::new()
            }
        })
        .collect();
    let active = host.active_tab();
    let show_sidebar = host.show_sidebar();
    let show_details = host.show_details();
    let details_tab = host.details_tab();
    let details_title = host.details_title();
    let details_rows = host.details_rows();
    let details_is_queue = host.details_is_queue();
    let host_read_only = host.read_only();
    let details_list = host.details_list();
    let status = host.status();
    let status_right = host.status_right();
    let queue_items = host.queue();
    let saved_hosts = host.hosts();
    let mut host_connect: Option<usize> = None;
    let mut queue_input = host.take_queue_input();

    let mut menu: Option<MenuId> = None;
    let mut minimize_window = false;
    let mut maximize_window = false;
    let mut switch = None;
    let mut close = None;
    let mut rename = None;
    let mut reorder = None;
    let mut duplicate = None;
    let mut close_others = None;
    let mut close_below = None;
    let mut move_up = None;
    let mut move_down = None;
    let mut set_prefix = None;
    let mut mark_tab = None;
    let mut group_tab = None;
    let mut ungroup_tab = None;
    let mut new_tab = false;
    let mut font_delta = 0.0f32;
    let mut toggle_sidebar = false;
    let mut toggle_details = false;
    let mut details_sel = None;
    let mut qev = QueueEvents::default();

    let menu_item = |ui: &mut egui::Ui, label: &str, id: MenuId, out: &mut Option<MenuId>| {
        if ui.button(label).clicked() {
            *out = Some(id);
            ui.close_menu();
        }
    };

    if host.draws_menu_bar() {
        let menu_table = crate::menu::menus(lang);
        egui::TopBottomPanel::top("menu")
            .exact_height(CHROME_MENU_H)
            .frame(panel_frame(&ch, egui::Margin::symmetric(6.0, 1.0)))
            .show(ctx, |ui| {
                // Menu-bar styling: transparent idle, subtle rounded hover/active.
                {
                    let v = ui.visuals_mut();
                    v.override_text_color = Some(bg_color(ch.text));
                    v.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
                    v.widgets.hovered.weak_bg_fill = bg_color(ch.hover);
                    v.widgets.active.weak_bg_fill = bg_color(ch.active);
                    v.widgets.hovered.bg_fill = bg_color(ch.hover);
                    v.widgets.active.bg_fill = bg_color(ch.active);
                    let r = egui::Rounding::same(5.0);
                    v.widgets.inactive.rounding = r;
                    v.widgets.hovered.rounding = r;
                    v.widgets.active.rounding = r;
                }
                ui.style_mut().spacing.button_padding = egui::vec2(8.0, 3.0);
                ui.style_mut().spacing.item_spacing.x = 2.0;
                ui.horizontal(|ui| {
                    for (title, entries) in &menu_table {
                        ui.menu_button(*title, |ui| {
                            for entry in entries {
                                match entry {
                                    crate::menu::Entry::Item { label, id, .. } => {
                                        let label = if *id == MenuId::ReadOnly && host_read_only {
                                            format!("{label}  \u{2713}")
                                        } else {
                                            label.clone()
                                        };
                                        menu_item(ui, &label, *id, &mut menu);
                                    }
                                    crate::menu::Entry::Separator => {
                                        ui.separator();
                                    }
                                    crate::menu::Entry::Link { label, url } => {
                                        ui.hyperlink_to(label, url);
                                    }
                                }
                            }
                        });
                    }
                });
            });
    }

    // The window frame: the session list runs the full height of the window, its
    // header beside the title row. With a transparent macOS title bar the
    // traffic lights sit over that header, so its controls keep to the right.
    let inset = host.titlebar_inset();
    let mut title_drag_hover = false;
    if show_sidebar {
        egui::SidePanel::left("sessions")
            .resizable(true)
            .default_width(host.sidebar_width())
            .width_range(SIDEBAR_RANGE)
            .frame(panel_frame_stroke(
                egui::Margin::same(6.0),
                ch.sidebar,
                ch.border,
            ))
            .show(ctx, |ui| {
                // The header lines up with the title row (the frame's top
                // margin is part of that height).
                let (header, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), CHROME_TITLE_H - 6.0),
                    egui::Sense::hover(),
                );
                title_drag_hover |= drag_region(ui, header.expand(6.0), "sidebar_header");
                ui.allocate_new_ui(
                    egui::UiBuilder::new()
                        .max_rect(header)
                        .layout(egui::Layout::right_to_left(egui::Align::Center)),
                    |ui| {
                        if icon_button(ui, Icon::Sidebar, bg_color(ch.text))
                            .on_hover_text(t(lang, "Toggle sidebar", "开关侧栏"))
                            .clicked()
                        {
                            toggle_sidebar = true;
                        }
                        if ui
                            .button("+")
                            .on_hover_text(t(lang, "New Tab", "新建标签"))
                            .clicked()
                        {
                            new_tab = true;
                        }
                    },
                );
                let heading = t(lang, "Sessions", "会话");
                let ev = sidebar(
                    ui,
                    &ch,
                    &titles,
                    &icons,
                    &badges,
                    &metas,
                    &locations,
                    &tab_groups,
                    active,
                    heading,
                    lang,
                );
                switch = ev.switch.or(switch);
                close = ev.close.or(close);
                rename = ev.rename.or(rename);
                reorder = ev.reorder.or(reorder);
                duplicate = ev.duplicate.or(duplicate);
                close_others = ev.close_others.or(close_others);
                close_below = ev.close_below.or(close_below);
                move_up = ev.move_up.or(move_up);
                move_down = ev.move_down.or(move_down);
                set_prefix = ev.set_prefix.or(set_prefix);
                mark_tab = ev.mark.or(mark_tab);
                group_tab = ev.group.or(group_tab);
                ungroup_tab = ev.ungroup.or(ungroup_tab);
                new_tab = new_tab || ev.new_tab;
                if !saved_hosts.is_empty() {
                    host_connect = host_list(ui, &ch, &saved_hosts, lang).or(host_connect);
                }
            });
    }

    // The title row: the active tab's title, or the tab strip while the
    // sidebar is hidden; panel and font controls at the top right.
    egui::TopBottomPanel::top("title")
        .exact_height(CHROME_TITLE_H)
        .frame(panel_frame(&ch, egui::Margin::symmetric(6.0, 3.0)))
        .show(ctx, |ui| {
            title_drag_hover |=
                drag_region(ui, ui.max_rect().expand2(egui::vec2(6.0, 3.0)), "title_row");
            ui.horizontal(|ui| {
                if show_sidebar {
                    let row = ui.max_rect();
                    let mut job = egui::text::LayoutJob::simple_singleline(
                        titles.get(active).cloned().unwrap_or_default(),
                        egui::FontId::proportional(13.0),
                        bg_color(ch.text),
                    );
                    // Leave the right-hand controls their room on both sides
                    // so the title stays centred.
                    job.wrap = egui::text::TextWrapping::truncate_at_width(
                        (row.width() - 2.0 * TITLE_CONTROLS_W).max(40.0),
                    );
                    let galley = ui.painter().layout_job(job);
                    ui.painter().galley(
                        row.center() - galley.size() * 0.5,
                        galley,
                        bg_color(ch.text),
                    );
                } else {
                    ui.add_space(inset);
                    let ev = tab_bar(ui, &ch, &titles, &icons, &tab_groups, active, lang);
                    switch = ev.switch;
                    close = ev.close;
                    rename = ev.rename;
                    reorder = ev.reorder;
                    duplicate = ev.duplicate;
                    close_others = ev.close_others;
                    close_below = ev.close_below;
                    move_up = ev.move_up;
                    move_down = ev.move_down;
                    set_prefix = ev.set_prefix;
                    mark_tab = ev.mark;
                    group_tab = ev.group;
                    ungroup_tab = ev.ungroup;
                    new_tab = ev.new_tab;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if host.window_controls() {
                        if ui
                            .button("×")
                            .on_hover_text(t(lang, "Close window", "关闭窗口"))
                            .clicked()
                        {
                            menu = Some(MenuId::Quit);
                        }
                        if ui
                            .button(if host.window_maximized() {
                                "❐"
                            } else {
                                "□"
                            })
                            .on_hover_text(t(lang, "Maximize / restore", "最大化 / 还原"))
                            .clicked()
                        {
                            maximize_window = true;
                        }
                        if ui
                            .button("−")
                            .on_hover_text(t(lang, "Minimize", "最小化"))
                            .clicked()
                        {
                            minimize_window = true;
                        }
                    }
                    if ui.button("A+").clicked() {
                        font_delta = 1.0;
                    }
                    if ui.button("A-").clicked() {
                        font_delta = -1.0;
                    }
                    // This layout runs right to left: details sits to the right of sidebar.
                    if icon_button(ui, Icon::Details, bg_color(ch.text))
                        .on_hover_text(t(lang, "Toggle details", "开关详情"))
                        .clicked()
                    {
                        toggle_details = true;
                    }
                    if icon_button(ui, Icon::Sidebar, bg_color(ch.text))
                        .on_hover_text(t(lang, "Toggle sidebar", "开关侧栏"))
                        .clicked()
                    {
                        toggle_sidebar = true;
                    }
                    if host.window_controls() {
                        ui.menu_button("≡", |ui| {
                            for (title, entries) in crate::menu::menus(lang) {
                                ui.menu_button(title, |ui| {
                                    for entry in entries {
                                        match entry {
                                            crate::menu::Entry::Item { label, id, .. } => {
                                                let label =
                                                    if id == MenuId::ReadOnly && host_read_only {
                                                        format!("{label}  \u{2713}")
                                                    } else {
                                                        label
                                                    };
                                                menu_item(ui, &label, id, &mut menu);
                                            }
                                            crate::menu::Entry::Separator => {
                                                ui.separator();
                                            }
                                            crate::menu::Entry::Link { label, url } => {
                                                ui.hyperlink_to(label, url);
                                            }
                                        }
                                    }
                                });
                            }
                        })
                        .response
                        .on_hover_text(t(lang, "Menu", "菜单"));
                    }
                });
            });
        });
    host.on_title_drag_hover(title_drag_hover);
    if minimize_window {
        host.on_minimize_window();
    }
    if maximize_window {
        host.on_maximize_window();
    }

    if show_details {
        let host = &mut *host;
        let tabs_icons = [
            (Icon::Info, t(lang, "Info", "信息")),
            (Icon::Agent, "Agent"),
            (Icon::Outline, t(lang, "Outline", "大纲")),
            (Icon::Git, "Git"),
            (Icon::Files, t(lang, "Files", "文件")),
            (Icon::Ports, t(lang, "Ports", "端口")),
            (Icon::Queue, t(lang, "Queue", "队列")),
        ];
        egui::SidePanel::right("details")
            .resizable(true)
            .default_width(host.details_width())
            .width_range(DETAILS_RANGE)
            .frame(panel_frame_stroke(
                egui::Margin::same(8.0),
                ch.details,
                ch.border,
            ))
            .show(ctx, |ui| {
                if let Some(i) = details_tabs(ui, &ch, &tabs_icons, details_tab) {
                    details_sel = Some(i);
                }
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if !host.details_body(ui, lang) {
                            if details_is_queue {
                                qev = queue(ui, &ch, &queue_items, &mut queue_input, lang);
                            } else if let Some(items) = &details_list {
                                list(ui, &ch, items);
                            } else {
                                info(ui, &ch, &details_title, &details_rows);
                            }
                        }
                    });
            });
    }

    egui::TopBottomPanel::bottom("status")
        .exact_height(CHROME_STATUS_H)
        .frame(panel_frame(&ch, egui::Margin::symmetric(8.0, 2.0)))
        .show(ctx, |ui| {
            ui.visuals_mut().override_text_color = Some(bg_color(ch.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "{}  {}",
                        shortcut_hint("\u{2318}K", "Ctrl+Shift+K"),
                        t(lang, "commands", "命令")
                    ))
                    .size(11.0)
                    .color(bg_color(ch.muted)),
                );
                if !status_right.is_empty() {
                    ui.label(
                        egui::RichText::new(format!("\u{25cf} {status_right}"))
                            .size(11.0)
                            .color(bg_color(ch.positive)),
                    );
                    ui.add_space(10.0);
                }
                ui.add_sized(
                    [ui.available_width(), CHROME_STATUS_H],
                    egui::Label::new(egui::RichText::new(&status).size(11.0)).truncate(),
                )
                .on_hover_text(&status);
            });
        });

    // Apply.
    host.set_queue_input(queue_input);
    if let Some(id) = menu {
        host.on_menu(id);
    }
    host.on_panel_widths(
        show_sidebar.then(|| panel_width(ctx, "sessions")).flatten(),
        show_details.then(|| panel_width(ctx, "details")).flatten(),
    );
    if let Some(i) = host_connect {
        host.on_host_connect(i);
    }
    if let Some(i) = switch {
        host.on_switch_tab(i);
    }
    // Per-pane close button, top-right of each pane. The host decides which
    // panes want one: every pane of a split tab, and a lone editor pane, whose
    // tab has no other close affordance when the sidebar is shown.
    let mut close_pane: Option<String> = None;
    {
        let panes = host.pane_close_rects();
        if !panes.is_empty() {
            for (id, r) in &panes {
                egui::Area::new(egui::Id::new(("pane-close", id)))
                    .order(egui::Order::Foreground)
                    .fixed_pos(egui::pos2(r.max.x - 24.0, r.min.y + 6.0))
                    .show(ctx, |ui| {
                        let (rect, resp) =
                            ui.allocate_exact_size(egui::vec2(18.0, 16.0), egui::Sense::click());
                        if resp.hovered() {
                            ui.painter().rect_filled(
                                rect,
                                egui::Rounding::same(4.0),
                                bg_color(ch.hover),
                            );
                        }
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "\u{00d7}",
                            egui::FontId::proportional(13.0),
                            if resp.hovered() {
                                bg_color(ch.text)
                            } else {
                                bg_color(ch.muted)
                            },
                        );
                        if resp.clicked() {
                            close_pane = Some(id.clone());
                        }
                    });
            }
        }
    }
    if let Some(id) = close_pane {
        host.on_close_pane(&id);
    }

    if let Some(i) = close {
        host.on_close_tab(i);
    }
    if let Some(i) = rename {
        host.on_rename_tab(i);
    }
    if let Some((from, to)) = reorder {
        host.on_reorder_tab(from, to);
    }
    if let Some(i) = duplicate {
        host.on_duplicate_tab(i);
    }
    if let Some(i) = close_others {
        host.on_close_others(i);
    }
    if let Some(i) = close_below {
        host.on_close_below(i);
    }
    if let Some(i) = move_up {
        host.on_move_tab(i, -1);
    }
    if let Some(i) = move_down {
        host.on_move_tab(i, 1);
    }
    if let Some(i) = set_prefix {
        host.on_set_prefix(i);
    }
    if let Some(i) = mark_tab {
        host.on_mark_tab(i);
    }
    if let Some(i) = group_tab {
        host.on_group_tab(i);
    }
    if let Some(i) = ungroup_tab {
        host.on_ungroup_tab(i);
    }
    if new_tab {
        host.on_new_tab();
    }
    if font_delta != 0.0 {
        host.on_font_delta(font_delta);
    }
    if toggle_sidebar {
        host.on_toggle_sidebar();
    }
    if toggle_details {
        host.on_toggle_details();
    }
    if let Some(i) = details_sel {
        host.on_details_tab(i);
    }
    if qev.add {
        host.on_queue_add();
    }
    if let Some(i) = qev.send {
        host.on_queue_send(i);
    }
    if let Some(i) = qev.remove {
        host.on_queue_remove(i);
    }
    if qev.send_all {
        host.on_queue_send_all();
    }
    if qev.clear {
        host.on_queue_clear();
    }
}

#[cfg(test)]
mod tab_menu_tests {
    use super::*;

    fn actions(lang: Lang) -> Vec<TabMenuAction> {
        tab_menu_items(lang)
            .into_iter()
            .filter_map(|(_, a)| a)
            .collect()
    }

    #[test]
    fn menu_matches_tab_context_wording_and_order() {
        let labels: Vec<&str> = tab_menu_items(Lang::En)
            .into_iter()
            .filter(|(l, _)| !l.is_empty())
            .map(|(l, _)| l)
            .collect();
        assert_eq!(
            labels,
            vec![
                "Rename Tab…",
                "Prefix…",
                "Mark…",
                "Group…",
                "Remove from Group",
                "Duplicate Tab",
                "Move Up",
                "Move Down",
                "New Tab",
                "Close Tab",
                "Close Other Tabs",
                "Close Below",
            ]
        );
        assert_eq!(
            actions(Lang::En),
            vec![
                TabMenuAction::Rename,
                TabMenuAction::Prefix,
                TabMenuAction::Mark,
                TabMenuAction::Group,
                TabMenuAction::Ungroup,
                TabMenuAction::Duplicate,
                TabMenuAction::MoveUp,
                TabMenuAction::MoveDown,
                TabMenuAction::NewTab,
                TabMenuAction::Close,
                TabMenuAction::CloseOthers,
                TabMenuAction::CloseBelow,
            ]
        );
    }

    #[test]
    fn both_languages_describe_the_same_menu() {
        let en = tab_menu_items(Lang::En);
        let zh = tab_menu_items(Lang::Zh);
        assert_eq!(en.len(), zh.len());
        for (i, (en_item, zh_item)) in en.into_iter().zip(zh).enumerate() {
            assert_eq!(en_item.1, zh_item.1, "item {i} has a different action");
            assert_eq!(
                en_item.0.is_empty(),
                zh_item.0.is_empty(),
                "item {i}: separators must line up"
            );
            if !en_item.0.is_empty() {
                assert!(!zh_item.0.is_empty(), "item {i} has no Chinese label");
            }
        }
    }

    #[test]
    fn actions_record_the_right_event() {
        let mut ev = TabBarEvents::default();
        apply_tab_menu(&mut ev, 3, TabMenuAction::CloseOthers);
        assert_eq!(ev.close_others, Some(3));
        assert_eq!(ev.close, None);
        apply_tab_menu(&mut ev, 2, TabMenuAction::Close);
        assert_eq!(ev.close, Some(2));
        apply_tab_menu(&mut ev, 1, TabMenuAction::MoveUp);
        assert_eq!(ev.move_up, Some(1));
        apply_tab_menu(&mut ev, 0, TabMenuAction::MoveDown);
        assert_eq!(ev.move_down, Some(0));
        apply_tab_menu(&mut ev, 4, TabMenuAction::Prefix);
        assert_eq!(ev.set_prefix, Some(4));
        apply_tab_menu(&mut ev, 4, TabMenuAction::Rename);
        assert_eq!(ev.rename, Some(4));
        apply_tab_menu(&mut ev, 4, TabMenuAction::Duplicate);
        assert_eq!(ev.duplicate, Some(4));
        apply_tab_menu(&mut ev, 4, TabMenuAction::CloseBelow);
        assert_eq!(ev.close_below, Some(4));
        apply_tab_menu(&mut ev, 4, TabMenuAction::NewTab);
        assert!(ev.new_tab);
        apply_tab_menu(&mut ev, 5, TabMenuAction::Mark);
        assert_eq!(ev.mark, Some(5));
        apply_tab_menu(&mut ev, 5, TabMenuAction::Group);
        assert_eq!(ev.group, Some(5));
        apply_tab_menu(&mut ev, 5, TabMenuAction::Ungroup);
        assert_eq!(ev.ungroup, Some(5));
    }

    #[test]
    fn group_boundaries_include_entering_and_leaving_groups() {
        let groups = vec![
            None,
            Some("a".into()),
            Some("a".into()),
            Some("b".into()),
            None,
            None,
        ];
        let boundaries: Vec<_> = (0..groups.len())
            .filter(|&i| tab_group_boundary(&groups, i))
            .collect();
        assert_eq!(boundaries, vec![1, 3, 4]);
        assert!(!tab_group_boundary(&[Some("a".into())], 0));
        assert!(!tab_group_boundary(&[], 1));
    }

    #[test]
    fn menu_visibility_and_separator_positions() {
        for lang in [Lang::En, Lang::Zh] {
            let entries = tab_menu_items(lang);
            let separators: Vec<_> = entries
                .iter()
                .enumerate()
                .filter_map(|(i, (_, action))| action.is_none().then_some(i))
                .collect();
            assert_eq!(separators, vec![6, 9, 11]);
            for (_, action) in entries {
                if let Some(action) = action {
                    assert!(tab_menu_action_visible(action, true));
                    assert_eq!(
                        tab_menu_action_visible(action, false),
                        action != TabMenuAction::Ungroup
                    );
                }
            }
        }
    }

    /// Run sidebar frames with the pointer events given, returning the
    /// events of the last frame.
    fn sidebar_frames(titles: &[String], frames: &[Vec<egui::Event>]) -> TabBarEvents {
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "tabler".into(),
            egui::FontData::from_static(include_bytes!(
                "../../../assets/fonts/tabler-icons-subset.ttf"
            ))
            .into(),
        );
        fonts.families.insert(
            egui::FontFamily::Name("tabler".into()),
            vec!["tabler".into()],
        );
        ctx.set_fonts(fonts);
        let icons = vec![crate::icons::TabIcon::from(crate::icons::Icon::Terminal); titles.len()];
        let metas: Vec<String> = (1..=titles.len()).map(|i| format!("\u{2318}{i}")).collect();
        let groups = vec![None; titles.len()];
        let badges = vec![None; titles.len()];
        let mut last = TabBarEvents::default();
        for events in frames {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(240.0, 400.0),
                )),
                events: events.clone(),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    last = sidebar(
                        ui,
                        &ChromeColors::dark(),
                        titles,
                        &icons,
                        &badges,
                        &metas,
                        &[],
                        &groups,
                        0,
                        "SESSIONS",
                        Lang::En,
                    );
                });
            });
        }
        last
    }

    fn click_at(p: egui::Pos2) -> Vec<Vec<egui::Event>> {
        let button = |pressed| egui::Event::PointerButton {
            pos: p,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        vec![
            vec![egui::Event::PointerMoved(p)],
            vec![egui::Event::PointerMoved(p)],
            vec![button(true)],
            vec![button(false)],
        ]
    }

    #[test]
    fn hovering_a_session_row_shows_a_close_button() {
        let titles: Vec<String> = vec!["one".into(), "two".into(), "three".into()];
        // Sweep down the right edge, where the close button sits: each row
        // closes itself there, and nothing switches along the way.
        let mut first_y = [None; 3];
        for y in (0..200).step_by(3) {
            let ev = sidebar_frames(&titles, &click_at(egui::pos2(212.0, y as f32)));
            if let Some(i) = ev.close {
                assert!(ev.switch.is_none(), "closing does not also switch");
                first_y[i].get_or_insert(y);
            }
        }
        let rows: Vec<i32> = first_y
            .iter()
            .map(|y| y.expect("each row closes"))
            .collect();
        assert!(rows[0] < rows[1] && rows[1] < rows[2]);
        // The row's middle switches instead.
        let ev = sidebar_frames(&titles, &click_at(egui::pos2(80.0, rows[1] as f32 + 4.0)));
        assert_eq!((ev.switch, ev.close), (Some(1), None));
        let y0 = rows[0];
        // A lone session has no close button.
        let ev = sidebar_frames(&titles[..1], &click_at(egui::pos2(212.0, y0 as f32)));
        assert_eq!(ev.close, None);
    }

    #[test]
    fn grouped_tab_bar_actually_paints_dividers() {
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "tabler".into(),
            egui::FontData::from_static(include_bytes!(
                "../../../assets/fonts/tabler-icons-subset.ttf"
            ))
            .into(),
        );
        fonts.families.insert(
            egui::FontFamily::Name("tabler".into()),
            vec!["tabler".into()],
        );
        ctx.set_fonts(fonts);
        let titles = vec!["one".into(), "two".into(), "three".into()];
        let icons = vec![crate::icons::TabIcon::from(crate::icons::Icon::Terminal); 3];
        let groups = vec![Some("a".into()), Some("a".into()), Some("b".into())];
        let frame = |groups: &[Option<String>]| {
            ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        tab_bar(
                            ui,
                            &ChromeColors::dark(),
                            &titles,
                            &icons,
                            groups,
                            0,
                            Lang::En,
                        );
                    });
                });
            })
        };
        let _ = frame(&groups);
        let divider_count = |output: egui::FullOutput| {
            output
                .shapes
                .iter()
                .filter(|shape| {
                    matches!(&shape.shape, egui::Shape::LineSegment { points, .. }
                if (points[0].x - points[1].x).abs() < 0.01
                    && ((points[1].y - points[0].y) - 16.0).abs() < 0.01)
                })
                .count()
        };
        assert_eq!(divider_count(frame(&groups)), 1);
        assert_eq!(divider_count(frame(&[None, None, None])), 0);
    }

    #[test]
    fn dragging_a_tab_onto_another_reorders_them() {
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "tabler".into(),
            egui::FontData::from_static(include_bytes!(
                "../../../assets/fonts/tabler-icons-subset.ttf"
            ))
            .into(),
        );
        fonts.families.insert(
            egui::FontFamily::Name("tabler".into()),
            vec!["tabler".into()],
        );
        ctx.set_fonts(fonts);
        let titles: Vec<String> = vec!["alpha".into(), "beta".into(), "gamma".into()];
        let icons = vec![crate::icons::TabIcon::from(crate::icons::Icon::Terminal); 3];
        let groups = vec![None, None, None];
        let frame = |events: Vec<egui::Event>| {
            let mut ev = TabBarEvents::default();
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 200.0),
                )),
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ev = tab_bar(
                            ui,
                            &ChromeColors::dark(),
                            &titles,
                            &icons,
                            &groups,
                            0,
                            Lang::En,
                        );
                    });
                });
            });
            ev
        };
        let rects = frame(vec![]).tab_rects;
        assert_eq!(rects.len(), 3);
        let from = rects[0].center();
        let to = rects[2].center();
        frame(vec![egui::Event::PointerMoved(from)]);
        frame(vec![egui::Event::PointerButton {
            pos: from,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }]);
        let mut order = vec![0usize, 1, 2];
        let mut saw_switch = false;
        let note = |ev: &TabBarEvents, order: &mut Vec<usize>, saw_switch: &mut bool| {
            if let Some((f, t)) = ev.reorder {
                let x = order.remove(f);
                order.insert(t, x);
            }
            *saw_switch |= ev.switch.is_some();
        };
        for step in 1..=6 {
            let t = step as f32 / 6.0;
            let ev = frame(vec![egui::Event::PointerMoved(from + (to - from) * t)]);
            note(&ev, &mut order, &mut saw_switch);
        }
        let ev = frame(vec![egui::Event::PointerButton {
            pos: to,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        note(&ev, &mut order, &mut saw_switch);
        assert_eq!(order, vec![1, 2, 0], "the chip moves live to the drop spot");
        assert!(!saw_switch, "a drag is not a click");
    }

    struct PanelHost {
        widths: (Option<f32>, Option<f32>),
    }

    impl Chrome for PanelHost {
        fn show_sidebar(&self) -> bool {
            true
        }
        fn show_details(&self) -> bool {
            true
        }
        fn on_panel_widths(&mut self, sidebar: Option<f32>, details: Option<f32>) {
            self.widths = (sidebar, details);
        }
    }

    #[test]
    fn side_panels_can_be_dragged_wider_and_claim_the_pointer() {
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "tabler".into(),
            egui::FontData::from_static(include_bytes!(
                "../../../assets/fonts/tabler-icons-subset.ttf"
            ))
            .into(),
        );
        fonts.families.insert(
            egui::FontFamily::Name("tabler".into()),
            vec!["tabler".into()],
        );
        ctx.set_fonts(fonts);
        let mut host = PanelHost {
            widths: (None, None),
        };
        let cursor = std::cell::Cell::new(egui::CursorIcon::Default);
        let frame = |events: Vec<egui::Event>, host: &mut PanelHost| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 700.0),
                )),
                events,
                ..Default::default()
            };
            cursor.set(
                ctx.run(input, |ctx| render(ctx, host))
                    .platform_output
                    .cursor_icon,
            );
            host.widths
        };
        frame(vec![], &mut host);
        let (left, right) = frame(vec![], &mut host);
        let left = left.expect("sidebar width reported");
        assert!((left - CHROME_SIDEBAR_W).abs() < 1.0, "{left}");
        assert!((right.unwrap() - CHROME_DETAILS_W).abs() < 1.0);
        // Just outside the sidebar's edge, over what the host draws as terminal.
        let start = egui::pos2(left + 2.0, 300.0);
        frame(vec![egui::Event::PointerMoved(start)], &mut host);
        frame(vec![egui::Event::PointerMoved(start)], &mut host);
        // The host decides who owns a press before running the frame. Just
        // outside a panel egui does not report wanting the pointer, but it
        // shows the resize cursor, which the host treats as the UI's.
        assert!(resize_cursor(cursor.get()), "{:?}", cursor.get());
        frame(
            vec![egui::Event::PointerButton {
                pos: start,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
            &mut host,
        );
        assert!(
            ctx.wants_pointer_input(),
            "the panel edge belongs to the UI, not the terminal"
        );
        for step in 1..=5 {
            frame(
                vec![egui::Event::PointerMoved(
                    start + egui::vec2(20.0 * step as f32, 0.0),
                )],
                &mut host,
            );
        }
        let (dragged, _) = frame(
            vec![egui::Event::PointerButton {
                pos: start + egui::vec2(100.0, 0.0),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            &mut host,
        );
        let dragged = dragged.unwrap();
        assert!(
            (dragged - (left + 100.0)).abs() < 3.0,
            "sidebar is now {dragged}"
        );
    }

    /// The details panel keeps the width it was given: long names or prompts
    /// truncate or wrap instead of widening it, so switching icons does not
    /// change the panel's size.
    #[test]
    fn details_content_never_resizes_the_panel() {
        struct Host {
            queue: bool,
            label: String,
            widths: (Option<f32>, Option<f32>),
        }
        impl Chrome for Host {
            fn show_sidebar(&self) -> bool {
                false
            }
            fn show_details(&self) -> bool {
                true
            }
            fn details_is_queue(&self) -> bool {
                self.queue
            }
            fn queue(&self) -> Vec<String> {
                if self.queue {
                    vec![self.label.clone()]
                } else {
                    Vec::new()
                }
            }
            fn details_list(&self) -> Option<Vec<ChromeItem>> {
                if self.queue {
                    None
                } else {
                    Some(vec![ChromeItem {
                        icon: crate::icons::Icon::File.into(),
                        label: self.label.clone(),
                        meta: "876.4 K".into(),
                        label_color: None,
                    }])
                }
            }
            fn on_panel_widths(&mut self, sidebar: Option<f32>, details: Option<f32>) {
                self.widths = (sidebar, details);
            }
        }

        let ctx = test_ctx();
        let mut host = Host {
            queue: false,
            label: "a-name-far-too-long-to-ever-fit-in-the-panel-".repeat(8),
            widths: (None, None),
        };
        let run = |host: &mut Host, frames: usize| {
            for _ in 0..frames {
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 700.0),
                    )),
                    ..Default::default()
                };
                let _ = ctx.run(input, |ctx| render(ctx, host));
            }
            host.widths.1.unwrap_or(0.0)
        };

        // A list row with an over-long label...
        let width = run(&mut host, 2);
        assert!(
            (width - CHROME_DETAILS_W).abs() < 1.0,
            "details width drifted to {width} on the list tab"
        );
        // ...then the user clicks the Queue icon, whose prompt row is also too
        // wide: the panel must stay put.
        host.queue = true;
        let width = run(&mut host, 3);
        assert!(
            (width - CHROME_DETAILS_W).abs() < 1.0,
            "details width drifted to {width} after switching to the queue tab"
        );
    }

    fn test_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "tabler".into(),
            egui::FontData::from_static(include_bytes!(
                "../../../assets/fonts/tabler-icons-subset.ttf"
            ))
            .into(),
        );
        fonts.families.insert(
            egui::FontFamily::Name("tabler".into()),
            vec!["tabler".into()],
        );
        ctx.set_fonts(fonts);
        ctx
    }

    fn press(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// One sidebar frame over a 300x400 screen with three sessions.
    fn sidebar_frame(
        ctx: &egui::Context,
        groups: &[Option<String>],
        events: Vec<egui::Event>,
    ) -> (TabBarEvents, egui::FullOutput) {
        let titles: Vec<String> = vec!["alpha".into(), "beta".into(), "gamma".into()];
        let icons = vec![crate::icons::TabIcon::from(crate::icons::Icon::Terminal); 3];
        let badges = vec![None, Some(Rgb(0xa3, 0xbe, 0x8c)), None];
        let metas = vec!["\u{2318}1".into(), "\u{2318}2".into(), "\u{2318}3".into()];
        let mut ev = TabBarEvents::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(300.0, 400.0),
            )),
            events,
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ev = sidebar(
                    ui,
                    &ChromeColors::dark(),
                    &titles,
                    &icons,
                    &badges,
                    &metas,
                    &[],
                    groups,
                    0,
                    "Sessions",
                    Lang::En,
                );
            });
        });
        (ev, output)
    }

    #[test]
    fn tab_and_sidebar_hover_explain_the_agent_state() {
        for vertical in [false, true] {
            let ctx = test_ctx();
            let mut icon = crate::icons::TabIcon::from(crate::icons::Icon::StatePaused);
            icon.hint = Some("Agent state: Paused · unfinished".into());
            let frame = |time, pointer: Option<egui::Pos2>| {
                let mut ev = TabBarEvents::default();
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(300.0, 400.0),
                        )),
                        time: Some(time),
                        events: pointer.map(egui::Event::PointerMoved).into_iter().collect(),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            ev = if vertical {
                                sidebar(
                                    ui,
                                    &ChromeColors::dark(),
                                    &["Task".into()],
                                    &[icon.clone()],
                                    &[None],
                                    &[],
                                    &["/workspace".into()],
                                    &[],
                                    0,
                                    "Sessions",
                                    Lang::En,
                                )
                            } else {
                                tab_bar(
                                    ui,
                                    &ChromeColors::dark(),
                                    &["Task".into()],
                                    &[icon.clone()],
                                    &[],
                                    0,
                                    Lang::En,
                                )
                            };
                        });
                    },
                );
                (ev, output)
            };
            let pointer = frame(0.0, None).0.tab_rects[0].center();
            frame(0.1, Some(pointer));
            frame(1.0, Some(pointer));
            let output = frame(2.0, Some(pointer)).1;
            assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
                egui::Shape::Text(text) if text.galley.job.text.contains("Agent state: Paused · unfinished")
            )), "state explanation missing from hover (vertical={vertical})");
        }
    }

    #[test]
    fn dragging_a_session_row_reorders_the_tabs() {
        let ctx = test_ctx();
        let groups = vec![None, None, None];
        let rects = sidebar_frame(&ctx, &groups, vec![]).0.tab_rects;
        assert_eq!(rects.len(), 3);
        let (from, to) = (rects[0].center(), rects[2].center());
        sidebar_frame(&ctx, &groups, vec![egui::Event::PointerMoved(from)]);
        sidebar_frame(&ctx, &groups, vec![press(from, true)]);
        let mut order = vec![0usize, 1, 2];
        let mut saw_switch = false;
        let note = |ev: &TabBarEvents, order: &mut Vec<usize>, saw_switch: &mut bool| {
            if let Some((f, t)) = ev.reorder {
                let x = order.remove(f);
                order.insert(t, x);
            }
            *saw_switch |= ev.switch.is_some();
        };
        for step in 1..=6 {
            let t = step as f32 / 6.0;
            let p = from + (to - from) * t;
            let ev = sidebar_frame(&ctx, &groups, vec![egui::Event::PointerMoved(p)]).0;
            note(&ev, &mut order, &mut saw_switch);
        }
        let ev = sidebar_frame(&ctx, &groups, vec![press(to, false)]).0;
        note(&ev, &mut order, &mut saw_switch);
        assert_eq!(order, vec![1, 2, 0], "the row moves live to the drop spot");
        assert!(!saw_switch, "a drag is not a click");
        // A plain click still switches.
        let p = rects[1].center();
        sidebar_frame(&ctx, &groups, vec![egui::Event::PointerMoved(p)]);
        sidebar_frame(&ctx, &groups, vec![press(p, true)]);
        let ev = sidebar_frame(&ctx, &groups, vec![press(p, false)]).0;
        assert_eq!((ev.switch, ev.reorder), (Some(1), None));
    }

    #[test]
    fn grouped_session_list_paints_horizontal_dividers() {
        let ctx = test_ctx();
        let dividers = |groups: &[Option<String>]| {
            sidebar_frame(&ctx, groups, vec![]);
            let (ev, output) = sidebar_frame(&ctx, groups, vec![]);
            let lines: Vec<f32> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::LineSegment { points, .. }
                        if (points[0].y - points[1].y).abs() < 0.01
                            && (points[1].x - points[0].x) > 100.0 =>
                    {
                        Some(points[0].y)
                    }
                    _ => None,
                })
                .collect();
            (ev.tab_rects, lines)
        };
        // The heading's separator is one horizontal line already.
        let (_, plain) = dividers(&[None, None, None]);
        let (rects, grouped) = dividers(&[Some("a".into()), Some("a".into()), Some("b".into())]);
        assert_eq!(grouped.len(), plain.len() + 1, "{grouped:?} vs {plain:?}");
        let divider = grouped
            .iter()
            .find(|y| !plain.contains(y))
            .copied()
            .unwrap();
        assert!(rects[1].bottom() <= divider && divider <= rects[2].top());
    }

    #[test]
    fn compact_windows_caption_preserves_buttons_and_content_height() {
        #[derive(Default)]
        struct CaptionHost {
            minimized: bool,
            maximized: bool,
            closed: bool,
        }
        impl Chrome for CaptionHost {
            fn draws_menu_bar(&self) -> bool {
                false
            }
            fn window_controls(&self) -> bool {
                true
            }
            fn show_sidebar(&self) -> bool {
                false
            }
            fn show_details(&self) -> bool {
                false
            }
            fn on_minimize_window(&mut self) {
                self.minimized = true;
            }
            fn on_maximize_window(&mut self) {
                self.maximized = true;
            }
            fn on_menu(&mut self, id: MenuId) {
                self.closed = id == MenuId::Quit;
            }
        }
        let ctx = test_ctx();
        let mut host = CaptionHost::default();
        let mut run = |events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 700.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    render(ctx, &mut host);
                    assert!((ctx.available_rect().top() - CHROME_TITLE_H).abs() < 0.5);
                },
            )
        };
        run(vec![]);
        let output = run(vec![]);
        let positions: Vec<_> = ["−", "□", "×"]
            .into_iter()
            .map(|label| {
                output
                    .shapes
                    .iter()
                    .find_map(|shape| match &shape.shape {
                        egui::Shape::Text(text) if text.galley.job.text == label => {
                            Some(text.pos + text.galley.size() * 0.5)
                        }
                        _ => None,
                    })
                    .expect("caption button drawn")
            })
            .collect();
        assert!(positions[0].x < positions[1].x && positions[1].x < positions[2].x);
        for pos in positions {
            run(vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ]);
            run(vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }]);
        }
        assert!(host.minimized && host.maximized && host.closed);
    }

    struct FrameHost {
        sidebar: bool,
        menu: bool,
        inset: f32,
        drag_hover: bool,
        toggled_sidebar: bool,
    }

    impl Chrome for FrameHost {
        fn tabs(&self) -> Vec<ChromeTab> {
            ["one", "two"]
                .into_iter()
                .map(|t| ChromeTab {
                    title: t.into(),
                    badge: None,
                    icon: crate::icons::Icon::Terminal.into(),
                    location: String::new(),
                })
                .collect()
        }
        fn show_sidebar(&self) -> bool {
            self.sidebar
        }
        fn draws_menu_bar(&self) -> bool {
            self.menu
        }
        fn titlebar_inset(&self) -> f32 {
            self.inset
        }
        fn on_title_drag_hover(&mut self, hovered: bool) {
            self.drag_hover = hovered;
        }
        fn on_toggle_sidebar(&mut self) {
            self.toggled_sidebar = true;
        }
    }

    fn frame_run(
        ctx: &egui::Context,
        host: &mut FrameHost,
        events: Vec<egui::Event>,
    ) -> egui::Rect {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 700.0),
            )),
            events,
            ..Default::default()
        };
        let mut central = egui::Rect::NOTHING;
        let _ = ctx.run(input, |ctx| {
            render(ctx, host);
            central = ctx.available_rect();
        });
        central
    }

    #[test]
    fn terminal_area_starts_where_the_host_lays_out_its_grid() {
        for (sidebar, menu) in [(true, false), (true, true), (false, false), (false, true)] {
            let ctx = test_ctx();
            let mut host = FrameHost {
                sidebar,
                menu,
                inset: 76.0,
                drag_hover: false,
                toggled_sidebar: false,
            };
            frame_run(&ctx, &mut host, vec![]);
            let central = frame_run(&ctx, &mut host, vec![]);
            assert!(
                (central.top() - content_top(menu)).abs() < 0.5,
                "sidebar {sidebar}, menu {menu}: {central:?}"
            );
            let left = if sidebar { CHROME_SIDEBAR_W } else { 0.0 };
            assert!((central.left() - left).abs() < 1.0, "{central:?}");
            assert!(
                (central.bottom() - (700.0 - CHROME_STATUS_H)).abs() < 0.5,
                "{central:?}"
            );
        }
    }

    #[test]
    fn empty_title_row_space_moves_the_window_but_controls_do_not() {
        let ctx = test_ctx();
        let mut host = FrameHost {
            sidebar: true,
            menu: false,
            inset: 76.0,
            drag_hover: false,
            toggled_sidebar: false,
        };
        let hover = |p: egui::Pos2, host: &mut FrameHost| {
            frame_run(&ctx, host, vec![egui::Event::PointerMoved(p)]);
            frame_run(&ctx, host, vec![egui::Event::PointerMoved(p)]);
            host.drag_hover
        };
        // Over the centred title and over the sidebar header's empty start
        // (where the traffic lights sit).
        assert!(hover(egui::pos2(600.0, 15.0), &mut host));
        assert!(hover(egui::pos2(40.0, 15.0), &mut host));
        // Over the terminal, and over the top-right controls.
        assert!(!hover(egui::pos2(600.0, 300.0), &mut host));
        assert!(!hover(egui::pos2(1190.0, 15.0), &mut host));
        // The sidebar header's own toggle sits at its right end.
        let p = egui::pos2(CHROME_SIDEBAR_W - 14.0, 15.0);
        assert!(!hover(p, &mut host));
        frame_run(&ctx, &mut host, vec![press(p, true)]);
        frame_run(&ctx, &mut host, vec![press(p, false)]);
        assert!(host.toggled_sidebar);
    }

    #[test]
    fn tab_chips_do_not_start_a_window_drag() {
        let ctx = test_ctx();
        let mut host = FrameHost {
            sidebar: false,
            menu: false,
            inset: 76.0,
            drag_hover: false,
            toggled_sidebar: false,
        };
        let hover = |p: egui::Pos2, host: &mut FrameHost| {
            frame_run(&ctx, host, vec![egui::Event::PointerMoved(p)]);
            frame_run(&ctx, host, vec![egui::Event::PointerMoved(p)]);
            host.drag_hover
        };
        frame_run(&ctx, &mut host, vec![]);
        let on_chip = hover(egui::pos2(90.0, 15.0), &mut host);
        let empty = hover(egui::pos2(700.0, 15.0), &mut host);
        assert!(
            !on_chip,
            "hovering a tab chip must not report a window-drag region (got drag_hover=true)"
        );
        assert!(empty, "empty title space should still move the window");
    }
}
