# img2svs

把病理全切片图像转换为 Aperio SVS 格式。

## 仓库结构

| 路径 | 说明 |
| --- | --- |
| `img2svs-rust\` | 原生 Rust 实现（GUI + CLI），当前主力，持续开发 |
| `img2svs-python\` | 早期 Python 实现，保留作为参考基线，不再更新 |
| `scripts\` | 两个项目共用的构建辅助脚本 |
| `third_party\` | 两个项目共用的原生运行库，不入版本库 |

两套实现互不引用源码，只共用 `third_party\` 下的原生运行库。

## 首次准备

克隆之后运行一次获取脚本，`libvips` 与 `FFmpeg` 会被下载到 `third_party\`：

```powershell
pwsh -File scripts\fetch_native_runtimes.ps1
```

`third_party\` 的内容不入版本库。缺了它也能构建，只是这些能力不可用：

- 需要 libvips：`.ndpi` / `.mrxs` / `.tif` 输入；
- 需要 FFmpeg：HEVC 压缩的 `.sdpc` / `.dyqx` 输入。

细节见 [`third_party\README.md`](third_party/README.md)。

## 构建

Rust（Windows）：

```powershell
cd img2svs-rust
.\build_windows.ps1
```

Python 打包 EXE（Windows，需要 Python 3.11）：

```bat
cd img2svs-python
build_windows_exe.bat
```

各自的详细用法见 [img2svs-rust\README.md](img2svs-rust/README.md) 与
[img2svs-python\README_GUI.md](img2svs-python/README_GUI.md)。

## 运行库解析顺序

两个项目都不硬编码对方目录，按以下顺序查找，先命中先用：

1. 构建脚本参数：`-VipsHome` / `-FfmpegHome`；
2. 环境变量：`VIPS_HOME` / `FFMPEG_HOME`；
3. 仓库共享目录：`third_party\vips`、`third_party\av.libs`；
4. `%USERPROFILE%\vips`（仅 Rust 构建脚本）。

运行时（非构建时）由可执行文件自行发现：设置 `VIPS_HOME` / `FFMPEG_HOME`，
或把 `vips` 与 `av.libs` 放在可执行文件旁边。

## CI

`.github\workflows\build-rust-windows.yml` 只构建 Rust。它调用同一个
`scripts\fetch_native_runtimes.ps1` 组装运行库，产出可移植 ZIP 与 SHA256 校验和；
推送 `v*` 标签时额外创建 GitHub Release。
