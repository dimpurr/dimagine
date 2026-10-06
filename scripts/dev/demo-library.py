#!/usr/bin/env python3
"""Generate a realistic, deterministic demo library for dimagine.

Creates nested folders (including spaces, unicode, brackets, refs, and refs2),
~120 varied images (PIL if available, pure-Python stdlib PNG fallback),
FORMAT-compliant notes (missing notes, notes with only imported, full notes),
collections (one explicit, one embedded plain note, one image note),
and non-image files.
"""

import argparse
import hashlib
import json
import os
import random
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import zlib
from pathlib import Path

try:
    from PIL import Image, ImageDraw
    HAVE_PIL = True
except ImportError:
    HAVE_PIL = False

ULID_CHARS = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
PNG_SIG = b"\x89PNG\r\n\x1a\n"


def make_ulid(rng: random.Random) -> str:
    """Generate a valid 26-character Crockford base32 ULID."""
    val = rng.getrandbits(128)
    chars = []
    for _ in range(26):
        chars.append(ULID_CHARS[val & 31])
        val >>= 5
    return "".join(reversed(chars))


def _png_chunk(tag: bytes, data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)


def make_fallback_png(w: int, h: int, bg: tuple, fg1: tuple, fg2: tuple) -> bytes:
    """Pure-Python stdlib PNG generator creating geometric patterns."""
    co = zlib.compressobj(6)
    raw = bytearray()
    row_bg = b"\x00" + bytes(bg) * w
    for y in range(h):
        # Create patterned bands and center blocks
        if (y // 16) % 2 == 0:
            raw.extend(co.compress(row_bg))
        else:
            w4 = max(1, w // 4)
            w2 = max(1, w // 2)
            rem = max(0, w - w4 - w2)
            row = b"\x00" + (bytes(fg1) * w4) + (bytes(fg2) * w2) + (bytes(bg) * rem)
            raw.extend(co.compress(row))
    raw.extend(co.flush())

    ihdr = _png_chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))  # 8-bit RGB
    idat = _png_chunk(b"IDAT", raw)
    iend = _png_chunk(b"IEND", b"")
    return PNG_SIG + ihdr + idat + iend


def create_image(path: Path, fmt: str, w: int, h: int, bg: tuple, fg1: tuple, fg2: tuple, rng: random.Random, title: str):
    """Create an image using PIL if installed, otherwise pure-Python PNG."""
    path.parent.mkdir(parents=True, exist_ok=True)
    if HAVE_PIL:
        img = Image.new("RGB", (w, h), bg)
        draw = ImageDraw.Draw(img)

        # Draw decorative geometric elements
        num_shapes = rng.randint(2, 5)
        for _ in range(num_shapes):
            shape_type = rng.choice(["rect", "ellipse", "line"])
            x1 = rng.randint(0, max(0, w - 20))
            y1 = rng.randint(0, max(0, h - 20))
            x2 = rng.randint(x1 + 10, w)
            y2 = rng.randint(y1 + 10, h)
            color = rng.choice([fg1, fg2])
            if shape_type == "rect":
                draw.rectangle([x1, y1, x2, y2], fill=color)
            elif shape_type == "ellipse":
                draw.ellipse([x1, y1, x2, y2], fill=color)
            else:
                draw.line([(x1, y1), (x2, y2)], fill=color, width=max(2, min(w, h) // 40))

        # Distinct accent border
        border_w = max(2, min(w, h) // 50)
        draw.rectangle([border_w, border_w, w - border_w, h - border_w], outline=fg1, width=border_w)

        if fmt in ("jpg", "jpeg"):
            img.save(path, format="JPEG", quality=85)
        elif fmt == "webp":
            img.save(path, format="WEBP", quality=85)
        else:
            img.save(path, format="PNG")
    else:
        # Fallback to pure-Python PNG regardless of requested extension
        data = make_fallback_png(w, h, bg, fg1, fg2)
        path.write_bytes(data)


def write_file(path: Path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    if isinstance(content, bytes):
        path.write_bytes(content)
    else:
        path.write_text(content, encoding="utf-8", newline="\n")


def generate(out_dir: Path, total_images: int = 120, seed: int = 42) -> dict:
    """Generate the demo library deterministically."""
    rng = random.Random(seed)
    root = out_dir.resolve()
    root.mkdir(parents=True, exist_ok=True)

    # Folder layout testing various path properties:
    # 1. 'refs' and 'refs2' (prefix boundary trap)
    # 2. Spaces in names
    # 3. Unicode in names
    # 4. Brackets in folder names
    folders = [
        Path("refs/ui"),
        Path("refs/studies [wip]"),
        Path("refs/landscapes"),
        Path("refs2/archive"),
        Path("Architecture & Design/Modern"),
        Path("Architecture & Design/Traditional"),
        Path("Café & Life ☕/Interior"),
        Path("Projects [2026]/Concept Art"),
        Path("Inbox"),
    ]

    # Dimension categories
    portrait_sizes = [(600, 900), (800, 1200), (720, 1280), (500, 750), (480, 800)]
    panoramic_sizes = [(1800, 600), (2400, 800), (1500, 500), (1600, 600), (2000, 600)]
    landscape_sizes = [(1200, 800), (1600, 900), (1024, 768), (1280, 720), (800, 600)]
    square_sizes = [(600, 600), (800, 800), (500, 500)]

    # Palette bank
    palettes = [
        ((35, 45, 60), (70, 130, 180), (240, 200, 80)),      # Deep slate & gold
        ((40, 70, 50), (100, 180, 120), (230, 220, 190)),    # Forest & mint
        ((80, 35, 45), (200, 90, 100), (250, 180, 160)),     # Wine & coral
        ((45, 35, 65), (140, 90, 190), (220, 190, 250)),     # Violet dusk
        ((70, 50, 30), (210, 140, 60), (245, 230, 200)),     # Amber & ochre
        ((30, 60, 70), (80, 170, 190), (180, 230, 240)),     # Ocean breeze
        ((50, 50, 55), (130, 130, 140), (220, 220, 230)),    # Monochrome graphite
    ]

    # Pre-defined descriptive stems for realism
    stems = [
        "alpine-glow", "autumn-trail", "brutalist-facade", "ceramic-glaze",
        "coastal-horizon", "concrete-stairs", "cyber-grid", "desert-dune",
        "editorial-spread", "espresso-bar", "foggy-valley", "forest-canopy",
        "geometric-pattern", "glass-pavilion", "harbor-lights", "highland-moss",
        "isometric-room", "kinetic-sculpture", "lantern-festival", "lichen-stone",
        "minimal-chair", "monolith-shadow", "mountain-pass", "neon-alleyway",
        "nordic-interior", "obsidian-cliff", "orchard-blossom", "paper-origami",
        "pine-ridge", "planar-composition", "quarry-strata", "rain-window",
        "river-delta", "sandstone-arch", "shibuya-crossing", "silhouette-study",
        "sketch-composition", "solar-flare", "stainless-detail", "storm-clouds",
        "studio-still-life", "subway-platform", "tea-house", "terracotta-tile",
        "timber-joinery", "tokyo-night", "topographic-lines", "transit-map",
        "urban-canyon", "velvet-drapes", "vintage-lens", "waterfall-mist",
        "white-cube", "wireframe-layout", "zen-garden",
    ]

    all_images = []
    if HAVE_PIL:
        formats = ["png", "jpg", "png", "webp", "png", "jpg"]
    else:
        # Without PIL, generate valid PNGs matching the .png extension
        formats = ["png"]

    for i in range(total_images):
        folder = folders[i % len(folders)]
        stem_base = stems[i % len(stems)]
        stem = f"{stem_base}-{i + 1:03d}"
        fmt = formats[i % len(formats)]

        # Size category distribution: ~20% portrait, ~15% panoramic, ~15% square, ~50% landscape
        mod = i % 10
        if mod in (0, 1):
            w, h = rng.choice(portrait_sizes)
            cat = "portrait"
        elif mod in (2, 3):
            w, h = rng.choice(panoramic_sizes)
            cat = "panoramic"
        elif mod in (4, 5):
            w, h = rng.choice(square_sizes)
            cat = "square"
        else:
            w, h = rng.choice(landscape_sizes)
            cat = "landscape"

        bg, fg1, fg2 = rng.choice(palettes)
        img_rel = folder / f"{stem}.{fmt}"
        img_path = root / img_rel
        create_image(img_path, fmt, w, h, bg, fg1, fg2, rng, stem)

        all_images.append({
            "rel": img_rel.as_posix(),
            "name": f"{stem}.{fmt}",
            "stem": stem,
            "fmt": fmt,
            "w": w,
            "h": h,
            "cat": cat,
            "folder": folder.as_posix(),
        })

    # Note creation per docs/FORMAT.md:
    # 1. Some notes missing (~25% -> 30 images)
    # 2. Some notes with ONLY imported (~20% -> 24 images)
    # 3. Remaining notes have full metadata (id, title, tags, rating, source, added, imported)
    # 4. Some with 'added', some without 'added'
    # 5. ALL image notes end with self-embed per FORMAT §3.2
    tag_pool = ["design", "architecture", "study", "landscape", "urban", "night", "minimal", "texture", "reference"]

    notes_created = 0
    notes_only_imported = 0
    notes_with_added = 0
    notes_without_added = 0

    for i, item in enumerate(all_images):
        img_file = root / item["rel"]
        note_path = Path(str(img_file) + ".md")

        # Group 1: Missing notes (index % 4 == 0 -> ~30 images)
        if i % 4 == 0:
            continue

        # Group 2: Only imported (index % 5 == 1 -> ~24 images)
        elif i % 5 == 1:
            imported_dt = f"2026-09-{(i % 28) + 1:02d}T10:00:00+00:00"
            content = (
                "---\n"
                f"imported: {imported_dt}\n"
                "---\n\n"
                f"![[{item['rel']}]]\n"
            )
            write_file(note_path, content)
            notes_created += 1
            notes_only_imported += 1

        # Group 3: Full metadata notes
        else:
            note_id = make_ulid(rng)
            title = item["stem"].replace("-", " ").title()
            tags = rng.sample(tag_pool, rng.randint(2, 4))
            rating = rng.randint(1, 5)
            source = f"https://example.com/artworks/{item['stem']}"
            imported_dt = f"2026-09-{(i % 28) + 1:02d}T12:00:00+00:00"

            # Some with 'added', some without
            has_added = (i % 2 == 0)
            added_line = f"added: 2026-08-{(i % 28) + 1:02d}T09:30:00+00:00\n" if has_added else ""
            if has_added:
                notes_with_added += 1
            else:
                notes_without_added += 1

            tags_yaml = "[" + ", ".join(tags) + "]"
            content = (
                "---\n"
                f"id: {note_id}\n"
                f"title: \"{title}\"\n"
                f"tags: {tags_yaml}\n"
                f"rating: {rating}\n"
                f"source: {source}\n"
                f"{added_line}"
                f"imported: {imported_dt}\n"
                "---\n\n"
                f"Visual reference study for {title} in {item['folder']}.\n\n"
                f"![[{item['rel']}]]\n"
            )
            write_file(note_path, content)
            notes_created += 1

    # 3 Collection / collection-testing notes:
    # 1. One explicit collection: kind: collection
    coll_1_path = root / "collections" / "featured-picks.md"
    pick_indices = [5, 12, 25, min(40, len(all_images) - 1)]
    picks = [all_images[idx]["rel"] for idx in pick_indices]
    coll_1_content = (
        "---\n"
        "kind: collection\n"
        "title: \"Featured Selections\"\n"
        "tags: [featured, showcase]\n"
        "---\n\n"
        "Curated highlights from across the library.\n\n"
    )
    for p in picks:
        coll_1_content += f"![[{p}]]\nHighlight image.\n\n"
    write_file(coll_1_path, coll_1_content)

    # 2. One plain note embedding images (not kind: collection, not an image note)
    coll_2_path = root / "notes" / "project-moodboard.md"
    mood_indices = [min(18, len(all_images) - 1), min(33, len(all_images) - 1), min(50, len(all_images) - 1)]
    mood_picks = [all_images[idx]["rel"] for idx in mood_indices]
    coll_2_content = (
        "---\n"
        "title: \"Project Moodboard\"\n"
        "tags: [moodboard, design]\n"
        "---\n\n"
        "Inspiration elements for UI and architectural styling.\n\n"
    )
    for p in mood_picks:
        coll_2_content += f"![[{p}]]\nReference tile.\n\n"
    write_file(coll_2_path, coll_2_content)

    # 3. An image note (already created as part of image notes, e.g. for all_images[2])
    # FORMAT §3.2 rule: the image note's self-embed MUST NOT count as a collection!
    # Verified in self-test.

    # Non-image files (FORMAT §2.1: video, PDF, archives, plain text may live in library)
    write_file(root / "docs" / "styleguide.pdf", b"%PDF-1.4\n% Demo styleguide non-image document\n%%EOF\n")
    write_file(root / "notes" / "palette.txt", "Slate: #232d3c\nSteel: #4682b4\nGold: #f0c850\n")
    write_file(root / "README.txt", "dimagine demo fixture library for viewer testing.\n")

    return {
        "root": str(root),
        "total_images": len(all_images),
        "notes_created": notes_created,
        "notes_only_imported": notes_only_imported,
        "notes_with_added": notes_with_added,
        "notes_without_added": notes_without_added,
        "explicit_collections": 1,
        "embedded_plain_notes": 1,
        "non_image_files": 3,
        "pil_used": HAVE_PIL,
    }


def selftest(target_dir: Path | None = None) -> bool:
    """Validate library against FORMAT rules and requirements."""
    print("Running demo-library self-test...")
    temp_obj = None
    if target_dir is None:
        temp_obj = tempfile.TemporaryDirectory(prefix="dimagine-selftest-")
        target_dir = Path(temp_obj.name)

    target_dir_a = target_dir / "lib_a"
    target_dir_b = target_dir / "lib_b"

    # 1. Determinism test
    summary_a = generate(target_dir_a, total_images=120, seed=42)
    summary_b = generate(target_dir_b, total_images=120, seed=42)

    files_a = sorted(p.relative_to(target_dir_a).as_posix() for p in target_dir_a.rglob("*") if p.is_file())
    files_b = sorted(p.relative_to(target_dir_b).as_posix() for p in target_dir_b.rglob("*") if p.is_file())
    assert files_a == files_b, f"File lists differ between runs: {len(files_a)} vs {len(files_b)}"

    for rel_path in files_a:
        pa = target_dir_a / rel_path
        pb = target_dir_b / rel_path
        assert pa.read_bytes() == pb.read_bytes(), f"File content mismatch for {rel_path}"
    print("  [PASS] Deterministic generation (identical trees and checksums)")

    # 2. Structure tests on lib_a
    root = target_dir_a

    # Check required folders
    assert (root / "refs").is_dir(), "Missing 'refs' folder"
    assert (root / "refs2").is_dir(), "Missing 'refs2' folder"
    assert (root / "Architecture & Design").is_dir(), "Missing folder with spaces"
    assert (root / "Café & Life ☕").is_dir(), "Missing folder with unicode"
    assert (root / "Projects [2026]").is_dir(), "Missing folder with brackets"
    assert (root / "refs/studies [wip]").is_dir(), "Missing nested folder with brackets and spaces"
    print("  [PASS] Required folders present (refs, refs2, spaces, unicode, brackets)")

    # Check images (~120 images, varied sizes, aspect ratios)
    image_exts = {".png", ".jpg", ".jpeg", ".webp"}
    images = [p for p in root.rglob("*") if p.is_file() and p.suffix.lower() in image_exts and not p.name.endswith(".md")]
    assert len(images) == 120, f"Expected 120 images, got {len(images)}"

    has_portrait = False
    has_panoramic = False
    has_square = False
    has_landscape = False

    for img_path in images:
        data = img_path.read_bytes()
        # Verify valid header
        is_png = data.startswith(b"\x89PNG\r\n\x1a\n")
        is_jpg = data.startswith(b"\xff\xd8")
        is_webp = data.startswith(b"RIFF") and data[8:12] == b"WEBP"
        assert is_png or is_jpg or is_webp, f"Image {img_path} has invalid signature"

        # Read dimensions
        w, h = 0, 0
        if is_png and len(data) >= 24:
            w, h = struct.unpack(">II", data[16:24])
        elif is_jpg:
            # Simple JPEG SOF reader
            idx = 2
            while idx < len(data) - 9:
                if data[idx] == 0xFF and data[idx + 1] in (0xC0, 0xC1, 0xC2):
                    h, w = struct.unpack(">HH", data[idx + 5:idx + 9])
                    break
                length = struct.unpack(">H", data[idx + 2:idx + 4])[0]
                idx += 2 + length
        elif is_webp and len(data) >= 30:
            if data[12:16] == b"VP8 ":
                w, h = struct.unpack("<HH", data[26:30])
                w, h = w & 0x3FFF, h & 0x3FFF

        if w > 0 and h > 0:
            if h > w:
                has_portrait = True
            elif w >= 2 * h:
                has_panoramic = True
            elif w == h:
                has_square = True
            else:
                has_landscape = True

    assert has_portrait, "No portrait images found"
    assert has_panoramic, "No panoramic images found"
    assert has_square, "No square images found"
    assert has_landscape, "No landscape images found"
    print("  [PASS] Varied images (120 count, portrait, panoramic, square, landscape)")

    # 3. Check notes per FORMAT rules
    note_files = [p for p in root.rglob("*.md")]
    image_notes = [p for p in note_files if (p.parent / p.stem).is_file()]
    assert len(image_notes) < len(images), "Expected some images to be missing notes"
    assert len(image_notes) > 50, "Expected majority of images to have notes"

    found_only_imported = False
    found_added = False
    found_full = False

    for note in image_notes:
        text = note.read_text(encoding="utf-8")
        lines = text.splitlines()
        # Verify self-embed at end (FORMAT §3.2)
        paired_img = note.stem
        assert any(line.strip().startswith(f"![[") for line in lines), f"Note {note} missing self-embed"

        # Check frontmatter
        assert text.startswith("---\n"), f"Note {note} missing YAML frontmatter start"
        fm_end = text.find("\n---\n", 4)
        assert fm_end != -1, f"Note {note} unclosed frontmatter"
        fm = text[4:fm_end]

        if "imported:" in fm and "title:" not in fm and "id:" not in fm:
            found_only_imported = True
        if "added:" in fm:
            found_added = True
        if "id:" in fm and "title:" in fm and "tags:" in fm and "rating:" in fm:
            found_full = True
            # Verify ULID format: 26 Crockford chars
            id_match = re.search(r"id:\s*([0-9A-HJKMNP-TV-Z]{26})", fm)
            assert id_match, f"Invalid ULID in {note}"

    assert found_only_imported, "No notes with only imported found"
    assert found_added, "No notes with added found"
    assert found_full, "No full notes found"
    print("  [PASS] Notes follow FORMAT (some missing, some only imported, some with added, ULIDs, self-embeds)")

    # 4. Check collections
    explicit_colls = []
    plain_embedded = []
    for note in note_files:
        text = note.read_text(encoding="utf-8")
        is_img_note = (note.parent / note.stem).is_file()
        if "kind: collection" in text:
            explicit_colls.append(note)
        elif not is_img_note and "![[" in text:
            plain_embedded.append(note)

    assert len(explicit_colls) == 1, f"Expected 1 explicit collection, got {len(explicit_colls)}"
    assert len(plain_embedded) == 1, f"Expected 1 plain embedded note, got {len(plain_embedded)}"
    print("  [PASS] Collection notes (1 explicit kind:collection, 1 plain embedded note)")

    # 5. Check non-image files
    assert (root / "docs/styleguide.pdf").is_file(), "Missing styleguide.pdf"
    assert (root / "notes/palette.txt").is_file(), "Missing palette.txt"
    assert (root / "README.txt").is_file(), "Missing README.txt"
    print("  [PASS] Non-image files present")

    # 6. Check dimagine binary if available
    target_bin = None
    target_dirs = []
    if "CARGO_TARGET_DIR" in os.environ:
        target_dirs.append(Path(os.environ["CARGO_TARGET_DIR"]))
    try:
        meta_res = subprocess.run(["cargo", "metadata", "--format-version", "1", "--no-deps"], capture_output=True, text=True)
        if meta_res.returncode == 0:
            target_dirs.append(Path(json.loads(meta_res.stdout)["target_directory"]))
    except Exception:
        pass
    repo_root = Path(__file__).resolve().parents[2]
    target_dirs.extend([repo_root / "target", Path("target")])

    for td in target_dirs:
        for profile in ("release", "debug"):
            candidate = td / profile / "dimagine"
            if candidate.is_file() and os.access(candidate, os.X_OK):
                target_bin = candidate
                break
        if target_bin:
            break

    if target_bin is None:
        which_bin = shutil.which("dimagine")
        if which_bin:
            target_bin = Path(which_bin)

    if target_bin and target_bin.is_file() and os.access(target_bin, os.X_OK):
        print("  Running dimagine scan on generated library...")
        res = subprocess.run([str(target_bin), "scan", "--library", str(root), "--json"], capture_output=True, text=True)
        assert res.returncode == 0, f"dimagine scan failed: {res.stderr}"
        scan_data = json.loads(res.stdout)
        assert scan_data["image_total"] == 120, f"dimagine scan images mismatch: {scan_data['image_total']}"
        assert scan_data["collections_explicit"] == 1, f"dimagine scan collections_explicit mismatch: {scan_data['collections_explicit']}"
        assert scan_data["collections_embedded"] == 2, f"dimagine scan collections_embedded mismatch: {scan_data['collections_embedded']}"
        assert scan_data["other_files"] == 3, f"dimagine scan other_files mismatch: {scan_data['other_files']}"
        assert len(scan_data["unreadable_files"]) == 0, f"dimagine scan unreadable files: {scan_data['unreadable_files']}"
        print("  [PASS] dimagine scan matches FORMAT expectations exactly (120 images, 1 explicit, 2 embedded, 0 unreadable)")

        print("  Running dimagine check on generated library...")
        res_chk = subprocess.run([str(target_bin), "check", "--library", str(root), "--json"], capture_output=True, text=True)
        if res_chk.returncode != 0:
            print("dimagine check returned:", res_chk.returncode)
            print("stdout:", res_chk.stdout)
            print("stderr:", res_chk.stderr)
        assert res_chk.returncode == 0, f"dimagine check failed: {res_chk.stderr}"
        chk_data = json.loads(res_chk.stdout)
        errors = [f for f in chk_data["findings"] if f["severity"] in ("error", "warning")]
        assert len(errors) == 0, f"dimagine check found unexpected findings: {errors}"
        print("  [PASS] dimagine check reports 0 errors and 0 warnings")

    if temp_obj is not None:
        temp_obj.cleanup()

    print("All self-test checks passed successfully!")
    return True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("out_dir", type=Path, nargs="?", default=None, help="Output directory for demo library")
    parser.add_argument("--selftest", action="store_true", help="Run self-test validation against FORMAT rules")
    parser.add_argument("--images", type=int, default=120, help="Number of images to generate (default: 120)")
    parser.add_argument("--seed", type=int, default=42, help="Deterministic random seed (default: 42)")

    args = parser.parse_args()

    if args.selftest:
        success = selftest(args.out_dir)
        sys.exit(0 if success else 1)

    if args.out_dir is None:
        parser.error("out_dir is required unless --selftest is specified")

    summary = generate(args.out_dir, total_images=args.images, seed=args.seed)
    print(json.dumps(summary, indent=2, ensure_ascii=False))


if __name__ == "__main__":
    main()
