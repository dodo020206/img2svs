# third_party

`img2svs-rust` 与 `img2svs-python` 共用的原生运行库目录。

这里的运行库**不入版本库**（见仓库根目录 `.gitignore`），克隆之后需要运行一次获取脚本：

```powershell
pwsh -File scripts\fetch_native_runtimes.ps1
```

## 目录内容

| 目录 | 内容 | 用途 | 来源 |
| --- | --- | --- | --- |
| `vips\` | libvips 8.18.1（含 OpenSlide 模块） | 读取 `.ndpi` / `.mrxs` / `.tif` | `libvips/build-win64-mxe` 的 `vips-dev-w64-all` 包 |
| `av.libs\` | FFmpeg 运行库 | 解码 HEVC 压缩的 `.sdpc` / `.dyqx` | PyAV 18.1.0 wheel 内的 `av.libs` |

## 使用方

- `img2svs-rust\build_windows.ps1`：构建后把两者复制到 `target\release\` 下，随可执行文件分发。
- `img2svs-python\build_windows_exe.bat`：把 `third_party\vips` 作为 `VIPS_HOME` 候选，打进 PyInstaller 产物。
- `.github\workflows\build-rust-windows.yml`：调用同一个获取脚本组装发布用的 ZIP。

## 覆盖位置

不想放在这里时，可以用以下任一方式指定，优先级高于本目录：

- `VIPS_HOME` / `FFMPEG_HOME` 环境变量；
- `fetch_native_runtimes.ps1 -Destination <目录>`；
- `build_windows.ps1 -VipsHome <目录> -FfmpegHome <目录>`。
