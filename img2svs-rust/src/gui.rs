use crate::{dmetrix, indexed, sdpc, svs, vips};
use anyhow::{bail, Context, Result};
use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, RichText,
    Shadow, Stroke, TextStyle, Vec2,
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
    "csp", "dmetrix", "kfb", "mdsx", "msdx", "mrxs", "ndpi", "tif", "tiff", "sdpc", "dyqx",
];

/// Display names for the format strip in the header.
const FORMAT_LABELS: &[&str] = &[
    "CSP", "DMETRIX", "KFB", "MDSX", "MSDX", "MRXS", "NDPI", "TIF/TIFF", "SDPC", "DYQX",
];

/// Design tokens for the view layer.
///
/// Every colour, radius, margin and size used by the GUI lives here so the
/// whole palette can be re-themed (or a dark theme added) in a single place.
mod theme {
    use eframe::egui::{Color32, Margin};

    pub const CANVAS: Color32 = Color32::from_rgb(244, 246, 249);
    pub const SURFACE: Color32 = Color32::from_rgb(255, 255, 255);
    pub const SUNKEN: Color32 = Color32::from_rgb(238, 242, 247);

    pub const BORDER_SUBTLE: Color32 = Color32::from_rgb(227, 233, 240);
    pub const BORDER_STRONG: Color32 = Color32::from_rgb(203, 214, 226);

    pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(27, 43, 58);
    pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(91, 110, 128);
    pub const TEXT_DISABLED: Color32 = Color32::from_rgb(154, 170, 186);

    pub const PRIMARY: Color32 = Color32::from_rgb(14, 147, 132);
    pub const PRIMARY_HOVER: Color32 = Color32::from_rgb(11, 124, 112);
    pub const PRIMARY_SUBTLE: Color32 = Color32::from_rgb(230, 245, 243);

    pub const INFO: Color32 = Color32::from_rgb(46, 118, 199);
    pub const SUCCESS: Color32 = Color32::from_rgb(31, 146, 84);
    pub const SUCCESS_BG: Color32 = Color32::from_rgb(232, 246, 238);
    pub const WARNING: Color32 = Color32::from_rgb(183, 121, 31);
    pub const WARNING_BG: Color32 = Color32::from_rgb(252, 243, 227);
    pub const DANGER: Color32 = Color32::from_rgb(199, 68, 59);
    pub const DANGER_BG: Color32 = Color32::from_rgb(252, 237, 235);
    pub const NEUTRAL: Color32 = Color32::from_rgb(112, 128, 144);

    pub const RADIUS_CONTROL: u8 = 8;
    pub const RADIUS_CARD: u8 = 14;

    /// Page padding of the queue column (left/right/top/bottom).
    pub const QUEUE_MARGIN: Margin = Margin::symmetric(20, 18);
    /// Padding of the fixed settings rail; no left margin so the gutter is 20px.
    pub const RAIL_MARGIN: Margin = Margin {
        left: 0,
        right: 20,
        top: 18,
        bottom: 18,
    };
    pub const CARD_MARGIN: Margin = Margin::symmetric(18, 16);
    pub const RAIL_WIDTH: f32 = 340.0;
}

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

    /// Non-colour cue, so a state stays readable without relying on hue.
    fn icon(self) -> &'static str {
        match self {
            Self::Waiting => "●",
            Self::Running => "◐",
            Self::Done => "✓",
            Self::Failed => "✕",
            Self::Cancelled => "○",
        }
    }

    fn color(self) -> Color32 {
        match self {
            Self::Waiting => theme::NEUTRAL,
            Self::Running => theme::INFO,
            Self::Done => theme::SUCCESS,
            Self::Failed => theme::DANGER,
            Self::Cancelled => theme::WARNING,
        }
    }

    fn badge_background(self) -> Color32 {
        match self {
            Self::Waiting => theme::SUNKEN,
            Self::Running => theme::PRIMARY_SUBTLE,
            Self::Done => theme::SUCCESS_BG,
            Self::Failed => theme::DANGER_BG,
            Self::Cancelled => theme::WARNING_BG,
        }
    }
}

