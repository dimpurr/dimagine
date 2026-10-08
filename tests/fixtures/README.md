# Test fixtures

Tiny fixture files for the test suite. All are synthetic; none come from any
real image library.

| File | What it is |
| --- | --- |
| `pixel.png` | A real 1×1 PNG (70 bytes). |
| `pixel.jpg` | A real 1×1 JPEG, converted from `pixel.png` with `sips`. |
| `pixel.tiff` | A real 1×1 TIFF, converted from `pixel.png` with `sips`. |
| `pixel.gif` | A real 1×1 GIF89a, hand-built (35 bytes). |
| `pixel.bmp` | A real 1×1 24-bpp BMP, hand-built (58 bytes). |
| `pixel.webp` | RIFF/WEBP header stub for sniff tests (not a decodable image). |
| `pixel.avif` | ISO-BMFF `ftyp`/`avif` header stub for sniff tests. |
| `pixel.heic` | ISO-BMFF `ftyp`/`heic` header stub for sniff tests. |
| `exif-date.jpg` | A real 12×1 JPEG with EXIF `DateTimeOriginal` 2023-07-12 20:54:07.123 +01:00, hand-built. |
| `exif-naive.jpg` | The same JPEG with the date but no `OffsetTimeOriginal`. |
| `exif-zeros.jpg` | The same JPEG with the classic all-zero date. |
| `rotated-exif.jpg` | The same JPEG with EXIF orientation 6 (stored 12×1, displayed 1×12) and a −05:00 date. |

## Taken-time fixtures

The rest wrap one **camera stamp** — the `DateTimeOriginal` of
`exif-date.jpg`, with its `SubSecTimeOriginal` and `OffsetTimeOriginal` — in
the shapes a reader has to survive. Each is a hand-built EXIF or XMP block
over the same real 12×1 red JPEG, PNG or WebP; `exif.heic` was converted from
such a JPEG with `sips`.

| File | What it is |
| --- | --- |
| `exif-digitized.jpg` | The stamp under `DateTimeDigitized` only, with its own `OffsetTimeDigitized`. |
| `exif-datetime-only.jpg` | Only the TIFF `DateTime` of IFD 0 — no Exif sub-IFD at all. |
| `exif-no-date.jpg` | An EXIF block with entries and no date anywhere. |
| `exif-broken-entry.jpg` | The stamp beside one entry whose offset runs past the end of the block: a camera's MakerNote. |
| `exif-broken-block.jpg` | An APP1 `Exif` block that is not a TIFF past its header. |
| `exif-iso-date.jpg` | The stamp written ISO 8601 (`2021-06-30T17:45:12+02:00`) where EXIF expects colons and a space. |
| `exif-no-seconds.jpg` | A date known to the minute (`2017-02-28 11:20`). |
| `exif-undefined-date.jpg` | The date stored as UNDEFINED rather than ASCII, with a +09:00 offset. |
| `exif-far-date.jpg` | `9999:12:31 23:59:59` — a moment past what an epoch nanosecond can hold. |
| `exif-second-ifd.jpg` | The metadata hanging off the second IFD, as a file with its thumbnail first is written. |
| `xmp-date.jpg` | A JPEG stripped of EXIF, its only date an XMP APP1 packet (`xmp:CreateDate`). |
| `xmp-date.webp` | A WebP whose only date is an `XMP ` chunk, in attribute form (`photoshop:DateCreated`). |
| `xmp-in-exif.jpg` | The only date being an XMP packet stored as EXIF tag 700. |
| `exif.png` | The stamp in a PNG `eXIf` chunk. |
| `exif.webp` | A decodable 12×1 lossy WebP with the stamp in an `EXIF` chunk. |
| `exif.heic` | A real HEIC whose EXIF carries the stamp: no dimension reader here, a readable date. |

The `dimagine` tools never decode images, only read leading bytes and the
EXIF block (FORMAT §2.1), so the header stubs are sufficient for the
WebP/AVIF/HEIC cases; the others are fully valid images so the fixtures stay
useful to future versions that do more — the index refresh reads stored
dimensions and the EXIF block from exactly these (the real 12×1 JPEG/WebP
bitstreams the generator uses are embedded in
`crates/dimagine-core/src/image_meta.rs`, with their provenance).
