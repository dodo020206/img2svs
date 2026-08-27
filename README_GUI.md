# 病理图像转 SVS 桌面工具

这个项目现在包含一个面向医生使用的桌面界面入口：`svs_gui.py`。

支持的输入格式：

- `CSP`
- `DMETRIX`（帝麦克斯）
- `SDPC`
- `DYQX`
- `KFB`
- `MDSX`
- `MSDX`（按 `MDSX` 逻辑处理）
- `MRXS`
- `NDPI`

输出格式：

- `SVS`

## 直接运行

建议先进入项目虚拟环境，然后运行：

```bash
.venv/bin/python svs_gui.py
```

如果是在 Windows 环境中：

```bat
.venv\Scripts\python.exe svs_gui.py
```

命令行批量转换使用统一入口：

```bat
.venv\Scripts\python.exe convert_to_svs.py 输入文件或目录
```

## 项目结构

代码按功能集中归档，根目录只保留日常使用入口和构建文件：

```text
img2svs\
  app\          GUI、统一命令行调度和任务执行服务
  converters\   各厂商格式转换器
  core\         公共 SVS 数据结构、参数和批处理能力
tests\           自动化测试
tools\           性能测试等开发工具
packaging\       PyInstaller 打包配置
vips\            随 EXE 分发的 libvips 运行库
```

根目录的 `svs_gui.py` 和 `convert_to_svs.py` 是兼容入口，实际功能代码均在 `img2svs` 包内。

## 界面使用方式

1. 点击“添加文件”或“添加目录”。
2. 如需统一输出位置，选择“输出目录”；留空则输出到源文件同目录。
3. 保持“自动识别（推荐）”即可，除非你只想处理单一格式。
4. 可在“SVS 保存质量”中选择输出质量。数值越低，SVS 文件通常越小；“原始/推荐”会沿用源图质量，`NDPI` 默认按 `90` 保存，`MRXS` 默认按 `70` 保存。
5. 点击“开始转换”。

## 打包为 Windows 可执行文件

项目的 Windows EXE 固定使用 `Python 3.11` 构建；`build_windows_exe.bat` 会检查版本，不接受其他 Python 主次版本。

先在 Windows 环境准备依赖：

```bat
.venv\Scripts\python.exe -m pip install -r requirements-build.txt
```

如果需要支持 `NDPI/MRXS`，建议提前安装 `libvips`，并设置环境变量 `VIPS_HOME` 指向其目录，例如：

```bat
set VIPS_HOME=C:\vips
```

`build_windows_exe.bat` 也会自动尝试查找这些目录：项目下的 `vips`、`third_party\vips`、`C:\vips`、`C:\Program Files\vips*`。

如果安装了 `UPX`，也可以额外设置：

```bat
set UPX_DIR=C:\upx
```

默认会打包为目录版：

```bat
build_windows_exe.bat
```

也可以显式指定模式：

```bat
build_windows_exe.bat onedir
build_windows_exe.bat onefile
```

`onedir` 启动通常更快，也更适合携带 `libvips`、`imagecodecs` 这类运行库；`onefile` 分发更方便，但启动通常更慢。

`onedir` 打包成功后，主程序位置为：

```text
dist\PathologySVSConverter\PathologySVSConverter.exe
```

`onefile` 打包成功后，输出位置为：

```text
dist\PathologySVSConverter.exe
```

如果是 `onedir`，分发给医生时，建议整个 `dist\PathologySVSConverter` 文件夹一起拷贝，不要只拷出其中的 `exe`。

## 说明

- GUI 和命令行入口统一调度 `img2svs\converters` 中的各格式转换器。
- GUI 会把每个文件交给独立 worker 进程处理，避免大切片解析占用界面线程；DMetrix 索引使用紧凑数组保存，降低内存峰值。
- `NDPI/MRXS` 当前通过 `pyvips + libvips` 读取 OpenSlide 暴露的切片内容并生成金字塔 TIFF，以 `.svs` 扩展名输出。
- `CSP/DMETRIX/KFB/MDSX/MSDX/MRXS/SDPC/DYQX/NDPI` 现在都支持指定输出 JPEG 质量，能够直接影响生成的 `SVS` 体积。帝麦克斯文件保持“原始/推荐”时会直通复制 JPEG 瓦片，转换更快且避免重复压缩。
- “停止队列”会在当前文件完成后停止剩余任务，不会强制中断正在写入的文件。

开发验证可运行：

```bat
.venv-package\Scripts\python.exe -m unittest discover -s tests -v
```
