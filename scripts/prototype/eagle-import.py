#!/usr/bin/env python3
"""Prototype: copy an Eagle library into a dimagine library (docs/FORMAT.md 0.1).

The Eagle library is only read, never modified. The target folder must be empty
or missing. This script is a prototype; the `dimagine import eagle` tool will
replace it.

Usage: eagle-import.py <path/to/Name.library> <target-library> [--name NAME]
"""
import argparse, json, os, re, shutil, sys, glob, random, time, datetime, unicodedata
from collections import Counter

IMG = {'jpg', 'jpeg', 'png', 'gif', 'webp', 'avif', 'heic', 'heif', 'tif', 'tiff', 'bmp'}
B32 = '0123456789ABCDEFGHJKMNPQRSTVWXYZ'

ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
ap.add_argument('src', nargs='?', help='Eagle library folder (Name.library)')
ap.add_argument('dst', nargs='?', help='target dimagine library folder (empty or missing)')
ap.add_argument('--name', help='label for this Eagle library (default: folder name without .library)')
ap.add_argument('--selftest', action='store_true', help='check name_from_url against the shared fixture')
a = ap.parse_args()

TWO_PART_SUFFIXES = {'co.uk', 'co.jp', 'com.cn', 'com.au'}
# Four labels of one to three ASCII digits: any dotted-quad IPv4 literal, even a
# malformed one such as 999.999.999.999, yields no site label.
IPV4_LITERAL = re.compile(r'\A[0-9]{1,3}(?:\.[0-9]{1,3}){3}\Z')
GENERIC_EXACT = {'image', 'download', 'untitled'}
GENERIC_PREFIXES = ('pasted image', 'screenshot')
GENERIC_IMG = re.compile(r'\Aimg([_-]?[0-9]+)\Z')
GENERIC_HASH = re.compile(r'\A[0-9a-f]{16,}\Z')

def is_generic(name):
    r"""Whether an Eagle item name is a placeholder that carries no information.

    A name is generic when, after lowercasing, it is exactly `image`, `download`
    or `untitled`; starts with `pasted image` or `screenshot`; is `img` plus at
    most one `_` or `-` separator plus ASCII digits; or is at least 16 ASCII hex
    digits (a hash).

    Deliberately ASCII-only where the earlier implementations disagreed:
    separators are exactly one `_` or `-` (`img__12` is a real name, not a
    placeholder) and digits are ASCII (`img١٢` is a real name, not a
    placeholder), and the match never tolerates a trailing newline (`\A`/`\Z`,
    not `^`/`$`, and no IGNORECASE, which would case-fold non-ASCII letters).
    Mirrors Rust `is_generic`; both are checked against the shared rows in
    crates/dimagine-eagle/tests/fixtures/generic-names.json.
    """
    lower = name.lower()
    if lower in GENERIC_EXACT or lower.startswith(GENERIC_PREFIXES):
        return True
    if GENERIC_IMG.match(lower):
        return True
    return bool(GENERIC_HASH.match(lower))

def _strip_scheme(url):
    low = url.lower()
    for scheme in ('https://', 'http://'):
        if low.startswith(scheme):
            return url[len(scheme):]
    return None

def _site_from_authority(authority):
    """Site label for the host part of a URL authority, or None.

    Userinfo is dropped and any port is ignored, including the port after a
    bracketed IPv6 literal. An IP literal host never produces a label: not a
    bracketed IPv6 literal ([::1]:3000), not a dotted-quad IPv4 literal
    (192.168.1.5) and not a bare, unbracketed IPv6 literal such as 2001:db8::1
    (illegal in an authority but common in pasted URLs). A None result means
    the caller falls back to the time-based generated name; the host itself is
    never substituted. Mirrors Rust `site_from_authority`.
    """
    authority = authority.rsplit('@', 1)[-1]
    if authority.startswith('['):
        return None
    if authority.count(':') > 1:
        return None
    host = authority.split(':', 1)[0].lower()
    if IPV4_LITERAL.match(host):
        return None
    return _site_from_host(host)

