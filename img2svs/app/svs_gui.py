#!/usr/bin/env python3
from __future__ import annotations

import os
import queue
import sys
import threading
import time
import tkinter as tk
from pathlib import Path
from tkinter import filedialog, font as tkfont, messagebox, ttk

from tkinterdnd2 import COPY, DND_FILES, REFUSE_DROP, TkinterDnD

from img2svs.app.svs_gui_service import (
    GUI_SUPPORTED_SUFFIXES,
    GuiConversionOptions,
    execute_jobs_subprocess,
    plan_jobs,
)

APP_TITLE = "病理图像转 SVS 工具"
ACCENT_COLOR = "#0d9488"
ACCENT_DARK = "#0f766e"
ACCENT_SOFT = "#ccfbf1"
NAVY_COLOR = "#102a43"
SURFACE_COLOR = "#f2f6f8"
CARD_COLOR = "#ffffff"
TEXT_COLOR = "#243b53"
MUTED_COLOR = "#627d98"
BORDER_COLOR = "#d9e2ec"

FORMAT_LABELS = {
    "自动识别（推荐）": "auto",
    "仅 CSP": "csp",
    "仅 帝麦克斯 DMETRIX": "dmetrix",
    "仅 KFB": "kfb",
    "仅 MDSX / MSDX": "mdsx",
    "仅 MRXS": "mrxs",
    "仅 NDPI": "ndpi",
    "仅 SDPC / DYQX": "sdpc",
}

FILE_TYPES = [
    ("病理切片文件", "*.csp *.dmetrix *.sdpc *.dyqx *.kfb *.mdsx *.msdx *.mrxs *.ndpi"),
    ("CSP", "*.csp"),
    ("帝麦克斯 DMETRIX", "*.dmetrix"),
    ("SDPC / DYQX", "*.sdpc *.dyqx"),
    ("KFB", "*.kfb"),
    ("MDSX / MSDX", "*.mdsx *.msdx"),
    ("MRXS", "*.mrxs"),
    ("NDPI", "*.ndpi"),
    ("所有文件", "*.*"),
]
QUALITY_OPTIONS = ("原始/推荐", "60", "70", "80", "90", "95")


def run_headless_smoke_test() -> int:
    """在无显示环境下运行一次 GUI 后端自检，供打包验证使用。"""

    from img2svs.app.svs_gui_service import (
        GuiConversionOptions,
        execute_jobs_subprocess,
        plan_jobs,
    )

    output_dir = Path(
        os.environ.get("SVS_GUI_SMOKE_OUTPUT_DIR", "/tmp/svs_gui_binary_smoke")
    ).expanduser()
    inputs_text = os.environ.get("SVS_GUI_SMOKE_INPUTS", "").strip()
    if inputs_text:
        input_paths = tuple(
            Path(item).expanduser()
            for item in inputs_text.split(os.pathsep)
            if item.strip()
        )
    else:
        project_dir = Path.cwd()
        input_paths = (
            project_dir / "test_data/20220514_145829_0.sdpc",
            project_dir / "test_data/tt1.ndpi",
        )

    options = GuiConversionOptions(
        inputs=input_paths,
        output_dir=output_dir,
        input_format="auto",
        overwrite=True,
    )
    jobs = plan_jobs(options)
    summary = execute_jobs_subprocess(jobs, options)
    return 0 if summary.failed == 0 and not summary.cancelled else 1


def choose_font_family(root: tk.Misc) -> str:
    """优先选择适合中文桌面界面的字体。"""

    available = set(tkfont.families(root))
    for name in ("Microsoft YaHei UI", "Microsoft YaHei", "Segoe UI", "Noto Sans CJK SC"):
        if name in available:
            return name
    return "TkDefaultFont"


def bundled_resource_path(*parts: str) -> Path:
    """定位源码目录或 PyInstaller 临时目录内的资源。"""

    base_dir = Path(getattr(sys, "_MEIPASS", Path(__file__).resolve().parents[2]))
    return base_dir.joinpath(*parts)


def partition_drop_paths(paths) -> tuple[tuple[Path, ...], tuple[Path, ...]]:
    """把拖入路径拆分为可添加项和忽略项。"""

    accepted: list[Path] = []
    ignored: list[Path] = []
    seen: set[str] = set()
    for raw_path in paths:
        path = Path(raw_path).expanduser()
        try:
            resolved = path.resolve(strict=True)
        except OSError:
            ignored.append(path)
            continue

        key = str(resolved).casefold()
        if key in seen:
            continue
        seen.add(key)
        if resolved.is_dir() or (
            resolved.is_file() and resolved.suffix.lower() in GUI_SUPPORTED_SUFFIXES
        ):
            accepted.append(resolved)
        else:
            ignored.append(resolved)
    return tuple(accepted), tuple(ignored)