struct GuiOptions {
    output_dir: String,
    jpeg_quality: String,
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
    /// Screen rect of the output-path field, used to route folder drops.
    output_field_rect: Option<egui::Rect>,
}

impl SvsGui {
    fn new(cc: &CreationContext<'_>, smoke_test: bool) -> Self {
        apply_style(&cc.egui_ctx);
        install_windows_font(&cc.egui_ctx);
        Self {
            items: Vec::new(),
            options: GuiOptions {
                output_dir: String::new(),
                jpeg_quality: "原始".to_owned(),
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
            output_field_rect: None,
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

    fn choose_files(&mut self, parent: &Frame) {
        if let Some(paths) = FileDialog::new()
            .add_filter("Whole-slide files", SUPPORTED_EXTENSIONS)
            .set_parent(parent)
            .pick_files()
        {
            self.add_paths(paths);
        }
    }

    fn choose_folder(&mut self, parent: &Frame) {
        if let Some(path) = FileDialog::new().set_parent(parent).pick_folder() {
            self.add_paths([path]);
        }
    }

    fn choose_output(&mut self, parent: &Frame) {
        if let Some(path) = FileDialog::new().set_parent(parent).pick_folder() {
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
            .filter_map(|(index, item)| {
                matches!(item.state, ItemState::Waiting | ItemState::Cancelled).then_some(index)
            })
            .collect();
        if pending_indices.is_empty() {
            self.last_message = "没有待转换的切片".to_owned();
            self.log(format!("{}。", self.last_message));
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
        let overwrite = self.options.overwrite;
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        thread::spawn(move || run_jobs(jobs, quality, overwrite, sender, worker_cancel));
        self.receiver = Some(receiver);
        self.cancel = Some(cancel);
        self.running = true;
        self.completed = 0;
        self.failed = 0;
        self.batch_total = batch_total;
        self.active_indices = pending_indices;
        // Without this the status bar keeps showing the previous run's outcome,
        // e.g. "已停止：..." after stopping a run and starting it again.
        self.last_message = "正在转换…".to_owned();
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

/// Applies the shared widget style: spacing, type scale, radii and widget colours.
fn apply_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();

    style.spacing.item_spacing = Vec2::new(10.0, 10.0);
    style.spacing.button_padding = Vec2::new(14.0, 9.0);
    style.spacing.interact_size = Vec2::new(44.0, 38.0);

    style
        .text_styles
        .insert(TextStyle::Body, FontId::proportional(15.0));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::proportional(15.0));
    style
        .text_styles
        .insert(TextStyle::Small, FontId::proportional(13.0));
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::proportional(17.0));
    style
        .text_styles
        .insert(TextStyle::Monospace, FontId::monospace(13.0));

    let radius = CornerRadius::same(theme::RADIUS_CONTROL);
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = theme::CANVAS;
    visuals.window_fill = theme::SURFACE;
    visuals.window_stroke = Stroke::new(1.0_f32, theme::BORDER_SUBTLE);
    visuals.window_corner_radius = CornerRadius::same(theme::RADIUS_CARD);
    visuals.window_shadow = Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: Color32::from_black_alpha(30),
    };
    visuals.extreme_bg_color = theme::SURFACE;
    visuals.faint_bg_color = theme::SUNKEN;
    visuals.selection.bg_fill = theme::PRIMARY;
    visuals.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);

    visuals.widgets.noninteractive.bg_fill = theme::SURFACE;
    visuals.widgets.noninteractive.weak_bg_fill = theme::SUNKEN;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, theme::BORDER_SUBTLE);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, theme::TEXT_PRIMARY);
    visuals.widgets.noninteractive.corner_radius = radius;

