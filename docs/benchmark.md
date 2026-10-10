# Benchmark

One PC: RTX 5080, Ryzen 9 9950X3D, Logitech BRIO, NVIDIA Broadcast build 58338310 on
both systems. Linux: driver 615.78, PipeWire 1.6. Windows 11: driver 616.92.

## Linux

### Audio delay, ms

| Path | min | avg | p95 | p99 |
|---|---|---|---|---|
| Mic, noise removal | 115.3 | 115.3 | 115.3 | 115.3 |
| Mic, Studio Voice | 89.2 | 90.3 | 90.8 | 90.8 |
| Mic, no effects | 5.3 | 5.3 | 5.3 | 5.3 |
| Speaker, noise removal | 115.3 | 115.3 | 115.3 | 115.3 |
| Speaker, no effects | 5.3 | 5.3 | 5.3 | 5.3 |

Noise removal is 70 ms model look-ahead + 40 ms model frame + 5.3 ms reserve (one
256-sample period). Studio Voice gets two periods. A forced larger period raises the
reserve: 121 ms at 512 samples, 130 ms at 960.

### Camera delay, ms

| Stage | Delay |
|---|---|
| Webcam capture and delivery | ~33 |
| MJPEG decode | ~2.4 |
| GPU effects and transfers | ~10 |
| **Total** | **~46** |

1080p30 with video noise removal and Studio Light. `parallel_decode = "on"` cuts decode
to ~0.7 ms for about 30% more decode CPU. Eye Contact adds ~2.3 ms.

### Resources

| Path | CPU, % of one core | GPU power, W |
|---|---|---|
| Idle, every effect off | 0.4 | 26 |
| Mic, noise removal | 2.5 | 39 |
| Camera 1080p30, video noise removal + Studio Light | 15.6 | 113 |

## Windows, NVIDIA Broadcast

### Audio delay, ms

| Path | min | avg | p95 | p99 |
|---|---|---|---|---|
| Mic, noise removal | 115.6 | 145.6 | 173.1 | 173.1 |
| Mic, Studio Voice | 91.5 | 138.6 | 149.2 | 149.2 |
| Mic, no effects | 42.4 | 42.6 | 43.2 | 43.4 |
| Speaker, noise removal | 151.0 | 151.0 | 151.1 | 151.1 |
| Speaker, no effects | 80.8 | 86.4 | 91.0 | 91.0 |

The min is the fastest Broadcast managed. Most of its noise removal runs were 115.6 to
124 ms, and the average is higher because of two longer runs at 173 ms.

### Resources

| Path | CPU, % of one core | GPU power, W |
|---|---|---|
| Idle, every effect off | 10.5 | 17 |
| Mic, noise removal | 16.5 | 19 |
| Camera 1080p30, video noise removal + Studio Light | 31.7 | 106 |

## How it was measured

- Delay is the processed audio behind the raw audio, over every 5 s window of every run.
  Studio Voice regenerates the voice, so its delay comes from loudness envelopes. The
  others come from waveform cross-correlation.
- Every run is 30 s or longer. Linux: `tools/measure`, each run in a fresh instance. Mic
  noise removal and Studio Voice mixed sound played from headphones held against the mic
  with a digital feed, and both gave the same delay. The rest were digital. Windows:
  `tools/winbench`, headphones against the mic, with Broadcast left running between runs.
- CPU is summed over every process doing the work. GPU is the whole card's power draw,
  because utilization % changes with the clock and can't be compared.
- Linux's mic GPU power includes the keep-awake that holds the GPU at P3 while an app
  has the mic open. Windows stays at P5 without late frames.
- The camera ran at 29.8 fps on Linux and 29.5 fps on Windows.
- RTX 20, 30 and 40 cards aren't measured. Their audio models are separate builds of the
  same versions, so the look-ahead should match. A slower GPU may need a longer reserve.
