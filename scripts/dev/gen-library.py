#!/usr/bin/env python3
"""Generate deterministic, stdlib-only synthetic dimagine libraries."""
import argparse
import json
import os
import random
import struct
import zlib
from pathlib import Path

FORMATS = ("png", "gif", "bmp", "jpg", "webp")
PNG_SIG = b"\x89PNG\r\n\x1a\n"
JPEG = bytes.fromhex("ffd8ffe000104a46494600010100000100010000ffdb004300030202020202030202020303030304060404040404080606050609080a0a090809090a0c0f0c0a0b0e0b09090d110d0e0f101011100a0c12131210130f101010ffdb00430103030304030408040408100b090b1010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010101010ffc00011080001000c03011100021101031101ffc4001500010100000000000000000000000000000008ffc40014100100000000000000000000000000000000ffc400160101010100000000000000000000000000000708ffc40014110100000000000000000000000000000000ffda000c03010002110311003f00ad585d5301ffd9")
WEBP = bytes.fromhex("524946463800000057454250565038202c0000009001009d012a0c00010002003425a00274ba00039800fef34b97fed687febb3ffd767fd11ff57627c2872000")
ULID_CHARS = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"


def chunk(tag, data):
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xffffffff)


def png(w, h, rgba):
    row = b"\0" + bytes(rgba) * w
    co = zlib.compressobj(6)
    raw = bytearray()
    for _ in range(h):
        raw.extend(co.compress(row))
    raw.extend(co.flush())
    return PNG_SIG + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)) + chunk(b"IDAT", raw) + chunk(b"IEND", b"")


def bmp(w, h, rgba):
    stride = (3 * w + 3) & ~3
    size = stride * h
    head = b"BM" + struct.pack("<IHHI", 54 + size, 0, 0, 54)
    dib = struct.pack("<IiiHHIIiiII", 40, w, h, 1, 24, 0, size, 2835, 2835, 0, 0)
    row = bytes((rgba[2], rgba[1], rgba[0])) * w + bytes(stride - 3 * w)
    return head + dib + row * h


def gif(w, h):
    bits = bytearray()
    acc = nbits = 0
    for _ in range(w * h):
        for code in (4, 0):
            acc |= code << nbits
            nbits += 3
            if nbits >= 8:
                bits.append(acc & 255)
                acc >>= 8
                nbits -= 8
    if nbits:
        bits.append(acc & 255)
    blocks = bytearray()
    for i in range(0, len(bits), 255):
        b = bits[i:i + 255]
        blocks.extend((len(b),))
        blocks.extend(b)
    return (b"GIF89a" + struct.pack("<HHBBB", w, h, 0x91, 0, 0)
            + b"\x00\x00\x00\xff\xff\xff\x80\x80\x80\xff\x00\xff"
            + b"\x2c\0\0\0\0" + struct.pack("<HHB", w, h, 0) + b"\x02" + blocks + b"\0\x3b")


def image(fmt, w, h, rgba):
    return {"png": lambda: png(w, h, rgba), "gif": lambda: gif(w, h),
            "bmp": lambda: bmp(w, h, rgba), "jpg": lambda: JPEG,
            "webp": lambda: WEBP}[fmt]()


def write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    if isinstance(content, bytes):
        path.write_bytes(content)
    else:
        path.write_text(content, encoding="utf-8", newline="\n")


def rel(path, root):
    return path.relative_to(root).as_posix()


def ulid(rng):
    value = rng.getrandbits(128)
    chars = []
    for _ in range(26):
        chars.append(ULID_CHARS[value & 31])
        value >>= 5
    return "".join(reversed(chars))


