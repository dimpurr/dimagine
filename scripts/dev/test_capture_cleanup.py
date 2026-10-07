"""Tests for capture.mjs signal cleanup.

A node process killed by a signal (timeout, Ctrl-C, a closed tmux
pane) must not leave a headless Chrome behind: capture.mjs has to
kill its Chrome, remove the profile dir, and exit with the
conventional 128+signal code.
"""

import http.server
import json
import os
import re
import shutil
import signal
import subprocess
import tempfile
import threading
import time
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


class StubHandler(http.server.BaseHTTPRequestHandler):
    """A page that loads slowly, so capture.mjs is still mid-job
    when the test signals it."""

    def do_GET(self):
        time.sleep(3)
        body = b"<html><body>stub</body></html>"
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def pid_alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def process_command(pid):
    try:
        result = subprocess.run(
            ["ps", "-p", str(pid), "-ww", "-o", "command="],
            capture_output=True,
            text=True,
            timeout=5,
        )
        return result.stdout.strip()
    except (subprocess.SubprocessError, OSError):
        return ""


class CaptureCleanupTests(unittest.TestCase):
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
            self.skipTest(
                f"node 22 or newer is required (found {node_major})"
            )
        self.node = node

    @staticmethod
    def _drain(stream, lines):
        for line in iter(stream.readline, ""):
            lines.append(line)

    def _wait_for_chrome_pid(self, proc, stderr_lines):
        deadline = time.time() + 20
        while time.time() < deadline:
            if proc.poll() is not None:
                self.fail(
                    "capture.mjs exited early with "
                    f"{proc.returncode}: {''.join(stderr_lines)}"
                )
            for line in list(stderr_lines):
                match = re.search(r"chrome pid (\d+)(?: profile (.*))?", line)
                if match:
                    return int(match.group(1)), match.group(2)
            time.sleep(0.1)
        self.fail("capture.mjs never reported its Chrome pid")

    def _run_signal_case(self, sig, expected_code):
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), StubHandler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            port = server.server_address[1]
            with tempfile.TemporaryDirectory(prefix="dimagine-capture-test-") as temp:
                jobs = Path(temp) / "jobs.json"
                shot = Path(temp) / "shot.png"
                jobs.write_text(
                    json.dumps(
                        {
                            "chrome": self.chrome,
                            "jobs": [
                                {
                                    "url": f"http://127.0.0.1:{port}/",
                                    "width": 800,
                                    "height": 600,
                                    "theme": "light",
                                    "out": str(shot),
                                    "check": False,
                                }
                            ],
                        }
                    )
                )

                proc = subprocess.Popen(
                    [self.node, str(CAPTURE_MJS), str(jobs)],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                stdout_lines = []
                stderr_lines = []
                threading.Thread(
                    target=self._drain, args=(proc.stdout, stdout_lines), daemon=True
                ).start()
                threading.Thread(
                    target=self._drain, args=(proc.stderr, stderr_lines), daemon=True
                ).start()
                try:
                    chrome_pid, profile = self._wait_for_chrome_pid(proc, stderr_lines)
                    self.assertTrue(
                        pid_alive(chrome_pid),
                        "Chrome was not running when capture.mjs reported its pid",
                    )
                    command = process_command(chrome_pid)
                    if profile:
                        self.assertIn(
                            profile,
                            command,
                            "the recorded pid does not look like the spawned Chrome",
                        )

                    proc.send_signal(sig)
                    self.assertEqual(
                        proc.wait(timeout=10),
                        expected_code,
                        f"capture.mjs did not exit {expected_code} on {sig.name}",
                    )

                    deadline = time.time() + 5
                    while time.time() < deadline and pid_alive(chrome_pid):
                        time.sleep(0.1)
                    self.assertFalse(
                        pid_alive(chrome_pid),
                        f"Chrome {chrome_pid} survived {sig.name} to capture.mjs",
                    )
                    if profile:
                        self.assertFalse(
                            Path(profile).exists(),
                            "the Chrome profile dir survived the signal",
                        )
                finally:
                    if proc.poll() is None:
                        proc.send_signal(signal.SIGTERM)
                        try:
                            proc.wait(timeout=10)
                        except subprocess.TimeoutExpired:
                            proc.kill()
                            proc.wait(timeout=10)
        finally:
            server.shutdown()
            server.server_close()

    def test_sigint_sigterm_sighup_teardown(self):
        for sig, expected_code in (
            (signal.SIGINT, 130),
            (signal.SIGTERM, 143),
            (signal.SIGHUP, 129),
        ):
            with self.subTest(signal=sig.name):
                self._run_signal_case(sig, expected_code)


if __name__ == "__main__":
    unittest.main()
