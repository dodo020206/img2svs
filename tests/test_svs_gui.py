from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from img2svs.app.svs_gui import SvsConverterApp, partition_drop_paths
from img2svs.converters import csp_to_svs
from img2svs.converters.csp_to_svs import SvsWriter
from tkinterdnd2 import COPY


class GuiDropPathTests(unittest.TestCase):
    def test_csp_existing_output_is_skipped(self) -> None:
        with TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            output = root / "existing.svs"
            output.write_bytes(b"already converted")

            csp_to_svs.convert_one(
                input_path=root / "missing.csp",
                output_path=output,
                jpeg_quality=None,
                skip_associated=False,
                overwrite=False,
            )

            self.assertEqual(output.read_bytes(), b"already converted")

    def test_large_csp_enables_compatibility_jpeg_reencoding(self) -> None:
        metadata = SimpleNamespace(mpp=0.25, jpeg_quality=75)
        large_path = SimpleNamespace(
            stat=lambda: SimpleNamespace(st_size=2_000_000_000)
        )
        slide = SimpleNamespace(path=large_path, metadata=metadata, tile_size=256)

        self.assertTrue(SvsWriter(slide).reencode_jpeg_tiles)

    def test_partition_drop_paths_accepts_supported_files_and_directories(self) -> None:
        with TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            slide = root / "case.sdpc"
            slide.write_bytes(b"slide")
            folder = root / "batch"
            folder.mkdir()
            unsupported = root / "notes.txt"
            unsupported.write_text("notes", encoding="utf-8")

            accepted, ignored = partition_drop_paths(
                [slide, folder, unsupported, root / "missing.ndpi", slide]
            )

            self.assertEqual(accepted, (slide.resolve(), folder.resolve()))
            self.assertEqual(ignored, (unsupported.resolve(), root / "missing.ndpi"))

    def test_tkdnd_drop_event_adds_supported_slide(self) -> None:
        with TemporaryDirectory() as temp_dir:
            slide = Path(temp_dir) / "dragged case.ndpi"
            slide.write_bytes(b"slide")
            unsupported = Path(temp_dir) / "notes.txt"
            unsupported.write_text("notes", encoding="utf-8")
            app = SvsConverterApp()
            try:
                app.update()
                self.assertTrue(app.TkdndVersion)
                event = SimpleNamespace(
                    data=app.tk.call("list", str(slide.resolve()), str(unsupported.resolve()))
                )
                action = app.on_drop(event)

                self.assertEqual(action, COPY)
                self.assertIn(str(slide.resolve()), app.selected_paths)
                self.assertNotIn(str(unsupported.resolve()), app.selected_paths)
                self.assertIn("新增 1 项", app.status_var.get())
                self.assertIn("忽略不支持的文件 1 项", app.status_var.get())
            finally:
                app.destroy()

    def test_clicking_remove_cell_deletes_corresponding_item(self) -> None:
        with TemporaryDirectory() as temp_dir:
            first = Path(temp_dir) / "first.ndpi"
            second = Path(temp_dir) / "second.sdpc"
            first.write_bytes(b"first")
            second.write_bytes(b"second")
            app = SvsConverterApp()
            try:
                app.add_input_paths((first, second))
                app.update()
                item_id = str(first.resolve())
                x, y, width, height = app.source_tree.bbox(item_id, "remove")

                result = app.on_source_tree_click(
                    SimpleNamespace(x=x + width // 2, y=y + height // 2)
                )

                self.assertEqual(result, "break")
                self.assertNotIn(item_id, app.selected_paths)
                self.assertFalse(app.source_tree.exists(item_id))
                second_id = str(second.resolve())
                self.assertIn(second_id, app.selected_paths)

                x, y, width, height = app.source_tree.bbox(second_id, "path")
                result = app.on_source_tree_click(
                    SimpleNamespace(x=x + width // 2, y=y + height // 2)
                )

                self.assertIsNone(result)
                self.assertIn(second_id, app.selected_paths)
            finally:
                app.destroy()

    def test_input_dialog_does_not_follow_output_directory(self) -> None:
        with TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            output_dir = root / "output"
            input_dir = root / "inputs"
            output_dir.mkdir()
            input_dir.mkdir()
            slide = input_dir / "case.ndpi"
            slide.write_bytes(b"slide")
            app = SvsConverterApp()
            try:
                with (
                    patch(
                        "img2svs.app.svs_gui.filedialog.askdirectory",
                        side_effect=(str(output_dir), str(input_dir)),
                    ) as askdirectory,
                    patch(
                        "img2svs.app.svs_gui.filedialog.askopenfilenames",
                        return_value=(str(slide),),
                    ) as askopenfilenames,
                ):
                    app.choose_output_dir()
                    app.add_files()
                    app.add_folder()

                self.assertEqual(
                    askopenfilenames.call_args.kwargs["initialdir"],
                    str(Path.cwd()),
                )
                self.assertEqual(
                    askdirectory.call_args_list[0].kwargs["initialdir"],
                    str(Path.cwd()),
                )
                self.assertEqual(
                    askdirectory.call_args_list[1].kwargs["initialdir"],
                    str(input_dir.resolve()),
                )
                self.assertEqual(app.output_dir_var.get(), str(output_dir))
            finally:
                app.destroy()

    def test_log_has_internal_scrollbar_and_preserves_manual_position(self) -> None:
        app = SvsConverterApp()
        try:
            for index in range(40):
                app.append_log(f"日志 {index}")
            app.update_idletasks()

            self.assertIsNotNone(app.log_scrollbar)
            app.log_text.yview_moveto(0.0)
            app.update_idletasks()
            top_before = app.log_text.yview()[0]

            app.append_log("新到达的日志")
            app.update_idletasks()

            self.assertAlmostEqual(app.log_text.yview()[0], top_before, delta=0.05)
            app.log_text.yview_moveto(1.0)
            app.append_log("底部日志")
            app.update_idletasks()
            self.assertGreaterEqual(app.log_text.yview()[1], 0.95)
        finally:
            app.destroy()


if __name__ == "__main__":
    unittest.main()