    visuals.widgets.inactive.bg_fill = theme::SUNKEN;
    visuals.widgets.inactive.weak_bg_fill = theme::SUNKEN;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, theme::BORDER_STRONG);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, theme::TEXT_PRIMARY);
    visuals.widgets.inactive.corner_radius = radius;

    visuals.widgets.hovered.bg_fill = theme::PRIMARY_SUBTLE;
    visuals.widgets.hovered.weak_bg_fill = theme::PRIMARY_SUBTLE;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, theme::PRIMARY);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, theme::TEXT_PRIMARY);
    visuals.widgets.hovered.corner_radius = radius;

    visuals.widgets.active.bg_fill = theme::PRIMARY;
    visuals.widgets.active.weak_bg_fill = theme::PRIMARY;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, theme::PRIMARY_HOVER);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    visuals.widgets.active.corner_radius = radius;

    visuals.widgets.open.bg_fill = theme::SURFACE;
    visuals.widgets.open.weak_bg_fill = theme::PRIMARY_SUBTLE;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, theme::BORDER_STRONG);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, theme::TEXT_PRIMARY);
    visuals.widgets.open.corner_radius = radius;

    style.visuals = visuals;
    ctx.set_style(style);
}

/// The standard elevated white panel used for every card in the layout.
fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(theme::SURFACE)
        .stroke(Stroke::new(1.0_f32, theme::BORDER_SUBTLE))
        .corner_radius(CornerRadius::same(theme::RADIUS_CARD))
        .inner_margin(theme::CARD_MARGIN)
        .shadow(Shadow {
            offset: [0, 2],
            blur: 6,
            spread: 0,
            color: Color32::from_black_alpha(18),
        })
}

/// A small rounded pill, used for counters and format names.
fn chip(ui: &mut egui::Ui, text: impl Into<String>, fg: Color32, bg: Color32) {
    egui::Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(9, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(text.into()).size(12.0).color(fg));
        });
}

/// Placeholder shown while the queue has no items.
fn empty_state(ui: &mut egui::Ui) {
    let height = ui.available_height().max(180.0);
    egui::Frame::new()
        .fill(theme::SUNKEN)
        .stroke(Stroke::new(1.0_f32, theme::BORDER_STRONG))
        .corner_radius(CornerRadius::same(theme::RADIUS_CONTROL))
        .inner_margin(Margin::symmetric(16, 16))
        .show(ui, |ui| {
            let inner_height = (height - 34.0).max(120.0);
            ui.set_min_height(inner_height);
            ui.vertical_centered(|ui| {
                ui.add_space((inner_height / 2.0 - 46.0).max(8.0));
                ui.label(
                    RichText::new("将切片文件或文件夹拖到这里")
                        .size(16.0)
                        .color(theme::TEXT_SECONDARY),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new("支持批量拖入，也可以使用上方的添加按钮")
                        .size(13.0)
                        .color(theme::TEXT_DISABLED),
                );
            });
        });
}

/// Slim progress track.
///
/// egui's own `ProgressBar` clamps its fill to at least one corner diameter,
/// so at 0% it paints a stray rounded blob at the left end. Drawing the track
/// here keeps the empty state clean and only paints a fill once there is
/// progress.
fn progress_bar(ui: &mut egui::Ui, fraction: f32, width: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 8.0), egui::Sense::hover());
    let radius = CornerRadius::same(4);
    let fraction = fraction.clamp(0.0, 1.0);
    let painter = ui.painter();
    painter.rect_filled(rect, radius, theme::SUNKEN);
    if fraction > 0.0 {
        painter.rect_filled(
            egui::Rect::from_min_size(rect.min, Vec2::new(rect.width() * fraction, rect.height())),
            radius,
            color,
        );
    }
}

/// Clamps a log line so the collapsed drawer header stays on one line.
fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn install_windows_font(ctx: &egui::Context) {
    #[cfg(target_os = "windows")]
    {
        for (font_name, font_path) in [
            ("microsoft-yahei", r"C:\Windows\Fonts\msyh.ttc"),
            ("microsoft-yahei", r"C:\Windows\Fonts\msyh.ttf"),
            ("simhei", r"C:\Windows\Fonts\simhei.ttf"),
        ] {
            let Ok(bytes) = std::fs::read(font_path) else {
                continue;
            };
            let mut font_data = FontData::from_owned(bytes);
            font_data.index = 0;
            let mut fonts = FontDefinitions::default();
            fonts
                .font_data
                .insert(font_name.to_owned(), Arc::new(font_data));
            if let Some(proportional) = fonts.families.get_mut(&FontFamily::Proportional) {
                proportional.insert(0, font_name.to_owned());
            }
            if let Some(monospace) = fonts.families.get_mut(&FontFamily::Monospace) {
                monospace.insert(0, font_name.to_owned());
            }
            ctx.set_fonts(fonts);
            break;
        }
    }
}

