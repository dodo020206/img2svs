# img2svs Rust

这是 `../img2svs-python` 中转换器的原生 Rust 实现。
仓库布局、共享原生运行库和构建入口的说明见
[`../README.md`](../README.md)。

> 本文件会被 `scripts\package_windows.ps1` 和 CI 复制进发布包，
> 文中的 `../` 相对链接只在仓库目录内有效。

## 环境要求

| 项 | 值 | 核对来源 |
| --- | --- | --- |
| crate / 版本 | `img2svs-rust` `0.1.0` | `Cargo.toml` |
| edition | `2021` | `Cargo.toml` |
| 许可证 | MIT | `Cargo.toml` |
| Rust 工具链 | stable | `.github/workflows/build-rust-windows.yml`（`dtolnay/rust-toolchain@stable`） |
| MSRV | **未声明** —— `Cargo.toml` 无 `rust-version`，仓库无 `rust-toolchain.toml` → **待核实** | — |
| 目标平台 | Windows（GUI 变体用 `windows_subsystem = "windows"`） | `src/main.rs` |
| 外部运行库 | 仅 FFmpeg，运行期由 `libloading` 加载；无 OpenSlide / libvips | `src/hevc.rs` |

## 可执行文件的名字

同一份源码产出两个变体，**发布包里的文件名不同**，用途也不同：

| 文件名 | 变体 | 构建方式 | 用途 |
| --- | --- | --- | --- |
| `img2svs-gui.exe` | GUI（`gui` feature，默认） | `cargo build --release` | 双击即用；也接受全部 CLI 参数（给了输入文件就走命令行，不弹窗） |
| `img2svs-cli.exe` | 控制台 | `cargo build --release --no-default-features` | 脚本与批处理：输出留在控制台，没有 `--gui` / `--smoke-test` |

- Cargo 本身只按 crate 名产出 `target\release\img2svs-rust.exe`；
  `img2svs-gui.exe` / `img2svs-cli.exe` 是 `scripts\package_windows.ps1`
  和 CI 在拷出产物时改的名字。**本地 `cargo build` 后没有这两个文件**，
  要用就自己改名，或直接跑 `package_windows.ps1`。
- 两个可执行文件都支持 `--version`，输出分别为
  `img2svs 0.1.0 (gui)` / `img2svs 0.1.0 (cli)`，可据此确认手上是哪个。
