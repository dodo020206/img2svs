# -*- mode: python ; coding: utf-8 -*-
from __future__ import annotations

import os
from pathlib import Path

from PyInstaller.utils.hooks import collect_data_files, collect_dynamic_libs, collect_submodules

spec_dir = Path(globals().get("SPECPATH", Path.cwd())).resolve()
project_dir = spec_dir.parent
package_mode = os.environ.get("SVS_PACKAGE_MODE", "onedir").strip().lower()

if package_mode not in {"onedir", "onefile"}:
    raise ValueError(f"Unsupported SVS_PACKAGE_MODE: {package_mode!r}")

hiddenimports = (
    [
        "PIL.Image",
        "PIL.ImageFile",
        "PIL.JpegImagePlugin",
        "PIL.BmpImagePlugin",
        "PIL.PngImagePlugin",
        "PIL.TiffImagePlugin",
        "pyvips",
        "av",
        "tkinter",
        "tkinter.ttk",
        "tkinter.filedialog",
        "tkinter.messagebox",
        "tkinter.font",
        "img2svs.app.svs_worker",
    ]
    + collect_submodules("imagecodecs")
)

datas = (
    collect_data_files("PIL")
    + collect_data_files("pyvips")
    + collect_data_files("tifffile")
    + [
        (str(project_dir / "assets" / "app_icon.png"), "assets"),
        (str(project_dir / "assets" / "app_icon.ico"), "assets"),
    ]
)

binaries = (
    collect_dynamic_libs("imagecodecs")
    + collect_dynamic_libs("av")
)

vips_candidates = []
for raw in (
    os.environ.get("VIPS_HOME"),
    project_dir / "vips",
    project_dir / "third_party" / "vips",
):
    if not raw:
        continue
    root = Path(raw).expanduser().resolve()
    if root.exists() and root not in vips_candidates:
        vips_candidates.append(root)

for vips_root in vips_candidates:
    for file_path in vips_root.rglob("*"):
        if file_path.is_file():
            relative_parent = file_path.parent.relative_to(vips_root)
            target_dir = "." if str(relative_parent) == "." else str(relative_parent)
            binaries.append((str(file_path), target_dir))

a = Analysis(
    [str(project_dir / "svs_gui.py")],
    pathex=[str(project_dir)],
    binaries=binaries,
    datas=datas,
    hiddenimports=hiddenimports,
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[
        "IPython",
        "PyQt5",
        "PyQt6",
        "PySide2",
        "PySide6",
        "matplotlib",
        "notebook",
        "numba",
        "pandas",
        "pytest",
        "scipy",
        "skimage",
        "sklearn",
        "torch",
    ],
    noarchive=False,
    optimize=1,
)

# pyvips resolves libvips through ctypes and PyInstaller may add a duplicate
# root-level DLL automatically. Keep the bundled vips/bin copy as the only
# libvips runtime so its OpenSlide module directory is used consistently.
a.binaries = [
    entry
    for entry in a.binaries
    if Path(str(entry[0])).name.lower()
    not in {"libvips-42.dll", "libvips-cpp-42.dll"}
    or str(entry[0]).replace("\\", "/").startswith("bin/")
]
pyz = PYZ(a.pure)

exe_options = dict(
    name="PathologySVSConverter",
    icon=str(project_dir / "assets" / "app_icon.ico"),
    version=str(project_dir / "packaging" / "windows_version_info.txt"),
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=True,
    upx_exclude=[],
    runtime_tmpdir=None,
    console=False,
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
)

if package_mode == "onefile":
    exe = EXE(
        pyz,
        a.scripts,
        a.binaries,
        a.datas,
        [],
        **exe_options,
    )
else:
    exe = EXE(
        pyz,
        a.scripts,
        [],
        exclude_binaries=True,
        **exe_options,
    )

    coll = COLLECT(
        exe,
        a.binaries,
        a.datas,
        strip=False,
        upx=True,
        upx_exclude=[],
        name="PathologySVSConverter",
    )