impl App for SvsGui {
    fn update(&mut self, ctx: &egui::Context, frame: &mut Frame) {
        self.receive_events();
        if self.smoke_test {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        // The native file dialogs need an owner window. Without one Windows
        // gives them their own taskbar button and they are not modal to us.
        let window: &Frame = frame;
        self.handle_shortcuts(window, ctx);
        self.handle_dropped_files(ctx);
        ctx.request_repaint_after(std::time::Duration::from_millis(100));

        // The top bar is painted as a card inside this panel, so it gets the
        // same four rounded corners and page margin as the workspace cards.
        egui::TopBottomPanel::top("header")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(theme::CANVAS).inner_margin(Margin {
                left: 20,
                right: 20,
                top: 18,
                bottom: 0,
            }))
            .show(ctx, |ui| self.header(ui));

        egui::TopBottomPanel::bottom("status_bar")
            .exact_height(54.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::SURFACE)
                    .inner_margin(Margin {
                        left: 20,
                        right: 20,
                        top: 0,
                        bottom: 0,
                    }),
            )
            .show(ctx, |ui| self.status_bar(ui));

        egui::TopBottomPanel::bottom("logs")
            .show_separator_line(false)
            .exact_height(if self.logs_collapsed { 54.0 } else { 232.0 })
            .frame(egui::Frame::new().fill(theme::CANVAS).inner_margin(Margin {
                left: 20,
                right: 20,
                top: 6,
                bottom: 6,
            }))
            .show(ctx, |ui| self.logs_panel(ui));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::CANVAS)
                    .inner_margin(Margin::ZERO),
            )
            .show(ctx, |ui| self.workspace(window, ui));
    }
}

impl SvsGui {
    fn handle_shortcuts(&mut self, parent: &Frame, ctx: &egui::Context) {
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
            self.choose_folder(parent);
        } else if ctrl_o {
            self.choose_files(parent);
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let (paths, pointer) = ctx.input(|input| {
            (
                input
                    .raw
                    .dropped_files
                    .iter()
                    .filter_map(|file| file.path.clone())
                    .collect::<Vec<PathBuf>>(),
                input.pointer.latest_pos(),
            )
        });
        if paths.is_empty() {
            return;
        }
        // A folder dropped on the output field sets the output directory. Files
        // always go to the queue, so a slide dropped on the field is not
        // silently turned into a directory choice.
        let dropped_on_field = pointer.is_some_and(|position| {
            self.output_field_rect
                .is_some_and(|rect| rect.contains(position))
        });
        if dropped_on_field {
            if let Some(directory) = paths.iter().find(|path| path.is_dir()) {
                self.options.output_dir = directory.display().to_string();
                self.last_message = format!("输出目录：{}", directory.display());
                self.log(format!("输出目录已设置为 {}。", directory.display()));
                return;
            }
        }
        self.add_paths(paths);
    }

