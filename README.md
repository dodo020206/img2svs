# img2svs

把病理全切片图像（WSI）转换为 Aperio SVS 格式（金字塔 JPEG TIFF）。
提供原生 Rust 实现（GUI + CLI 双变体），早期 Python 实现保留为参考基线。

## 支持的输入格式

所有格式都由本仓库自己解析，不存在 OpenSlide/libvips 依赖。

| 格式 | 备注 |
| --- | --- |
| `.dmetrix` | JPEG 瓦片、金字塔层级、标签/宏观图像 |
| `.csp` | 索引式 JPEG 瓦片、金字塔层级、标签/宏观图像 |
| `.kfb` | KFBio 索引式 JPEG 瓦片、稀疏瓦片布局 |
| `.mdsx` / `.msdx` / `.mdss` | BKIO 容器，三种扩展名共用同一字节布局 |
| `.sdpc` / `.dyqx` | JPEG / HEVC 压缩；HEVC 需要 FFmpeg 运行库 |
| `.ndpi` | Hamamatsu 容器：层级为「整条 baseline JPEG 切成的重启区间」，每个区间即一个瓦片 |
| `.mrxs` | 3DHISTECH Pannoramic：`Index.dat` 页链 + `Data*.dat` 原始 JPEG 字节流 |
| `.tif` / `.tiff` | 瓦片式或扫描线式 TIFF，保留分辨率与物镜倍率元数据 |

输入格式的唯一清单是 `img2svs-rust\src\loader.rs` 的 `SUPPORTED_FORMATS`
（12 个扩展名，`.svs` 不在其中）。

输出统一为金字塔 JPEG SVS（经典 TIFF，含 Aperio 描述页与缩略图页）。
没有 BigTIFF 输出路径：经典 TIFF 的偏移是 32 位，单个输出超过 4 GiB 时会直接报错，
而不是写出损坏的文件。

## 仓库结构

