#!/usr/bin/env python3
"""Reads the virtual camera for 20 s and prints each broadcast-linux process's CPU use
(100% = one core) over seconds 8-18, plus the frames the reader received."""
import os
import re
import subprocess
import tempfile
import time

DEVICE = os.environ.get("DEVICE", "/dev/video10")
PATTERN = re.compile(r"broadcast-linux run|ffmpeg -nostdin|camera_stream|wineserver|winedevice")


def snapshot():
    ticks = {}
    for pid in filter(str.isdigit, os.listdir("/proc")):
        try:
            cmd = open(f"/proc/{pid}/cmdline", "rb").read().replace(b"\0", b" ").decode()
            if not PATTERN.search(cmd):
                continue
            fields = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
            name = cmd.split()[0].rsplit("/", 1)[-1]
            if "ffmpeg -nostdin" in cmd:
                name += " (capture)"
            ticks[pid] = (int(fields[11]) + int(fields[12]), name)
        except OSError:
            pass
    return ticks


with tempfile.NamedTemporaryFile(suffix=".progress") as progress:
    reader = subprocess.Popen(
        ["ffmpeg", "-loglevel", "error", "-f", "v4l2", "-i", DEVICE, "-t", "20",
         "-f", "null", "-", "-progress", progress.name],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(8)
    before, start = snapshot(), time.time()
    time.sleep(10)
    after, elapsed = snapshot(), time.time() - start
    hz = os.sysconf("SC_CLK_TCK")
    total = 0.0
    for pid, (ticks, name) in after.items():
        if pid in before:
            percent = (ticks - before[pid][0]) / hz / elapsed * 100
            total += percent
            if percent > 0.05:
                print(f"{percent:6.1f}%  {name}")
    print(f"{total:6.1f}%  total")
    reader.wait()
    frames = re.findall(r"frame=(\d+)", open(progress.name).read())
    print(f"reader: {frames[-1] if frames else 0} frames in 20 s")
