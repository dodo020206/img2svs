# img2svs Rust

这是 `../img2svs-python` 中转换器的原生 Rust 实现。
仓库布局、共享原生运行库和构建入口的说明见
[`../README.md`](../README.md)。

## GUI

不带参数直接运行可执行文件，或传入 `--gui`：

```powershell
.\target\release\img2svs-rust.exe
.\target\release\img2svs-rust.exe --gui
```

GUI 支持多文件选择、递归文件夹扫描、Windows 拖放、输出文件夹选择、
JPEG 质量、覆盖/关联图像选项、队列式后台工作线程、进度显示、逐文件状态、
日志以及协作式取消。每一行都有独立的 `移除` 按钮。快捷键为
`Ctrl+O`（文件）、`Ctrl+Shift+O`（文件夹）、`F5`（开始）和 `Esc`（停止）。

原生后端包括全部支持的格式，不依赖任何外部原生运行库
（既不需要 OpenSlide/libvips，也不需要 FFmpeg）：

- `.dmetrix`：JPEG 瓦片、金字塔层级、标签和宏观图像。
- `.csp`：索引式 JPEG 瓦片、金字塔层级、标签/宏观图像。
- `.kfb`：KFBio 索引式 JPEG 瓦片、稀疏瓦片布局、金字塔层级、
  标签/宏观图像。
- `.mdsx` / `.msdx` / `.mdss`：BKIO 容器、UTF-16/Base64 XML、INI 元数据、
  JPEG 瓦片、标签/宏观图像。三种扩展名共用同一种字节布局，
  因此由同一个读取器处理。
- `.sdpc` / `.dyqx`：JPEG 和 HEVC 压缩的 SDPC 文件，包括非 16 对齐的源瓦片
  （如 `616x880`）；相邻源瓦片会被合成为合法的 TIFF 输出瓦片，
  最后一行/列用白色填充。HEVC 由纯 Rust 的 `rusty_h265` 解码。
- `.ndpi`：Hamamatsu 容器。每一层是一条被重启区间切开的 baseline JPEG，
  一个重启区间正好是一个瓦片：读取时把 SOF 的高宽改写成区间几何，
  再接上该区间的熵编码数据与 EOI，因此瓦片无需重编码即可直通写出。
  每层有各自的瓦片尺寸（DRI 声明的 MCU 数不等于像素宽）。
- `.mrxs`：3DHISTECH Pannoramic。`.mrxs` 只是占位文件，真实内容在同名目录里：
  `Slidedat.ini` 描述层级与相机网格，`Index.dat` 的页链给出瓦片索引，
  `Data*.dat` 是原始 JPEG 字节流。瓦片按像素坐标精确落位。
- `.tif` / `.tiff`：瓦片式或扫描线式 TIFF 输入，TIFF 分辨率和物镜倍率
  元数据会带入金字塔 JPEG SVS。同时支持经典 TIFF 与 BigTIFF 头。

CLI 和输出布局有意与可用的 Python 版本保持一致：

```text
cargo run --release -- test_data/dmetrix/1.dmetrix -o test_output-rust/1.svs --overwrite
cargo run --release -- test_data/2605551-jpeg.sdpc -o test_output-rust/2605551.svs --overwrite
```

CLI 参数（两个变体共用，`--gui` / `--smoke-test` 仅 GUI 变体）：

| 参数 | 说明 |
| --- | --- |
| `<输入文件>` | 支持 `.csp/.dmetrix/.kfb/.mdss/.mdsx/.msdx/.mrxs/.ndpi/.tif/.tiff/.svs/.sdpc/.dyqx`；GUI 变体省略时启动界面 |
| `-o, --output <路径>` | 输出 `.svs` 路径，默认为输入路径改扩展名 |
| `--jpeg-quality <1-100>` | 输出 JPEG 质量；默认沿用源文件元数据 |
| `--overwrite` | 覆盖已存在的输出文件 |
| `--info` | 只解析并打印切片元数据，不转换 |
| `--gui` | 启动 GUI（仅 GUI 变体） |
| `--version` | 输出版本并带 `(gui)` / `(cli)` 变体后缀 |