| 路径 | 说明 |
| --- | --- |
| `img2svs-rust\` | 原生 Rust 实现（GUI + CLI），当前主力，持续开发 |
| `img2svs-python\` | 早期 Python 实现，保留作为参考基线，不再更新 |
| `scripts\` | 共用脚本：运行库下载、Windows 打包、冒烟样本生成 |
| `third_party\` | 共用的原生运行库，不入版本库（只有 `README.md` 入库） |
| `.github\workflows\` | 只构建 Rust 的 Windows CI |
| `test_data\` | 切片样本，**不入版本库**（根 `.gitignore`），只在本机可用 |
| `dist\` | 打包产物，不入版本库 |

两套实现互不引用源码，只共用 `third_party\` 下的原生运行库：
`av.libs`（FFmpeg，Rust 与 Python 都可能用到）、`vips`（只有 Python 基线用得到）。

## 首次准备

克隆之后运行一次获取脚本：

```powershell
pwsh -File scripts\fetch_native_runtimes.ps1
```

`third_party\` 不入版本库。缺了它也能构建，只是这些能力不可用：

- 需要 FFmpeg：HEVC 压缩的 `.sdpc` / `.dyqx` 输入
- 需要 libvips：只有 `img2svs-python` 基线用得到，Rust 实现不需要

因此只做 Rust 开发时可以跳过 libvips 下载：

```powershell
pwsh -File scripts\fetch_native_runtimes.ps1 -SkipLibvips
```

## 构建与打包（Rust，Windows）

```console
cd img2svs-rust
.\build_windows.ps1                        # fmt 检查 + 构建，并把运行库复制到产物旁边
```

测试与两个变体的检查（与 CI 一致）：

```console
cargo test --locked
cargo check --locked --all-targets --no-default-features   # RUSTFLAGS=-D warnings
```

同一份源码产出两个变体（`gui` cargo feature，默认开启）：

```console
cargo build --release                        # GUI 版：隐藏控制台，双击即用
cargo build --release --no-default-features  # CLI 版：纯控制台
```

CLI 版可执行文件约 2.1 MB，GUI 版约 6.7 MB（约 1/3）。

**可执行文件命名**：Cargo 只按 crate 名产出 `img2svs-rust\target\release\img2svs-rust.exe`；
发布包在拷出时改名，两个名字各对应一个变体：

| 文件名 | 变体 | 用途 |
| --- | --- | --- |
| `img2svs-gui.exe` | GUI（`gui` feature，默认） | 双击即用；给了输入文件就走命令行，不弹窗 |
| `img2svs-cli.exe` | 控制台（`--no-default-features`） | 脚本与批处理；命令行输出可见，无 `--gui` / `--smoke-test` |

两者的 CLI 参数完全相同，可用 `--version` 确认变体：
`img2svs 0.1.0 (gui)` / `img2svs 0.1.0 (cli)`。

一键构建两个变体并打便携包（含 README、av.libs，各附 ZIP + SHA256）：

```powershell
pwsh -File scripts\package_windows.ps1
```

产物输出到 `dist\`：

| 包 | 可执行文件 | 用途 |
| --- | --- | --- |
| `PathologySVSConverter-rust-gui\` | `img2svs-gui.exe` | 双击即用，也接受 CLI 参数 |
| `PathologySVSConverter-rust-cli\` | `img2svs-cli.exe` | 脚本与批处理 |

两个包各带一份完整的 `av.libs`（约 63 MB）；少了它，HEVC 压缩的
`.sdpc` / `.dyqx` 就无法转换。

安装即解压：整个目录一起拷（不能只拿可执行文件），然后

- 图形界面：双击 `img2svs-gui.exe`
- 命令行：`img2svs-cli.exe D:\slides\input.csp -o D:\out\input.svs --overwrite`
  （`img2svs-gui.exe` 带输入文件时行为完全相同）

GUI 特性：多文件选择、递归文件夹扫描、拖放、输出目录选择、JPEG 质量、
覆盖选项、逐文件状态与日志、协作式取消；快捷键
`Ctrl+O` / `Ctrl+Shift+O` / `F5` / `Esc`。

详细用法见 [img2svs-rust\README.md](img2svs-rust/README.md)。

## 运行库解析顺序

构建时按以下顺序查找 FFmpeg，先命中先用：

1. 构建脚本参数：`-FfmpegHome`
2. 环境变量：`FFMPEG_HOME`
3. 仓库共享目录：`third_party\av.libs`

运行时（非构建时）由可执行文件自行发现，先命中先用：
`FFMPEG_HOME` → 可执行文件旁的 `ffmpeg\` → 可执行文件旁的 `av.libs\` → `PATH`。
不依赖开发机器路径。

## 性能要点

- JPEG/HEVC 瓦片解码编码在有界工作线程池中并行，默认按逻辑 CPU 数
  （JPEG 上限 64，HEVC 上限 32 且留一个 CPU），可用 `IMG2SVS_THREADS` 覆盖
- JPEG 编码走 SIMD 路径；非 4:2:0 瓦片直接解码到 YCbCr，省去 RGB 转换
- 源瓦片走共享只读内存映射，输出按源顺序写出，整图不进内存
- 无需重排、已是 4:2:0、且输出质量与源一致的瓦片直接拷贝字节流，不重编码

## Python 基线

`img2svs-python\` 不再更新，仅作对照。它仍然依赖 libvips，
打包 EXE（需 Python 3.11，见 `build_windows_exe.bat` 中的版本检查）：

```bat
cd img2svs-python
build_windows_exe.bat
```

说明见 [img2svs-python\README_GUI.md](img2svs-python/README_GUI.md)。

## CI 与发布

`.github\workflows\build-rust-windows.yml` 只构建 Rust（Windows runner）：
安装 stable Rust + Python 3.12（只用 Pillow 造样本），跑
`cargo fmt --all -- --check`、`cargo test --locked`、
`cargo check --locked --all-targets --no-default-features`（`RUSTFLAGS=-D warnings`）、
`cargo build --release --locked`；调用
`scripts\fetch_native_runtimes.ps1 -SkipLibvips` 只组装固定版本
`PYAV_VERSION=18.1.0` 的 FFmpeg 运行库，用 `scripts\make_smoke_tiff.py`
现场生成 TIFF 样本后执行 TIFF 转 SVS 冒烟测试（读回 + 直接解析 TIFF 标签，
不需要 libvips），产出**一个**便携 ZIP 与 SHA256 校验和并上传为 Actions 产物。
推送 `v*` 标签时额外创建 GitHub Release。

CI 只构建 GUI 变体；GUI/CLI 两个包是 `scripts\package_windows.ps1` 才做的。

## Rust 版本

- crate `img2svs-rust` 0.1.0，edition 2021，许可证 MIT
- 工具链：stable（CI 用 `dtolnay/rust-toolchain@stable`）
- MSRV：**未声明** —— `Cargo.toml` 无 `rust-version`，仓库无 `rust-toolchain.toml` → **待核实**
