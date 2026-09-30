#!/usr/bin/env python3
"""Writes a raw f32 mono file to stdout in 40 ms frames at real-time pace. The sleep is
relative to each write, so a stall is never followed by a burst (the worker drops
backlog)."""
import sys
import time

FRAME_BYTES = 1920 * 4

data = open(sys.argv[1], "rb").read()
for offset in range(0, len(data), FRAME_BYTES):
    start = time.monotonic()
    sys.stdout.buffer.write(data[offset:offset + FRAME_BYTES])
    sys.stdout.buffer.flush()
    time.sleep(max(0.0, 0.04 - (time.monotonic() - start)))