def _site_from_host(host):
    labels = [p for p in host.split('.') if p]
    if not labels:
        return None
    if len(labels) >= 2 and '.'.join(labels[-2:]) in TWO_PART_SUFFIXES:
        suffix = 2
    elif len(labels) >= 2:
        suffix = 1
    else:
        suffix = 0
    idx = len(labels) - suffix - 1
    return labels[idx] if idx >= 0 else None

def name_from_url(url):
    """Generic <site>-<id> name from a source URL, or None (K24 rule)."""
    if not isinstance(url, str):
        return None
    rest = _strip_scheme(url)
    if rest is None:
        return None
    cut = len(rest)
    for sep in ('?', '#'):
        i = rest.find(sep)
        if i >= 0:
            cut = min(cut, i)
    rest = rest[:cut]
    slash = rest.find('/')
    authority, path = (rest[:slash], rest[slash:]) if slash >= 0 else (rest, '')
    authority = authority.rsplit('@', 1)[-1]
    site = _site_from_authority(authority)
    if not site:
        return None
    first_digits = first_mixed = None
    for segment in path.split('/'):
        if not segment or '=' in segment:
            continue
        for token in re.split(r'[^A-Za-z0-9]+', segment):
            if not token or len(token) > 24:
                continue
            if token.isdigit():
                if 5 <= len(token) <= 20 and first_digits is None:
                    first_digits = token
            elif any(c.isalpha() for c in token) and any(c.isdigit() for c in token):
                if 6 <= len(token) <= 24 and first_mixed is None:
                    first_mixed = token
    ident = first_digits or first_mixed
    return f'{site}-{ident}' if ident else None

def selftest():
    """Check this prototype against the fixtures the Rust tests also read.

    Returns a list of (check, input, expected, actual) failures and the number
    of rows checked. Run with --selftest; this is what CI runs.
    """
    base = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..',
                        'crates', 'dimagine-eagle', 'tests', 'fixtures')

    def rows(fixture):
        with open(os.path.join(base, fixture), encoding='utf-8') as handle:
            return json.load(handle)

    failures, checked = [], 0
    for row in rows('url-names.json'):
        checked += 1
        actual = name_from_url(row['url'])
        if actual != row['name']:
            failures.append(('name_from_url', row['url'], row['name'], actual))
    for row in rows('generic-names.json'):
        checked += 1
        actual = is_generic(row['name'])
        if actual != row['generic']:
            failures.append(('is_generic', row['name'], row['generic'], actual))
    forbidden = '\x00\x07\x1f\x7f\x80\x81\x85\x9f\u2028\u2029\ufffe\ufffd'
    for value in (f'plain{forbidden}title', 'back\\slash "quoted"', '١٢'):
        checked += 1
        raw = sorted({f'U+{ord(character):04X}'
                      for character in yaml_scalar(value) if not yaml_printable(character)})
        if raw:
            failures.append(('yaml_scalar', value, 'no YAML-forbidden characters', raw))
    return failures, checked

def yaml_printable(character):
    """YAML printable characters (YAML 1.2 c-printable).

    Tab, LF, CR, U+0020..U+007E, U+0085, U+00A0..U+D7FF, U+E000..U+FFFD and
    U+10000..U+10FFFF. Mirrors Rust `yaml_printable`.
    """
    return (character in '\t\n\r\x85'
            or '\x20' <= character <= '\x7e'
            or '\xa0' <= character <= '\ud7ff'
            or '\ue000' <= character <= '\ufffd'
            or '\U00010000' <= character <= '\U0010ffff')

YAML_FORBIDDEN = re.compile('[^\\t\\n\\r\\x20-\\x7e\\x85\\xa0-\\ud7ff\\ue000-\\ufffd'
                            '\\U00010000-\\U0010ffff]')

def yaml_scalar(value):
    """Render a value as a YAML scalar, escaping everything YAML forbids.

    The value is serialized as JSON first — that keeps quoting, escaping and
    non-string types consistent — and then every character YAML does not accept
    becomes a \\uXXXX escape: the C0 controls (already escaped by JSON), DEL,
    the C1 control block U+0080..U+009F, the non-characters U+FFFE and U+FFFF,
    and the line separators U+2028 and U+2029. Escapes JSON already produced are
    printable ASCII, so they are never escaped twice. Mirrors Rust
    `escape_yaml_forbidden`.
    """
    return YAML_FORBIDDEN.sub(lambda match: '\\u%04X' % ord(match.group()),
                              json.dumps(value, ensure_ascii=False))

