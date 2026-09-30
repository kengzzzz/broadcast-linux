"""Analyse a stereo float32 48 kHz recording: left = mic input, right = virtual mic.

Prints the end-to-end delay, how much quiet (noise-only) frames were reduced,
silent gaps in the output, and, for recordings of two minutes or more, the delay
per minute so clock drift would show up. Needs numpy.
"""
import sys

import numpy as np

RATE = 48000

x = np.fromfile(sys.argv[1], dtype="<f4").astype(np.float64).reshape(-1, 2)[RATE:]
a, b = x[:, 0], x[:, 1]


def delay(s, o, n=1 << 22):
    m = min(n, len(s))
    xc = np.fft.irfft(np.fft.rfft(o[:m], n) * np.conj(np.fft.rfft(s[:m], n)), n)
    lag = int(np.argmax(np.abs(xc[:RATE])))
    corr = xc[lag] / np.sqrt((s[:m] ** 2).sum() * (o[:m] ** 2).sum())
    return lag, corr


def gaps(o):
    z = (np.abs(o) < 1e-7).astype(int)
    runs = np.diff(np.r_[0, z, 0])
    return [(e - s) / 48 for s, e in zip(np.where(runs == 1)[0], np.where(runs == -1)[0]) if e - s > 96]


lag, corr = delay(a, b)
print(f"recorded {len(a) / RATE:.0f} s; end-to-end delay {lag / 48:.1f} ms (correlation {corr:.3f})")
o, s = b[lag:], a[: len(b) - lag]
frame = 480
env = lambda v: np.sqrt(np.mean(v[: len(v) // frame * frame].reshape(-1, frame) ** 2, axis=1))
A, O = env(s), env(o)
db = lambda v: 20 * np.log10(v + 1e-12)
for p in (10, 50, 90):
    sel = (A >= np.percentile(A, p - 5)) & (A <= np.percentile(A, p + 5))
    print(f"input loudness percentile {p}: {db(A[sel].mean()):.1f} -> {db(O[sel].mean()):.1f} dBFS")
g = gaps(o)
print(f"silent gaps over 2 ms in the output: {len(g)} {[round(v, 1) for v in g[:5]]}")

minute = RATE * 60
if len(a) >= 2 * minute:
    print("minute  delay_ms  correlation")
    for start in range(0, len(a) - minute + 1, minute):
        lag, corr = delay(a[start : start + minute], b[start : start + minute], 1 << 23)
        print(f"{start // minute + 1:6d}  {lag / 48:8.1f}  {corr:.3f}")
