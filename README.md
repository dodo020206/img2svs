# img2svs

把病理全切片图像（WSI）转换为 Aperio SVS 格式（金字塔 JPEG TIFF）。
提供原生 Rust 实现（GUI + CLI 双变体），早期 Python 实现保留为参考基线。

## 支持的输入格式

所有格式都由本仓库自己解析，既不依赖 OpenSlide/libvips，也不依赖 FFmpeg。

| 格式 | 备注 |
| --- | --- |
| `.dmetrix` | JPEG 瓦片、金字塔层级、标签/宏观图像 |
| `.csp` | 索引式 JPEG 瓦片 |
| `.kfb` | KFBio 索引式 JPEG 瓦片、稀疏瓦片布局 |
| `.mdsx` / `.msdx` / `.mdss` | BKIO 容器，三种扩展名共用同一字节布局 |
| `.sdpc` / `.dyqx` | JPEG / HEVC 压缩；HEVC 由纯 Rust 的 `rusty_h265` 解码 |
| `.ndpi` | Hamamatsu 容器：层级为「整条 baseline JPEG 切成的重启区间」，每个区间即一个瓦片 |
| `.mrxs` | 3DHISTECH Pannoramic：`Index.dat` 页链 + `Data*.dat` 原始 JPEG 字节流 |
| `.tif` / `.tiff` | 瓦片式或扫描线式 TIFF，保留分辨率与物镜倍率元数据 |

输出统一为金字塔 JPEG SVS（经典 TIFF，含 Aperio 描述页与缩略图页）。
经典 TIFF 的偏移是 32 位，因此单个输出超过 4 GiB 时会直接报错，
而不是写出损坏的文件。

## 仓库结构

| 路径 | 说明 |
| --- | --- |
| `img2svs-rust\` | 原生 Rust 实现（GUI + CLI），当前主力，持续开发 |
| `img2svs-python\` | 早期 Python 实现，保留作为参考基线，不再更新 |
| `scripts\` | 共用脚本：运行库下载、Windows 打包、冒烟样本生成 |
| `third_party\` | `img2svs-python` 基线的原生运行库（libvips / PyAV），不入版本库 |

两套实现互不引用源码。Rust 实现没有任何原生运行库依赖，
`third_party\` 只服务于 `img2svs-python` 基线。

## 首次准备

只做 Rust 开发时无需任何准备：不依赖外部运行库，克隆后直接构建。

要跑 `img2svs-python` 基线，则先获取它用的运行库：

```powershell
pwsh -File scripts\fetch_native_runtimes.ps1
```

`third_party\` 不入版本库，只有 `img2svs-python` 会用到它。

## 构建与打包（Rust，Windows）

```powershell
cd img2svs-rust
.\build_windows.ps1                        # 格式检查 + 构建
```

同一份源码产出两个变体（`gui` cargo feature，默认开启）：

```powershell
cargo build --release                        # GUI 版：隐藏控制台，双击即用
cargo build --release --no-default-features  # CLI 版：纯控制台，体积约为 GUI 的 1/3
```

`--version` 输出带 `(gui)` / `(cli)` 后缀以区分变体。

一键构建两个变体并打便携包（仅可执行文件与 README，各附 ZIP + SHA256）：

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

## 运行库依赖

Rust 实现不依赖任何外部运行库：全部输入格式，包括 HEVC 压缩的
`.sdpc` / `.dyqx`，都由仓库内的代码解码。便携包只有可执行文件和
README，不需要附带目录，也不需要设置任何环境变量。

代价在 HEVC 解码速度上：纯 Rust 解码器比原先动态加载的 FFmpeg
慢约 1.2~1.3 倍（SDPC 样本 15.5s → 18.4s、2.3s → 3.1s）。
非 HEVC 格式不受影响，产物与之前逐字节一致。

## 性能要点

- JPEG/HEVC 瓦片解码编码在有界工作线程池中并行，默认按逻辑 CPU 数
  （HEVC 上限 32），可用 `IMG2SVS_THREADS` 覆盖
- JPEG 编码走 SIMD 路径；非 4:2:0 瓦片直接解码到 YCbCr，省去 RGB 转换
- 源瓦片走共享只读内存映射，输出按源顺序写出，整图不进内存

## Python 基线

`img2svs-python\` 不再更新，仅作对照。它仍然依赖 libvips，
打包 EXE（需 Python 3.11）：

```bat
cd img2svs-python
build_windows_exe.bat
```

说明见 [img2svs-python\README_GUI.md](img2svs-python/README_GUI.md)。

## CI 与发布

`.github\workflows\build-rust-windows.yml` 只构建 Rust：不下载任何运行库，
用 `scripts\make_smoke_tiff.py` 现场生成 TIFF 样本后执行
TIFF 转 SVS 冒烟测试，产出便携 ZIP 与 SHA256 校验和并上传为 Actions 产物。
推送 `v*` 标签时额外创建 GitHub Release。
