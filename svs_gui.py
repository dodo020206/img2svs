#!/usr/bin/env python3
from __future__ import annotations

import os
import queue
import threading
import time
import tkinter as tk
from pathlib import Path
from tkinter import filedialog, font as tkfont, messagebox, ttk

from svs_gui_service import GUI_SUPPORTED_SUFFIXES, GuiConversionOptions, execute_jobs, plan_jobs

APP_TITLE = "病理图像转 SVS 工具"
ACCENT_COLOR = "#0f766e"
ACCENT_DARK = "#134e4a"
SURFACE_COLOR = "#f4f7f8"
CARD_COLOR = "#ffffff"
TEXT_COLOR = "#1f2937"
MUTED_COLOR = "#5b6472"

FORMAT_LABELS = {
    "自动识别（推荐）": "auto",
    "仅 CSP": "csp",
    "仅 KFB": "kfb",
    "仅 MDSX / MSDX": "mdsx",
    "仅 MRXS": "mrxs",
    "仅 NDPI": "ndpi",
    "仅 SDPC / DYQX": "sdpc",
}

FILE_TYPES = [
    ("病理切片文件", "*.csp *.sdpc *.dyqx *.kfb *.mdsx *.msdx *.mrxs *.ndpi"),
    ("CSP", "*.csp"),
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

    from svs_gui_service import GuiConversionOptions, execute_jobs, plan_jobs

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
    summary = execute_jobs(jobs)
    return 0 if summary.failed == 0 and not summary.cancelled else 1


def choose_font_family(root: tk.Misc) -> str:
    """优先选择适合中文桌面界面的字体。"""

    available = set(tkfont.families(root))
    for name in ("Microsoft YaHei UI", "Microsoft YaHei", "Segoe UI", "Noto Sans CJK SC"):
        if name in available:
            return name
    return "TkDefaultFont"


class SvsConverterApp(tk.Tk):
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
        self.scroll_canvas: tk.Canvas | None = None
        self.scroll_content: ttk.Frame | None = None
        self.scroll_window_id: int | None = None
        self.sources_panel: ttk.Labelframe | None = None
        self.settings_panel: ttk.Labelframe | None = None
        self.log_panel: ttk.Labelframe | None = None
        self.sources_body: ttk.Frame | None = None
        self.log_body: ttk.Frame | None = None
        self.sources_toggle_text_var = tk.StringVar(value="收起")
        self.log_toggle_text_var = tk.StringVar(value="展开")
        self.sources_collapsed = False
        self.log_collapsed = True

        self.output_dir_var = tk.StringVar()
        self.jpeg_quality_var = tk.StringVar(value=QUALITY_OPTIONS[0])
        self.format_label_var = tk.StringVar(value=next(iter(FORMAT_LABELS)))
        self.overwrite_var = tk.BooleanVar(value=False)
        self.skip_associated_var = tk.BooleanVar(value=False)
        self.selection_summary_var = tk.StringVar()
        self.status_var = tk.StringVar(value="请先选择切片文件或目录，然后开始转换。")

        self.ui_font = choose_font_family(self)
        self.protocol("WM_DELETE_WINDOW", self.on_close)

        self.configure_window_geometry()
        self.configure_styles()
        self.build_layout()
        self.bind("<F5>", lambda _event: self.start_conversion())
        self.update_selection_summary()
        self.after(120, self.drain_events)

    def configure_window_geometry(self) -> None:
        """根据当前屏幕尺寸设置一个更稳妥的初始窗口大小。"""

        screen_width = max(self.winfo_screenwidth(), 1024)
        screen_height = max(self.winfo_screenheight(), 720)
        width = min(1180, max(920, screen_width - 120))
        height = min(760, max(620, screen_height - 120))
        pos_x = max((screen_width - width) // 2, 0)
        pos_y = max((screen_height - height) // 2, 0)
        self.geometry(f"{width}x{height}+{pos_x}+{pos_y}")
        self.minsize(min(width, 920), min(height, 620))

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
        style.configure("Card.TLabelframe", background=CARD_COLOR, borderwidth=1, relief="solid")
        style.configure(
            "Card.TLabelframe.Label",
            background=CARD_COLOR,
            foreground=TEXT_COLOR,
            font=(self.ui_font, 11, "bold"),
        )
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
            padding=(12, 8),
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
            "Treeview",
            font=(self.ui_font, 10),
            rowheight=30,
            fieldbackground="#ffffff",
            background="#ffffff",
            foreground=TEXT_COLOR,
        )
        style.configure(
            "Treeview.Heading",
            font=(self.ui_font, 10, "bold"),
            background="#e7eef0",
            foreground=TEXT_COLOR,
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
            thickness=16,
            troughcolor="#dce8e6",
            background=ACCENT_COLOR,
            bordercolor="#dce8e6",
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

        self.scroll_content = ttk.Frame(self.scroll_canvas, padding=20)
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

        if self.scroll_canvas is None:
            return None

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

        if width >= 1320:
            self.scroll_content.columnconfigure(0, weight=3)
            self.scroll_content.columnconfigure(1, weight=2)
            self.scroll_content.rowconfigure(1, weight=1)
            self.scroll_content.rowconfigure(2, weight=1)

            self.sources_panel.grid(row=1, column=0, sticky="nsew", padx=(0, 10), pady=(0, 10))
            self.settings_panel.grid(row=1, column=1, sticky="nsew", pady=(0, 10))
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

    def build_header(self, parent: ttk.Frame) -> None:
        """顶部标题区。"""

        header = ttk.Frame(parent)
        header.grid(row=0, column=0, columnspan=2, sticky="ew", pady=(0, 16))
        title = ttk.Label(header, text=APP_TITLE, style="Title.TLabel")
        title.pack(anchor="w")
        subtitle = ttk.Label(
            header,
            text=(
                "支持 SDPC/DYQX、KFB、MDSX/MSDX、MRXS、NDPI。"
            ),
            style="Subtitle.TLabel",
        )
        subtitle.pack(anchor="w", pady=(4, 0))

        action_row = ttk.Frame(header)
        action_row.pack(fill="x", pady=(12, 0))
        action_row.columnconfigure(0, weight=1)
        action_row.columnconfigure(1, weight=1)

        top_start_button = ttk.Button(
            action_row,
            text="开始转换",
            style="Accent.TButton",
            command=self.start_conversion,
        )
        top_start_button.grid(row=0, column=0, sticky="ew", padx=(0, 6))
        self.start_buttons.append(top_start_button)

        top_stop_button = ttk.Button(
            action_row,
            text="停止队列",
            command=self.stop_conversion,
        )
        top_stop_button.grid(row=0, column=1, sticky="ew", padx=(6, 10))
        top_stop_button.state(["disabled"])
        self.stop_buttons.append(top_stop_button)

        self.progress_bar = ttk.Progressbar(header, mode="determinate")
        self.progress_bar.pack(fill="x", pady=(10, 0))

        ttk.Label(
            header,
            text="按 F5 也能开始转换。文件选择区和日志区支持收起 / 展开。",
            style="Subtitle.TLabel",
        ).pack(anchor="w", pady=(8, 0))

    def build_sources_panel(self, parent: ttk.Frame) -> None:
        """左侧输入文件区。"""

        frame = ttk.Labelframe(parent, text="1. 选择切片文件或目录", style="Card.TLabelframe")
        frame.columnconfigure(0, weight=1)
        frame.rowconfigure(1, weight=1)
        self.sources_panel = frame

        controls = ttk.Frame(frame)
        controls.grid(row=0, column=0, sticky="ew", padx=14, pady=(14, 6))
        controls.columnconfigure(0, weight=1)
        ttk.Label(
            controls,
            text="可添加单个切片文件，也可直接添加整个目录。",
            style="Hint.TLabel",
        ).grid(row=0, column=0, sticky="w")
        ttk.Button(
            controls,
            textvariable=self.sources_toggle_text_var,
            command=self.toggle_sources_panel,
            width=8,
        ).grid(row=0, column=1, sticky="e")

        body = ttk.Frame(frame)
        body.grid(row=1, column=0, sticky="nsew", padx=14, pady=(0, 14))
        body.columnconfigure(0, weight=1)
        body.rowconfigure(2, weight=1)
        self.sources_body = body

        button_row = ttk.Frame(body)
        button_row.grid(row=0, column=0, sticky="ew", pady=(0, 6))
        for column in range(2):
            button_row.columnconfigure(column, weight=1)

        ttk.Button(button_row, text="添加文件", command=self.add_files).grid(
            row=0, column=0, sticky="ew", padx=(0, 6), pady=(0, 6)
        )
        ttk.Button(button_row, text="添加目录", command=self.add_folder).grid(
            row=0, column=1, sticky="ew", padx=(6, 0), pady=(0, 6)
        )
        ttk.Button(button_row, text="移除选中", command=self.remove_selected).grid(
            row=1, column=0, sticky="ew", padx=(0, 6)
        )
        ttk.Button(button_row, text="清空列表", command=self.clear_selected).grid(
            row=1, column=1, sticky="ew", padx=(6, 0)
        )

        ttk.Label(body, textvariable=self.selection_summary_var, style="Hint.TLabel").grid(
            row=1, column=0, sticky="w", pady=(0, 8)
        )

        table_frame = ttk.Frame(body)
        table_frame.grid(row=2, column=0, sticky="nsew")
        table_frame.columnconfigure(0, weight=1)
        table_frame.rowconfigure(0, weight=1)

        self.source_tree = ttk.Treeview(
            table_frame,
            columns=("type", "path"),
            show="headings",
            selectmode="extended",
        )
        self.source_tree.heading("type", text="类型")
        self.source_tree.heading("path", text="路径")
        self.source_tree.column("type", width=120, anchor="center", stretch=False)
        self.source_tree.column("path", width=720, anchor="w")
        self.source_tree.grid(row=0, column=0, sticky="nsew")

        scrollbar = ttk.Scrollbar(table_frame, orient="vertical", command=self.source_tree.yview)
        scrollbar.grid(row=0, column=1, sticky="ns")
        self.source_tree.configure(yscrollcommand=scrollbar.set)

    def build_settings_panel(self, parent: ttk.Frame) -> None:
        """右侧参数与操作区。"""

        frame = ttk.Labelframe(parent, text="2. 转换设置", style="Card.TLabelframe")
        frame.columnconfigure(1, weight=1)
        self.settings_panel = frame

        ttk.Label(frame, text="输出目录").grid(row=0, column=0, sticky="w", padx=14, pady=(16, 8))
        output_row = ttk.Frame(frame)
        output_row.grid(row=0, column=1, sticky="ew", padx=(0, 14), pady=(16, 8))
        output_row.columnconfigure(0, weight=1)
        output_row.columnconfigure(1, weight=1)
        ttk.Entry(output_row, textvariable=self.output_dir_var).grid(
            row=0, column=0, columnspan=2, sticky="ew", pady=(0, 6)
        )
        ttk.Button(output_row, text="选择", command=self.choose_output_dir).grid(
            row=1, column=0, sticky="ew", padx=(0, 6)
        )
        ttk.Button(output_row, text="就地输出", command=self.clear_output_dir).grid(
            row=1, column=1, sticky="ew", padx=(6, 0)
        )
        ttk.Label(
            frame,
            text="留空时会输出到源文件同目录；指定目录时会自动生成 .svs 文件。",
            style="Hint.TLabel",
        ).grid(row=1, column=1, sticky="w", padx=(0, 14), pady=(0, 8))

        ttk.Label(frame, text="输入格式").grid(row=2, column=0, sticky="w", padx=14, pady=8)
        ttk.Combobox(
            frame,
            textvariable=self.format_label_var,
            values=list(FORMAT_LABELS),
            state="readonly",
        ).grid(row=2, column=1, sticky="ew", padx=(0, 14), pady=8)

        ttk.Label(frame, text="SVS 保存质量").grid(row=3, column=0, sticky="w", padx=14, pady=8)
        ttk.Combobox(
            frame,
            textvariable=self.jpeg_quality_var,
            values=QUALITY_OPTIONS,
            state="readonly",
        ).grid(row=3, column=1, sticky="ew", padx=(0, 14), pady=8)
        ttk.Label(
            frame,
            text="数值越低，输出的 SVS 越小；“原始/推荐”会沿用源图质量，NDPI 默认按 90 保存，MRXS 默认按 70 保存。",
            style="Hint.TLabel",
        ).grid(row=4, column=1, sticky="w", padx=(0, 14), pady=(0, 8))

        options_row = ttk.Frame(frame)
        options_row.grid(row=5, column=0, columnspan=2, sticky="ew", padx=14, pady=(8, 8))
        ttk.Checkbutton(
            options_row,
            text="覆盖已有 SVS 文件",
            variable=self.overwrite_var,
        ).pack(anchor="w", pady=4)
        ttk.Checkbutton(
            options_row,
            text="跳过标签图 / 宏观图",
            variable=self.skip_associated_var,
        ).pack(anchor="w", pady=4)

        status_box = tk.Frame(frame, bg="#ebf4f3", highlightthickness=1, highlightbackground="#d2e5e2")
        status_box.grid(row=6, column=0, columnspan=2, sticky="ew", padx=14, pady=(4, 16))
        status_box.grid_columnconfigure(0, weight=1)
        tk.Label(
            status_box,
            text="当前状态",
            bg="#ebf4f3",
            fg=ACCENT_DARK,
            font=(self.ui_font, 10, "bold"),
            anchor="w",
        ).grid(row=0, column=0, sticky="ew", padx=12, pady=(10, 4))
        tk.Label(
            status_box,
            textvariable=self.status_var,
            bg="#ebf4f3",
            fg=TEXT_COLOR,
            wraplength=320,
            justify="left",
            anchor="w",
            font=(self.ui_font, 10),
        ).grid(row=1, column=0, sticky="ew", padx=12, pady=(0, 12))

    def build_log_panel(self, parent: ttk.Frame) -> None:
        """底部日志区。"""

        frame = ttk.Labelframe(parent, text="3. 运行日志", style="Card.TLabelframe")
        frame.columnconfigure(0, weight=1)
        frame.rowconfigure(1, weight=1)
        self.log_panel = frame

        controls = ttk.Frame(frame)
        controls.grid(row=0, column=0, sticky="ew", padx=14, pady=(14, 6))
        controls.columnconfigure(1, weight=1)
        ttk.Button(
            controls,
            textvariable=self.log_toggle_text_var,
            command=self.toggle_log_panel,
            width=8,
        ).grid(row=0, column=0, sticky="w")
        ttk.Label(
            controls,
            text=(
                "支持格式："
                + ", ".join(sorted(suffix.replace(".", "").upper() for suffix in GUI_SUPPORTED_SUFFIXES))
            ),
            style="Hint.TLabel",
        ).grid(row=0, column=1, sticky="e")

        body = ttk.Frame(frame)
        body.grid(row=1, column=0, sticky="nsew", padx=14, pady=(0, 14))
        body.columnconfigure(0, weight=1)
        body.rowconfigure(0, weight=1)
        self.log_body = body

        self.log_text = tk.Text(
            body,
            bg="#0f172a",
            fg="#e2e8f0",
            insertbackground="#e2e8f0",
            relief="flat",
            wrap="word",
            font=("Consolas", 10) if self.ui_font == "TkDefaultFont" else ("Consolas", 10),
            padx=12,
            pady=12,
        )
        self.log_text.grid(row=0, column=0, sticky="nsew", pady=(0, 10))
        self.log_text.configure(state="disabled")

        body_controls = ttk.Frame(body)
        body_controls.grid(row=1, column=0, sticky="ew")
        ttk.Button(body_controls, text="清空日志", command=self.clear_logs).pack(side="left")

        self.apply_collapsible_state()

    def add_files(self) -> None:
        """添加多个切片文件。"""

        paths = filedialog.askopenfilenames(title="选择病理切片文件", filetypes=FILE_TYPES)
        self.add_input_paths(Path(path) for path in paths)

    def add_folder(self) -> None:
        """添加包含切片的目录。"""

        folder = filedialog.askdirectory(title="选择包含切片文件的目录")
        if folder:
            self.add_input_paths([Path(folder)])

    def add_input_paths(self, paths) -> None:
        """把输入路径加入列表。"""

        for path in paths:
            resolved = path.expanduser().resolve()
            item_key = str(resolved)
            if item_key in self.selected_paths:
                continue
            kind = "目录" if resolved.is_dir() else resolved.suffix.lower().lstrip(".").upper()
            self.source_tree.insert("", "end", iid=item_key, values=(kind, item_key))
            self.selected_paths[item_key] = resolved
        self.update_selection_summary()

    def remove_selected(self) -> None:
        """移除当前选中的路径。"""

        for item_id in self.source_tree.selection():
            self.source_tree.delete(item_id)
            self.selected_paths.pop(item_id, None)
        self.update_selection_summary()

    def clear_selected(self) -> None:
        """清空输入列表。"""

        for item_id in self.source_tree.get_children():
            self.source_tree.delete(item_id)
        self.selected_paths.clear()
        self.update_selection_summary()

    def choose_output_dir(self) -> None:
        """选择输出目录。"""

        folder = filedialog.askdirectory(title="选择 SVS 输出目录")
        if folder:
            self.output_dir_var.set(folder)

    def clear_output_dir(self) -> None:
        """恢复为源目录输出。"""

        self.output_dir_var.set("")

    def clear_logs(self) -> None:
        """清空日志文本。"""

        self.log_text.configure(state="normal")
        self.log_text.delete("1.0", "end")
        self.log_text.configure(state="disabled")

    def append_log(self, message: str) -> None:
        """向日志区域追加一行。"""

        timestamp = time.strftime("%H:%M:%S")
        self.log_text.configure(state="normal")
        self.log_text.insert("end", f"[{timestamp}] {message}\n")
        self.log_text.see("end")
        self.log_text.configure(state="disabled")

    def update_selection_summary(self) -> None:
        """刷新文件列表摘要。"""

        file_count = sum(1 for path in self.selected_paths.values() if path.is_file())
        dir_count = sum(1 for path in self.selected_paths.values() if path.is_dir())
        self.selection_summary_var.set(
            f"当前已选择 {len(self.selected_paths)} 个输入项，其中目录 {dir_count} 个、文件 {file_count} 个。"
        )

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

        for button in self.start_buttons:
            if running:
                button.state(["disabled"])
            else:
                button.state(["!disabled"])
        for button in self.stop_buttons:
            if running:
                button.state(["!disabled"])
            else:
                button.state(["disabled"])

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

        if self.log_collapsed:
            self.log_collapsed = False
            self.apply_collapsible_state()

        self.stop_event.clear()
        self.progress_bar.configure(maximum=max(len(jobs), 1), value=0)
        self.set_running_state(True)
        self.status_var.set(f"已规划 {len(jobs)} 个任务，正在开始转换。")
        self.append_log("=" * 72)
        self.append_log(f"任务开始：共 {len(jobs)} 个待转换文件")

        self.worker_thread = threading.Thread(
            target=self.run_jobs_worker,
            args=(jobs,),
            daemon=True,
        )
        self.worker_thread.start()

    def stop_conversion(self) -> None:
        """请求在当前文件完成后停止队列。"""

        if self.worker_thread and self.worker_thread.is_alive():
            self.stop_event.set()
            self.status_var.set("已请求停止。当前文件完成后，剩余任务将不再继续。")
            self.append_log("收到停止请求：将在当前文件处理完成后停止剩余任务。")

    def run_jobs_worker(self, jobs) -> None:
        """后台线程：执行任务并把事件投递回主线程。"""

        def log_callback(message: str) -> None:
            self.event_queue.put(("log", message))

        def progress_callback(completed: int, total: int, job, phase: str) -> None:
            self.event_queue.put(("progress", completed, total, job.input_path.name, phase))

        try:
            summary = execute_jobs(
                jobs,
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
                self.status_var.set("转换过程中出现异常，详情见日志。")
                self.append_log(f"Unhandled error: {event[1]}")
                messagebox.showerror(APP_TITLE, event[1])
            elif kind == "done":
                self.handle_done_event(event[1])

        self.after(120, self.drain_events)

    def handle_progress_event(self, completed: int, total: int, name: str, phase: str) -> None:
        """根据后台状态刷新进度和状态文案。"""

        if phase == "starting":
            self.progress_bar.configure(maximum=max(total, 1), value=completed)
            self.status_var.set(f"正在处理：{name}（{completed + 1}/{total}）")
        elif phase == "succeeded":
            self.progress_bar.configure(maximum=max(total, 1), value=completed)
            self.status_var.set(f"已完成：{name}（{completed}/{total}）")
        elif phase == "failed":
            self.progress_bar.configure(maximum=max(total, 1), value=completed)
            self.status_var.set(f"处理失败：{name}（{completed}/{total}）")

    def handle_done_event(self, summary) -> None:
        """整批任务结束后的收尾逻辑。"""

        self.set_running_state(False)
        self.progress_bar.configure(maximum=max(summary.total, 1), value=summary.completed)

        if summary.cancelled:
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
        if summary.failed:
            messagebox.showwarning(
                APP_TITLE,
                (
                    f"转换完成，但有失败文件。\n\n"
                    f"总数：{summary.total}\n"
                    f"成功：{summary.succeeded}\n"
                    f"失败：{summary.failed}\n\n"
                    f"可在下方日志中查看失败原因。"
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
    if os.environ.get("SVS_GUI_SMOKE_TEST") == "1":
        raise SystemExit(run_headless_smoke_test())
    app = SvsConverterApp()
    app.mainloop()


if __name__ == "__main__":
    main()