def generate(out, count, seed, notes_ratio, collections, depth, dup_ratio, unicode_names, bad_cases):
    rng = random.Random(seed)
    out.mkdir(parents=True, exist_ok=True)
    root = out.resolve()
    images, info, problems = [], [], []
    duplicates = round(count * dup_ratio)
    folders = [Path()] + [Path("/".join(f"folder-{(i + j) % 12:02d}" for j in range(i))) for i in range(1, depth + 1)]
    for i in range(count):
        folder = folders[i % len(folders)]
        stem = "shared-name" if duplicates and i >= count - duplicates else f"image-{i:06d}"
        if unicode_names and i % 17 == 0:
            stem += "-café"
        fmt = "png" if duplicates and i >= count - duplicates else FORMATS[i % len(FORMATS)]
        if i == 5:
            w, h = 4000, 3000
        elif i % 4 == 1:
            w, h = 12, 1
        elif i % 4 == 2:
            w, h = 1, 12
        else:
            w, h = 64, 48
        if fmt in ("jpg", "webp"):
            w, h = 12, 1
        col = tuple(rng.randrange(256) for _ in range(3)) + (255,)
        path = root / folder / f"{stem}.{fmt}"
        if path in images:
            path = root / f"folder-extra-{i:04d}" / path.name
        write(path, image(fmt, w, h, col))
        images.append(path)
        info.append((fmt, w, h))
    names = {}
    for p in images:
        names[p.name.casefold()] = names.get(p.name.casefold(), 0) + 1
    selected = [i for i in range(count) if rng.random() < notes_ratio]
    note_ids = {}
    for ni, i in enumerate(selected):
        p = images[i]
        fmt, w, h = info[i]
        note_ids[i] = ulid(rng)
        target = p.name if names[p.name.casefold()] == 1 else rel(p, root)
        note = ("---\n" + f"id: {json.dumps(note_ids[i])}\ntitle: {json.dumps(p.stem, ensure_ascii=False)}\n"
                "tags: [synthetic, benchmark]\n" + f"rating: {ni % 6}\ncreated: 2024-01-01\n"
                "sources:\n  - type: synthetic\n" + f"    raw: {json.dumps(p.name + '.eagle.json')}\n---\n\n"
                f"Synthetic {w}×{h} {fmt} test image.\n\n![[{target}]]\n")
        write(Path(str(p) + ".md"), note)
        write(Path(str(p) + ".eagle.json"), json.dumps({"id": f"synthetic-{i:08d}", "name": p.stem, "ext": fmt, "width": w, "height": h, "seed": seed}, ensure_ascii=False, sort_keys=True, indent=2) + "\n")
    for c in range(collections):
        members = images[c::max(1, collections)][:min(100, count)]
        lines = ["---", "kind: collection", f"title: {json.dumps('Synthetic collection ' + str(c + 1))}", "---", ""]
        for j, p in enumerate(members):
            lines += [f"![[{p.name if names[p.name.casefold()] == 1 else rel(p, root)}]]", f"Synthetic member {j + 1}."]
        write(root / "collections" / f"collection-{c + 1:04d}.md", "\n".join(lines) + "\n")
        nodes = [{"id": f"node-{c}-{j}", "type": "file", "file": rel(p, root), "x": j * 240, "y": 0, "width": 220, "height": 180} for j, p in enumerate(members[:25])]
        write(root / "canvases" / f"board-{c + 1:04d}.canvas", json.dumps({"nodes": nodes, "edges": []}, indent=2) + "\n")
    def problem(path, kind):
        problems.append({"path": rel(path, root), "kind": kind})
    if bad_cases:
        if not images:
            raise ValueError("--bad-cases requires at least one image")
        p = root / "bad" / "extension-mismatch.jpg"; write(p, png(2, 2, (200, 10, 20, 255))); problem(p, "extension_mismatch")
        p = root / "bad" / "truncated.png"; write(p, PNG_SIG + b"\0\0\0\rIHDR"); problem(p, "truncated_image")
        p = Path(str(images[0]) + ".md"); write(p, "---\ntitle: [broken\n---\n![[" + images[0].name + "]]\n"); problem(p, "invalid_yaml")
        duplicate_value = json.dumps(note_ids.get(0, "00000000000000000000000001"))
        for name in ("duplicate-id-a.md", "duplicate-id-b.md"):
            p = root / "bad" / name
            write(p, f"---\nid: {duplicate_value}\ntitle: duplicate\n---\n"); problem(p, "duplicate_id")
        p = root / "bad" / "missing-link.md"; write(p, "![[missing-target.png]]\n"); problem(p, "missing_link")
        dupe = next((p for p in images if names[p.name.casefold()] > 1), None)
        if dupe is None:
            for d, rgb in (("dup-a", (1,2,3,255)), ("dup-b", (4,5,6,255))):
                p = root / d / "same-name.png"; write(p, png(1, 1, rgb)); images.append(p)
            dupe = root / "dup-a" / "same-name.png"
        p = root / "bad" / "ambiguous-link.md"; write(p, f"![[{dupe.name}]]\n"); problem(p, "ambiguous_link")
        p = root / "bad" / "name [x] #y ^z |q.png"; write(p, png(1, 1, (1, 2, 3, 255))); problem(p, "special_characters_name")
        for name in ("._resource.png", ".DS_Store"):
            p = root / name; write(p, b"synthetic ignored data\n"); problem(p, "ignored_file")
        p = root / "bad" / "image-link.png"
        try:
            p.symlink_to(os.path.relpath(images[0], p.parent)); problem(p, "symlink")
        except (OSError, NotImplementedError):
            problem(p, "symlink_unavailable")
        for folder, name, kind in (("nfc", "café.png", "unicode_nfc"), ("nfd", "cafe\u0301.png", "unicode_nfd")):
            p = root / "unicode" / folder / name; write(p, png(1, 1, (8, 9, 10, 255))); problem(p, kind)
    expected = {"seed": seed, "images_requested": count, "problems": sorted(problems, key=lambda x: (x["kind"], x["path"]))}
    write(root / ".expected.json", json.dumps(expected, ensure_ascii=False, sort_keys=True, indent=2) + "\n")
    return {"root": str(root), "images": count, "notes": len(selected), "collections": collections, "problems": len(problems)}


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("out_dir", type=Path)
    ap.add_argument("--images", type=int, required=True)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--notes-ratio", type=float, default=0.5)
    ap.add_argument("--collections", type=int, default=10)
    ap.add_argument("--depth", type=int, default=3)
    ap.add_argument("--dup-names-ratio", type=float, default=0.05)
    ap.add_argument("--unicode", dest="unicode_names", action="store_true")
    ap.add_argument("--bad-cases", action="store_true")
    a = ap.parse_args()
    if a.images < 0 or a.collections < 0 or a.depth < 0 or not 0 <= a.notes_ratio <= 1 or not 0 <= a.dup_names_ratio <= 1:
        ap.error("counts must be non-negative and ratios between 0 and 1")
    print(json.dumps(generate(a.out_dir, a.images, a.seed, a.notes_ratio, a.collections, a.depth, a.dup_names_ratio, a.unicode_names, a.bad_cases), ensure_ascii=False, sort_keys=True))


if __name__ == "__main__":
    main()
