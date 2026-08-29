use crate::{dmetrix, indexed, sdpc, svs, vips};
use anyhow::{bail, Context, Result};
use eframe::egui::{
    self, Color32, FontData, FontDefinitions, FontFamily, FontId, RichText, TextStyle, Vec2,
};
use eframe::{App, CreationContext, Frame, NativeOptions};
use rfd::FileDialog;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

const SUPPORTED_EXTENSIONS: &[&str] = &[
    "csp", "dmetrix", "kfb", "mdsx", "msdx", "mrxs", "ndpi", "sdpc", "dyqx",
];

const PAGE_BACKGROUND: Color32 = Color32::from_rgb(242, 246, 249);
const CARD_BACKGROUND: Color32 = Color32::from_rgb(255, 255, 255);
const HEADER_BACKGROUND: Color32 = Color32::from_rgb(17, 52, 79);
const PRIMARY: Color32 = Color32::from_rgb(13, 151, 143);
const PRIMARY_LIGHT: Color32 = Color32::from_rgb(225, 249, 247);
const BORDER: Color32 = Color32::from_rgb(210, 220, 228);
const TEXT: Color32 = Color32::from_rgb(22, 52, 77);
const MUTED_TEXT: Color32 = Color32::from_rgb(84, 119, 145);

pub struct LaunchOptions {
    pub smoke_test: bool,
}

