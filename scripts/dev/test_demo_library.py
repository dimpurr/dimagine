"""Unit tests for scripts/dev/demo-library.py."""

import importlib.util
import json
import os
import struct
import subprocess
import tempfile
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).with_name("demo-library.py")
SPEC = importlib.util.spec_from_file_location("demo_library", MODULE_PATH)
DEMO = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DEMO)


class DemoLibraryTests(unittest.TestCase):
    def test_deterministic_generation(self):
        with tempfile.TemporaryDirectory(prefix="dimagine-test-det-") as temp_dir:
            base = Path(temp_dir)
            dir_a = base / "a"
            dir_b = base / "b"

            summary_a = DEMO.generate(dir_a, total_images=30, seed=123)
            summary_b = DEMO.generate(dir_b, total_images=30, seed=123)

            self.assertEqual(summary_a["total_images"], 30)
            self.assertEqual(summary_a["total_images"], summary_b["total_images"])

            files_a = sorted(p.relative_to(dir_a).as_posix() for p in dir_a.rglob("*") if p.is_file())
            files_b = sorted(p.relative_to(dir_b).as_posix() for p in dir_b.rglob("*") if p.is_file())
            self.assertEqual(files_a, files_b)

            for rel_path in files_a:
                self.assertEqual((dir_a / rel_path).read_bytes(), (dir_b / rel_path).read_bytes(), rel_path)

    def test_library_structure_and_rules(self):
        with tempfile.TemporaryDirectory(prefix="dimagine-test-rules-") as temp_dir:
            root = Path(temp_dir) / "demo"
            summary = DEMO.generate(root, total_images=120, seed=42)

            self.assertEqual(summary["total_images"], 120)
            self.assertEqual(summary["explicit_collections"], 1)
            self.assertEqual(summary["embedded_plain_notes"], 1)
            self.assertEqual(summary["non_image_files"], 3)

            # Check folders
            self.assertTrue((root / "refs").is_dir())
            self.assertTrue((root / "refs2").is_dir())
            self.assertTrue((root / "Architecture & Design").is_dir())
            self.assertTrue((root / "Café & Life ☕").is_dir())
            self.assertTrue((root / "Projects [2026]").is_dir())
            self.assertTrue((root / "refs/studies [wip]").is_dir())

            # Check non-image files
            self.assertTrue((root / "docs/styleguide.pdf").is_file())
            self.assertTrue((root / "notes/palette.txt").is_file())
            self.assertTrue((root / "README.txt").is_file())

            # Check image headers and aspect ratio variety
            image_files = [
                p for p in root.rglob("*")
                if p.is_file() and p.suffix.lower() in {".png", ".jpg", ".jpeg", ".webp"} and not p.name.endswith(".md")
            ]
            self.assertEqual(len(image_files), 120)

            has_portrait = False
            has_panoramic = False
            for img in image_files:
                data = img.read_bytes()
                self.assertTrue(
                    data.startswith(b"\x89PNG\r\n\x1a\n") or data.startswith(b"\xff\xd8") or (data.startswith(b"RIFF") and data[8:12] == b"WEBP"),
                    f"Invalid magic bytes in {img}",
                )
                w, h = 0, 0
                if data.startswith(b"\x89PNG\r\n\x1a\n") and len(data) >= 24:
                    w, h = struct.unpack(">II", data[16:24])
                elif data.startswith(b"\xff\xd8"):
                    idx = 2
                    while idx < len(data) - 9:
                        if data[idx] == 0xFF and data[idx + 1] in (0xC0, 0xC1, 0xC2):
                            h, w = struct.unpack(">HH", data[idx + 5:idx + 9])
                            break
                        idx += 2 + struct.unpack(">H", data[idx + 2:idx + 4])[0]
                elif data.startswith(b"RIFF") and len(data) >= 30 and data[12:16] == b"VP8 ":
                    w, h = struct.unpack("<HH", data[26:30])
                    w, h = w & 0x3FFF, h & 0x3FFF

                if w > 0 and h > 0:
                    if h > w:
                        has_portrait = True
                    if w >= 2 * h:
                        has_panoramic = True

            self.assertTrue(has_portrait, "No portrait image found")
            self.assertTrue(has_panoramic, "No panoramic image found")

            # Check notes
            notes = list(root.rglob("*.md"))
            image_notes = [n for n in notes if (n.parent / n.stem).is_file()]
            self.assertLess(len(image_notes), 120, "Some notes must be missing")
            self.assertGreater(summary["notes_only_imported"], 0, "Some notes must have only imported")
            self.assertGreater(summary["notes_with_added"], 0, "Some notes must have added")

            # Self-embed check
            for note in image_notes:
                content = note.read_text(encoding="utf-8")
                self.assertIn("![[", content)

    def test_selftest_passes(self):
        with tempfile.TemporaryDirectory(prefix="dimagine-test-selftest-") as temp_dir:
            self.assertTrue(DEMO.selftest(Path(temp_dir)))


if __name__ == "__main__":
    unittest.main()
