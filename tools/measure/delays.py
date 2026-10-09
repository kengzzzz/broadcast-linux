"""Prints the delay of the right channel behind the left in each 5 s of a stereo
float32 48 kHz recording, so a delay that changes during a run shows up. Needs numpy.
"""
import sys

import numpy as np

RATE = 48000
CHUNK = 5 * RATE

x = np.fromfile(sys.argv[1], dtype="<f4").astype(np.float64).reshape(-1, 2)
delays = []
for start in range(0, len(x) - CHUNK + 1, CHUNK):
    s, o = x[start : start + CHUNK, 0], x[start : start + CHUNK, 1]
    n = 1 << 19
    xc = np.fft.irfft(np.fft.rfft(o, n) * np.conj(np.fft.rfft(s, n)), n)
    delays.append(f"{int(np.argmax(np.abs(xc[: RATE // 2]))) / 48:.1f}")
print("delay per 5 s:", " ".join(delays), "ms")