pub fn run(options: LaunchOptions) -> Result<()> {
    let native_options = NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Pathology SVS Converter")
            .with_inner_size([1280.0, 840.0])
            .with_min_inner_size([980.0, 680.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Pathology SVS Converter",
        native_options,
        Box::new(move |cc| Ok(Box::new(SvsGui::new(cc, options.smoke_test)))),
    )
    .map_err(|error| anyhow::anyhow!("GUI failed to start: {error}"))
}

#[derive(Clone)]
struct InputItem {
    path: PathBuf,
    state: ItemState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ItemState {
    Waiting,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl ItemState {
    fn label(self) -> &'static str {
        match self {
            Self::Waiting => "待转换",
            Self::Running => "转换中",
            Self::Done => "完成",
            Self::Failed => "失败",
            Self::Cancelled => "已停止",
        }
    }

    fn color(self) -> Color32 {
        match self {
            Self::Waiting => Color32::from_rgb(112, 128, 144),
            Self::Running => Color32::from_rgb(34, 123, 197),
            Self::Done => Color32::from_rgb(42, 140, 92),
            Self::Failed => Color32::from_rgb(198, 67, 67),
            Self::Cancelled => Color32::from_rgb(180, 126, 45),
        }
    }
}

struct GuiOptions {
    output_dir: String,
    jpeg_quality: String,
    skip_associated: bool,
    overwrite: bool,
}

struct Job {
    input: PathBuf,
    output: PathBuf,
}

enum WorkerEvent {
    Started { index: usize },
    Log(String),
    Finished { index: usize, elapsed: f64 },
    Failed { index: usize, message: String },
    Cancelled { index: usize },
    Complete { cancelled: bool },
}

struct SvsGui {
    items: Vec<InputItem>,
    options: GuiOptions,
    logs: Vec<String>,
    receiver: Option<Receiver<WorkerEvent>>,
    cancel: Option<Arc<AtomicBool>>,
    running: bool,
    completed: usize,
    failed: usize,
    batch_total: usize,
    active_indices: Vec<usize>,
    smoke_test: bool,
    last_message: String,
    logs_collapsed: bool,
}

impl SvsGui {
    fn new(cc: &CreationContext<'_>, smoke_test: bool) -> Self {
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = Vec2::new(10.0, 10.0);
        style.spacing.button_padding = Vec2::new(13.0, 9.0);
        style.spacing.interact_size = Vec2::new(44.0, 38.0);
        style
            .text_styles
            .insert(TextStyle::Body, FontId::proportional(17.0));
        style
            .text_styles
            .insert(TextStyle::Button, FontId::proportional(16.0));
        style
            .text_styles
            .insert(TextStyle::Small, FontId::proportional(14.0));
        style
            .text_styles
            .insert(TextStyle::Heading, FontId::proportional(25.0));
        style.visuals = egui::Visuals::light();
        style.visuals.panel_fill = PAGE_BACKGROUND;
        style.visuals.window_fill = CARD_BACKGROUND;
        style.visuals.widgets.noninteractive.bg_fill = CARD_BACKGROUND;
        style.visuals.widgets.noninteractive.fg_stroke.color = TEXT;
        style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(232, 241, 246);
        style.visuals.widgets.inactive.fg_stroke.color = TEXT;
        style.visuals.widgets.hovered.bg_fill = PRIMARY_LIGHT;
        style.visuals.widgets.hovered.fg_stroke.color = TEXT;
        style.visuals.selection.bg_fill = PRIMARY;
        style.visuals.selection.stroke.color = PRIMARY;
        cc.egui_ctx.set_style(style);
        install_windows_font(&cc.egui_ctx);
        Self {
            items: Vec::new(),
            options: GuiOptions {
                output_dir: String::new(),
                jpeg_quality: "原始".to_owned(),
                skip_associated: false,
                overwrite: false,
            },
            logs: vec!["就绪：可拖入切片文件或目录。".to_owned()],
            receiver: None,
            cancel: None,
            running: false,
            completed: 0,
            failed: 0,
            batch_total: 0,
            active_indices: Vec::new(),
            smoke_test,
            last_message: "等待添加切片".to_owned(),
            logs_collapsed: true,
        }
    }

    fn add_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        if self.running {
            self.log("转换运行中，暂时不能添加文件。".to_owned());
            return;
        }
        let mut added = 0;
        let mut refreshed = 0;
        let mut ignored = 0;
        let mut duplicate = 0;
        let mut candidates = Vec::new();
        for path in paths {
            if path.is_dir() {
                collect_supported_files(&path, &mut candidates);
            } else {
                candidates.push(path);
            }
        }
        let existing: HashSet<String> = self
            .items
            .iter()
            .map(|item| normalize_path(&item.path))
            .collect();
        let mut seen = existing;
        for path in candidates {
            if !is_supported(&path) {
                ignored += 1;
                continue;
            }
            let key = normalize_path(&path);
            if seen.contains(&key) {
                if let Some(item) = self
                    .items
                    .iter_mut()
                    .find(|item| normalize_path(&item.path) == key)
                {
                    if item.state != ItemState::Waiting {
                        item.state = ItemState::Waiting;
                        refreshed += 1;
                    } else {
                        duplicate += 1;
                    }
                } else {
                    duplicate += 1;
                }
                continue;
            }
            seen.insert(key);
            self.items.push(InputItem {
                path,
                state: ItemState::Waiting,
            });
            added += 1;
        }
        self.last_message = format!(
            "新增 {added} 项，重新加入 {refreshed} 项，重复 {duplicate} 项，忽略 {ignored} 项"
        );
        if added > 0 || refreshed > 0 {
            self.completed = 0;
            self.failed = 0;
            self.batch_total = self
                .items
                .iter()
                .filter(|item| item.state == ItemState::Waiting)
                .count();
        }
        self.log(format!("{}。", self.last_message));
    }

    fn log(&mut self, message: String) {
        self.logs.push(message);
        if self.logs.len() > 500 {
            self.logs.drain(0..100);
        }
    }

    fn choose_files(&mut self) {
        if let Some(paths) = FileDialog::new()
            .add_filter("Whole-slide files", SUPPORTED_EXTENSIONS)
            .pick_files()
        {
            self.add_paths(paths);
        }
    }

    fn choose_folder(&mut self) {
        if let Some(path) = FileDialog::new().pick_folder() {
            self.add_paths([path]);
        }
    }

    fn choose_output(&mut self) {
        if let Some(path) = FileDialog::new().pick_folder() {
            self.options.output_dir = path.display().to_string();
        }
    }

    fn remove_at(&mut self, index: usize) {
        if self.running {
            return;
        }
        if index < self.items.len() {
            self.items.remove(index);
            self.batch_total = self
                .items
                .iter()
                .filter(|item| item.state == ItemState::Waiting)
                .count();
        }
    }

    fn start(&mut self) {
        if self.running {
            return;
        }
        if self.items.is_empty() {
            if self.items.is_empty() {
                self.last_message = "请先添加切片文件".to_owned();
            }
            return;
        }
        let pending_indices: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| (item.state == ItemState::Waiting).then_some(index))
            .collect();
        if pending_indices.is_empty() {
            self.log("没有新添加的切片需要转换。".to_owned());
            return;
        }
        let quality = match self.options.jpeg_quality.as_str() {
            "原始" => None,
            value => match value.parse::<u8>() {
                Ok(value) if (1..=100).contains(&value) => Some(value),
                _ => {
                    self.last_message = "JPEG 质量必须为 1-100".to_owned();
                    return;
                }
            },
        };
        let output_dir = (!self.options.output_dir.trim().is_empty())
            .then(|| PathBuf::from(self.options.output_dir.trim()));
        let jobs = plan_jobs(
            pending_indices
                .iter()
                .map(|&index| self.items[index].path.clone()),
            output_dir.as_deref(),
        );
        let batch_total = jobs.len();
        let skip_associated = self.options.skip_associated;
        let overwrite = self.options.overwrite;
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        thread::spawn(move || {
            run_jobs(
                jobs,
                quality,
                skip_associated,
                overwrite,
                sender,
                worker_cancel,
            )
        });
        self.receiver = Some(receiver);
        self.cancel = Some(cancel);
        self.running = true;
        self.completed = 0;
        self.failed = 0;
        self.batch_total = batch_total;
        self.active_indices = pending_indices;
        self.log("开始转换队列。".to_owned());
    }

    fn stop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
            self.last_message = "已请求停止，当前文件完成后退出".to_owned();
            self.log("已请求停止剩余队列；不会中断当前正在写入的文件。".to_owned());
        }
    }

    fn receive_events(&mut self) {
        let events: Vec<WorkerEvent> = self
            .receiver
            .as_ref()
            .map(|receiver| receiver.try_iter().collect())
            .unwrap_or_default();
        for event in events {
            match event {
                WorkerEvent::Started { index } => {
                    if let Some(item_index) = self.active_indices.get(index).copied() {
                        if let Some(item) = self.items.get_mut(item_index) {
                            item.state = ItemState::Running;
                        }
                    }
                }
                WorkerEvent::Log(message) => self.log(message),
                WorkerEvent::Finished { index, elapsed } => {
                    if let Some(item_index) = self.active_indices.get(index).copied() {
                        let path = self.items[item_index].path.display().to_string();
                        self.items[item_index].state = ItemState::Done;
                        self.completed += 1;
                        self.log(format!("完成：{path}（{elapsed:.1}s）"));
                    }
                }
                WorkerEvent::Failed { index, message } => {
                    if let Some(item_index) = self.active_indices.get(index).copied() {
                        let path = self.items[item_index].path.display().to_string();
                        self.items[item_index].state = ItemState::Failed;
                        self.failed += 1;
                        self.log(format!("失败：{path}：{message}"));
                    }
                }
                WorkerEvent::Cancelled { index } => {
                    if let Some(item_index) = self.active_indices.get(index).copied() {
                        if let Some(item) = self.items.get_mut(item_index) {
                            item.state = ItemState::Cancelled;
                        }
                    }
                }
                WorkerEvent::Complete { cancelled } => {
                    self.running = false;
                    self.cancel = None;
                    self.receiver = None;
                    self.last_message = if cancelled {
                        format!(
                            "已停止：完成 {} 项，失败 {} 项",
                            self.completed, self.failed
                        )
                    } else {
                        format!("完成：{} 项，失败 {} 项", self.completed, self.failed)
                    };
                    self.log(self.last_message.clone());
                    self.active_indices.clear();
                }
            }
        }
    }
}

