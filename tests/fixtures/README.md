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

The `dimagine` tools never decode images, only read leading bytes (FORMAT
§2.1), so the header stubs are sufficient for the WebP/AVIF/HEIC cases; the
other five are fully valid images so the fixtures stay useful to future
versions that do more.
