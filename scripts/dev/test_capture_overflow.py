"""Tests for the horizontal-overflow gate in capture.mjs.

A `check` job must fail the run when the page is laid out wider than
the emulated device width. Comparing `documentElement.scrollWidth` to
`window.innerWidth` alone lets broken mobile pages through:
shrink-to-fit inflates the two together (a 600px page on a 390px
device measures 608/608), which is how a too-wide toolbar shipped
while the gate reported "no overflow".
"""

import http.server
import json
import os
import re
import shutil
import subprocess
import tempfile
import threading
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
CAPTURE_MJS = SCRIPT_DIR / "capture.mjs"

CHROME_CANDIDATES = [
    os.environ.get("CHROME_BIN", ""),
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    str(Path.home() / "Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
]


def find_chrome():
    for candidate in CHROME_CANDIDATES:
        if not candidate:
            continue
        if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
            return candidate
        if shutil.which(candidate):
            return candidate
    return None


# The same viewport meta the serve app ships, so the stub pages lay out
# like the real ones under mobile emulation.
VIEWPORT = b"<meta name='viewport' content='width=device-width,initial-scale=1'>"
PAGES = {
    # Modestly too wide for a 390px phone: wide enough that shrink-to-fit
    # inflates the layout viewport (measured 608/608 at 390), narrow
    # enough that scrollWidth never exceeds innerWidth — the exact
    # shape the old innerWidth-only comparison could not see.
    "/wide": (
        b"<html><head>" + VIEWPORT + b"</head><body><div style='width:600px'>"
        b"row too wide for a 390px phone</div></body></html>"
    ),
    "/narrow": (
        b"<html><head>" + VIEWPORT + b"</head><body><main>fits</main></body></html>"
    ),
}


class StubHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = PAGES.get(self.path)
        if body is None:
            self.send_response(404)
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class CaptureOverflowTests(unittest.TestCase):
    def setUp(self):
        chrome = find_chrome()
        if chrome is None:
            self.skipTest(
                "Chrome/Chromium not found; set CHROME_BIN to run this test"
            )
        self.chrome = chrome
        node = shutil.which("node")
        if node is None:
            self.skipTest("node is required to drive Chrome over the DevTools Protocol")
        node_major = int(subprocess.run(
            [node, "-p", "process.versions.node.split('.')[0]"],
            capture_output=True,
            text=True,
            timeout=10,
        ).stdout.strip() or 0)
        if node_major < 22:
            self.skipTest(f"node 22 or newer is required (found {node_major})")
        self.node = node
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), StubHandler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def tearDown(self):
        server = getattr(self, "server", None)
        if server is not None:
            server.shutdown()
            server.server_close()

    def _run_capture(self, page, width, height, out=True):
        """Run capture.mjs on one check job; return (rc, stdout, stderr,
        shot_written)."""
        port = self.server.server_address[1]
        with tempfile.TemporaryDirectory(prefix="overflow-test-") as temp:
            shot = Path(temp) / "shot.png"
            job = {
                "url": f"http://127.0.0.1:{port}{page}",
                "width": width,
                "height": height,
                "theme": "light",
                "check": True,
            }
            if out:
                job["out"] = str(shot)
            jobs = {"chrome": self.chrome, "jobs": [job]}
            jobs_file = Path(temp) / "jobs.json"
            jobs_file.write_text(json.dumps(jobs))
            proc = subprocess.Popen(
                [self.node, str(CAPTURE_MJS), str(jobs_file)],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            try:
                stdout, stderr = proc.communicate(timeout=90)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.communicate()
                self.fail("capture.mjs never finished")
            # The temp dir (and the shot with it) vanishes on return, so
            # whether the shot was written must be observed here.
            shot_written = out and shot.exists()
            return proc.returncode, stdout, stderr, shot_written

    def _check_line(self, stdout, width):
        match = re.search(
            rf"CHECK {width}px (\S+) inner=(\d+) scroll=(\d+) body=(\d+) (\S+)",
            stdout,
        )
        self.assertIsNotNone(match, f"no CHECK line in capture.mjs output: {stdout!r}")
        return match

    def test_wide_mobile_page_is_reported_as_overflow(self):
        rc, stdout, stderr, shot_written = self._run_capture("/wide", 390, 844)
        match = self._check_line(stdout, 390)
        inner, scroll, verdict = int(match.group(2)), int(match.group(3)), match.group(5)
        self.assertEqual(
            verdict, "OVERFLOW", f"the 600px page at 390px was not flagged: {stdout!r}"
        )
        self.assertEqual(rc, 1, "capture.mjs must exit 1 on horizontal overflow")
        self.assertIn(
            "capture: horizontal overflow at 390px",
            stderr,
            "the failure must be named on stderr",
        )
        # The shrink-to-fit signature itself: both metrics inflated past
        # the 390px device, which is why innerWidth alone never caught it.
        self.assertGreater(inner, 390, f"innerWidth not inflated: {stdout!r}")
        self.assertGreater(scroll, 390, f"scrollWidth not inflated: {stdout!r}")

    def test_fitting_mobile_page_passes_the_check(self):
        rc, stdout, stderr, shot_written = self._run_capture("/narrow", 390, 844)
        match = self._check_line(stdout, 390)
        self.assertEqual(match.group(5), "ok", f"a fitting page was flagged: {stdout!r}")
        self.assertEqual(rc, 0)
        self.assertTrue(shot_written, "the screenshot was not written")
        self.assertIn("no overflow", stdout)

    def test_fitting_desktop_page_passes_the_check(self):
        rc, stdout, stderr, shot_written = self._run_capture("/narrow", 900, 700)
        match = self._check_line(stdout, 900)
        self.assertEqual(match.group(5), "ok", f"a fitting page was flagged: {stdout!r}")
        self.assertEqual(rc, 0)


if __name__ == "__main__":
    unittest.main()