fn install_windows_font(ctx: &egui::Context) {
    #[cfg(target_os = "windows")]
    if let Ok(bytes) = std::fs::read(r"C:\Windows\Fonts\simhei.ttf") {
        let mut fonts = FontDefinitions::default();
        fonts
            .font_data
            .insert("simhei".to_owned(), Arc::new(FontData::from_owned(bytes)));
        if let Some(proportional) = fonts.families.get_mut(&FontFamily::Proportional) {
            proportional.insert(0, "simhei".to_owned());
        }
        if let Some(monospace) = fonts.families.get_mut(&FontFamily::Monospace) {
            monospace.insert(0, "simhei".to_owned());
        }
        ctx.set_fonts(fonts);
    }
}

impl App for SvsGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        self.receive_events();
        if self.smoke_test {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        self.handle_shortcuts(ctx);
        self.handle_dropped_files(ctx);
        ctx.request_repaint_after(std::time::Duration::from_millis(100));

        egui::TopBottomPanel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(HEADER_BACKGROUND)
                    .inner_margin(18.0),
            )
            .show(ctx, |ui| self.header(ui));
        egui::TopBottomPanel::bottom("logs")
            .resizable(true)
            .default_height(if self.logs_collapsed { 54.0 } else { 220.0 })
            .frame(egui::Frame::new().fill(PAGE_BACKGROUND).inner_margin(18.0))
            .show(ctx, |ui| self.logs_panel(ui));

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(PAGE_BACKGROUND).inner_margin(18.0))
            .show(ctx, |ui| {
                ui.columns(2, |columns| {
                    egui::Frame::new()
                        .fill(CARD_BACKGROUND)
                        .stroke(egui::Stroke::new(1.0_f32, BORDER))
                        .inner_margin(18.0)
                        .show(&mut columns[0], |ui| self.sources(ui));
                    egui::Frame::new()
                        .fill(CARD_BACKGROUND)
                        .stroke(egui::Stroke::new(1.0_f32, BORDER))
                        .inner_margin(18.0)
                        .show(&mut columns[1], |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("settings_scroll")
                                .auto_shrink([false, false])
                                .show(ui, |ui| self.settings(ui));
                        });
                });
            });
    }
}