- 两个变体都需要同目录的 `av.libs\`，否则 HEVC 压缩的 `.sdpc` / `.dyqx` 无法转换。

下文示例都用 `target\release\` 下的 Cargo 产物名；换成发布包时把
`img2svs-rust.exe` 换成 `img2svs-gui.exe` 或 `img2svs-cli.exe` 即可，
参数完全相同。

## GUI

不带参数直接运行可执行文件，或传入 `--gui`（仅 GUI 变体）：

```console
.\target\release\img2svs-rust.exe
.\target\release\img2svs-rust.exe --gui
```

发布包里对应的命令：

```console
.\img2svs-gui.exe
.\img2svs-gui.exe --gui
```

GUI 支持多文件选择、递归文件夹扫描、拖放（文件与文件夹）、输出目录选择、
JPEG 质量、覆盖选项、队列式后台工作线程、进度显示、逐文件状态、
日志以及协作式取消。每一行都有独立的 `移除` 按钮。快捷键为
`Ctrl+O`（文件）、`Ctrl+Shift+O`（文件夹）、`F5`（开始）和 `Esc`（停止）。

原生后端包括全部支持的格式，没有任何 OpenSlide/libvips 依赖：

- `.dmetrix`：JPEG 瓦片、金字塔层级、标签和宏观图像。
- `.csp`：索引式 JPEG 瓦片、金字塔层级、标签/宏观图像。
- `.kfb`：KFBio 索引式 JPEG 瓦片、稀疏瓦片布局、金字塔层级、
  标签/宏观图像。
- `.mdsx` / `.msdx` / `.mdss`：BKIO 容器、UTF-16/Base64 XML、INI 元数据、
  JPEG 瓦片、标签/宏观图像。三种扩展名共用同一种字节布局，
  因此由同一个读取器处理。
- `.sdpc` / `.dyqx`：JPEG 和 HEVC 压缩的 SDPC 文件，包括非 16 对齐的源瓦片
  （如 `616x880`）；相邻源瓦片会被合成为合法的 TIFF 输出瓦片，
  最后一行/列用白色填充。HEVC 在可用时使用随附的 FFmpeg 原生运行库。
- `.ndpi`：Hamamatsu 容器。每一层是一条被重启区间切开的 baseline JPEG，
  一个重启区间正好是一个瓦片：读取时把 SOF 的高宽改写成区间几何，
  再接上该区间的熵编码数据与 EOI，还原出一条完整的 JPEG 流
  （像素仍按输出瓦片的需要编码一次）。
  每层有各自的瓦片尺寸（DRI 声明的 MCU 数不等于像素宽）。
- `.mrxs`：3DHISTECH Pannoramic。`.mrxs` 只是占位文件，真实内容在同名目录里：
  `Slidedat.ini` 描述层级与相机网格，`Index.dat` 的页链给出瓦片索引，
  `Data*.dat` 是原始 JPEG 字节流。瓦片按像素坐标精确落位。
- `.tif` / `.tiff`：瓦片式或扫描线式 TIFF 输入，TIFF 分辨率和物镜倍率
  元数据会带入金字塔 JPEG SVS。同时支持经典 TIFF 与 BigTIFF 头。

`.svs` 是**输出**格式，不作为输入：它本身就是一个 TIFF，喂回转换器没有意义，
GUI 的目录扫描也会跳过 `.svs` 文件。确需查看产物时把文件改成 `.tif` 即可用 `--info` 读。

CLI 和输出布局与 Python 版本保持一致。样本文件放在仓库根的 `test_data\`
（**不入版本库**，见根 `.gitignore`），因此示例命令要在 `img2svs-rust\` 下
用 `..\` 回指：

```console
cargo run --release -- ..\test_data\dmetrix\t-dmetrix.dmetrix -o ..\test-output\t-dmetrix.svs --overwrite
cargo run --release -- ..\test_data\t-sdpc.sdpc -o ..\test-output\t-sdpc.svs --overwrite
```

发布包里的两个可执行文件用同样的参数（路径换成自己的样本）：

```console
.\img2svs-gui.exe D:\slides\t-dmetrix.dmetrix -o D:\out\t-dmetrix.svs --overwrite
.\img2svs-cli.exe D:\slides\t-dmetrix.dmetrix -o D:\out\t-dmetrix.svs --overwrite
```

CLI 参数（两个变体共用；`--gui` / `--smoke-test` 仅 GUI 变体，
即只有 `img2svs-gui.exe` 认这两个参数）：

| 参数 | 说明 |
| --- | --- |
| `<输入文件>` | 支持 `.csp/.dmetrix/.kfb/.mdss/.mdsx/.msdx/.mrxs/.ndpi/.tif/.tiff/.sdpc/.dyqx`；GUI 变体省略时启动界面 |
| `-o, --output <路径>` | 输出 `.svs` 路径，默认为输入路径改扩展名 |
| `--jpeg-quality <1-100>` | 输出 JPEG 质量；默认沿用源文件元数据 |
| `--overwrite` | 覆盖已存在的输出文件 |
| `--info` | 只解析并打印切片元数据，不转换 |
| `--gui` | 启动 GUI（仅 GUI 变体） |
| `--smoke-test` | 启动 GUI 并在第一帧后关闭（仅 GUI 变体，`--help` 中不显示） |
| `-h, --help` | 打印帮助 |
| `--version` | 输出版本并带 `(gui)` / `(cli)` 变体后缀，例如 `img2svs 0.1.0 (cli)` |

Rust GUI/CLI 支持 JPEG/HEVC 的 SDPC/DYQX 以及上述所有格式，
唯一的外部运行库是 FFmpeg，且只有 HEVC 源用得到。

输出是经典 TIFF，偏移为 32 位：切片大到输出超过 4 GiB 时会直接报错
（`TIFF exceeds classic 4 GiB offsets`），不会写出损坏的文件。
没有 BigTIFF 输出路径。

## 性能

原生 JPEG 和 HEVC 瓦片解码/编码工作运行在有界工作线程池中。
JPEG 转换默认使用操作系统报告的逻辑 CPU 数量（最多 64 个工作线程）。
HEVC 保留一个逻辑 CPU，且上限为 32 个工作线程，因为每个工作线程
独占一个解码器。在启动 CLI 或 GUI 前设置 `IMG2SVS_THREADS`
可覆盖检测到的值（仍受上述上限约束），例如：

```powershell
$env:IMG2SVS_THREADS = '8'
.\img2svs-gui.exe input.csp -o output.svs --overwrite
.\img2svs-cli.exe input.csp -o output.svs --overwrite
```

JPEG 瓦片使用 Rust 编码器（`jpeg-encoder` 的 `simd` feature）。非 4:2:0 的
JPEG 瓦片在 4:2:0 编码前直接解码为 YCbCr，避免了不必要的 RGB 色彩转换。
源瓦片通过共享的只读内存映射读取；编码后的瓦片仅以有界批次缓冲，
并按源顺序写出，因此并行转换不会将整张切片加载进内存，
也不会改变 TIFF 瓦片偏移顺序。

源瓦片只有在**全部**满足下列条件时才直接拷贝字节流，不走编码：

- 输出瓦片不需要合成多个源瓦片（合并行列均为 1），且不是最后一行/列；
- 源是 JPEG（非 HEVC），且该层的瓦片本身就是一条完整 JPEG 流 ——
  即没有共享的 `JPEGTables`、也不是从单条 JPEG 条带切出来的重启区间
  （`.ndpi` 属于后者；带 `JPEGTables` 的 `.tif` 瓦片缺 SOI，
  两者都要先拼回完整流，因此不走直通）；
- 输出质量与源元数据里的质量一致；
- 该瓦片已经是 4:2:0 采样，否则先做一次保采样表的转码。

其余瓦片解码后重排、重新编码一次。

## 构建、测试与冒烟

在正常的 Rust Windows 环境上：

```console
cargo fmt --all -- --check
cargo test --locked
cargo build --release
.\target\release\img2svs-rust.exe --smoke-test
```

（`--smoke-test` 只存在于 GUI 变体；在发布包里就是
`.\img2svs-gui.exe --smoke-test`。`img2svs-cli.exe` 不认识这个参数。）

控制台变体单独编译（`gui` feature 关掉后未使用的代码项不同，
不单独检查就看不到它的警告；CI 里正是这一步）：

```console
$env:RUSTFLAGS = '-D warnings'
cargo check --locked --all-targets --no-default-features
```

仓库在 `..\third_party` 中只保留一份原生运行库。
在仓库根目录执行一次以下命令填充它（Rust 只需要 FFmpeg）：

```powershell
pwsh -File ..\scripts\fetch_native_runtimes.ps1 -SkipLibvips
```

随后 `build_windows.ps1` 会将 `av.libs` 复制到可执行文件旁边。
它依次从 `-FfmpegHome`、`FFMPEG_HOME` 或 `..\third_party\av.libs`
解析该运行库，缺失时发出警告而不是静默跳过。
分发时请发布完整的 release 目录（包括 `av.libs`），
而不是只发可执行文件。

`--smoke-test` 会初始化原生窗口并在第一帧后关闭；
适用于 CI 或打包检查，不会留下运行中的 GUI 进程。

## 运行库解析顺序

构建时（`build_windows.ps1` / `package_windows.ps1`）按以下顺序查找 FFmpeg，
先命中先用：

1. 脚本参数：`-FfmpegHome`
2. 环境变量：`FFMPEG_HOME`
3. 仓库共享目录：`..\third_party\av.libs`

运行时由可执行文件自行发现（`src/hevc.rs::locate_ffmpeg_dir`），同样先命中先用：

1. 环境变量：`FFMPEG_HOME`
2. 可执行文件旁的 `ffmpeg\` 目录
3. 可执行文件旁的 `av.libs\` 目录
4. `PATH` 中的每个目录

必须同时找到 `avcodec-*.dll`、`avutil-*.dll`、`swscale-*.dll`。
不依赖开发机器上的路径；找不到时 HEVC 转换直接失败，`--info` 与其他格式不受影响。

## GUI 与控制台构建

GUI 位于默认的 `gui` cargo feature 之后，两个变体来自同一份源码：

```console
cargo build --release                        # GUI 构建，隐藏控制台
cargo build --release --no-default-features  # 控制台构建，无 eframe(egui)/rfd
```

控制台构建去掉了 `eframe`（及其 egui）和 `rfd` 依赖，并保留控制台子系统，
因此命令行输出保持可见，且不接受 `--gui` / `--smoke-test`。
可执行文件体积约为 GUI 的三分之一（实测 GUI 约 6.7 MB、控制台约 2.1 MB）。
`--version` 会报告当前运行的变体，例如 `img2svs 0.1.0 (cli)`。

生成两个便携式 Windows 包（可执行文件、README 和 `av.libs`），
各带一个 ZIP 和一个 SHA256 校验和：

```powershell
pwsh -File ..\scripts\package_windows.ps1
```

输出位于 `..\dist`：

| 包 | 可执行文件 | 内容 |
| --- | --- | --- |
| `PathologySVSConverter-rust-gui\` | `img2svs-gui.exe` | GUI 构建，双击即用，也可传入 CLI 参数 |
| `PathologySVSConverter-rust-cli\` | `img2svs-cli.exe` | 控制台构建，用于脚本和批处理任务 |

两个包各带一份完整的 `av.libs`（约 63 MB），所以 ZIP 体积远大于可执行文件本身。
只分发单个可执行文件会让 HEVC 源不可用。

包内文件名即上表所示：解压后直接运行 `img2svs-gui.exe`（图形界面）或
`img2svs-cli.exe`（命令行），两者参数一致，只是前者在不给输入文件时会弹窗。

## GitHub Actions Windows 打包

[`build-rust-windows.yml`](../.github/workflows/build-rust-windows.yml)
工作流在 GitHub 的 Windows runner 上构建并测试 Rust 转换器。

触发条件：`workflow_dispatch`；`push` 到 `main` 或 `v*` 标签；相关 pull request。
三者都带路径过滤，只关心 `img2svs-rust/**`、`scripts/**` 和该工作流文件本身。

流程要点：

1. 安装 stable Rust（含 `rustfmt`）与 Python 3.12（只为 Pillow）。
2. `cargo fmt --all -- --check`、`cargo test --locked`、
   `cargo check --locked --all-targets --no-default-features`（`RUSTFLAGS=-D warnings`）、
   `cargo build --release --locked`。
3. 用 `scripts\fetch_native_runtimes.ps1 -SkipLibvips` 只取固定版本
   `PYAV_VERSION=18.1.0` 的 FFmpeg 运行库，与 `img2svs-gui.exe`（由 Cargo 产物
   `img2svs-rust.exe` 改名）、本 README 一起组装成**一个**便携目录
   `PathologySVSConverter-rust\`（CI 只构建 GUI 变体；
   `img2svs-gui.exe` + `img2svs-cli.exe` 两个包是 `package_windows.ps1` 才做的）。
4. 用 `scripts\make_smoke_tiff.py <out.tif> 512 384` 现场生成 TIFF 样本，
   转成 SVS 后做两项独立校验：用转换器自身读回（`复制成 .tif` 再 `--info`），
   以及直接解析产物的 TIFF 标签（不需要 libvips）。
5. 打包成 `PathologySVSConverter-rust.zip` 及 SHA256 校验和，
   作为 Actions 产物上传（保留 30 天）。
6. 推送以 `v` 开头的标签时，还会额外创建或更新一个
   包含相同 ZIP 和校验和的 GitHub Release。

## 待核实

以下信息无法从仓库中确认，刻意未写入正文：

- **MSRV**：`Cargo.toml` 未声明 `rust-version`，也没有 `rust-toolchain.toml`，
  只有 CI 用 stable。不假设具体最低版本。
- **示例样本**：`test_data\` 被根 `.gitignore` 忽略，新克隆里没有样本，
  示例命令中的文件名只在有样本的本机可用。
- **随包分发 FFmpeg DLL 的许可证义务**：`av.libs` 来自 PyAV wheel，
  仓库中没有任何许可证/源码获取说明，发布前需确认（不在本文档断言具体许可证）。
- **性能数字**：本文不写具体耗时，机器与样本不同差异很大，需要时用
  `.workbuddy/tools/` 下的基准脚本实测。
