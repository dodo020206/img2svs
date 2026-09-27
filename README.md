# img2svs

把病理全切片图像（WSI）转换为 Aperio SVS 格式（金字塔 JPEG TIFF）。
提供原生 Rust 实现（GUI + CLI 双变体），早期 Python 实现保留为参考基线。

## 支持的输入格式

| 格式 | 后端 | 备注 |
| --- | --- | --- |
| `.dmetrix` | 原生 | JPEG 瓦片、金字塔层级、标签/宏观图像 |
| `.csp` | 原生 | 索引式 JPEG 瓦片 |
| `.kfb` | 原生 | KFBio 索引式 JPEG 瓦片、稀疏瓦片布局 |
| `.mdsx` / `.msdx` / `.mdss` | 原生 | BKIO 容器，三种扩展名共用同一字节布局 |
| `.sdpc` / `.dyqx` | 原生 | JPEG / HEVC 压缩；HEVC 需要 FFmpeg 运行库 |
| `.ndpi` / `.mrxs` | OpenSlide/libvips | 流式加载，不物化整图；需要 libvips 运行库 |
| `.tif` / `.tiff` | libvips | 瓦片式或扫描线式 TIFF，保留分辨率与物镜倍率元数据 |

输出统一为金字塔 JPEG SVS（尽可能经典 TIFF，必要时 BigTIFF），
含 Aperio 描述页与缩略图页。

## 仓库结构

| 路径 | 说明 |
| --- | --- |
| `img2svs-rust\` | 原生 Rust 实现（GUI + CLI），当前主力，持续开发 |
| `img2svs-python\` | 早期 Python 实现，保留作为参考基线，不再更新 |
| `scripts\` | 共用脚本：运行库下载、Windows 打包 |
| `third_party\` | 共用的原生运行库（vips、av.libs），不入版本库 |

两套实现互不引用源码，只共用 `third_party\` 下的原生运行库。

## 首次准备

克隆之后运行一次获取脚本，`libvips` 与 `FFmpeg`（PyAV）会被下载到 `third_party\`：

```powershell
pwsh -File scripts\fetch_native_runtimes.ps1
```

`third_party\` 不入版本库。缺了它也能构建，只是这些能力不可用：

- 需要 libvips：`.ndpi` / `.mrxs` / `.tif` 输入
- 需要 FFmpeg：HEVC 压缩的 `.sdpc` / `.dyqx` 输入

## 构建与打包（Rust，Windows）

```powershell
cd img2svs-rust
.\build_windows.ps1                        # 构建并把运行库复制到产物旁边
```

同一份源码产出两个变体（`gui` cargo feature，默认开启）：

```powershell
cargo build --release                        # GUI 版：隐藏控制台，双击即用
cargo build --release --no-default-features  # CLI 版：纯控制台，体积约为 GUI 的 1/3
```

`--version` 输出带 `(gui)` / `(cli)` 后缀以区分变体。

一键构建两个变体并打便携包（含 README、vips、av.libs，各附 ZIP + SHA256）：

```powershell
pwsh -File scripts\package_windows.ps1
```

产物输出到 `dist\`：

| 包 | 可执行文件 | 用途 |
| --- | --- | --- |
| `PathologySVSConverter-rust-gui\` | `img2svs-rust.exe` | 双击即用，也接受 CLI 参数 |
| `PathologySVSConverter-rust-cli\` | `img2svs-cli.exe` | 脚本与批处理 |

GUI 特性：多文件选择、递归文件夹扫描、拖放、JPEG 质量、逐文件状态与日志、
协作式取消；快捷键 `Ctrl+O` / `Ctrl+Shift+O` / `F5` / `Esc`。

详细用法见 [img2svs-rust\README.md](img2svs-rust/README.md)。

## 运行库解析顺序

构建时按以下顺序查找，先命中先用：

1. 构建脚本参数：`-VipsHome` / `-FfmpegHome`
2. 环境变量：`VIPS_HOME` / `FFMPEG_HOME`
3. 仓库共享目录：`third_party\vips`、`third_party\av.libs`
4. `%USERPROFILE%\vips`（仅 Rust 构建脚本）

运行时（非构建时）由可执行文件自行发现：设置 `VIPS_HOME` / `FFMPEG_HOME`，
或把 `vips` 与 `av.libs` 放在可执行文件旁边。不依赖开发机器路径。

## 性能要点

- JPEG/HEVC 瓦片解码编码在有界工作线程池中并行，默认按逻辑 CPU 数
  （HEVC 上限 32），可用 `IMG2SVS_THREADS` 覆盖
- JPEG 编码走 SIMD 路径；非 4:2:0 瓦片直接解码到 YCbCr，省去 RGB 转换
- 源瓦片走共享只读内存映射，输出按源顺序写出，整图不进内存

## Python 基线

`img2svs-python\` 不再更新，仅作对照。打包 EXE（需 Python 3.11）：

```bat
cd img2svs-python
build_windows_exe.bat
```

说明见 [img2svs-python\README_GUI.md](img2svs-python/README_GUI.md)。

## CI 与发布

`.github\workflows\build-rust-windows.yml` 只构建 Rust：调用同一个
`scripts\fetch_native_runtimes.ps1` 组装运行库，执行 GUI 与 TIFF 转 SVS
冒烟测试，产出便携 ZIP 与 SHA256 校验和并上传为 Actions 产物。
推送 `v*` 标签时额外创建 GitHub Release。