if a.selftest:
    failures, checked = selftest()
    if failures:
        print('FAIL', failures)
        sys.exit(1)
    print(f'ok {checked} cases')
    sys.exit(0)

if not a.src or not a.dst:
    ap.error('src and dst are required')
SRC, DST = os.path.abspath(os.path.expanduser(a.src)), os.path.abspath(os.path.expanduser(a.dst))
LIB = a.name or re.sub(r'\.library$', '', os.path.basename(SRC.rstrip('/')))

def ulid():
    n = (int(time.time() * 1000) << 80) | random.getrandbits(80)
    return ''.join(B32[(n >> (5 * i)) & 31] for i in range(25, -1, -1))

def clean(s):
    # \x7f-\x9f keeps DEL and the C1 controls, which Rust treats as control
    # characters, out of file names as well.
    s = unicodedata.normalize('NFC', s or '')
    s = re.sub(r'[\\/:*?"<>|\[\]#^\x00-\x1f\x7f-\x9f]', '-', s).strip().strip('.')
    return s[:120]

q = yaml_scalar
now = datetime.datetime.now().astimezone()
NOW = now.isoformat(timespec='seconds')

if not os.path.exists(os.path.join(SRC, 'metadata.json')):
    sys.exit(f'not an Eagle library (no metadata.json): {SRC}')
if os.path.exists(DST) and any(not n.startswith('.') for n in os.listdir(DST)):
    sys.exit(f'refuse: {DST} exists and is not empty')

root = json.load(open(os.path.join(SRC, 'metadata.json')))
fpath = {}
def walk(fs, p=''):
    for f in fs:
        sub = (p + '/' if p else '') + (clean(f['name']) or f['id'])
        fpath[f['id']] = sub
        walk(f.get('children', []), sub)
walk(root.get('folders', []))

taken = {}  # folder -> lower-case names in use
def unique(d, name, ext):
    s = taken.setdefault(d, {n.lower() for n in os.listdir(d)} if os.path.isdir(d) else set())
    base, k = name, 1
    while f'{base}.{ext}'.lower() in s:
        k += 1
        base = f'{name}-{k}'
    s.add(f'{base}.{ext}'.lower())
    return f'{base}.{ext}'

skipped, members, notes, renamed, dangling = [], {}, [], 0, 0
for d in sorted(glob.glob(os.path.join(SRC, 'images', '*.info'))):
    item = os.path.basename(d)[:-5]
    mp = os.path.join(d, 'metadata.json')
    if not os.path.exists(mp):
        skipped.append((item, 'no metadata.json')); continue
    try:
        raw = open(mp, 'rb').read(); m = json.loads(raw)
    except Exception as e:
        skipped.append((item, f'unreadable metadata: {e}')); continue
    if m.get('isDeleted'):
        skipped.append((item, 'in Eagle trash')); continue
    ext = (m.get('ext') or '').lower()
    if ext not in IMG:
        skipped.append((item, f'not an image ({ext or "no ext"})')); continue
    orig = os.path.join(d, f"{m.get('name')}.{m.get('ext')}")
    if not os.path.exists(orig):
        c = [f for f in os.listdir(d) if f != 'metadata.json' and not f.endswith('_thumbnail.png')]
        if not c:
            skipped.append((item, 'original file missing')); continue
        orig = os.path.join(d, c[0])
    fids = m.get('folders') or []
    paths = [fpath[f] for f in fids if f in fpath]
    dangling += len(fids) - len(paths)
    home = os.path.join(DST, 'Eagle', LIB, paths[0]) if paths else os.path.join(DST, 'inbox')
    os.makedirs(home, exist_ok=True)
    name = clean(m.get('name'))
    if not name or is_generic(name):
        derived = name_from_url(m.get('url'))
        name = clean(derived) if derived else now.strftime('%Y%m%d-%H%M%S') + '-' + ulid()[-4:].lower()
        renamed += 1
    fname = unique(home, name, ext)
    try:
        shutil.copy2(orig, os.path.join(home, fname))
    except Exception as e:
        skipped.append((item, f'copy failed (not downloaded?): {e}')); continue
    open(os.path.join(home, fname + '.eagle.json'), 'wb').write(raw)
    fm = ['---', f'id: {ulid()}', f'title: {q(m.get("name") or fname)}']
    if m.get('tags'): fm.append(f'tags: {q(m["tags"])}')
    if m.get('star'): fm.append(f'rating: {int(m["star"])}')
    if m.get('url'): fm.append(f'source: {q(m["url"])}')
    if m.get('width') and m.get('height'): fm += [f'width: {m["width"]}', f'height: {m["height"]}']
    fm += [f'imported: {NOW}', 'sources:', '  - type: eagle', f'    library: {q(LIB)}',
           f'    item: {q(m.get("id") or item)}', f'    folders: {q(paths)}', f'    imported: {NOW}',
           '    importer: "eagle-import prototype 0.2"', f'    raw: {q(fname + ".eagle.json")}', '---', '']
    rel = os.path.relpath(os.path.join(home, fname), DST)
    notes.append((os.path.join(home, fname + '.md'), fm, (m.get('annotation') or '').strip(), fname, rel))
    for p in (paths or ['(no Eagle folder)']):
        members.setdefault(p, []).append(rel)

