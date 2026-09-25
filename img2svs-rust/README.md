# img2svs Rust

这里是 `../img2svs-python` 中那个转换器的原生 Rust 实现（GUI + CLI）。
仓库结构、共用的原生运行库与构建入口见 [`../README.md`](../README.md)。

## 界面（GUI）

直接运行可执行文件即可，也可以加上 `--gui`：

```powershell
.\target\release\img2svs-rust.exe
.\target\release\img2svs-rust.exe --gui
```

界面支持多选文件、递归扫描目录、Windows 拖拽、输出目录选择、JPEG 质量、
覆盖与关联图像选项，并提供后台队列 worker、进度、单文件状态、日志与协作式
取消。每行都有独立的「移除」按钮。快捷键：`Ctrl+O`（添加文件）、
`Ctrl+Shift+O`（添加目录）、`F5`（开始）、`Esc`（停止）。

原生后端支持以下格式：

- `.dmetrix`：JPEG 瓦片、金字塔层级、标签与缩略图图像。
- `.csp`：索引式 JPEG 瓦片、金字塔层级、标签与缩略图图像。
- `.kfb`：KFBio 索引式 JPEG 瓦片、稀疏瓦片排布、金字塔层级、标签与缩略图图像。
- `.mdsx` / `.msdx` / `.mdss`：BKIO 容器、UTF-16/Base64 XML、INI 元数据、
  JPEG 瓦片与标签/缩略图图像。三种扩展名的字节布局一致，由同一个读取器处理。
- `.sdpc` / `.dyqx`：JPEG 与 HEVC 压缩的 SDPC 文件，包含源瓦片非 16 对齐的
  情况（如 `616x880`）；相邻源瓦片会拼接成合法的 TIFF 输出瓦片，最后一行/
  列用白色补齐。有随附的 FFmpeg 运行库时，HEVC 由它解码。
- `.ndpi` / `.mrxs`：由 OpenSlide/libvips 流式加载，输出金字塔 JPEG 的 SVS
  （能写经典 TIFF 就用经典 TIFF，仅在必要时用 BigTIFF），并包含 Aperio
  description 与缩略图页面；Rust 适配层不会把整张切片展开成未压缩的中间图像。
- `.tif` / `.tiff`：通过 libvips 读取分块或逐行 TIFF，并把 TIFF 的分辨率与
  物镜倍数等元数据带入金字塔 JPEG SVS。

CLI 与输出布局刻意与可用的 Python 版本保持一致：

```text
cargo run --release -- test_data/dmetrix/1.dmetrix -o test_output-rust/1.svs --overwrite
cargo run --release -- test_data/2605551-jpeg.sdpc -o test_output-rust/2605551.svs --overwrite
```

JPEG/HEVC 的 SDPC/DYQX 以及上面列出的全部格式都由 Rust GUI/CLI 支持。
HEVC 需要 FFmpeg 运行库：设置 `FFMPEG_HOME`，或把随附的 `av.libs` 目录放在
可执行文件旁边。NDPI/MRXS 与 TIFF 输入需要 OpenSlide/libvips 运行库：设置
`VIPS_HOME`，或把运行库放在可执行文件旁的 `vips\bin`。
运行库查找基于可执行文件的相对位置与环境变量，不依赖开发机上的路径。
NDPI/MRXS 转换沿用 libvips 的硬件感知并发默认值；如需覆盖，可在对目标机器
实测之后使用 `VIPS_CONCURRENCY`。

## 性能

原生 JPEG 与 HEVC 瓦片的编解码运行在有界 worker 池中。JPEG 转换默认使用
操作系统报告的逻辑 CPU 数（最多 64 个 worker）。HEVC 会留出一个逻辑 CPU，
并因每个 worker 独占一个解码器而限制在 32 个 worker。启动 CLI 或 GUI 前设置
`IMG2SVS_THREADS` 可以覆盖自动检测的值，例如：

```powershell
$env:IMG2SVS_THREADS = '8'
.\target\release\img2svs-rust.exe input.csp -o output.svs --overwrite
```

JPEG 瓦片走 Rust 编码器的 SIMD 路径。非 4:2:0 的 JPEG 瓦片会先直接解码为
YCbCr 再做 4:2:0 编码，省掉一次多余的 RGB 色彩转换。源瓦片通过共享只读内存
映射读取；编码后的瓦片按有界批次缓冲并按源顺序写出，因此并行转换既不会把
完整切片读入内存，也不会改变 TIFF 的瓦片偏移顺序。

## 构建与冒烟测试

在正常的 Rust Windows 环境下：

```powershell
cargo fmt --all -- --check
cargo build --release
.\target\release\img2svs-rust.exe --smoke-test
```

仓库只在 `../third_party` 保留一份原生运行库，在仓库根目录运行一次即可填充：

```powershell
pwsh -File ..\scripts\fetch_native_runtimes.ps1
```

`build_windows.ps1` 随后会把 `vips` 与 `av.libs` 复制到可执行文件旁边。它按
`-VipsHome` / `-FfmpegHome`、`VIPS_HOME` / `FFMPEG_HOME`、`../third_party`
的顺序解析运行库，缺任何一个都会给出警告而不是静默跳过。分发时要拷贝完整的
release 目录（含 `vips` 与 `av.libs`），只拷贝可执行文件是不够的。

`--smoke-test` 会初始化原生窗口，并在第一帧之后关闭；适合用于 CI 或打包检查，
不会残留 GUI 进程。

## GitHub Actions Windows 打包

[`build-rust-windows.yml`](../.github/workflows/build-rust-windows.yml) 工作流
在 GitHub 的 Windows runner 上构建并测试 Rust 转换器：下载固定版本的 libvips
与 PyAV 运行库，执行 GUI 与 TIFF 转 SVS 两项冒烟测试，并把可移植 ZIP 及其
SHA256 校验和作为 Actions artifact 上传。该工作流会在相关 PR 与 `main` 更新时
运行，也支持手动触发。

推送以 `v` 开头的标签时，会额外创建或更新一个 GitHub Release，附件是同一个
ZIP 与校验和。
