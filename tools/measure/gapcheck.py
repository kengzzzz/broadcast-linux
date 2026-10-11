"""Silent gaps (over 2 ms of exact zeros) in the processed channel of a stereo float32
48 kHz recording: left = input, right = processed. Gaps where the input was also near
silent are the denoiser muting a quiet passage. The rest are dropouts. Needs numpy.
"""
import sys

import numpy as np

RATE = 48000
QUIET_DB = -60

x = np.fromfile(sys.argv[1], dtype="<f4").astype(np.float64).reshape(-1, 2)[RATE:]
a, b = x[:, 0], x[:, 1]
n = 1 << 22
m = min(n, len(a))
xc = np.fft.irfft(np.fft.rfft(b[:m], n) * np.conj(np.fft.rfft(a[:m], n)), n)
lag = int(np.argmax(np.abs(xc[:RATE])))
o, s = b[lag:], a[: len(b) - lag]

z = (np.abs(o) < 1e-7).astype(int)
runs = np.diff(np.r_[0, z, 0])
starts, ends = np.where(runs == 1)[0], np.where(runs == -1)[0]
quiet, dropouts = [], []
for st, en in zip(starts, ends):
    if en - st <= 96:
        continue
    rms = np.sqrt(np.mean(s[st:en] ** 2)) if en > st else 0.0
    level = 20 * np.log10(rms + 1e-12)
    (quiet if level < QUIET_DB else dropouts).append(((en - st) / 48, level, st / RATE))
print(
    f"{sys.argv[1].rsplit('/', 1)[-1]}: lag {lag / 48:.1f} ms, dropouts {len(dropouts)}, "
    f"muted quiet input {len(quiet)}"
    + "".join(f"\n  dropout {d:.1f} ms at {t:.2f} s, input {lv:.0f} dBFS" for d, lv, t in dropouts)
)