impl SvsGui {
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let (f5, escape, ctrl_o, ctrl_shift_o) = ctx.input(|input| {
            (
                input.key_pressed(egui::Key::F5),
                input.key_pressed(egui::Key::Escape),
                input.modifiers.command && input.key_pressed(egui::Key::O),
                input.modifiers.command && input.modifiers.shift && input.key_pressed(egui::Key::O),
            )
        });
        if f5 {
            self.start();
        }
        if escape {
            self.stop();
        }
        if ctrl_shift_o {
            self.choose_folder();
        } else if ctrl_o {
            self.choose_files();
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let paths: Vec<PathBuf> = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect()
        });
        if !paths.is_empty() {
            self.add_paths(paths);
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            egui::Frame::new()
                .fill(PRIMARY)
                .inner_margin(14.0)
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("SVS")
                            .strong()
                            .size(20.0)
                            .color(Color32::WHITE),
                    );
                });
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("病理图像转 SVS 工具")
                        .strong()
                        .size(25.0)
                        .color(Color32::WHITE),
                );
                ui.label(
                    RichText::new("把常见数字病理切片批量转换为兼容性更好的 SVS")
                        .size(15.0)
                        .color(Color32::from_rgb(184, 216, 235)),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let stop = ui.add_enabled(
                    self.running,
                    egui::Button::new(RichText::new("停止").size(17.0))
                        .fill(Color32::from_rgb(239, 245, 248))
                        .min_size(Vec2::new(112.0, 44.0)),
                );
                if stop.clicked() {
                    self.stop();
                }
                let start = ui.add_enabled(
                    !self.running,
                    egui::Button::new(RichText::new("开始转换  F5").size(17.0))
                        .fill(PRIMARY)
                        .min_size(Vec2::new(158.0, 44.0)),
                );
                if start.clicked() {
                    self.start();
                }
            });
        });
        let total = self.batch_total;
        let fraction = if total == 0 {
            0.0
        } else {
            (self.completed + self.failed) as f32 / total as f32
        };
        ui.add_space(14.0);
        ui.add(egui::ProgressBar::new(fraction).fill(PRIMARY).text(format!(
            "{} / {}",
            self.completed + self.failed,
            total
        )));
        ui.add_space(2.0);
    }

    fn sources(&mut self, ui: &mut egui::Ui) {
        ui.heading("1  选择切片");
        ui.label(
            RichText::new("支持多文件、目录添加，也可以直接拖拽到下方列表")
                .size(15.0)
                .color(MUTED_TEXT),
        );
        ui.horizontal(|ui| {
            if ui
                .add_sized([132.0, 40.0], egui::Button::new("＋ 添加文件"))
                .clicked()
            {
                self.choose_files();
            }
            if ui
                .add_sized([112.0, 40.0], egui::Button::new("添加目录"))
                .clicked()
            {
                self.choose_folder();
            }
            if ui
                .add_sized([88.0, 40.0], egui::Button::new("清空"))
                .clicked()
                && !self.running
            {
                self.items.clear();
                self.completed = 0;
                self.failed = 0;
                self.batch_total = 0;
            }
        });
        ui.label(
            RichText::new(format!(
                "已添加 {} 项 · 支持：CSP / DMETRIX / KFB / MDSX / MRXS / NDPI / SDPC / DYQX",
                self.items.len()
            ))
            .weak(),
        );
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                let action_width = 78.0;
                let column_spacing = ui.spacing().item_spacing.x;
                let path_width = (ui.available_width() - action_width - column_spacing).max(120.0);
                egui::Frame::new()
                    .fill(Color32::from_rgb(232, 239, 244))
                    .inner_margin(egui::Margin::symmetric(0, 6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (path_rect, _) = ui.allocate_exact_size(
                                Vec2::new(path_width, 24.0),
                                egui::Sense::hover(),
                            );
                            let (action_rect, _) = ui.allocate_exact_size(
                                Vec2::new(action_width, 24.0),
                                egui::Sense::hover(),
                            );
                            let painter = ui.painter();
                            painter.text(
                                path_rect.left_center(),
                                egui::Align2::LEFT_CENTER,
                                "文件或目录",
                                FontId::proportional(17.0),
                                TEXT,
                            );
                            painter.text(
                                action_rect.center(),
                                egui::Align2::CENTER_CENTER,
                                "操作",
                                FontId::proportional(17.0),
                                TEXT,
                            );
                        });
                    });
                let mut remove_index = None;
                for index in 0..self.items.len() {
                    let item = &self.items[index];
                    let path = item.path.display().to_string();
                    let label = item.state.label();
                    ui.horizontal(|ui| {
                        let (path_rect, _) = ui
                            .allocate_exact_size(Vec2::new(path_width, 44.0), egui::Sense::hover());
                        let painter = ui.painter().with_clip_rect(path_rect);
                        painter.text(
                            path_rect.left_center() + Vec2::new(0.0, -9.0),
                            egui::Align2::LEFT_CENTER,
                            label,
                            FontId::proportional(15.0),
                            item.state.color(),
                        );
                        painter.text(
                            path_rect.left_center() + Vec2::new(0.0, 9.0),
                            egui::Align2::LEFT_CENTER,
                            &path,
                            FontId::proportional(15.0),
                            item.state.color(),
                        );
                        if ui
                            .add_enabled(
                                !self.running,
                                egui::Button::new("移除").min_size(Vec2::new(action_width, 38.0)),
                            )
                            .clicked()
                        {
                            remove_index = Some(index);
                        }
                    });
                    ui.separator();
                }
                if let Some(index) = remove_index {
                    self.remove_at(index);
                }
                if self.items.is_empty() {
                    ui.add_space(28.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new("将切片文件或文件夹拖到这里")
                                .size(18.0)
                                .weak(),
                        );
                        ui.label(RichText::new("支持一次拖入多个项目").size(15.0).weak());
                    });
                }
            });
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("2  转换设置");
        ui.label(
            RichText::new("默认选项适合大多数切片")
                .size(15.0)
                .color(MUTED_TEXT),
        );
        ui.add_space(8.0);
        ui.label(RichText::new("输出位置").strong().color(TEXT));
        ui.horizontal(|ui| {
            let input_width = (ui.available_width() - 92.0).max(120.0);
            egui::Frame::new()
                .fill(Color32::from_rgb(250, 252, 253))
                .stroke(egui::Stroke::new(1.0_f32, BORDER))
                .inner_margin(0.0)
                .show(ui, |ui| {
                    ui.add_sized(
                        [input_width, 40.0],
                        egui::TextEdit::singleline(&mut self.options.output_dir)
                            .frame(false)
                            .background_color(Color32::from_rgb(250, 252, 253))
                            .vertical_align(egui::Align::Center)
                            .desired_rows(1),
                    );
                });
            if ui
                .add_sized([82.0, 40.0], egui::Button::new("浏览..."))
                .clicked()
            {
                self.choose_output();
            }
        });
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("留空即保存到源文件同目录")
                    .size(14.0)
                    .color(MUTED_TEXT),
            );
            if ui
                .button(RichText::new("恢复为源目录输出").size(14.0))
                .clicked()
            {
                self.options.output_dir.clear();
            }
        });
        ui.separator();
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("SVS 保存质量").strong().color(TEXT));
                egui::ComboBox::from_id_salt("quality")
                    .selected_text(&self.options.jpeg_quality)
                    .width(180.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.options.jpeg_quality,
                            "原始".to_owned(),
                            "原始 / 推荐",
                        );
                        for quality in [95, 90, 85, 80, 70, 60] {
                            ui.selectable_value(
                                &mut self.options.jpeg_quality,
                                quality.to_string(),
                                quality.to_string(),
                            );
                        }
                    });
            });
        });
        ui.label(
            RichText::new("“原始/推荐”会尽量沿用源图质量，降低数值可减小文件体积。")
                .size(14.0)
                .color(MUTED_TEXT),
        );
        ui.checkbox(&mut self.options.overwrite, "覆盖已存在的 SVS");
        ui.checkbox(
            &mut self.options.skip_associated,
            "跳过 label / macro 关联图",
        );
    }

    fn logs_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("3  运行日志");
            ui.label(
                RichText::new("需要排查失败原因时再展开")
                    .size(14.0)
                    .color(MUTED_TEXT),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(if self.logs_collapsed {
                        "展开"
                    } else {
                        "收起"
                    })
                    .clicked()
                {
                    self.logs_collapsed = !self.logs_collapsed;
                }
            });
        });
        if self.logs_collapsed {
            return;
        }
        ui.horizontal(|ui| {
            if ui.button("清空日志").clicked() {
                self.logs.clear();
            }
        });
        let log_width = ui.available_width();
        egui::Frame::new()
            .fill(CARD_BACKGROUND)
            .stroke(egui::Stroke::new(1.0_f32, BORDER))
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.set_min_width((log_width - 16.0).max(0.0));
                egui::ScrollArea::vertical()
                    .id_salt("logs_scroll")
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        for line in &self.logs {
                            ui.label(RichText::new(line).monospace().small());
                        }
                    });
            });
    }
}