Rust GUI/CLI 支持 JPEG/HEVC 的 SDPC/DYQX 以及上述所有格式。
两个变体都是自包含的可执行文件：HEVC 解码由 [`rusty_h265`]
(https://crates.io/crates/rusty_h265) 完成（纯 Rust，Apache-2.0，
自带运行时分派的 SSE2/SSE4.1/AVX2 内核，零运行时依赖），
因此没有任何需要随包分发的运行库，也不需要 `FFMPEG_HOME`
之类的环境变量。

输出是经典 TIFF，偏移为 32 位：切片大到输出超过 4 GiB 时会直接报错
（`TIFF exceeds classic 4 GiB offsets`），不会写出损坏的文件。

## 性能

原生 JPEG 和 HEVC 瓦片解码/编码工作运行在有界工作线程池中。
JPEG 转换默认使用操作系统报告的逻辑 CPU 数量（最多 64 个工作线程）。
HEVC 保留一个逻辑 CPU，且上限为 32 个工作线程，因为每个工作线程
独占一个解码器。纯 Rust 的 HEVC 解码比原先动态加载的 FFmpeg 解码器
慢约 1.2~1.3 倍（SDPC 样本实测 15.5s → 18.4s / 2.3s → 3.1s），
换来的是零运行库依赖。解码器实例在线程内复用，YUV→RGB 走查表
BT.601，这两项把纯 Rust 侧的代价从 1.8 倍压到 1.2~1.3 倍；
产物与 FFmpeg 版逐字节一致。
在启动 CLI 或 GUI 前设置 `IMG2SVS_THREADS`
可覆盖检测到的值，例如：

```powershell
$env:IMG2SVS_THREADS = '8'
.\target\release\img2svs-rust.exe input.csp -o output.svs --overwrite
```

JPEG 瓦片使用 Rust 编码器的 SIMD 路径。非 4:2:0 的 JPEG 瓦片在
4:2:0 编码前直接解码为 YCbCr，避免了不必要的 RGB 色彩转换。
源瓦片通过共享的只读内存映射读取；编码后的瓦片仅以有界批次缓冲，
并按源顺序写出，因此并行转换不会将整张切片加载进内存，
也不会改变 TIFF 瓦片偏移顺序。

源瓦片只要能原样放进输出瓦片（`.tif`、`.ndpi` 走这条路），就直接拷贝
字节流而不重编码；否则只对必须重排的瓦片做一次 JPEG 编码。

## 构建与冒烟测试

在正常的 Rust Windows 环境上：

```powershell
 cargo fmt --all -- --check
cargo build --release
 .\target\release\img2svs-rust.exe --smoke-test
```

Rust 版本没有任何原生运行库依赖，`build_windows.ps1` 只做格式检查、
构建和启动校验，产物可以直接分发，不需要附带任何目录。
`../third_party` 中保留的运行库只服务于 Python 版本。

`--smoke-test` 会初始化原生窗口并在第一帧后关闭；
适用于 CI 或打包检查，不会留下运行中的 GUI 进程。

## GUI 与控制台构建

GUI 位于默认的 `gui` cargo feature 之后，两个变体来自同一份源码：

```powershell
cargo build --release                        # GUI 构建，隐藏控制台
cargo build --release --no-default-features  # 控制台构建，无 egui/rfd
```

控制台构建去掉了 egui 和 rfd 依赖，并保留控制台子系统，
因此命令行输出保持可见，且不接受 `--gui` / `--smoke-test`。
体积约为 GUI 可执行文件的三分之一。`--version` 会报告
当前运行的变体，例如 `img2svs 0.1.0 (cli)`。

生成两个便携式 Windows 包（可执行文件与 README），
各带一个 ZIP 和一个 SHA256 校验和：

```powershell
pwsh -File ..\scripts\package_windows.ps1
```

输出位于 `../dist`：

| 包 | 可执行文件 | 内容 |
| --- | --- | --- |
| `PathologySVSConverter-rust-gui\` | `img2svs-rust.exe` | GUI 构建，双击即用，也可传入 CLI 参数 |
| `PathologySVSConverter-rust-cli\` | `img2svs-cli.exe` | 控制台构建，用于脚本和批处理任务 |

## GitHub Actions Windows 打包

[`build-rust-windows.yml`](../.github/workflows/build-rust-windows.yml)
工作流在 GitHub 的 Windows runner 上构建并测试 Rust 转换器。
它不下载任何外部运行库，用
`../scripts/make_smoke_tiff.py` 现场生成 TIFF 样本后执行
TIFF 转 SVS 冒烟测试（直接校验产物的 TIFF 标签），
并将便携 ZIP 及其 SHA256 校验和作为 Actions 产物上传。
该工作流会在相关 pull request 和 `main` 更新时运行，也可手动触发。

推送以 `v` 开头的标签时，还会额外创建或更新一个
包含相同 ZIP 和校验和的 GitHub Release。
