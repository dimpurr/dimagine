#!/usr/bin/env python3
"""Prototype: add the missing self-embed (docs/FORMAT.md §3.2) to image notes.

Dry run by default; pass --write to change files. Only notes named
`<image>.<ext>.md` whose image sits next to them are touched, and only by
appending one line. Nothing else in the note changes.

Usage: add-self-embeds.py <library> [--write]
"""
import argparse, os, re, sys
from collections import Counter

IMG = {'jpg', 'jpeg', 'png', 'gif', 'webp', 'avif', 'heic', 'heif', 'tif', 'tiff', 'bmp'}

ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
ap.add_argument('library')
ap.add_argument('--write', action='store_true', help='apply changes (default: report only)')
a = ap.parse_args()
LIB = os.path.abspath(os.path.expanduser(a.library))
if not os.path.isdir(LIB):
    sys.exit(f'not a folder: {LIB}')

files = []
for dp, dn, fn in os.walk(LIB):
    dn[:] = [d for d in dn if not d.startswith('.')]
    files += [os.path.join(dp, f) for f in fn if not f.startswith('.')]
names = Counter(os.path.basename(f).lower() for f in files)

stats = Counter()
for note in files:
    img = note[:-3]
    if not note.endswith('.md') or img.rsplit('.', 1)[-1].lower() not in IMG:
        continue
    if not os.path.exists(img):
        stats['note without image (skipped)'] += 1; continue
    base, rel = os.path.basename(img), os.path.relpath(img, LIB)
    text = open(note, encoding='utf-8').read()
    targets = {base, rel}
    found = any(t.split('|')[0].strip() in targets for t in re.findall(r'!\[\[([^\]]+)\]\]', text))
    found = found or any(os.path.normpath(os.path.join(os.path.dirname(note), t)) == img
                         for t in re.findall(r'!\[[^\]]*\]\(<?([^)>]+)>?\)', text))
    if found:
        stats['already has self-embed'] += 1; continue
    embed = base if names[base.lower()] == 1 else rel
    stats['added (bare name)' if embed == base else 'added (path, name not unique)'] += 1
    if a.write:
        sep = '' if text.endswith('\n\n') else ('\n' if text.endswith('\n') else '\n\n')
        open(note, 'a', encoding='utf-8').write(f'{sep}![[{embed}]]\n')

print(('written' if a.write else 'dry run') + ':', dict(stats))
