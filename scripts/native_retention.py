#!/usr/bin/env python3
"""Opt-in native regression: one-shot launch, retained capture and private-bus death.

Run with an explicit candidate: python3 scripts/native_retention.py /path/to/lcu
Creates only disposable owned desktops in a fresh namespace. No main input or
permission changes. Requires the GTK fixture's dependencies. Failed cleanup keeps
its namespace and prints the recovery location rather than losing ownership data.
"""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time

BINARY = str(Path(sys.argv[1]).resolve())
REPO = Path(__file__).resolve().parents[1]
ROOT = Path(tempfile.mkdtemp(prefix="lcu-retention-"))
ENV = dict(os.environ, LCU_RUNTIME_DIR=str(ROOT / "run"), LCU_STATE_DIR=str(ROOT / "state"))


def cli(*args):
    result = subprocess.run([BINARY, *args], env=ENV, text=True, capture_output=True, timeout=90)
    if result.returncode:
        raise RuntimeError(f"{args}: {result.stderr}")
    return json.loads(result.stdout)


def request(session, value):
    session.stdin.write(json.dumps(value) + "\n")
    session.stdin.flush()
    # The whole native probe is externally timeout-bounded; no desktop input is sent.
    value = json.loads(session.stdout.readline())
    if "error" in value:
        raise RuntimeError(value["error"])
    return value


def capture(desktop, name):
    path = ROOT / f"{name}.png"
    reply = cli("screenshot", "--desktop", desktop, "--output", str(path))
    assert reply["kind"] == "observation", reply
    assert path.read_bytes().startswith(b"\x89PNG\r\n\x1a\n")
    return reply


def alive(identity):
    try:
        fields = Path(f"/proc/{identity['pid']}/stat").read_text().rsplit(")", 1)[1].split()
        return fields[0] not in ("Z", "X") and int(fields[19]) == identity["start"]
    except FileNotFoundError:
        return False


desktop = None
session = None
clean = False
print(f"Native retention namespace: {ROOT}", flush=True)
try:
    descriptor = cli("desktop", "create")["result"]
    desktop = descriptor["id"]
    launch = cli("desktop", "launch", desktop, "--cwd", str(REPO), "--",
                 "/usr/bin/python3", str(REPO / "scripts/desktop_fixture.py"),
                 "--state-file", str(ROOT / "fixture.json"), "--title", "LCU RETENTION REGRESSION")
    assert launch["temporary_claim_released"] is True
    capture(desktop, "one-shot")  # Exact create -> one-shot launch -> screenshot.
    info = cli("desktop", "show", desktop)["result"][0]
    assert not info["claimed"], info
    print("PASS exact one-shot workflow without an input claim", flush=True)

    session = subprocess.Popen([BINARY, "session", "--desktop", desktop], env=ENV,
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    request(session, {"command": "claim"})
    request(session, {"command": "release"})
    capture(desktop, "released")
    request(session, {"command": "claim"})
    session.stdin.close()
    assert session.wait(timeout=10) == 0
    session = None
    # Client exit precedes asynchronous worker cleanup. Allow that barrier to finish.
    deadline = time.monotonic() + 10
    while True:
        try:
            capture(desktop, "disconnected")
            assert not cli("desktop", "show", desktop)["result"][0]["claimed"]
            break
        except (RuntimeError, AssertionError):
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)
    print("PASS explicit release and real stdin EOF retain unclaimed capture", flush=True)
    cli("stop", "--desktop", desktop)
    try:
        capture(desktop, "halted")
    except RuntimeError:
        pass
    else:
        raise AssertionError("Emergency stop silently reopened capture")
    print("PASS targeted emergency stop stays halted until a new claim", flush=True)

    session = subprocess.Popen([BINARY, "session", "--desktop", desktop], env=ENV,
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    request(session, {"command": "claim"})
    # The descriptor's recorded first service is the PRIVATE foreground bus.
    descriptor = json.loads((Path(descriptor["state"]) / "descriptor.json").read_text())
    record = descriptor["ownership"]
    assert record["boot"] == Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    bus = record["processes"][0]
    fd = os.pidfd_open(bus["pid"])
    try:
        assert alive(bus)
        argv = Path(f"/proc/{bus['pid']}/cmdline").read_bytes().split(b"\0")
        assert Path(os.fsdecode(argv[0])).name == "dbus-daemon", argv
        assert f"--address=unix:path={descriptor['runtime']}/session/bus".encode() in argv, argv
        signal.pidfd_send_signal(fd, signal.SIGKILL)
    finally:
        os.close(fd)
    for command in ("release", "claim"):
        try:
            request(session, {"command": command})
        except RuntimeError:
            pass
        else:
            raise AssertionError(f"{command} accepted unconfirmed private session cleanup")
    print("PASS unconfirmed portal cleanup refuses new ownership", flush=True)
    session.stdin.close()
    assert session.wait(timeout=10) == 0
    session = None
    destroyed = cli("desktop", "destroy", desktop)
    assert destroyed["kind"] == "destroyed", destroyed
    assert not Path(descriptor["runtime"]).exists()
    assert not Path(descriptor["state"]).exists()
    assert all(not alive(identity) for identity in record["processes"])
    assert (REPO / "Cargo.toml").is_file()
    desktop = None
    clean = True
    print("PASS private bus death -> owned destruction; recorded roots exited, resources removed", flush=True)
finally:
    if session is not None:
        session.stdin.close()
        session.wait(timeout=10)
    if desktop is not None:
        try:
            cli("desktop", "destroy", desktop, "--force")
            clean = True
        except Exception as error:
            print(f"Cleanup incomplete; preserve {ROOT}: {error}", file=sys.stderr)
    subprocess.run([BINARY, "daemon", "stop"], env=ENV, capture_output=True, timeout=10)
    if clean:
        shutil.rmtree(ROOT)
