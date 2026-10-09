#!/usr/bin/env python3
"""Writes the raw BGR24 1080p frames of a file to stdout in a loop at 30 fps for the
given number of seconds."""
import sys
import time

FRAME_BYTES = 1920 * 1080 * 3

with open(sys.argv[1], "rb") as f:
    frames = []
    while len(frame := f.read(FRAME_BYTES)) == FRAME_BYTES:
        frames.append(frame)
end = time.monotonic() + float(sys.argv[2])
i = 0
try:
    while time.monotonic() < end:
        start = time.monotonic()
        sys.stdout.buffer.write(frames[i % len(frames)])
        sys.stdout.buffer.flush()
        i += 1
        time.sleep(max(0.0, 1 / 30 - (time.monotonic() - start)))
except BrokenPipeError:
    pass
