# Benchmark

One PC: RTX 5080, Ryzen 9 9950X3D, Logitech BRIO, NVIDIA Broadcast build 58338310 on
both systems. Linux: driver 615.78, PipeWire 1.6. Windows 11: driver 616.92.

## Linux

### Audio delay, ms

| Path | min | avg | p95 | p99 |
|---|---|---|---|---|
| Mic, noise removal | 112.7 | 112.7 | 112.7 | 112.7 |
| Mic, Studio Voice | 92.3 | 92.5 | 92.8 | 93.0 |
| Mic, no effects | 5.3 | 5.3 | 5.3 | 5.3 |
| Speaker, noise removal | 112.7 | 112.7 | 112.7 | 112.7 |
| Speaker, no effects | 5.3 | 5.3 | 5.3 | 5.3 |

Noise removal is 70 ms model look-ahead + 40 ms model frame + 2.7 ms: one 5.3 ms period
of worker time, less half a period ([why](architecture.md#audio)). Other cases:

| Noise removal | Delay |
|---|---|
| First 10 s, or GPU slow or busy (camera effects on) | 118.0 |
| Forced 512-sample periods | 118.0 |
| Forced 960-sample periods, where frames end with a period | 110.0 |

### Camera delay, ms

| Stage | Delay |
|---|---|
| Webcam capture and delivery | ~33 |
| MJPEG decode | ~2.4 |
| GPU effects and transfers | ~10 |
| **Total** | **~46** |

1080p30 with video noise removal and Studio Light. `parallel_decode = "on"` cuts decode
to ~0.7 ms for about 30% more decode CPU. Eye Contact adds ~2.3 ms.

### Camera delay on a slower GPU, ms

Same effects with the GPU locked at 600 MHz. They take about 52 ms per frame instead of
10.7 ms, so the camera runs at about 19 fps.

| Decode | fps | avg | p50 | p95 |
|---|---|---|---|---|
| Right away (0.4.3) | 19.0 | 155.3 | 154.8 | 171.4 |
| Just before the GPU is free | 19.3 | 107.1 | 107.1 | 122.2 |

The difference is the time a frame waits for the GPU: 50.4 ms before, 3.0 ms now. At full
clock the GPU keeps up, and both give about 46.5 ms.

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

Most noise removal runs were 115.6 to 124 ms. Two runs at 173 ms raise the average.

### Resources

| Path | CPU, % of one core | GPU power, W |
|---|---|---|
| Idle, every effect off | 10.5 | 17 |
| Mic, noise removal | 16.5 | 19 |
| Camera 1080p30, video noise removal + Studio Light | 31.7 | 106 |

## How it was measured

- **Delay:** processed audio behind raw audio, per 5 s window of every run. Waveform
  cross-correlation, except Studio Voice, which regenerates the voice and uses loudness
  envelopes.
- **Runs:** 30 s or longer. Linux: `tools/measure`, digital, a fresh instance per run,
  recording from 15 s after the app links. Headphones held against the mic gave the same
  noise removal and Studio Voice delay. Windows: `tools/winbench`, headphones against the mic, Broadcast left running.
- **Dropouts:** with rtkit, no gap over 2 ms at a fixed buffer size, in 30 min runs,
  digital and on the real mic, also beside the camera effects, and 10 min under a full CPU
  load.
- **CPU:** summed over every process doing the work. **GPU:** the whole card's power
  draw, since utilization % changes with the clock.
- **Keep-awake:** Linux's mic GPU power includes it. It usually holds the GPU at P3 while
  the model is loaded. Windows stays at P5 without late frames.
- **Camera:** 29.8 fps on Linux, 29.5 fps on Windows. Slower GPU: `tools/camera-latency`,
  30 s each, from the webcam's capture timestamp to the processed frame, GPU locked with
  `nvidia-smi -lgc 600,600`.
- **Other GPUs:** RTX 20, 30 and 40 aren't measured. Their audio models are separate
  builds of the same versions, so the look-ahead should match. A slower GPU may need a
  longer reserve.