fn run_jobs(
    jobs: Vec<Job>,
    quality: Option<u8>,
    skip_associated: bool,
    overwrite: bool,
    sender: Sender<WorkerEvent>,
    cancel: Arc<AtomicBool>,
) {
    for (index, job) in jobs.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            let _ = sender.send(WorkerEvent::Cancelled { index });
            continue;
        }
        let _ = sender.send(WorkerEvent::Started { index });
        let started = Instant::now();
        let result = convert_one(&job.input, &job.output, quality, skip_associated, overwrite);
        match result {
            Ok(output) => {
                let _ = sender.send(WorkerEvent::Log(format!("输出：{}", output.display())));
                let _ = sender.send(WorkerEvent::Finished {
                    index,
                    elapsed: started.elapsed().as_secs_f64(),
                });
            }
            Err(error) => {
                let _ = sender.send(WorkerEvent::Failed {
                    index,
                    message: format!("{error:#}"),
                });
            }
        }
    }
    let _ = sender.send(WorkerEvent::Complete {
        cancelled: cancel.load(Ordering::Relaxed),
    });
}

fn convert_one(
    input: &Path,
    output: &Path,
    quality: Option<u8>,
    skip_associated: bool,
    overwrite: bool,
) -> Result<PathBuf> {
    let input = input
        .canonicalize()
        .with_context(|| format!("input not found: {}", input.display()))?;
    let extension = input
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let slide = match extension.as_str() {
        "dmetrix" => dmetrix::parse(&input)?,
        "sdpc" | "dyqx" => sdpc::parse(&input)?,
        "csp" | "kfb" | "mdsx" | "msdx" => indexed::parse(&input)?,
        "ndpi" | "mrxs" => {
            let selected_quality = quality.unwrap_or(75);
            vips::convert(&input, output, selected_quality, skip_associated, overwrite)?;
            return Ok(output.to_path_buf());
        }
        other => bail!("unsupported input extension .{other}"),
    };
    let selected_quality = quality.unwrap_or(slide.metadata.jpeg_quality);
    svs::write_slide(
        &slide,
        output,
        &svs::WriteOptions {
            jpeg_quality: selected_quality,
            skip_associated,
            overwrite,
        },
    )?;
    Ok(output.to_path_buf())
}

