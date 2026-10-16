//! Structured snapshots of buffered editor text. Rendering never writes files.
use super::*;
use mtty_ui::data_preview::{self as model, Format, Preview};
use std::collections::BTreeSet;

#[derive(Clone)]
struct Source {
    editor: String,
    title: String,
    revision: u64,
    text: Arc<str>,
    format: Format,
}
pub(super) struct View {
    pub epoch: u64,
    pub loading: bool,
    pub data: Option<Preview>,
    pub error: Option<String>,
    source: Source,
    query: String,
    headers: bool,
    open: BTreeSet<Vec<usize>>,
    rows: Vec<model::JsonRow>,
    limited: bool,
    visible: Vec<usize>,
    sort: Option<(usize, bool)>,
    json_selection: Vec<usize>,
    cell: Option<(usize, usize)>,
}
impl View {
    fn new(epoch: u64, source: Source) -> Self {
        let headers = matches!(source.format, Format::Csv | Format::Tsv);
        Self {
            epoch,
            source,
            headers,
            loading: true,
            data: None,
            error: None,
            query: String::new(),
            open: BTreeSet::from([Vec::new()]),
            rows: Vec::new(),
            limited: false,
            visible: Vec::new(),
            sort: None,
            json_selection: Vec::new(),
            cell: None,
        }
    }
    pub(super) fn rebuild(&mut self) {
        match &self.data {
            Some(Preview::Json(value)) => {
                (self.rows, self.limited) = model::json_rows(value, &self.open, &self.query);
            }
            Some(Preview::Table(table)) => {
                let query = self.query.to_lowercase();
                self.visible = (usize::from(self.headers)..table.rows.len())
                    .filter(|index| {
                        query.is_empty()
                            || table.rows[*index]
                                .iter()
                                .any(|value| value.to_lowercase().contains(&query))
                    })
                    .collect();
                if let Some((column, descending)) = self.sort {
                    self.visible.sort_by(|a, b| {
                        let order = table.rows[*a]
                            .get(column)
                            .map(String::as_str)
                            .unwrap_or("")
                            .cmp(table.rows[*b].get(column).map(String::as_str).unwrap_or(""));
                        if descending {
                            order.reverse()
                        } else {
                            order
                        }
                    });
                }
                if self
                    .cell
                    .is_some_and(|(row, _)| !self.visible.contains(&row))
                {
                    self.cell = None;
                }
                self.limited = table.truncated;
            }
            None => {}
        }
    }
    fn has_selection(&self) -> bool {
        match &self.data {
            Some(Preview::Json(_)) => self.rows.iter().any(|row| row.path == self.json_selection),
            Some(Preview::Table(table)) => self.cell.is_some_and(|(row, column)| {
                table
                    .rows
                    .get(row)
                    .and_then(|row| row.get(column))
                    .is_some()
            }),
            None => false,
        }
    }
    fn selected(&self) -> Option<String> {
        match self.data.as_ref()? {
            Preview::Json(value) => {
                if !self.rows.iter().any(|row| row.path == self.json_selection) {
                    return None;
                }
                serde_json::to_string_pretty(model::json_value(value, &self.json_selection)?).ok()
            }
            Preview::Table(table) => {
                let (row, column) = self.cell?;
                table.rows.get(row)?.get(column).cloned()
            }
        }
    }
    fn location(&self) -> String {
        match &self.data {
            Some(Preview::Json(value)) => {
                model::json_pointer(value, &self.json_selection).unwrap_or_default()
            }
            Some(Preview::Table(table)) => self
                .cell
                .and_then(|(row, column)| {
                    table
                        .line_starts
                        .get(row)
                        .map(|line| format!("line {line}, column {}", column + 1))
                })
                .unwrap_or_default(),
            None => String::new(),
        }
    }
}

enum Action {
    Refresh,
    Format(Format),
    Rebuild,
    Toggle(Vec<usize>),
    SelectJson(Vec<usize>),
    SelectCell(usize, usize),
    Sort(usize),
    Copy,
    Pointer,
    Composer,
    Source,
}