    /// True while a dragged item hovers the output-path field, so the field can
    /// show that dropping a folder there picks the directory.
    fn output_field_accepts_drop(&self, ctx: &egui::Context) -> bool {
        let Some(rect) = self.output_field_rect else {
            return false;
        };
        ctx.input(|input| {
            !input.raw.hovered_files.is_empty()
                && input
                    .pointer
                    .hover_pos()
                    .is_some_and(|position| rect.contains(position))
        })
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        // A card like the workspace panels, so all four corners are rounded
        // instead of running to the window edges.
        card().show(ui, |ui| {
            ui.horizontal(|ui| {
                egui::Frame::new()
                    .fill(theme::PRIMARY)
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::symmetric(12, 7))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("SVS")
                                .strong()
                                .size(17.0)
                                .color(Color32::WHITE),
                        );
                    });
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        RichText::new("病理图像转 SVS 工具")
                            .strong()
                            .size(19.0)
                            .color(theme::TEXT_PRIMARY),
                    );
                    ui.label(
                        RichText::new("把常见数字病理切片批量转换为兼容性更好的 SVS")
                            .size(13.0)
                            .color(theme::TEXT_SECONDARY),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let stop_enabled = self.running;
                    let stop = if stop_enabled {
                        egui::Button::new(
                            RichText::new("停止").size(15.0).color(theme::TEXT_PRIMARY),
                        )
                        .fill(theme::SURFACE)
                    } else {
                        egui::Button::new(
                            RichText::new("停止").size(15.0).color(theme::TEXT_DISABLED),
                        )
                        .fill(theme::SUNKEN)
                    }
                    .corner_radius(CornerRadius::same(theme::RADIUS_CONTROL))
                    .min_size(Vec2::new(96.0, 40.0));
                    if ui.add_enabled(stop_enabled, stop).clicked() {
                        self.stop();
                    }

                    let start_enabled = !self.running;
                    let start = if start_enabled {
                        egui::Button::new(
                            RichText::new("开始转换  F5")
                                .size(15.0)
                                .color(Color32::WHITE),
                        )
                        .fill(theme::PRIMARY)
                    } else {
                        egui::Button::new(
                            RichText::new("开始转换  F5")
                                .size(15.0)
                                .color(theme::TEXT_DISABLED),
                        )
                        .fill(theme::SUNKEN)
                    }
                    .corner_radius(CornerRadius::same(theme::RADIUS_CONTROL))
                    .min_size(Vec2::new(150.0, 40.0));
                    if ui.add_enabled(start_enabled, start).clicked() {
                        self.start();
                    }
                });
            });
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("支持格式")
                        .size(12.0)
                        .color(theme::TEXT_SECONDARY),
                );
                ui.add_space(2.0);
                for label in FORMAT_LABELS {
                    chip(ui, *label, theme::TEXT_SECONDARY, theme::SUNKEN);
                }
            });
        });
    }

    /// Queue on the left (flexible) and the fixed-width settings rail on the right.
    fn workspace(&mut self, parent: &Frame, ui: &mut egui::Ui) {
        egui::SidePanel::right("settings_rail")
            .resizable(false)
            .show_separator_line(false)
            .exact_width(theme::RAIL_WIDTH)
            .frame(
                egui::Frame::new()
                    .fill(theme::CANVAS)
                    .inner_margin(theme::RAIL_MARGIN),
            )
            .show_inside(ui, |ui| self.settings(parent, ui));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::CANVAS)
                    .inner_margin(theme::QUEUE_MARGIN),
            )
            .show_inside(ui, |ui| self.sources(parent, ui));
    }

    /// Slim strip holding the progress bar, live counters and the latest status.
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let total = self.batch_total;
        let processed = self.completed + self.failed;
        let fraction = if total == 0 {
            0.0
        } else {
            processed as f32 / total as f32
        };
        let waiting = self
            .items
            .iter()
            .filter(|item| item.state == ItemState::Waiting)
            .count();
        let running = self.running;
        let completed = self.completed;
        let failed = self.failed;
        let finished_with_failures = !running && failed > 0;
        let message = self.last_message.as_str();

        ui.horizontal_centered(|ui| {
            if running {
                ui.add(egui::Spinner::new().size(15.0).color(theme::PRIMARY));
            } else {
                ui.add_space(15.0);
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new(message)
                    .size(14.0)
                    .color(theme::TEXT_SECONDARY),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if failed > 0 {
                    chip(
                        ui,
                        format!("✕ 失败 {failed}"),
                        theme::DANGER,
                        theme::DANGER_BG,
                    );
                }
                if completed > 0 {
                    chip(
                        ui,
                        format!("✓ 完成 {completed}"),
                        theme::SUCCESS,
                        theme::SUCCESS_BG,
                    );
                }
                if waiting > 0 {
                    chip(
                        ui,
                        format!("● 等待 {waiting}"),
                        theme::NEUTRAL,
                        theme::SUNKEN,
                    );
                }
                ui.add_space(10.0);
                ui.label(
                    RichText::new(format!("{processed} / {total}"))
                        .size(13.0)
                        .color(theme::TEXT_PRIMARY),
                );
                ui.add_space(10.0);
                progress_bar(
                    ui,
                    fraction,
                    200.0,
                    if finished_with_failures {
                        theme::WARNING
                    } else {
                        theme::PRIMARY
                    },
                );
            });
        });
    }

    fn sources(&mut self, parent: &Frame, ui: &mut egui::Ui) {
        let item_count = self.items.len();
        let can_modify = !self.running;
        let can_clear = can_modify && item_count > 0;

        card().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("1  切片队列")
                        .strong()
                        .size(16.0)
                        .color(theme::TEXT_PRIMARY),
                );
                chip(
                    ui,
                    format!("{item_count} 项"),
                    theme::TEXT_SECONDARY,
                    theme::SUNKEN,
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            can_clear,
                            egui::Button::new("清空").min_size(Vec2::new(72.0, 34.0)),
                        )
                        .clicked()
                    {
                        self.items.clear();
                        self.completed = 0;
                        self.failed = 0;
                        self.batch_total = 0;
                        self.last_message = "等待添加切片".to_owned();
                    }
                    if ui
                        .add_enabled(
                            can_modify,
                            egui::Button::new("添加目录").min_size(Vec2::new(96.0, 34.0)),
                        )
                        .clicked()
                    {
                        self.choose_folder(parent);
                    }
                    if ui
                        .add_enabled(
                            can_modify,
                            egui::Button::new(RichText::new("＋ 添加文件").color(Color32::WHITE))
                                .fill(theme::PRIMARY)
                                .corner_radius(CornerRadius::same(theme::RADIUS_CONTROL))
                                .min_size(Vec2::new(118.0, 34.0)),
                        )
                        .clicked()
                    {
                        self.choose_files(parent);
                    }
                });
            });
            ui.add_space(2.0);
            ui.label(
                RichText::new("支持多文件与目录批量添加，也可以把切片直接拖拽到下方列表。")
                    .size(13.0)
                    .color(theme::TEXT_SECONDARY),
            );
            ui.add_space(12.0);

            if self.items.is_empty() {
                empty_state(ui);
                return;
            }

            egui::Frame::new()
                .fill(theme::SURFACE)
                .stroke(Stroke::new(1.0_f32, theme::BORDER_SUBTLE))
                .corner_radius(CornerRadius::same(theme::RADIUS_CONTROL))
                .inner_margin(Margin::ZERO)
                .show(ui, |ui| {
                    let action_width = 72.0;
                    let status_width = 92.0;
                    let row_padding = 14.0;
                    let spacing = ui.spacing().item_spacing.x;
                    // Leading inset + status + path + action, plus the item
                    // spacing egui inserts between them, plus a 12px right inset.
                    let path_width = (ui.available_width()
                        - row_padding
                        - status_width
                        - action_width
                        - spacing * 3.0
                        - 12.0)
                        .max(120.0);

                    // Header band. It repeats the exact allocation sequence of a
                    // row so that the three columns line up pixel for pixel.
                    egui::Frame::new()
                        .fill(theme::SUNKEN)
                        .corner_radius(CornerRadius {
                            nw: theme::RADIUS_CONTROL,
                            ne: theme::RADIUS_CONTROL,
                            sw: 0,
                            se: 0,
                        })
                        .inner_margin(Margin {
                            left: 0,
                            right: 0,
                            top: 7,
                            bottom: 7,
                        })
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let _ = ui.allocate_exact_size(
                                    Vec2::new(row_padding, 20.0),
                                    egui::Sense::hover(),
                                );
                                let (status_rect, _) = ui.allocate_exact_size(
                                    Vec2::new(status_width, 20.0),
                                    egui::Sense::hover(),
                                );
                                let (path_rect, _) = ui.allocate_exact_size(
                                    Vec2::new(path_width, 20.0),
                                    egui::Sense::hover(),
                                );
                                let action_center = egui::pos2(
                                    path_rect.right() + spacing + action_width / 2.0,
                                    status_rect.center().y,
                                );
                                let painter = ui.painter();
                                painter.text(
                                    status_rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    "状态",
                                    FontId::proportional(13.0),
                                    theme::TEXT_SECONDARY,
                                );
                                painter.text(
                                    path_rect.left_center(),
                                    egui::Align2::LEFT_CENTER,
                                    "文件或目录",
                                    FontId::proportional(13.0),
                                    theme::TEXT_SECONDARY,
                                );
                                painter.text(
                                    action_center,
                                    egui::Align2::CENTER_CENTER,
                                    "操作",
                                    FontId::proportional(13.0),
                                    theme::TEXT_SECONDARY,
                                );
                            });
                        });

                    egui::ScrollArea::vertical()
                        .id_salt("queue_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            let mut remove_index = None;
                            for index in 0..self.items.len() {
                                let item = &self.items[index];
                                let path = item.path.display().to_string();
                                let status_text =
                                    format!("{} {}", item.state.icon(), item.state.label());
                                let status_color = item.state.color();
                                let status_background = item.state.badge_background();
                                ui.horizontal(|ui| {
                                    let _ = ui.allocate_exact_size(
                                        Vec2::new(row_padding, 26.0),
                                        egui::Sense::hover(),
                                    );
                                    let (status_rect, _) = ui.allocate_exact_size(
                                        Vec2::new(status_width, 26.0),
                                        egui::Sense::hover(),
                                    );
                                    let painter = ui.painter();
                                    painter.rect_filled(
                                        status_rect,
                                        CornerRadius::same(9),
                                        status_background,
                                    );
                                    painter.text(
                                        status_rect.center(),
                                        egui::Align2::CENTER_CENTER,
                                        &status_text,
                                        FontId::proportional(12.0),
                                        status_color,
                                    );
                                    let (path_rect, response) = ui.allocate_exact_size(
                                        Vec2::new(path_width, 46.0),
                                        egui::Sense::hover(),
                                    );
                                    let painter = ui.painter().with_clip_rect(path_rect);
                                    painter.text(
                                        path_rect.left_center(),
                                        egui::Align2::LEFT_CENTER,
                                        &path,
                                        FontId::proportional(14.0),
                                        theme::TEXT_PRIMARY,
                                    );
                                    let _ = response.on_hover_text(path.clone());
                                    if ui
                                        .add_enabled(
                                            !self.running,
                                            egui::Button::new("移除")
                                                .min_size(Vec2::new(action_width, 32.0)),
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
                        });
                });
        });
    }

    fn settings(&mut self, parent: &Frame, ui: &mut egui::Ui) {
        // The card fills the rail and scrolls internally, so the settings card
        // always matches the height of the queue card while resizing.
        card().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("settings_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| self.settings_content(parent, ui));
        });
    }

    fn settings_content(&mut self, parent: &Frame, ui: &mut egui::Ui) {
        let quality_text = self.options.jpeg_quality.clone();
        let drop_target = self.output_field_accepts_drop(ui.ctx());

        ui.label(
            RichText::new("2  转换设置")
                .strong()
                .size(16.0)
                .color(theme::TEXT_PRIMARY),
        );
        ui.add_space(2.0);
        ui.label(
            RichText::new("默认选项适合大多数切片。")
                .size(13.0)
                .color(theme::TEXT_SECONDARY),
        );
        ui.add_space(14.0);

        ui.label(
            RichText::new("输出位置")
                .strong()
                .size(14.0)
                .color(theme::TEXT_PRIMARY),
        );
        ui.add_space(6.0);
        let browse_width = 68.0;
        let item_spacing = ui.spacing().item_spacing.x;
        let input_width = (ui.available_width() - browse_width - item_spacing).max(120.0);
        ui.horizontal(|ui| {
            let field = egui::Frame::new()
                .fill(if drop_target {
                    theme::PRIMARY_SUBTLE
                } else {
                    theme::SURFACE
                })
                .stroke(Stroke::new(
                    1.0_f32,
                    if drop_target {
                        theme::PRIMARY
                    } else {
                        theme::BORDER_STRONG
                    },
                ))
                .corner_radius(CornerRadius::same(theme::RADIUS_CONTROL))
                .inner_margin(Margin {
                    left: 10,
                    right: 10,
                    top: 0,
                    bottom: 0,
                })
                .show(ui, |ui| {
                    ui.add_sized(
                        [input_width - 20.0, 36.0],
                        egui::TextEdit::singleline(&mut self.options.output_dir)
                            .frame(false)
                            .vertical_align(egui::Align::Center),
                    );
                });
            // Remembered so `handle_dropped_files` knows where a dropped
            // folder should land.
            self.output_field_rect = Some(field.response.rect);
            if ui
                .add_sized([browse_width, 38.0], egui::Button::new("浏览"))
                .clicked()
            {
                self.choose_output(parent);
            }
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new(if drop_target {
                "松开即可把该文件夹设为输出目录"
            } else {
                "留空则输出到源文件目录，也可以把文件夹拖到这里"
            })
            .size(12.0)
            .color(if drop_target {
                theme::PRIMARY
            } else {
                theme::TEXT_SECONDARY
            }),
        );
        ui.add_space(12.0);

        ui.label(
            RichText::new("SVS 保存质量")
                .strong()
                .size(14.0)
                .color(theme::TEXT_PRIMARY),
        );
        ui.add_space(6.0);
        egui::ComboBox::from_id_salt("quality")
            .selected_text(quality_text)
            .width(ui.available_width())
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
        ui.add_space(6.0);
        ui.label(
            RichText::new("“原始/推荐”沿用源图质量，数值越低文件越小。")
                .size(12.0)
                .color(theme::TEXT_SECONDARY),
        );
        ui.add_space(14.0);
        ui.checkbox(
            &mut self.options.overwrite,
            RichText::new("覆盖已存在的 SVS").size(14.0),
        );
    }

    fn logs_panel(&mut self, ui: &mut egui::Ui) {
        let preview = self
            .logs
            .last()
            .map(|line| truncate(line, 60))
            .unwrap_or_default();
        let collapsed = self.logs_collapsed;

        ui.horizontal(|ui| {
            ui.label(
                RichText::new("3  运行日志")
                    .strong()
                    .size(15.0)
                    .color(theme::TEXT_PRIMARY),
            );
            ui.label(
                RichText::new(preview)
                    .size(13.0)
                    .color(theme::TEXT_SECONDARY),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Both toolbar buttons share one size so they read as a pair.
                let button_size = Vec2::new(96.0, 34.0);
                let toggle_label = if collapsed { "展开" } else { "收起" };
                if ui
                    .add(egui::Button::new(toggle_label).min_size(button_size))
                    .clicked()
                {
                    self.logs_collapsed = !self.logs_collapsed;
                }
                if !collapsed
                    && ui
                        .add(egui::Button::new("清空日志").min_size(button_size))
                        .clicked()
                {
                    self.logs.clear();
                }
            });
        });

        if collapsed {
            return;
        }

        ui.add_space(6.0);
        let body_height = (ui.available_height() - 6.0).max(80.0);
        egui::Frame::new()
            .fill(theme::SURFACE)
            .stroke(Stroke::new(1.0_f32, theme::BORDER_SUBTLE))
            .corner_radius(CornerRadius::same(theme::RADIUS_CONTROL))
            .inner_margin(Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical()
                    .id_salt("logs_scroll")
                    .auto_shrink([false, false])
                    .max_height((body_height - 24.0).max(40.0))
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        for line in &self.logs {
                            ui.label(
                                RichText::new(line)
                                    .monospace()
                                    .size(12.0)
                                    .color(theme::TEXT_SECONDARY),
                            );
                        }
                    });
            });
    }
}

fn run_jobs(
    jobs: Vec<Job>,
    quality: Option<u8>,
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
        let result = convert_one(&job.input, &job.output, quality, overwrite);
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
        "ndpi" | "mrxs" | "tif" | "tiff" => {
            let selected_quality = quality.unwrap_or(75);
            vips::convert(&input, output, selected_quality, overwrite)?;
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
    use super::{is_supported, output_path_for, plan_jobs};
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

    #[test]
    fn tiff_extensions_are_supported_case_insensitively() {
        assert!(is_supported(Path::new(r"C:\slides\sample.tif")));
        assert!(is_supported(Path::new(r"C:\slides\sample.TIFF")));
    }
}
