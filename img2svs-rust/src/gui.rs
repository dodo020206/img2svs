use crate::{dmetrix, indexed, sdpc, svs, vips};
use anyhow::{bail, Context, Result};
use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, RichText, Vec2};
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

pub struct LaunchOptions {
    pub smoke_test: bool,
}

pub fn run(options: LaunchOptions) -> Result<()> {
    let native_options = NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Pathology SVS Converter")
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([860.0, 560.0]),
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
    selected: HashSet<usize>,
    options: GuiOptions,
    logs: Vec<String>,
    receiver: Option<Receiver<WorkerEvent>>,
    cancel: Option<Arc<AtomicBool>>,
    running: bool,
    completed: usize,
    failed: usize,
    smoke_test: bool,
    last_message: String,
}

impl SvsGui {
    fn new(cc: &CreationContext<'_>, smoke_test: bool) -> Self {
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = Vec2::new(8.0, 7.0);
        style.visuals = egui::Visuals::dark();
        style.visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(29, 38, 51);
        style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(42, 54, 70);
        style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(51, 83, 111);
        style.visuals.selection.bg_fill = Color32::from_rgb(34, 123, 197);
        cc.egui_ctx.set_style(style);
        install_windows_font(&cc.egui_ctx);
        Self {
            items: Vec::new(),
            selected: HashSet::new(),
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
            smoke_test,
            last_message: "等待添加切片".to_owned(),
        }
    }