impl State {
    pub(super) fn open_data_preview(&mut self, editor: Option<String>) {
        let ed = match editor {
            Some(id) => self
                .tabs
                .iter()
                .flat_map(|tab| &tab.editors)
                .find(|ed| ed.id == id),
            None => self.active_editor(),
        };
        let Some(ed) = ed else {
            self.show_notice(
                mtty_ui::i18n::t(
                    self.lang,
                    "Open a text editor buffer to preview its data",
                    "请打开文本编辑缓冲区以预览数据",
                )
                .into(),
            );
            return;
        };
        if ed.is_view_only() || ed.doc.rope().len_bytes() > model::MAX_BYTES {
            self.show_notice(
                mtty_ui::i18n::t(
                    self.lang,
                    "Structured previews support buffered text up to 2 MiB",
                    "结构化预览支持最多 2 MiB 的文本缓冲区",
                )
                .into(),
            );
            return;
        }
        let title = ed
            .remote
            .as_ref()
            .map(|remote| format!("{}:{}", remote.dest, remote.path))
            .unwrap_or_else(|| ed.path.display().to_string());
        let source = Source {
            editor: ed.id.clone(),
            title,
            revision: ed.doc.revision(),
            text: Arc::from(ed.doc.rope().to_string()),
            format: Format::for_path(&ed.path),
        };
        self.parse_data_snapshot(source);
    }
    fn parse_data_snapshot(&mut self, source: Source) {
        self.data_preview_epoch = self.data_preview_epoch.saturating_add(1);
        let epoch = self.data_preview_epoch;
        let text = source.text.clone();
        let format = source.format;
        self.data_preview = Some(View::new(epoch, source));
        self.spawn_job(move || JobDone::DataPreviewLoaded {
            epoch,
            result: model::parse(&text, format),
        });
    }
    pub(super) fn data_preview_window(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let Some(mut view) = self.data_preview.take() else {
            return;
        };
        let lang = self.lang;
        let mut open = true;
        let mut action = None;
        app_window(t(lang,"Data Preview","数据预览"),ctx).open(&mut open).default_size([900.0,620.0]).show(ctx,|ui|window_body(ui,|ui|{
            ui.label(&view.source.title);
            ui.small(format!("{} · {} {}",t(lang,"Snapshot","快照"),t(lang,"revision","版本"),view.source.revision));
            ui.horizontal(|ui|{
                let mut format=view.source.format;
                egui::ComboBox::from_id_salt("data-format").selected_text(format.name()).show_ui(ui,|ui|{for kind in [Format::Json,Format::Csv,Format::Tsv,Format::Lines]{ui.selectable_value(&mut format,kind,kind.name());}});
                if format!=view.source.format {action=Some(Action::Format(format));}
                if ui.button(t(lang,"Refresh buffer","刷新缓冲区")).clicked(){action=Some(Action::Refresh);}
                if ui.add(egui::TextEdit::singleline(&mut view.query).hint_text(t(lang,"Filter keys / values / rows…","筛选键／值／行…"))).changed(){action=Some(Action::Rebuild);}
                if view.loading{ui.spinner();}
            });
            if matches!(view.data,Some(Preview::Table(_))) && ui.checkbox(&mut view.headers,t(lang,"First row is header","首行作为表头")).changed(){action=Some(Action::Rebuild);}
            let selected=view.has_selection();
            ui.horizontal_wrapped(|ui|{
                if ui.add_enabled(selected,egui::Button::new(t(lang,"Copy selected value","复制选中值"))).clicked(){action=Some(Action::Copy);}
                if matches!(view.data,Some(Preview::Json(_))) && ui.add_enabled(selected,egui::Button::new(t(lang,"Copy JSON Pointer","复制 JSON Pointer"))).clicked(){action=Some(Action::Pointer);}
                if ui.add_enabled(selected,egui::Button::new(t(lang,"To Composer","发到 Composer"))).clicked(){action=Some(Action::Composer);}
                if matches!(view.data,Some(Preview::Table(_))) && ui.add_enabled(view.cell.is_some(),egui::Button::new(t(lang,"Go to source record","定位到源记录"))).clicked(){action=Some(Action::Source);}
            });
            if let Some(error)=&view.error{ui.colored_label(chrome_rgb(self.theme.chrome().negative),error);}
            if view.limited{ui.label(t(lang,"Preview limit reached (JSON: 20k scanned nodes/10k visible rows; table: 10k records/128 columns).","已达到预览上限（JSON：扫描 2 万节点／显示 1 万行；表格：1 万记录／128 列）。"));}
            match &view.data {
                Some(Preview::Json(_))=>{
                    let height=ui.text_style_height(&egui::TextStyle::Monospace)+4.0;
                    egui::ScrollArea::both().id_salt("json-preview").max_height(440.0).show_rows(ui,height,view.rows.len(),|ui,rows|{
                        for index in rows {let row=&view.rows[index];ui.horizontal(|ui|{
                            ui.add_space(row.depth as f32*14.0);
                            if row.expandable {if ui.add_enabled(view.query.is_empty(),egui::Button::new(if row.expanded{"−"}else{"+"})).clicked(){action=Some(Action::Toggle(row.path.clone()));}}
                            else {ui.add_space(20.0);}
                            let text=egui::RichText::new(&row.label).monospace().background_color(if view.json_selection==row.path{ui.visuals().selection.bg_fill}else{egui::Color32::TRANSPARENT});
                            if ui.add(egui::Label::new(text).wrap_mode(egui::TextWrapMode::Extend).sense(egui::Sense::click())).clicked(){action=Some(Action::SelectJson(row.path.clone()));}
                        });}
                    });
                }
                Some(Preview::Table(table))=>{
                    ui.small(format!("{} / {} {}",view.visible.len(),table.rows.len().saturating_sub(usize::from(view.headers)),t(lang,"records · headers sort as text","条记录 · 点击表头按文本排序")));
                    let columns=table.columns.max(1);
                    let height=ui.text_style_height(&egui::TextStyle::Monospace)+6.0;
                    egui::ScrollArea::horizontal().id_salt("table-preview-horizontal").show(ui,|ui|{
                        use egui_extras::{Column,TableBuilder};
                        ui.set_min_width(60.0+columns as f32*150.0);
                        TableBuilder::new(ui).id_salt("data-table").striped(true).resizable(true).max_scroll_height(410.0)
                            .column(Column::initial(52.0)).columns(Column::initial(150.0),columns)
                            .header(height,|mut header|{
                                header.col(|ui|{ui.label("#");});
                                for column in 0..columns {header.col(|ui|{
                                    let label=if view.headers {table.rows.first().and_then(|row|row.get(column)).map(|value|value.chars().take(80).flat_map(char::escape_debug).collect::<String>()).unwrap_or_else(||format!("{}",column+1))}else{format!("{}",column+1)};
                                    if ui.selectable_label(view.sort.is_some_and(|(index,_)|index==column),label).clicked(){action=Some(Action::Sort(column));}
                                });}
                            }).body(|body|{body.rows(height,view.visible.len(),|mut row|{
                                let index=view.visible[row.index()];
                                row.col(|ui|{ui.label(table.line_starts[index].to_string());});
                                for column in 0..columns {row.col(|ui|{
                                    let value=table.rows[index].get(column).map(String::as_str).unwrap_or("");
                                    let text:String=value.chars().take(160).flat_map(char::escape_debug).collect();
                                    let selected=view.cell==Some((index,column));
                                    let text=egui::RichText::new(text).monospace().background_color(if selected{ui.visuals().selection.bg_fill}else{egui::Color32::TRANSPARENT});
                                    if ui.add(egui::Label::new(text).wrap_mode(egui::TextWrapMode::Extend).sense(egui::Sense::click())).clicked(){action=Some(Action::SelectCell(index,column));}
                                });}
                            });});
                    });
                }
                None=>{}
            }
        }));
        let mut parse = None;
        match action {
            Some(Action::Refresh) => {
                let editor = view.source.editor.clone();
                self.open_data_preview(Some(editor));
                return;
            }
            Some(Action::Format(format)) => {
                let mut source = view.source.clone();
                source.format = format;
                parse = Some(source);
            }
            Some(Action::Rebuild) => view.rebuild(),
            Some(Action::Toggle(path)) => {
                if !view.open.remove(&path) {
                    view.open.insert(path);
                }
                view.rebuild();
            }
            Some(Action::SelectJson(path)) => view.json_selection = path,
            Some(Action::SelectCell(row, column)) => view.cell = Some((row, column)),
            Some(Action::Sort(column)) => {
                let descending = view
                    .sort
                    .is_some_and(|(old, descending)| old == column && !descending);
                view.sort = Some((column, descending));
                view.rebuild();
            }
            Some(Action::Copy) => {
                if let Some(text) = view.selected() {
                    ctx.copy_text(text);
                }
            }
            Some(Action::Pointer) => ctx.copy_text(view.location()),
            Some(Action::Composer) => {
                if let Some(text) = view.selected() {
                    let content = text
                        .lines()
                        .map(|line| format!("    {line}\n"))
                        .collect::<String>();
                    let draft = self.composer.take().unwrap_or_default();
                    self.composer = Some(format!(
                        "{draft}\n\nSource: {}\n{}\n\n{content}",
                        view.source.title,
                        view.location()
                    ));
                }
            }
            Some(Action::Source) => {
                if let Some(line) = view.cell.and_then(|(row, _)| match &view.data {
                    Some(Preview::Table(table)) => table.line_starts.get(row).copied(),
                    _ => None,
                }) {
                    let target = self.tabs.iter().enumerate().find_map(|(index, tab)| {
                        tab.editors
                            .iter()
                            .find(|editor| editor.id == view.source.editor)
                            .map(|editor| (index, editor.doc.revision()))
                    });
                    if let Some((index, revision)) = target {
                        if revision == view.source.revision {
                            self.active_tab = index;
                            self.tabs[index].active = view.source.editor.clone();
                            self.go_active_editor_to(Some(line), None);
                            open = false;
                        } else {
                            self.show_notice(
                                t(
                                    lang,
                                    "Buffer changed; refresh the preview before locating a record",
                                    "缓冲区已变化，请刷新预览后再定位记录",
                                )
                                .into(),
                            );
                        }
                    }
                }
            }
            None => {}
        }
        if let Some(source) = parse {
            self.parse_data_snapshot(source);
        } else if open {
            self.data_preview = Some(view);
        }
    }
}
