#!/usr/bin/env python3
"""Exercise the pinned companion with the shipped protocol configuration."""

import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def main():
    root = Path(__file__).resolve().parent.parent
    binary = Path(sys.argv[1]).resolve(strict=True)
    contract = dict(
        line.split("=", 1)
        for line in (root / "config/mediamtx.lock").read_text().splitlines()
        if line and not line.startswith("#")
    )
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == contract["sha256"]
    assert subprocess.check_output([binary, "--version"], text=True).strip() == contract["version"]
    env = {key: value for key, value in os.environ.items() if not key.startswith("MTX_")}
    # Change only supported listener addresses, never the protocol switches.
    ports = {}
    reservations = []
    for name in ["API", "METRICS", "PLAYBACK", "RTSP", "HLS", "WEBRTC"]:
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        reservations.append(listener)
        ports[name] = listener.getsockname()[1]
        env[f"MTX_{name}ADDRESS"] = f"127.0.0.1:{ports[name]}"
    udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    udp.bind(("127.0.0.1", 0))
    env["MTX_WEBRTCLOCALUDPADDRESS"] = f"127.0.0.1:{udp.getsockname()[1]}"
    for listener in reservations:
        listener.close()
    udp.close()
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with tempfile.TemporaryDirectory(prefix="sentinel-companion-smoke-") as fixture:
        fixture = Path(fixture)
        log_path = fixture / "companion.log"
        env["MTX_PATHDEFAULTS_RECORDPATH"] = str(fixture / "recordings/%path/%Y-%m-%d_%H-%M-%S-%f")
        with log_path.open("wb") as log:
            process = subprocess.Popen(
                [binary, root / "config/mediamtx.yml"], cwd=fixture, env=env,
                stdout=log, stderr=subprocess.STDOUT,
            )
            try:
                deadline = time.monotonic() + 20
                while time.monotonic() < deadline:
                    if process.poll() is not None:
                        raise AssertionError(log_path.read_text())
                    try:
                        with opener.open(f"http://127.0.0.1:{ports['API']}/v3/info", timeout=1) as response:
                            assert json.load(response)["version"] == contract["version"]
                        break
                    except (urllib.error.URLError, TimeoutError):
                        time.sleep(0.1)
                else:
                    raise AssertionError("companion API did not become ready")
                text = log_path.read_text()
                for protocol in ["RTMP", "SRT", "MoQ"]:
                    assert f"[{protocol}]" not in text, text
                for protocol in ["RTSP", "HLS", "WebRTC", "API", "metrics", "playback"]:
                    assert f"[{protocol}] started with listener" in text, text
                assert not (fixture / "auto.key").exists()
                assert not (fixture / "auto.crt").exists()
            finally:
                if process.poll() is None:
                    process.send_signal(signal.SIGTERM)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
            assert process.returncode == 0, log_path.read_text()
    print("Pinned companion config: supported listeners ready; RTMP/SRT/MoQ and automatic certificates absent; clean shutdown")


if __name__ == "__main__":
    main()