class SvsConverterApp(TkinterDnD.Tk):
    """给非开发用户使用的桌面转换工具。"""

    def __init__(self) -> None:
        super().__init__()
        self.title(APP_TITLE)
        self.configure(bg=SURFACE_COLOR)

        self.worker_thread: threading.Thread | None = None
        self.stop_event = threading.Event()
        self.event_queue: queue.Queue[tuple] = queue.Queue()
        self.selected_paths: dict[str, Path] = {}
        self.start_buttons: list[ttk.Button] = []
        self.stop_buttons: list[ttk.Button] = []
        self.clear_buttons: list[ttk.Button] = []
        self.input_buttons: list[ttk.Button] = []
        self.scroll_canvas: tk.Canvas | None = None
        self.scroll_content: ttk.Frame | None = None
        self.scroll_window_id: int | None = None
        self.sources_panel: tk.Frame | None = None
        self.settings_panel: tk.Frame | None = None
        self.log_panel: tk.Frame | None = None
        self.sources_body: ttk.Frame | None = None
        self.log_body: ttk.Frame | None = None
        self.sources_toggle_text_var = tk.StringVar(value="收起")
        self.log_toggle_text_var = tk.StringVar(value="展开")
        self.sources_collapsed = False
        self.log_collapsed = True

        self.output_dir_var = tk.StringVar()
        # 文件/目录选择和输出目录选择分别记忆最近位置，避免 Tk 的全局
        # filedialog 状态让“添加输入”意外跳到上次的输出目录。
        self.input_dialog_dir = Path.cwd()
        self.output_dialog_dir = Path.cwd()
        self.jpeg_quality_var = tk.StringVar(value=QUALITY_OPTIONS[0])
        self.format_label_var = tk.StringVar(value=next(iter(FORMAT_LABELS)))
        self.overwrite_var = tk.BooleanVar(value=False)
        self.skip_associated_var = tk.BooleanVar(value=False)
        self.selection_summary_var = tk.StringVar()
        self.status_var = tk.StringVar(value="请先选择切片文件或目录，然后开始转换。")
        self.status_badge_var = tk.StringVar(value="准备就绪")
        self.progress_text_var = tk.StringVar(value="尚未开始")
        self.status_badge: tk.Label | None = None
        self.drop_hint: tk.Label | None = None
        self.log_scrollbar: ttk.Scrollbar | None = None
        self.window_icon_image: tk.PhotoImage | None = None
        self.is_running = False

        self.ui_font = choose_font_family(self)
        self.apply_window_icon()
        self.protocol("WM_DELETE_WINDOW", self.on_close)

        self.configure_window_geometry()
        self.configure_styles()
        self.build_layout()
        self.bind("<F5>", lambda _event: self.start_conversion())
        self.bind("<Control-o>", lambda _event: self.add_files())
        self.bind("<Control-O>", lambda _event: self.add_folder())
        self.bind("<Delete>", lambda _event: self.remove_selected())
        self.bind("<Escape>", lambda _event: self.stop_conversion())
        self.update_selection_summary()
        self.after_idle(self.enable_file_drop)
        self.after(120, self.drain_events)

    def apply_window_icon(self) -> None:
        """为源码运行和打包程序设置统一窗口图标。"""

        icon_ico = bundled_resource_path("assets", "app_icon.ico")
        icon_png = bundled_resource_path("assets", "app_icon.png")
        try:
            if sys.platform == "win32" and icon_ico.is_file():
                self.iconbitmap(default=str(icon_ico))
            if icon_png.is_file():
                self.window_icon_image = tk.PhotoImage(file=str(icon_png))
                self.iconphoto(True, self.window_icon_image)
        except tk.TclError:
            self.window_icon_image = None

    def enable_file_drop(self) -> None:
        """把文件列表及空状态提示注册为 TkDND 拖放目标。"""

        try:
            targets = (self.sources_panel, self.source_tree, self.drop_hint)
            for target in targets:
                if target is None:
                    continue
                target.drop_target_register(DND_FILES)
                target.dnd_bind("<<DropEnter>>", self.on_drag_enter)
                target.dnd_bind("<<DropPosition>>", self.on_drag_enter)
                target.dnd_bind("<<DropLeave>>", self.on_drag_leave)
                target.dnd_bind("<<Drop>>", self.on_drop)
        except tk.TclError as exc:
            self.append_log(f"文件拖放初始化失败：{exc}")
            return
        self.append_log("文件拖放已启用：可将切片文件或文件夹拖入选择区。")

    def on_drag_enter(self, _event):
        """拖入选择区时给出高亮反馈。"""

        if self.is_running:
            self.status_var.set("转换正在运行，暂时不能添加新的切片。")
            return REFUSE_DROP
        if self.sources_panel is not None:
            self.sources_panel.configure(highlightbackground=ACCENT_COLOR)
        if self.drop_hint is not None:
            self.drop_hint.configure(bg=ACCENT_SOFT, fg=ACCENT_DARK)
        return COPY

    def on_drag_leave(self, _event=None):
        """拖离或完成放置后恢复选择区样式。"""

        if self.sources_panel is not None:
            self.sources_panel.configure(highlightbackground=BORDER_COLOR)
        if self.drop_hint is not None:
            self.drop_hint.configure(bg=CARD_COLOR, fg=MUTED_COLOR)
        return COPY

    def on_drop(self, event):
        """解析 TkDND 文件列表并加入任务列表。"""

        self.on_drag_leave()
        if self.is_running:
            return REFUSE_DROP
        paths = tuple(Path(value) for value in self.tk.splitlist(event.data))
        self.handle_dropped_paths(paths)
        return COPY

    def configure_window_geometry(self) -> None:
        """根据当前屏幕尺寸设置一个更稳妥的初始窗口大小。"""

        screen_width = max(self.winfo_screenwidth(), 1024)
        screen_height = max(self.winfo_screenheight(), 720)
        width = min(1240, max(980, screen_width - 120))
        height = min(780, max(680, screen_height - 100))
        pos_x = max((screen_width - width) // 2, 0)
        pos_y = max((screen_height - height) // 2, 0)
        self.geometry(f"{width}x{height}+{pos_x}+{pos_y}")
        self.minsize(min(width, 980), min(height, 680))

    def configure_styles(self) -> None:
        """设置统一的界面视觉样式。"""

        default_font = tkfont.nametofont("TkDefaultFont")
        default_font.configure(family=self.ui_font, size=10)
        text_font = tkfont.nametofont("TkTextFont")
        text_font.configure(family=self.ui_font, size=10)
        fixed_font = tkfont.nametofont("TkFixedFont")
        fixed_font.configure(size=10)

        style = ttk.Style(self)
        style.theme_use("clam")

        style.configure("TFrame", background=SURFACE_COLOR)
        style.configure("Card.TFrame", background=CARD_COLOR)
        style.configure("Panel.TFrame", background=CARD_COLOR)
        style.configure(
            "Title.TLabel",
            background=SURFACE_COLOR,
            foreground=TEXT_COLOR,
            font=(self.ui_font, 22, "bold"),
        )
        style.configure(
            "Subtitle.TLabel",
            background=SURFACE_COLOR,
            foreground=MUTED_COLOR,
            font=(self.ui_font, 10),
        )
        style.configure(
            "HeroTitle.TLabel",
            background=NAVY_COLOR,
            foreground="#ffffff",
            font=(self.ui_font, 19, "bold"),
        )
        style.configure(
            "HeroSubtitle.TLabel",
            background=NAVY_COLOR,
            foreground="#bcccdc",
            font=(self.ui_font, 10),
        )
        style.configure(
            "TLabel",
            background=CARD_COLOR,
            foreground=TEXT_COLOR,
            font=(self.ui_font, 10),
        )
        style.configure(
            "Hint.TLabel",
            background=CARD_COLOR,
            foreground=MUTED_COLOR,
            font=(self.ui_font, 9),
        )
        style.configure(
            "TButton",
            font=(self.ui_font, 10),
            padding=(12, 7),
            background="#e9f0f5",
            foreground=TEXT_COLOR,
            borderwidth=0,
        )
        style.map(
            "TButton",
            background=[("active", "#dbe7ef"), ("disabled", "#edf2f6")],
            foreground=[("disabled", "#9fb3c8")],
        )
        style.configure(
            "Secondary.TButton",
            font=(self.ui_font, 10),
            padding=(12, 8),
            background="#e6fffa",
            foreground=ACCENT_DARK,
            borderwidth=0,
        )
        style.map(
            "Secondary.TButton",
            background=[("active", ACCENT_SOFT), ("disabled", "#edf2f6")],
            foreground=[("disabled", "#9fb3c8")],
        )
        style.configure(
            "Accent.TButton",
            font=(self.ui_font, 10, "bold"),
            padding=(14, 9),
            background=ACCENT_COLOR,
            foreground="#ffffff",
            borderwidth=0,
        )
        style.map(
            "Accent.TButton",
            background=[("active", ACCENT_DARK), ("disabled", "#8ab7b2")],
            foreground=[("disabled", "#edf6f5")],
        )
        style.configure(
            "Quiet.TButton",
            font=(self.ui_font, 9),
            padding=(10, 5),
            background=CARD_COLOR,
            foreground=MUTED_COLOR,
            borderwidth=0,
        )
        style.map(
            "Quiet.TButton",
            background=[("active", "#edf2f6"), ("disabled", CARD_COLOR)],
            foreground=[("active", TEXT_COLOR), ("disabled", "#bcccdc")],
        )
        style.configure(
            "Stop.TButton",
            font=(self.ui_font, 10, "bold"),
            padding=(12, 8),
            background="#fee2e2",
            foreground="#b91c1c",
            borderwidth=0,
        )
        style.map(
            "Stop.TButton",
            background=[("active", "#fecaca"), ("disabled", "#edf2f6")],
            foreground=[("disabled", "#9fb3c8")],
        )
        style.configure(
            "Treeview",
            font=(self.ui_font, 10),
            rowheight=32,
            fieldbackground="#ffffff",
            background="#ffffff",
            foreground=TEXT_COLOR,
            borderwidth=0,
        )
        style.configure(
            "Treeview.Heading",
            font=(self.ui_font, 10, "bold"),
            background="#edf2f7",
            foreground=TEXT_COLOR,
            relief="flat",
        )
        style.configure(
            "TCheckbutton",
            background=CARD_COLOR,
            foreground=TEXT_COLOR,
            font=(self.ui_font, 10),
        )
        style.configure(
            "TCombobox",
            font=(self.ui_font, 10),
            padding=6,
        )
        style.configure(
            "TEntry",
            font=(self.ui_font, 10),
            padding=6,
        )
        style.configure(
            "Horizontal.TProgressbar",
            thickness=8,
            troughcolor="#294e6b",
            background=ACCENT_COLOR,
            bordercolor="#294e6b",
            lightcolor=ACCENT_COLOR,
            darkcolor=ACCENT_COLOR,
        )

    def build_layout(self) -> None:
        """创建界面布局。"""

        shell = ttk.Frame(self)
        shell.pack(fill="both", expand=True)
        shell.columnconfigure(0, weight=1)
        shell.rowconfigure(0, weight=1)

        self.scroll_canvas = tk.Canvas(
            shell,
            bg=SURFACE_COLOR,
            highlightthickness=0,
            bd=0,
        )
        self.scroll_canvas.grid(row=0, column=0, sticky="nsew")

        scrollbar = ttk.Scrollbar(shell, orient="vertical", command=self.scroll_canvas.yview)
        scrollbar.grid(row=0, column=1, sticky="ns")
        self.scroll_canvas.configure(yscrollcommand=scrollbar.set)

        self.scroll_content = ttk.Frame(self.scroll_canvas, padding=(18, 16))
        self.scroll_content.columnconfigure(0, weight=1)
        self.scroll_window_id = self.scroll_canvas.create_window(
            (0, 0),
            window=self.scroll_content,
            anchor="nw",
        )

        self.scroll_content.bind("<Configure>", self.on_scroll_content_configure)
        self.scroll_canvas.bind("<Configure>", self.on_scroll_canvas_configure)

        self.build_header(self.scroll_content)
        self.build_sources_panel(self.scroll_content)
        self.build_settings_panel(self.scroll_content)
        self.build_log_panel(self.scroll_content)
        self.install_mousewheel_support(shell)
        self.after_idle(self.arrange_main_panels)

    def on_scroll_content_configure(self, _event) -> None:
        """内容尺寸变化时同步滚动区域。"""

        if self.scroll_canvas is not None:
            self.scroll_canvas.configure(scrollregion=self.scroll_canvas.bbox("all"))

    def on_scroll_canvas_configure(self, event) -> None:
        """画布尺寸变化时同步内容宽度和响应式布局。"""

        if self.scroll_canvas is not None and self.scroll_window_id is not None:
            self.scroll_canvas.itemconfigure(self.scroll_window_id, width=event.width)
        self.arrange_main_panels(event.width)

    def install_mousewheel_support(self, root: tk.Misc) -> None:
        """为主界面安装鼠标滚轮支持。"""

        self.bind_canvas_mousewheel(root)

    def bind_canvas_mousewheel(self, widget: tk.Misc) -> None:
        """把滚轮事件绑定到整个界面组件树。"""

        for sequence in ("<MouseWheel>", "<Button-4>", "<Button-5>"):
            widget.bind(sequence, self.handle_canvas_mousewheel, add="+")

        for child in widget.winfo_children():
            self.bind_canvas_mousewheel(child)

    def handle_canvas_mousewheel(self, event) -> str | None:
        """把鼠标滚轮统一转换为主界面的纵向滚动。"""

        delta_units = 0
        if getattr(event, "delta", 0):
            delta_units = -int(event.delta / 120)
            if delta_units == 0:
                delta_units = -1 if event.delta > 0 else 1
        else:
            event_num = getattr(event, "num", None)
            if event_num == 4:
                delta_units = -1
            elif event_num == 5:
                delta_units = 1

        if delta_units == 0:
            return None

        # 日志区有自己的滚动条。外层画布的递归滚轮绑定会覆盖 Text
        # 默认行为，因此在日志文本上优先滚动日志本身。
        if getattr(event, "widget", None) == self.log_text:
            self.log_text.yview_scroll(delta_units, "units")
            return "break"

        if self.scroll_canvas is None:
            return None

        view_top, view_bottom = self.scroll_canvas.yview()
        if delta_units < 0 and view_top <= 0.0:
            return "break"
        if delta_units > 0 and view_bottom >= 1.0:
            return "break"

        self.scroll_canvas.yview_scroll(delta_units, "units")
        return "break"

    def arrange_main_panels(self, available_width: int | None = None) -> None:
        """根据可用宽度决定左右分栏还是上下堆叠。"""

        if (
            self.scroll_content is None
            or self.sources_panel is None
            or self.settings_panel is None
            or self.log_panel is None
        ):
            return

        width = available_width or (
            self.scroll_canvas.winfo_width() if self.scroll_canvas is not None else self.winfo_width()
        )

        for panel in (self.sources_panel, self.settings_panel, self.log_panel):
            panel.grid_forget()

        for column in range(2):
            self.scroll_content.columnconfigure(column, weight=0)
        for row in range(1, 4):
            self.scroll_content.rowconfigure(row, weight=0)

        if width >= 1080:
            self.scroll_content.columnconfigure(0, weight=3)
            self.scroll_content.columnconfigure(1, weight=2)
            self.scroll_content.rowconfigure(1, weight=1)
            self.scroll_content.rowconfigure(2, weight=1)

            self.sources_panel.grid(row=1, column=0, sticky="nsew", padx=(0, 8), pady=(0, 12))
            self.settings_panel.grid(row=1, column=1, sticky="nsew", padx=(8, 0), pady=(0, 12))
            self.log_panel.grid(row=2, column=0, columnspan=2, sticky="nsew")
        else:
            self.scroll_content.columnconfigure(0, weight=1)
            self.scroll_content.rowconfigure(1, weight=1)
            self.scroll_content.rowconfigure(2, weight=1)
            self.scroll_content.rowconfigure(3, weight=1)

            self.sources_panel.grid(row=1, column=0, sticky="nsew", pady=(0, 10))
            self.settings_panel.grid(row=2, column=0, sticky="nsew", pady=(0, 10))
            self.log_panel.grid(row=3, column=0, sticky="nsew")

    def apply_collapsible_state(self) -> None:
        """根据折叠状态显示或隐藏可折叠面板内容。"""

        if self.sources_body is not None:
            if self.sources_collapsed:
                self.sources_body.grid_remove()
                self.sources_toggle_text_var.set("展开")
            else:
                self.sources_body.grid()
                self.sources_toggle_text_var.set("收起")

        if self.log_body is not None:
            if self.log_collapsed:
                self.log_body.grid_remove()
                self.log_toggle_text_var.set("展开")
            else:
                self.log_body.grid()
                self.log_toggle_text_var.set("收起")

        self.after_idle(self.refresh_scroll_layout)

    def refresh_scroll_layout(self) -> None:
        """刷新滚动区域和响应式布局。"""

        self.arrange_main_panels()
        if self.scroll_canvas is not None:
            self.scroll_canvas.configure(scrollregion=self.scroll_canvas.bbox("all"))

    def toggle_sources_panel(self) -> None:
        """切换文件选择区的折叠状态。"""

        self.sources_collapsed = not self.sources_collapsed
        self.apply_collapsible_state()

    def toggle_log_panel(self) -> None:
        """切换日志区的折叠状态。"""

        self.log_collapsed = not self.log_collapsed
        self.apply_collapsible_state()
        if not self.log_collapsed:
            self.after(50, self.scroll_to_log_panel)

    def reveal_log_panel(self) -> None:
        """展开日志并滚动到可见位置。"""

        self.log_collapsed = False
        self.apply_collapsible_state()
        self.after(50, self.scroll_to_log_panel)

    def scroll_to_log_panel(self) -> None:
        """把主滚动区域移动到底部日志位置。"""

        if self.scroll_canvas is not None:
            self.scroll_canvas.configure(scrollregion=self.scroll_canvas.bbox("all"))
            self.scroll_canvas.yview_moveto(1.0)

    def build_header(self, parent: ttk.Frame) -> None:
        """顶部标题区。"""

        header = tk.Frame(
            parent,
            bg=NAVY_COLOR,
            highlightthickness=0,
            bd=0,
        )
        header.grid(row=0, column=0, columnspan=2, sticky="ew", pady=(0, 14))
        header.columnconfigure(1, weight=1)

        brand = tk.Frame(header, bg=ACCENT_COLOR, width=54, height=54)
        brand.grid(row=0, column=0, rowspan=2, padx=(18, 14), pady=(16, 10), sticky="nw")
        brand.grid_propagate(False)
        tk.Label(
            brand,
            text="SVS",
            bg=ACCENT_COLOR,
            fg="#ffffff",
            font=(self.ui_font, 11, "bold"),
        ).place(relx=0.5, rely=0.5, anchor="center")

        title = ttk.Label(header, text=APP_TITLE, style="HeroTitle.TLabel")
        title.grid(row=0, column=1, sticky="sw", pady=(15, 0))
        subtitle = ttk.Label(
            header,
            text="把常见数字病理切片批量转换为兼容性更好的 SVS",
            style="HeroSubtitle.TLabel",
        )
        subtitle.grid(row=1, column=1, sticky="nw", pady=(2, 10))

        top_stop_button = ttk.Button(
            header,
            text="停止",
            style="Stop.TButton",
            command=self.stop_conversion,
            width=8,
        )
        top_stop_button.grid(row=0, column=2, rowspan=2, padx=(8, 8), pady=(18, 12), sticky="e")
        top_stop_button.state(["disabled"])
        self.stop_buttons.append(top_stop_button)

        top_start_button = ttk.Button(
            header,
            text="开始转换  F5",
            style="Accent.TButton",
            command=self.start_conversion,
            width=16,
        )
        top_start_button.grid(row=0, column=3, rowspan=2, padx=(0, 18), pady=(18, 12), sticky="e")
        self.start_buttons.append(top_start_button)

        self.progress_bar = ttk.Progressbar(header, mode="determinate")
        self.progress_bar.grid(row=2, column=0, columnspan=4, sticky="ew", padx=18, pady=(0, 9))

        self.status_badge = tk.Label(
            header,
            textvariable=self.status_badge_var,
            bg=ACCENT_SOFT,
            fg=ACCENT_DARK,
            font=(self.ui_font, 9, "bold"),
            padx=10,
            pady=4,
        )
        self.status_badge.grid(row=3, column=0, sticky="w", padx=(18, 10), pady=(0, 12))
        tk.Label(
            header,
            textvariable=self.progress_text_var,
            bg=NAVY_COLOR,
            fg="#d9e2ec",
            font=(self.ui_font, 9),
        ).grid(row=3, column=1, columnspan=3, sticky="w", pady=(0, 12))

    def build_sources_panel(self, parent: ttk.Frame) -> None:
        """左侧输入文件区。"""

        frame = tk.Frame(
            parent,
            bg=CARD_COLOR,
            highlightthickness=1,
            highlightbackground=BORDER_COLOR,
            bd=0,
        )
        frame.columnconfigure(0, weight=1)
        frame.rowconfigure(1, weight=1)
        self.sources_panel = frame

        controls = tk.Frame(frame, bg=CARD_COLOR)
        controls.grid(row=0, column=0, sticky="ew", padx=16, pady=(13, 8))
        controls.columnconfigure(0, weight=1)
        tk.Label(
            controls,
            text="1  选择切片",
            bg=CARD_COLOR,
            fg=TEXT_COLOR,
            font=(self.ui_font, 12, "bold"),
        ).grid(row=0, column=0, sticky="w")
        tk.Label(
            controls,
            text="支持多选、目录添加，也可以直接拖拽到列表",
            bg=CARD_COLOR,
            fg=MUTED_COLOR,
            font=(self.ui_font, 9),
        ).grid(row=1, column=0, sticky="w", pady=(3, 0))
        ttk.Button(
            controls,
            textvariable=self.sources_toggle_text_var,
            command=self.toggle_sources_panel,
            style="Quiet.TButton",
            width=6,
        ).grid(row=0, column=1, rowspan=2, sticky="e")

        body = ttk.Frame(frame, style="Panel.TFrame")
        body.grid(row=1, column=0, sticky="nsew", padx=16, pady=(0, 16))
        body.columnconfigure(0, weight=1)
        body.rowconfigure(2, weight=1)
        self.sources_body = body

        button_row = ttk.Frame(body, style="Panel.TFrame")
        button_row.grid(row=0, column=0, sticky="ew", pady=(0, 9))
        button_row.columnconfigure(0, weight=1)
        button_row.columnconfigure(1, weight=1)

        add_file_button = ttk.Button(
            button_row,
            text="＋ 添加文件",
            command=self.add_files,
            style="Accent.TButton",
        )
        add_file_button.grid(row=0, column=0, sticky="ew", padx=(0, 5))
        add_folder_button = ttk.Button(
            button_row,
            text="添加目录",
            command=self.add_folder,
            style="Secondary.TButton",
        )
        add_folder_button.grid(row=0, column=1, sticky="ew", padx=(5, 10))
        clear_button = ttk.Button(
            button_row,
            text="清空",
            command=self.clear_selected,
            style="Quiet.TButton",
        )
        clear_button.grid(row=0, column=2, padx=(10, 0))
        self.input_buttons.extend((add_file_button, add_folder_button))
        self.clear_buttons.append(clear_button)

        ttk.Label(body, textvariable=self.selection_summary_var, style="Hint.TLabel").grid(
            row=1, column=0, sticky="w", pady=(0, 7)
        )

        table_frame = ttk.Frame(body, style="Panel.TFrame")
        table_frame.grid(row=2, column=0, sticky="nsew")
        table_frame.columnconfigure(0, weight=1)
        table_frame.rowconfigure(0, weight=1)

        self.source_tree = ttk.Treeview(
            table_frame,
            columns=("type", "path", "remove"),
            show="headings",
            selectmode="extended",
            height=8,
        )
        self.source_tree.heading("type", text="格式")
        self.source_tree.heading("path", text="文件或目录")
        self.source_tree.heading("remove", text="操作")
        self.source_tree.column("type", width=92, anchor="center", stretch=False)
        self.source_tree.column("path", width=460, anchor="w")
        self.source_tree.column("remove", width=64, anchor="center", stretch=False)
        self.source_tree.tag_configure("stripe", background="#f7fafb")
        self.source_tree.grid(row=0, column=0, sticky="nsew")
        self.source_tree.bind("<Button-1>", self.on_source_tree_click)
        self.source_tree.bind("<Motion>", self.on_source_tree_motion)
        self.source_tree.bind("<Leave>", lambda _event: self.source_tree.configure(cursor=""))

        scrollbar = ttk.Scrollbar(table_frame, orient="vertical", command=self.source_tree.yview)
        scrollbar.grid(row=0, column=1, sticky="ns")
        self.source_tree.configure(yscrollcommand=scrollbar.set)

        self.drop_hint = tk.Label(
            table_frame,
            text="拖拽切片文件或文件夹到这里\n支持一次拖入多个项目",
            bg="#ffffff",
            fg=MUTED_COLOR,
            justify="center",
            font=(self.ui_font, 10, "bold"),
            padx=18,
            pady=12,
        )

    def build_settings_panel(self, parent: ttk.Frame) -> None:
        """右侧参数与操作区。"""

        frame = tk.Frame(
            parent,
            bg=CARD_COLOR,
            highlightthickness=1,
            highlightbackground=BORDER_COLOR,
            bd=0,
        )
        frame.columnconfigure(0, weight=1)
        self.settings_panel = frame

        heading = tk.Frame(frame, bg=CARD_COLOR)
        heading.grid(row=0, column=0, sticky="ew", padx=16, pady=(13, 11))
        tk.Label(
            heading,
            text="2  转换设置",
            bg=CARD_COLOR,
            fg=TEXT_COLOR,
            font=(self.ui_font, 12, "bold"),
        ).pack(anchor="w")
        tk.Label(
            heading,
            text="默认选项适合大多数切片",
            bg=CARD_COLOR,
            fg=MUTED_COLOR,
            font=(self.ui_font, 9),
        ).pack(anchor="w", pady=(3, 0))

        content = ttk.Frame(frame, style="Panel.TFrame")
        content.grid(row=1, column=0, sticky="nsew", padx=16, pady=(0, 16))
        content.columnconfigure(0, weight=1)

        ttk.Label(content, text="输出位置", font=(self.ui_font, 9, "bold")).grid(
            row=0, column=0, sticky="w", pady=(0, 5)
        )
        output_row = ttk.Frame(content, style="Panel.TFrame")
        output_row.grid(row=1, column=0, sticky="ew")
        output_row.columnconfigure(0, weight=1)
        ttk.Entry(output_row, textvariable=self.output_dir_var).grid(row=0, column=0, sticky="ew")
        ttk.Button(output_row, text="浏览…", command=self.choose_output_dir).grid(
            row=0, column=1, padx=(7, 0)
        )
        ttk.Label(
            content,
            text="留空即保存到源文件同目录",
            style="Hint.TLabel",
        ).grid(row=2, column=0, sticky="w", pady=(4, 0))
        ttk.Button(
            content,
            text="恢复为源目录输出",
            command=self.clear_output_dir,
            style="Quiet.TButton",
        ).grid(row=2, column=0, sticky="e", pady=(2, 0))

        ttk.Separator(content).grid(row=3, column=0, sticky="ew", pady=12)

        fields = ttk.Frame(content, style="Panel.TFrame")
        fields.grid(row=4, column=0, sticky="ew")
        fields.columnconfigure(0, weight=1)
        fields.columnconfigure(1, weight=1)
        ttk.Label(fields, text="输入格式", font=(self.ui_font, 9, "bold")).grid(
            row=0, column=0, sticky="w", padx=(0, 5), pady=(0, 5)
        )
        ttk.Label(fields, text="SVS 保存质量", font=(self.ui_font, 9, "bold")).grid(
            row=0, column=1, sticky="w", padx=(5, 0), pady=(0, 5)
        )
        ttk.Combobox(
            fields,
            textvariable=self.format_label_var,
            values=list(FORMAT_LABELS),
            state="readonly",
        ).grid(row=1, column=0, sticky="ew", padx=(0, 5))
        ttk.Combobox(
            fields,
            textvariable=self.jpeg_quality_var,
            values=QUALITY_OPTIONS,
            state="readonly",
        ).grid(row=1, column=1, sticky="ew", padx=(5, 0))
        ttk.Label(
            content,
            text="“原始/推荐”会尽量沿用源图质量；降低数值可减小文件体积。",
            style="Hint.TLabel",
            wraplength=410,
            justify="left",
        ).grid(row=5, column=0, sticky="w", pady=(5, 0))

        options_row = ttk.Frame(content, style="Panel.TFrame")
        options_row.grid(row=6, column=0, sticky="ew", pady=(10, 10))
        ttk.Checkbutton(
            options_row,
            text="覆盖已有 SVS 文件",
            variable=self.overwrite_var,
        ).pack(side="left")
        ttk.Checkbutton(
            options_row,
            text="跳过标签图 / 宏观图",
            variable=self.skip_associated_var,
        ).pack(side="left", padx=(16, 0))

        status_box = tk.Frame(
            content,
            bg="#e6fffa",
            highlightthickness=1,
            highlightbackground="#99f6e4",
        )
        status_box.grid(row=7, column=0, sticky="ew", pady=(2, 0))
        status_box.grid_columnconfigure(0, weight=1)
        tk.Label(
            status_box,
            text="当前状态",
            bg="#e6fffa",
            fg=ACCENT_DARK,
            font=(self.ui_font, 9, "bold"),
            anchor="w",
        ).grid(row=0, column=0, sticky="ew", padx=11, pady=(9, 2))
        tk.Label(
            status_box,
            textvariable=self.status_var,
            bg="#e6fffa",
            fg=TEXT_COLOR,
            wraplength=390,
            justify="left",
            anchor="w",
            font=(self.ui_font, 9),
        ).grid(row=1, column=0, sticky="ew", padx=11, pady=(0, 8))
        ttk.Button(
            status_box,
            text="打开输出目录",
            command=self.open_output_dir,
            style="Secondary.TButton",
        ).grid(row=2, column=0, sticky="ew", padx=8, pady=(0, 8))

    def build_log_panel(self, parent: ttk.Frame) -> None:
        """底部日志区。"""

        frame = tk.Frame(
            parent,
            bg=CARD_COLOR,
            highlightthickness=1,
            highlightbackground=BORDER_COLOR,
            bd=0,
        )
        frame.columnconfigure(0, weight=1)
        frame.rowconfigure(1, weight=1)
        self.log_panel = frame

        controls = tk.Frame(frame, bg=CARD_COLOR)
        controls.grid(row=0, column=0, sticky="ew", padx=16, pady=10)
        controls.columnconfigure(1, weight=1)
        tk.Label(
            controls,
            text="3  运行日志",
            bg=CARD_COLOR,
            fg=TEXT_COLOR,
            font=(self.ui_font, 11, "bold"),
        ).grid(row=0, column=0, sticky="w")
        tk.Label(
            controls,
            text="需要排查失败原因时再展开",
            bg=CARD_COLOR,
            fg=MUTED_COLOR,
            font=(self.ui_font, 9),
        ).grid(row=0, column=1, sticky="w", padx=(12, 0))
        ttk.Button(
            controls,
            textvariable=self.log_toggle_text_var,
            command=self.toggle_log_panel,
            style="Quiet.TButton",
            width=6,
        ).grid(row=0, column=2, sticky="e")

        body = ttk.Frame(frame, style="Panel.TFrame")
        body.grid(row=1, column=0, sticky="nsew", padx=16, pady=(0, 14))
        body.columnconfigure(0, weight=1)
        body.columnconfigure(1, weight=0)
        body.rowconfigure(0, weight=1)
        self.log_body = body

        self.log_text = tk.Text(
            body,
            bg="#0f172a",
            fg="#e2e8f0",
            insertbackground="#e2e8f0",
            relief="flat",
            wrap="word",
            font=("Consolas", 9),
            padx=12,
            pady=12,
            height=8,
        )
        self.log_text.grid(row=0, column=0, sticky="nsew", pady=(0, 10))
        self.log_scrollbar = ttk.Scrollbar(body, orient="vertical", command=self.log_text.yview)
        self.log_scrollbar.grid(row=0, column=1, sticky="ns", padx=(8, 0), pady=(0, 10))
        self.log_text.configure(yscrollcommand=self.log_scrollbar.set)
        self.log_text.configure(state="disabled")

        body_controls = ttk.Frame(body, style="Panel.TFrame")
        body_controls.grid(row=1, column=0, sticky="ew")
        ttk.Button(
            body_controls,
            text="清空日志",
            command=self.clear_logs,
            style="Quiet.TButton",
        ).pack(side="left")
        ttk.Label(
            body_controls,
            text=(
                "支持："
                + "  ".join(sorted(suffix.replace(".", "").upper() for suffix in GUI_SUPPORTED_SUFFIXES))
            ),
            style="Hint.TLabel",
        ).pack(side="right")

        self.apply_collapsible_state()

    def add_files(self) -> None:
        """添加多个切片文件。"""

        paths = filedialog.askopenfilenames(
            title="选择病理切片文件",
            initialdir=str(self.input_dialog_dir),
            filetypes=FILE_TYPES,
        )
        if paths:
            self.input_dialog_dir = Path(paths[0]).expanduser().resolve().parent
        self.add_input_paths(Path(path) for path in paths)

    def add_folder(self) -> None:
        """添加包含切片的目录。"""

        folder = filedialog.askdirectory(
            title="选择包含切片文件的目录",
            initialdir=str(self.input_dialog_dir),
        )
        if folder:
            self.input_dialog_dir = Path(folder).expanduser().resolve()
            self.add_input_paths([Path(folder)])

    def handle_dropped_paths(self, paths) -> None:
        """接收系统拖放路径，过滤后加入选择列表。"""

        if self.is_running:
            self.status_var.set("转换正在运行，暂时不能添加新的切片。")
            return

        accepted, ignored = partition_drop_paths(paths)
        added = self.add_input_paths(accepted)
        duplicates = len(accepted) - added
        details = [f"新增 {added} 项"]
        if duplicates:
            details.append(f"重复 {duplicates} 项")
        if ignored:
            details.append(f"忽略不支持的文件 {len(ignored)} 项")
        self.status_var.set("拖拽完成：" + "，".join(details) + "。")
        if added:
            self.set_status_badge("待转换")
        elif ignored:
            self.set_status_badge("未添加")

    def add_input_paths(self, paths) -> int:
        """把输入路径加入列表。"""

        added = 0
        for path in paths:
            resolved = path.expanduser().resolve()
            item_key = str(resolved)
            if item_key in self.selected_paths:
                continue
            kind = "目录" if resolved.is_dir() else resolved.suffix.lower().lstrip(".").upper()
            row_tags = ("stripe",) if len(self.source_tree.get_children()) % 2 else ()
            self.source_tree.insert(
                "",
                "end",
                iid=item_key,
                values=(kind, item_key, "删除"),
                tags=row_tags,
            )
            self.selected_paths[item_key] = resolved
            added += 1
        self.update_selection_summary()
        return added

    def on_source_tree_click(self, event):
        """点击文件列表中的“删除”单元格时移除对应项目。"""

        if self.source_tree.identify_region(event.x, event.y) != "cell":
            return None
        if self.source_tree.identify_column(event.x) != "#3":
            return None
        item_id = self.source_tree.identify_row(event.y)
        if not item_id:
            return None
        if self.is_running:
            self.status_var.set("转换正在运行，暂时不能移除切片。")
            return "break"
        self.remove_source_items((item_id,))
        return "break"

    def on_source_tree_motion(self, event) -> None:
        """鼠标经过删除单元格时显示可点击指针。"""

        is_remove_cell = (
            self.source_tree.identify_region(event.x, event.y) == "cell"
            and self.source_tree.identify_column(event.x) == "#3"
            and bool(self.source_tree.identify_row(event.y))
        )
        self.source_tree.configure(cursor="hand2" if is_remove_cell else "")

    def remove_source_items(self, item_ids) -> None:
        """从列表和内部选择状态中同步移除项目。"""

        for item_id in item_ids:
            if self.source_tree.exists(item_id):
                self.source_tree.delete(item_id)
            self.selected_paths.pop(item_id, None)
        self.refresh_source_row_stripes()
        self.update_selection_summary()

    def refresh_source_row_stripes(self) -> None:
        """删除行后重新应用交替行背景。"""

        for index, item_id in enumerate(self.source_tree.get_children()):
            self.source_tree.item(item_id, tags=("stripe",) if index % 2 else ())

    def remove_selected(self) -> None:
        """移除当前选中的路径。"""

        self.remove_source_items(self.source_tree.selection())

    def clear_selected(self) -> None:
        """清空输入列表。"""

        for item_id in self.source_tree.get_children():
            self.source_tree.delete(item_id)
        self.selected_paths.clear()
        self.update_selection_summary()

    def choose_output_dir(self) -> None:
        """选择输出目录。"""

        output_text = self.output_dir_var.get().strip()
        current_output = Path(output_text).expanduser() if output_text else None
        if current_output is not None and current_output.is_dir():
            initialdir = current_output
        elif self.output_dialog_dir.is_dir():
            initialdir = self.output_dialog_dir
        else:
            initialdir = Path.cwd()
        folder = filedialog.askdirectory(
            title="选择 SVS 输出目录",
            initialdir=str(initialdir),
        )
        if folder:
            self.output_dir_var.set(folder)
            self.output_dialog_dir = Path(folder).expanduser().resolve()

    def clear_output_dir(self) -> None:
        """恢复为源目录输出。"""

        self.output_dir_var.set("")

    def open_output_dir(self) -> None:
        """打开当前任务对应的输出目录。"""

        output_text = self.output_dir_var.get().strip()
        if output_text:
            target = Path(output_text).expanduser()
        elif self.selected_paths:
            first_input = next(iter(self.selected_paths.values()))
            target = first_input if first_input.is_dir() else first_input.parent
        else:
            messagebox.showinfo(APP_TITLE, "请先选择切片文件，或指定输出目录。")
            return

        if not target.exists() or not target.is_dir():
            messagebox.showwarning(APP_TITLE, f"输出目录尚不存在：\n{target}")
            return

        startfile = getattr(os, "startfile", None)
        if startfile is None:
            messagebox.showinfo(APP_TITLE, f"输出目录：\n{target}")
            return
        try:
            startfile(str(target))
        except OSError as exc:
            messagebox.showerror(APP_TITLE, f"无法打开输出目录：\n{exc}")

    def clear_logs(self) -> None:
        """清空日志文本。"""

        self.log_text.configure(state="normal")
        self.log_text.delete("1.0", "end")
        self.log_text.configure(state="disabled")

    def append_log(self, message: str) -> None:
        """向日志区域追加一行。"""

        timestamp = time.strftime("%H:%M:%S")
        view_top, view_bottom = self.log_text.yview()
        follow_tail = view_bottom >= 0.999
        self.log_text.configure(state="normal")
        self.log_text.insert("end", f"[{timestamp}] {message}\n")
        if follow_tail:
            self.log_text.see("end")
        else:
            self.log_text.yview_moveto(view_top)
        self.log_text.configure(state="disabled")

    def update_selection_summary(self) -> None:
        """刷新文件列表摘要。"""

        file_count = sum(1 for path in self.selected_paths.values() if path.is_file())
        dir_count = sum(1 for path in self.selected_paths.values() if path.is_dir())
        total = len(self.selected_paths)
        if total:
            self.selection_summary_var.set(
                f"已添加 {total} 项  ·  {file_count} 个文件  ·  {dir_count} 个目录"
            )
            if not self.is_running:
                self.status_var.set("切片已就绪。确认输出位置后即可开始转换。")
                self.set_status_badge("待转换")
        else:
            self.selection_summary_var.set("尚未添加切片  ·  可拖拽到列表  ·  Ctrl+O 选择文件")
            if not self.is_running:
                self.status_var.set("请先选择切片文件或目录，然后开始转换。")
                self.set_status_badge("准备就绪")
        if self.drop_hint is not None:
            if total:
                self.drop_hint.place_forget()
            else:
                self.drop_hint.place(relx=0.5, rely=0.58, anchor="center")
        self.update_action_states()

    def update_action_states(self) -> None:
        """根据列表和运行状态刷新高频操作按钮。"""

        has_items = bool(self.selected_paths)
        for button in self.start_buttons:
            button.state(["disabled"] if self.is_running or not has_items else ["!disabled"])
        for button in self.clear_buttons:
            button.state(["!disabled"] if has_items and not self.is_running else ["disabled"])
        for button in self.input_buttons:
            button.state(["disabled"] if self.is_running else ["!disabled"])

    def parse_gui_options(self) -> GuiConversionOptions:
        """读取界面参数并转换为后端配置对象。"""

        jpeg_quality_text = self.jpeg_quality_var.get().strip()
        jpeg_quality = None if not jpeg_quality_text or jpeg_quality_text == QUALITY_OPTIONS[0] else int(
            jpeg_quality_text
        )
        if jpeg_quality is not None and not 1 <= jpeg_quality <= 100:
            raise ValueError("SVS 保存质量必须在 1 到 100 之间")
        output_dir_text = self.output_dir_var.get().strip()
        output_dir = Path(output_dir_text).expanduser() if output_dir_text else None
        if output_dir is not None and output_dir.exists() and not output_dir.is_dir():
            raise ValueError("输出目录必须是目录路径，不能是单个文件")

        return GuiConversionOptions(
            inputs=tuple(self.selected_paths.values()),
            output_dir=output_dir,
            input_format=FORMAT_LABELS[self.format_label_var.get()],
            tile_size=None,
            jpeg_quality=jpeg_quality,
            skip_associated=self.skip_associated_var.get(),
            overwrite=self.overwrite_var.get(),
        )

    def set_running_state(self, running: bool) -> None:
        """切换界面按钮状态。"""

        self.is_running = running
        for button in self.stop_buttons:
            if running:
                button.state(["!disabled"])
            else:
                button.state(["disabled"])
        self.update_action_states()
        self.set_status_badge("转换中" if running else "准备就绪", running)

    def set_status_badge(self, text: str, running: bool = False, failed: bool = False) -> None:
        """刷新顶部状态徽章。"""

        self.status_badge_var.set(text)
        if self.status_badge is None:
            return
        if failed:
            background, foreground = "#fff1f0", "#b42318"
        elif running:
            background, foreground = "#e7f3f0", ACCENT_DARK
        else:
            background, foreground = "#eef2f2", MUTED_COLOR
        self.status_badge.configure(bg=background, fg=foreground)

    def start_conversion(self) -> None:
        """规划任务并启动后台转换线程。"""

        if self.worker_thread and self.worker_thread.is_alive():
            return
        if not self.selected_paths:
            messagebox.showwarning(APP_TITLE, "请先选择至少一个切片文件或目录。")
            return

        try:
            options = self.parse_gui_options()
            jobs = plan_jobs(options)
        except ValueError as exc:
            messagebox.showerror(APP_TITLE, str(exc))
            return
        except FileNotFoundError as exc:
            messagebox.showerror(APP_TITLE, str(exc))
            return

        self.stop_event.clear()
        self.progress_bar.configure(maximum=max(len(jobs), 1), value=0)
        self.progress_text_var.set(f"0 / {len(jobs)}  ·  正在准备任务")
        self.set_running_state(True)
        self.status_var.set(f"已规划 {len(jobs)} 个任务，正在开始转换。")
        self.append_log("=" * 72)
        self.append_log(f"任务开始：共 {len(jobs)} 个待转换文件")

        self.worker_thread = threading.Thread(
            target=self.run_jobs_worker,
            args=(jobs, options),
            daemon=True,
        )
        self.worker_thread.start()

    def stop_conversion(self) -> None:
        """请求在当前文件完成后停止队列。"""

        if self.worker_thread and self.worker_thread.is_alive():
            self.stop_event.set()
            self.set_status_badge("停止中")
            self.status_var.set("已请求停止。当前文件完成后，剩余任务将不再继续。")
            self.append_log("收到停止请求：将在当前文件处理完成后停止剩余任务。")

    def run_jobs_worker(self, jobs, options: GuiConversionOptions) -> None:
        """后台线程：启动独立 worker 并把事件投递回主线程。"""

        def log_callback(message: str) -> None:
            self.event_queue.put(("log", message))

        def progress_callback(completed: int, total: int, job, phase: str) -> None:
            self.event_queue.put(("progress", completed, total, job.input_path.name, phase))

        try:
            summary = execute_jobs_subprocess(
                jobs,
                options,
                log_callback=log_callback,
                progress_callback=progress_callback,
                stop_event=self.stop_event,
            )
        except Exception as exc:
            self.event_queue.put(("error", str(exc)))
        else:
            self.event_queue.put(("done", summary))

    def drain_events(self) -> None:
        """在主线程中消费后台事件。"""

        while True:
            try:
                event = self.event_queue.get_nowait()
            except queue.Empty:
                break

            kind = event[0]
            if kind == "log":
                self.append_log(event[1])
            elif kind == "progress":
                self.handle_progress_event(*event[1:])
            elif kind == "error":
                self.set_running_state(False)
                self.set_status_badge("出现异常", failed=True)
                self.progress_text_var.set("任务异常中止")
                self.status_var.set("转换过程中出现异常，详情见日志。")
                self.append_log(f"Unhandled error: {event[1]}")
                self.reveal_log_panel()
                messagebox.showerror(APP_TITLE, event[1])
            elif kind == "done":
                self.handle_done_event(event[1])

        self.after(120, self.drain_events)

    def handle_progress_event(self, completed: int, total: int, name: str, phase: str) -> None:
        """根据后台状态刷新进度和状态文案。"""

        if phase == "starting":
            self.set_status_badge("转换中", running=True)
            self.progress_bar.configure(maximum=max(total, 1), value=completed)
            self.progress_text_var.set(f"{completed} / {total}  ·  正在处理 {name}")
            self.status_var.set(f"正在处理：{name}（{completed + 1}/{total}）")
        elif phase == "succeeded":
            self.progress_bar.configure(maximum=max(total, 1), value=completed)
            self.progress_text_var.set(f"{completed} / {total}  ·  最近完成 {name}")
            self.status_var.set(f"已完成：{name}（{completed}/{total}）")
        elif phase == "failed":
            self.set_status_badge("有失败项", failed=True)
            self.progress_bar.configure(maximum=max(total, 1), value=completed)
            self.progress_text_var.set(f"{completed} / {total}  ·  {name} 处理失败")
            self.status_var.set(f"处理失败：{name}（{completed}/{total}）")

    def handle_done_event(self, summary) -> None:
        """整批任务结束后的收尾逻辑。"""

        self.set_running_state(False)
        self.progress_bar.configure(maximum=max(summary.total, 1), value=summary.completed)

        if summary.cancelled:
            self.set_status_badge("已停止")
            self.progress_text_var.set(f"已停止  ·  完成 {summary.completed} / {summary.total}")
            self.status_var.set(
                f"任务已停止：已完成 {summary.completed}/{summary.total}，成功 {summary.succeeded}，失败 {summary.failed}。"
            )
            messagebox.showwarning(
                APP_TITLE,
                (
                    f"队列已停止。\n\n"
                    f"已完成：{summary.completed}/{summary.total}\n"
                    f"成功：{summary.succeeded}\n"
                    f"失败：{summary.failed}"
                ),
            )
            return

        self.status_var.set(
            f"全部任务结束：成功 {summary.succeeded}，失败 {summary.failed}，共 {summary.total} 个文件。"
        )
        self.progress_text_var.set(
            f"已完成 {summary.completed} / {summary.total}  ·  成功 {summary.succeeded}  ·  失败 {summary.failed}"
        )
        self.set_status_badge("有失败项" if summary.failed else "已完成", failed=bool(summary.failed))
        if summary.failed:
            self.reveal_log_panel()
            failed_details = "\n".join(
                f"{result.input_path.name}：{result.message or '未返回具体错误'}"
                for result in summary.results
                if not result.success
            )
            if len(failed_details) > 1600:
                failed_details = failed_details[:1600].rstrip() + "…"
            messagebox.showwarning(
                APP_TITLE,
                (
                    f"转换完成，但有失败文件。\n\n"
                    f"总数：{summary.total}\n"
                    f"成功：{summary.succeeded}\n"
                    f"失败：{summary.failed}\n\n"
                    f"失败原因：\n{failed_details}\n\n"
                    f"详细过程也已展开在下方日志中。"
                ),
            )
        else:
            messagebox.showinfo(
                APP_TITLE,
                f"转换完成。\n\n总数：{summary.total}\n成功：{summary.succeeded}\n失败：0",
            )

    def on_close(self) -> None:
        """关闭窗口前处理后台任务。"""

        if self.worker_thread and self.worker_thread.is_alive():
            if not messagebox.askyesno(
                APP_TITLE,
                "仍有转换任务在运行。关闭窗口后不会再显示进度，是否继续退出？",
            ):
                return
        self.destroy()


def main() -> None:
    if len(sys.argv) > 1 and sys.argv[1] == "--worker":
        from img2svs.app.svs_worker import main as worker_main

        raise SystemExit(worker_main(sys.argv[2:]))
    if os.environ.get("SVS_GUI_SMOKE_TEST") == "1":
        raise SystemExit(run_headless_smoke_test())
    app = SvsConverterApp()
    app.mainloop()


if __name__ == "__main__":
    main()
