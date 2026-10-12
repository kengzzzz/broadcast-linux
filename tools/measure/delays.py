"""Prints the delay of the right channel behind the left in each 5 s of a stereo
float32 48 kHz recording, so a delay that changes during a run shows up. Needs numpy.

The second line compares loudness envelopes instead of waveforms. Use it for Studio
Voice: it regenerates the voice, so its waveform also matches a pitch period off.
"""
import sys

import numpy as np

RATE = 48000
CHUNK = 5 * RATE


def envelope(v):
    k = np.ones(96) / 96
    e = np.convolve(np.convolve(np.abs(v), k, "same"), k, "same")
    return e - e.mean()


def lag(s, o):
    n = 1 << 19
    xc = np.fft.irfft(np.fft.rfft(o, n) * np.conj(np.fft.rfft(s, n)), n)
    return int(np.argmax(np.abs(xc[: RATE // 2]))) / 48


x = np.fromfile(sys.argv[1], dtype="<f4").astype(np.float64).reshape(-1, 2)
chunks = [x[start : start + CHUNK] for start in range(0, len(x) - CHUNK + 1, CHUNK)]
waves = [lag(c[:, 0], c[:, 1]) for c in chunks]
envelopes = [lag(envelope(c[:, 0]), envelope(c[:, 1])) for c in chunks]
print("delay per 5 s:", " ".join(f"{d:.1f}" for d in waves), "ms")
print("envelope delay per 5 s:", " ".join(f"{d:.1f}" for d in envelopes), "ms")