fn plan_jobs(inputs: impl IntoIterator<Item = PathBuf>, output_dir: Option<&Path>) -> Vec<Job> {
    let mut reserved = HashSet::new();
    inputs
        .into_iter()
        .map(|input| {
            let base = output_path_for(&input, output_dir);
            let output = unique_output_path(base, &mut reserved);
            Job { input, output }
        })
        .collect()
}

fn output_path_for(input: &Path, output_dir: Option<&Path>) -> PathBuf {
    let mut name = input
        .file_stem()
        .map(|value| value.to_os_string())
        .unwrap_or_else(|| "output".into());
    name.push(".svs");
    output_dir
        .map(|directory| directory.join(&name))
        .unwrap_or_else(|| input.with_file_name(name))
}

fn unique_output_path(base: PathBuf, reserved: &mut HashSet<String>) -> PathBuf {
    if reserved.insert(normalize_path(&base)) {
        return base;
    }
    let stem = base
        .file_stem()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_owned());
    let extension = base
        .extension()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "svs".to_owned());
    for suffix in 2.. {
        let candidate = base
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("{stem}_{suffix}.{extension}"));
        if reserved.insert(normalize_path(&candidate)) {
            return candidate;
        }
    }
    unreachable!("output suffix range exhausted")
}

fn collect_supported_files(directory: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_supported_files(&path, output);
        } else if is_supported(&path) {
            output.push(path);
        }
    }
}

fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| {
            SUPPORTED_EXTENSIONS
                .iter()
                .any(|extension| value.eq_ignore_ascii_case(extension))
        })
        .unwrap_or(false)
}

fn normalize_path(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::{output_path_for, plan_jobs};
    use std::path::Path;

    #[test]
    fn duplicate_names_in_one_output_directory_get_unique_paths() {
        let output_dir = Path::new(r"C:\converted");
        let jobs = plan_jobs(
            [
                r"C:\slides\one\sample.kfb".into(),
                r"C:\slides\two\sample.kfb".into(),
                r"C:\slides\three\sample_2.kfb".into(),
            ],
            Some(output_dir),
        );

        let outputs: Vec<_> = jobs
            .iter()
            .map(|job| job.output.to_string_lossy().to_lowercase())
            .collect();
        assert_eq!(outputs[0], r"c:\converted\sample.svs");
        assert_eq!(outputs[1], r"c:\converted\sample_2.svs");
        assert_eq!(outputs[2], r"c:\converted\sample_2_2.svs");
    }

    #[test]
    fn empty_output_directory_keeps_source_directory() {
        let input = Path::new(r"C:\slides\nested\sample.sdpc");
        assert_eq!(
            output_path_for(input, None),
            Path::new(r"C:\slides\nested\sample.svs")
        );
    }
}