    fn add_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        if self.running {
            self.log("转换运行中，暂时不能添加文件。".to_owned());
            return;
        }
        let mut added = 0;
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
            if !seen.insert(key) {
                duplicate += 1;
                continue;
            }
            self.items.push(InputItem {
                path,
                state: ItemState::Waiting,
            });
            added += 1;
        }
        self.last_message = format!("新增 {added} 项，重复 {duplicate} 项，忽略 {ignored} 项");
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

    fn remove_selected(&mut self) {
        if self.running {
            return;
        }
        let selected = &self.selected;
        self.items = self
            .items
            .drain(..)
            .enumerate()
            .filter_map(|(index, item)| (!selected.contains(&index)).then_some(item))
            .collect();
        self.selected.clear();
    }

    fn start(&mut self) {
        if self.running || self.items.is_empty() {
            if self.items.is_empty() {
                self.last_message = "请先添加切片文件".to_owned();
            }
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
        let jobs: Vec<PathBuf> = self.items.iter().map(|item| item.path.clone()).collect();
        let output_dir = (!self.options.output_dir.trim().is_empty())
            .then(|| PathBuf::from(self.options.output_dir.trim()));
        let skip_associated = self.options.skip_associated;
        let overwrite = self.options.overwrite;
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        thread::spawn(move || {
            run_jobs(
                jobs,
                output_dir,
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
        self.items
            .iter_mut()
            .for_each(|item| item.state = ItemState::Waiting);
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
                    if let Some(item) = self.items.get_mut(index) {
                        item.state = ItemState::Running;
                    }
                }
                WorkerEvent::Log(message) => self.log(message),
                WorkerEvent::Finished { index, elapsed } => {
                    if let Some(item) = self.items.get_mut(index) {
                        item.state = ItemState::Done;
                    }
                    self.completed += 1;
                    self.log(format!(
                        "完成：{}（{elapsed:.1}s）",
                        self.items[index].path.display()
                    ));
                }
                WorkerEvent::Failed { index, message } => {
                    if let Some(item) = self.items.get_mut(index) {
                        item.state = ItemState::Failed;
                    }
                    self.failed += 1;
                    self.log(format!(
                        "失败：{}：{message}",
                        self.items[index].path.display()
                    ));
                }
                WorkerEvent::Cancelled { index } => {
                    if let Some(item) = self.items.get_mut(index) {
                        item.state = ItemState::Cancelled;
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
                }
            }
        }
    }

    fn output_path(&self, input: &Path) -> PathBuf {
        let mut name = input
            .file_stem()
            .map(|value| value.to_os_string())
            .unwrap_or_else(|| "output".into());
        name.push(".svs");
        if self.options.output_dir.trim().is_empty() {
            input.with_file_name(name)
        } else {
            PathBuf::from(self.options.output_dir.trim()).join(name)
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

        egui::TopBottomPanel::top("header").show(ctx, |ui| self.header(ui));
        egui::SidePanel::right("settings")
            .resizable(true)
            .default_width(310.0)
            .show(ctx, |ui| self.settings(ui));
        egui::CentralPanel::default().show(ctx, |ui| self.sources(ui));
        egui::TopBottomPanel::bottom("logs")
            .resizable(true)
            .default_height(145.0)
            .show(ctx, |ui| self.logs_panel(ui));
    }
}

impl SvsGui {
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let (f5, escape, delete, ctrl_o, ctrl_shift_o) = ctx.input(|input| {
            (
                input.key_pressed(egui::Key::F5),
                input.key_pressed(egui::Key::Escape),
                input.key_pressed(egui::Key::Delete),
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
        if delete {
            self.remove_selected();
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
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            ui.label(
                RichText::new("SVS")
                    .strong()
                    .size(18.0)
                    .color(Color32::WHITE),
            );
            ui.vertical(|ui| {
                ui.label(RichText::new("病理图像转 SVS").strong().size(20.0));
                ui.label(RichText::new("Rust 原生桌面版 · 批量转换 Aperio SVS").weak());
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let stop = ui.add_enabled(self.running, egui::Button::new("停止  Esc"));
                if stop.clicked() {
                    self.stop();
                }
                let start = ui.add_enabled(!self.running, egui::Button::new("开始转换  F5"));
                if start.clicked() {
                    self.start();
                }
            });
        });
        let total = self.items.len();
        let fraction = if total == 0 {
            0.0
        } else {
            (self.completed + self.failed) as f32 / total as f32
        };
        ui.add(egui::ProgressBar::new(fraction).text(format!(
            "{} / {}",
            self.completed + self.failed,
            total
        )));
        ui.horizontal(|ui| {
            ui.label(RichText::new(&self.last_message).color(if self.failed > 0 {
                Color32::LIGHT_RED
            } else {
                Color32::LIGHT_BLUE
            }));
            ui.label(RichText::new(format!("队列 {} 项 · 失败 {} 项", total, self.failed)).weak());
        });
        ui.add_space(8.0);
    }

    fn sources(&mut self, ui: &mut egui::Ui) {
        ui.heading("1  选择切片");
        ui.label(RichText::new("支持多选、目录添加，也可以直接拖拽到下方列表").weak());
        ui.horizontal(|ui| {
            if ui.button("＋ 添加文件").clicked() {
                self.choose_files();
            }
            if ui.button("添加目录").clicked() {
                self.choose_folder();
            }
            if ui.button("移除选中").clicked() {
                self.remove_selected();
            }
            if ui.button("清空").clicked() && !self.running {
                self.items.clear();
                self.selected.clear();
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
                for index in 0..self.items.len() {
                    let item = &self.items[index];
                    let selected = self.selected.contains(&index);
                    let path = item.path.display().to_string();
                    let label = format!("{}  ·  {}", format_kind(&item.path), item.state.label());
                    let response = ui.selectable_label(
                        selected,
                        RichText::new(format!("{label}\n{path}")).color(item.state.color()),
                    );
                    if response.clicked() {
                        if ui.input(|input| input.modifiers.command) {
                            if !self.selected.insert(index) {
                                self.selected.remove(&index);
                            }
                        } else {
                            self.selected.clear();
                            self.selected.insert(index);
                        }
                    }
                    ui.separator();
                }
                if self.items.is_empty() {
                    ui.add_space(80.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new("将切片文件或文件夹拖到这里")
                                .size(18.0)
                                .weak(),
                        );
                        ui.label(RichText::new("支持一次拖入多个项目").weak());
                    });
                }
            });
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("2  输出设置");
        ui.label(RichText::new("统一输出目录（留空则写入源文件目录）").weak());
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.options.output_dir).desired_width(190.0));
            if ui.button("选择").clicked() {
                self.choose_output();
            }
        });
        ui.add_space(10.0);
        ui.label("SVS JPEG 保存质量");
        egui::ComboBox::from_id_salt("quality")
            .selected_text(&self.options.jpeg_quality)
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
        ui.checkbox(&mut self.options.overwrite, "覆盖已存在的 SVS");
        ui.checkbox(
            &mut self.options.skip_associated,
            "跳过 label / macro 关联图",
        );
        ui.separator();
        ui.heading("3  任务控制");
        if ui
            .add_enabled(!self.running, egui::Button::new("开始转换  F5"))
            .clicked()
        {
            self.start();
        }
        if ui
            .add_enabled(self.running, egui::Button::new("请求停止  Esc"))
            .clicked()
        {
            self.stop();
        }
        if let Some(first) = self.items.first() {
            ui.add_space(8.0);
            ui.label("示例输出路径");
            ui.label(
                RichText::new(self.output_path(&first.path).display().to_string())
                    .small()
                    .weak(),
            );
        }
        ui.add_space(12.0);
        ui.label(
            RichText::new(
                "提示：NDPI / MRXS 使用 OpenSlide + libvips；HEVC SDPC 需要 FFmpeg 运行库。",
            )
            .color(Color32::from_rgb(220, 170, 90)),
        );
    }

    fn logs_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("运行日志");
            if ui.button("清空日志").clicked() {
                self.logs.clear();
            }
            if ui.button("打开输出目录").clicked() {
                let path = if self.options.output_dir.trim().is_empty() {
                    self.items
                        .first()
                        .and_then(|item| item.path.parent())
                        .map(Path::to_path_buf)
                } else {
                    Some(PathBuf::from(self.options.output_dir.trim()))
                };
                if let Some(path) = path {
                    open_folder(&path);
                }
            }
        });
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.logs {
                    ui.label(RichText::new(line).monospace().small());
                }
            });
    }
}

