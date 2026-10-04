"""Tests for the synthetic library generator."""
import importlib.util
import json
import struct
import tempfile
import unittest
import zlib
from collections import Counter
from pathlib import Path

MODULE_PATH = Path(__file__).with_name("gen-library.py")
SPEC = importlib.util.spec_from_file_location("gen_library", MODULE_PATH)
GEN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GEN)


class GeneratorTests(unittest.TestCase):
    def test_deterministic_tree_and_expected_manifest(self):
        with tempfile.TemporaryDirectory() as temp:
            base = Path(temp)
            a, b = base / "a", base / "b"
            summary_a = GEN.generate(a, 24, 17, 0.5, 3, 2, 0.25, True, True)
            summary_b = GEN.generate(b, 24, 17, 0.5, 3, 2, 0.25, True, True)
            self.assertEqual(summary_a["images"], 24)
            self.assertEqual(summary_a["notes"], summary_b["notes"])
            files_a = sorted(p.relative_to(a).as_posix() for p in a.rglob("*") if p.is_file() and p.name != ".expected.json")
            files_b = sorted(p.relative_to(b).as_posix() for p in b.rglob("*") if p.is_file() and p.name != ".expected.json")
            self.assertEqual(files_a, files_b)
            for relative in files_a:
                self.assertEqual((a / relative).read_bytes(), (b / relative).read_bytes(), relative)
            self.assertEqual((a / ".expected.json").read_bytes(), (b / ".expected.json").read_bytes())

    def test_counts_and_bad_cases_manifest(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "library"
            result = GEN.generate(root, 15, 5, 1.0, 2, 1, 0.2, True, True)
            manifest = json.loads((root / ".expected.json").read_text(encoding="utf-8"))
            self.assertEqual(result["images"], 15)
            self.assertGreaterEqual(len(list(root.rglob("*.md"))), 17)
            image_names = Counter(p.name for p in root.rglob("*.png") if p.name.startswith("shared-name"))
            self.assertTrue(any(number > 1 for number in image_names.values()))
            kinds = {item["kind"] for item in manifest["problems"]}
            expected = {"extension_mismatch", "truncated_image", "invalid_yaml", "duplicate_id",
                        "missing_link", "ambiguous_link", "special_characters_name", "ignored_file",
                        "symlink", "unicode_nfc", "unicode_nfd"}
            self.assertTrue(expected <= kinds)
            self.assertEqual(sum(item["kind"] == "duplicate_id" for item in manifest["problems"]), 2)
            self.assertEqual(sum(item["kind"].startswith("unicode_") for item in manifest["problems"]), 2)
            for item in manifest["problems"]:
                self.assertTrue((root / item["path"]).exists() or (root / item["path"]).is_symlink())

    def test_png_and_gif_headers(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            png_path = root / "x.png"
            gif_path = root / "x.gif"
            GEN.write(png_path, GEN.png(12, 1, (1, 2, 3, 255)))
            GEN.write(gif_path, GEN.gif(1, 12))
            data = png_path.read_bytes()
            self.assertEqual(data[:8], GEN.PNG_SIG)
            self.assertEqual(struct.unpack(">II", data[16:24]), (12, 1))
            self.assertEqual(zlib.decompress(data[41:-16]), b"\0" + bytes((1, 2, 3, 255)) * 12)
            gif_data = gif_path.read_bytes()
            self.assertEqual(gif_data[:6], b"GIF89a")
            self.assertEqual(struct.unpack("<HH", gif_data[6:10]), (1, 12))
            self.assertTrue(gif_data.endswith(b"\x3b"))


if __name__ == "__main__":
    unittest.main()