# Write notes last, so the self-embed (FORMAT §3.2) can use the bare name only when it is unique.
names = Counter(f.lower() for dp, dn, fn in os.walk(DST) for f in fn
                if not any(part.startswith('.') for part in os.path.relpath(dp, DST).split(os.sep) if part != '.'))
for path, fm, body, fname, rel in notes:
    embed = fname if names[fname.lower()] == 1 else rel
    text = '\n'.join(fm) + '\n' + (body + '\n\n' if body else '') + f'![[{embed}]]\n'
    open(path, 'w').write(text)

cdir = os.path.join(DST, 'Eagle', LIB); os.makedirs(cdir, exist_ok=True)
lines = ['---', 'kind: collection', f'title: {q(LIB + " (Eagle import)")}', 'cssclasses: [dimagine-gallery]', '---', '',
         f'All images imported from the Eagle library {LIB}, grouped by their Eagle folder.', '']
for p in sorted(members, key=lambda x: (x.startswith('('), x)):
    lines += [f'## {p} ({len(members[p])})', ''] + [f'![[{r}]]' for r in members[p]] + ['']
open(os.path.join(cdir, f'{LIB}.md'), 'w').write('\n'.join(lines))

rep = ['---', f'title: {q("Import report " + LIB)}', f'imported: {NOW}', '---', '', f'# Import report: Eagle → {LIB}', '',
       f'- Source: Eagle library `{os.path.basename(SRC)}` (read only, not modified)', f'- Imported: {len(notes)}',
       f'- Skipped: {len(skipped)}', f'- Renamed (no meaningful name): {renamed}',
       f'- Folder references that pointed to deleted Eagle folders: {dangling}', '',
       '| Eagle item | Reason skipped |', '|---|---|'] + [f'| {i} | {r} |' for i, r in skipped]
open(os.path.join(cdir, f'_import-{LIB}-{now.strftime("%Y%m%d")}.md'), 'w').write('\n'.join(rep) + '\n')

# Optional viewing aid for Obsidian: lays out collection embeds as a grid. Safe to delete.
sn = os.path.join(DST, '.obsidian', 'snippets'); os.makedirs(sn, exist_ok=True)
open(os.path.join(sn, 'dimagine-gallery.css'), 'w').write(
    '.dimagine-gallery .image-embed { display:inline-block; width:24%; margin:0.4%; vertical-align:top; }\n'
    '.dimagine-gallery .image-embed img { width:100%; height:auto; border-radius:4px; }\n')
apj = os.path.join(DST, '.obsidian', 'appearance.json')
if not os.path.exists(apj):
    json.dump({'enabledCssSnippets': ['dimagine-gallery']}, open(apj, 'w'))

print(json.dumps({'dst': DST, 'imported': len(notes), 'skipped': len(skipped),
                  'skip_reasons': Counter(r.split(':')[0].split(' (')[0] for _, r in skipped),
                  'renamed': renamed, 'dangling_folder_refs': dangling,
                  'folders': {k: len(v) for k, v in members.items()}}, ensure_ascii=False, indent=1))