fn run_jobs(
    jobs: Vec<PathBuf>,
    output_dir: Option<PathBuf>,
    quality: Option<u8>,
    skip_associated: bool,
    overwrite: bool,
    sender: Sender<WorkerEvent>,
    cancel: Arc<AtomicBool>,
) {
    for (index, input) in jobs.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            let _ = sender.send(WorkerEvent::Cancelled { index });
            continue;
        }
        let _ = sender.send(WorkerEvent::Started { index });
        let started = Instant::now();
        let result = convert_one(
            input,
            output_dir.as_deref(),
            quality,
            skip_associated,
            overwrite,
        );
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
    output_dir: Option<&Path>,
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
            let output = output_dir
                .map(|directory| {
                    directory
                        .join(input.file_stem().unwrap_or_default())
                        .with_extension("svs")
                })
                .unwrap_or_else(|| input.with_extension("svs"));
            let selected_quality = quality.unwrap_or(75);
            vips::convert(
                &input,
                &output,
                selected_quality,
                skip_associated,
                overwrite,
            )?;
            return Ok(output);
        }
        other => bail!("unsupported input extension .{other}"),
    };
    let output = output_dir
        .map(|directory| {
            directory
                .join(input.file_stem().unwrap_or_default())
                .with_extension("svs")
        })
        .unwrap_or_else(|| input.with_extension("svs"));
    let selected_quality = quality.unwrap_or(slide.metadata.jpeg_quality);
    svs::write_slide(
        &slide,
        &output,
        &svs::WriteOptions {
            jpeg_quality: selected_quality,
            skip_associated,
            overwrite,
        },
    )?;
    Ok(output)
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

fn format_kind(path: &Path) -> String {
    path.extension()
        .and_then(|value| value.to_str())
        .unwrap_or("?")
        .to_ascii_uppercase()
}

fn normalize_path(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
        .to_ascii_lowercase()
}

fn open_folder(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}
