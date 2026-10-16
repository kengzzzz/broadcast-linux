#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <sys/ioctl.h>
#include <poll.h>
#include <windows.h>

typedef int (WINAPI *create_effect_fn)(const char *, void **);
typedef int (WINAPI *destroy_effect_fn)(void *);
typedef int (WINAPI *set_u32_fn)(void *, const char *, unsigned int);
typedef int (WINAPI *set_string_fn)(void *, const char *, const char *);
typedef int (WINAPI *set_float_fn)(void *, const char *, float);
typedef int (WINAPI *get_u32_fn)(void *, const char *, unsigned int *);
typedef int (WINAPI *load_fn)(void *);
typedef int (WINAPI *run_fn)(void *, const float **, float **, unsigned int, unsigned int);

static int read_full(float *buf, size_t count);

/* windows.h pulls in winsock.h, which redefines FIONREAD to the Windows value */
#define LINUX_FIONREAD 0x541B
#define WARMUP_FRAMES 10
/* Run on silence after this many 40 ms waits without input. */
#define IDLE_POLLS 3

static size_t pending_bytes(void) {
    int bytes = 0;
    if (ioctl(fileno(stdin), LINUX_FIONREAD, &bytes) < 0 || bytes < 0)
        return 0;
    return (size_t)bytes;
}

/* audio queued in the input pipe is pure added latency, so drop whole frames beyond one */
static unsigned long drop_backlog(float *scratch, unsigned int frame) {
    size_t frame_bytes = frame * sizeof(float);
    unsigned long dropped = 0;
    while (pending_bytes() >= 2 * frame_bytes) {
        if (!read_full(scratch, frame))
            break;
        dropped++;
    }
    return dropped;
}

static int read_full(float *buf, size_t count) {
    size_t done = 0;
    while (done < count) {
        size_t got = fread(buf + done, sizeof(float), count - done, stdin);
        if (!got)
            return 0;
        done += got;
    }
    return 1;
}

struct stage {
    const char *name;
    void *effect;
    float *out;
    double max_ms;
};

static void run_chain(struct stage *stages, int count, run_fn run, const float *in, unsigned int frame) {
    for (int i = 0; i < count; i++) {
        const float *in_ptrs[1] = {in};
        float *out_ptrs[1] = {stages[i].out};
        run(stages[i].effect, in_ptrs, out_ptrs, frame, 1);
        in = stages[i].out;
    }
}

/* With only an audio model on the GPU, the driver may keep switching power states, and runs
 * near a switch take up to 25 ms. Clearing 128 MB per frame usually holds P3 while audio
 * flows. Smaller clears made the switching worse. */
#define KEEPAWAKE_BYTES (128u << 20)
#define CU_STREAM_NON_BLOCKING 1

typedef int (WINAPI *memset_async_fn)(unsigned long long, unsigned int, size_t, void *);

struct keepawake {
    memset_async_fn memset_async;
    unsigned long long buf;
    void *stream;
};

/* Runs after the effects load, so the context NVIDIA's runtime made current is reused. */
static int keepawake_start(struct keepawake *k) {
    typedef int (WINAPI *get_ctx_fn)(void **);
    typedef int (WINAPI *retain_fn)(void **, int);
    typedef int (WINAPI *set_ctx_fn)(void *);
    typedef int (WINAPI *alloc_fn)(unsigned long long *, size_t);
    typedef int (WINAPI *stream_fn)(void **, unsigned int);
    HMODULE cuda = LoadLibraryA("nvcuda.dll");
    if (!cuda)
        return -1;
    get_ctx_fn get_ctx = (get_ctx_fn)GetProcAddress(cuda, "cuCtxGetCurrent");
    retain_fn retain = (retain_fn)GetProcAddress(cuda, "cuDevicePrimaryCtxRetain");
    set_ctx_fn set_ctx = (set_ctx_fn)GetProcAddress(cuda, "cuCtxSetCurrent");
    alloc_fn alloc = (alloc_fn)GetProcAddress(cuda, "cuMemAlloc_v2");
    stream_fn stream_create = (stream_fn)GetProcAddress(cuda, "cuStreamCreate");
    k->memset_async = (memset_async_fn)GetProcAddress(cuda, "cuMemsetD32Async");
    if (!get_ctx || !retain || !set_ctx || !alloc || !stream_create || !k->memset_async)
        return -1;
    void *ctx = NULL;
    if ((get_ctx(&ctx) || !ctx) && (retain(&ctx, 0) || set_ctx(ctx)))
        return -1;
    return alloc(&k->buf, KEEPAWAKE_BYTES) || stream_create(&k->stream, CU_STREAM_NON_BLOCKING) ? -1 : 0;
}

int main(int argc, char **argv) {
    const char *profile_model = NULL;
    int profile_id = 0, offline = 0;
    while (argc > 1 && !strncmp(argv[1], "--", 2)) {
        if (!strcmp(argv[1], "--profile") && argc > 3) {
            profile_model = argv[2];
            profile_id = atoi(argv[3]);
            argv += 3;
            argc -= 3;
        } else if (!strcmp(argv[1], "--offline")) {
            offline = 1;
            argv++;
            argc--;
        } else {
            argc = 0;
        }
    }
    if (argc < 5 || (argc - 2) % 3) {
        fprintf(stderr, "Usage: afx_stream.exe [--profile MODEL_PATH ID] [--offline] KEEP_AWAKE\n"
                        "           EFFECT MODEL_PATH INTENSITY [EFFECT MODEL_PATH INTENSITY]...\n"
                        "--profile sets the Studio Voice mic profile. --offline processes every frame of a file:\n"
                        "nothing is dropped and the GPU is not kept awake.\n");
        return 2;
    }
    int keep_awake = offline ? 0 : atoi(argv[1]);
    int count = (argc - 2) / 3;
    setvbuf(stdin, NULL, _IONBF, 0);
    /* a small output pipe pushes any backlog back to stdin, where drop_backlog discards it */
    if (!offline)
        fcntl(fileno(stdout), F_SETPIPE_SZ, 8192);

    HMODULE afx = LoadLibraryA("NVAudioEffects.dll");
    if (!afx) {
        fprintf(stderr, "LoadLibrary(NVAudioEffects.dll) failed: %u\n", GetLastError());
        return 1;
    }
    create_effect_fn create = (create_effect_fn)GetProcAddress(afx, "NvAFX_CreateEffect");
    destroy_effect_fn destroy = (destroy_effect_fn)GetProcAddress(afx, "NvAFX_DestroyEffect");
    set_u32_fn set_u32 = (set_u32_fn)GetProcAddress(afx, "NvAFX_SetU32");
    set_string_fn set_string = (set_string_fn)GetProcAddress(afx, "NvAFX_SetString");
    set_float_fn set_float = (set_float_fn)GetProcAddress(afx, "NvAFX_SetFloat");
    get_u32_fn get_u32 = (get_u32_fn)GetProcAddress(afx, "NvAFX_GetU32");
    load_fn load = (load_fn)GetProcAddress(afx, "NvAFX_Load");
    run_fn run = (run_fn)GetProcAddress(afx, "NvAFX_Run");
    if (!create || !destroy || !set_u32 || !set_string || !set_float || !get_u32 || !load || !run) {
        fprintf(stderr, "Required audio effect exports are missing\n");
        return 1;
    }

    struct stage *stages = calloc(count, sizeof(struct stage));
    unsigned int frame = 0;
    int status;
    char names[256] = "";
    for (int i = 0; i < count; i++) {
        struct stage *st = &stages[i];
        const char *model = argv[3 + 3 * i];
        float intensity = (float)atof(argv[4 + 3 * i]);
        st->name = argv[2 + 3 * i];
        status = create(st->name, &st->effect);
        if (status || !st->effect) {
            fprintf(stderr, "CreateEffect(%s) failed: %d\n", st->name, status);
            return 3;
        }
        set_string(st->effect, "model_path", model);
        set_u32(st->effect, "sample_rate", 48000);
        if ((status = set_float(st->effect, "intensity_ratio", intensity)))
            fprintf(stderr, "%s: intensity not supported (%d); ignored\n", st->name, status);
        int profiled = profile_model && !strcmp(st->name, "studio_voice_low_latency");
        if (profiled && (status = set_string(st->effect, "mic_profile_model", profile_model))) {
            fprintf(stderr, "%s: setting the mic profile model failed: %d\n", st->name, status);
            return 4;
        }
        if ((status = load(st->effect))) {
            fprintf(stderr, "Load(%s) failed: %d\n", st->name, status);
            return 4;
        }
        if (profiled && (status = set_u32(st->effect, "mic_profile_id", (unsigned)profile_id))) {
            fprintf(stderr, "%s: mic profile %d failed: %d\n", st->name, profile_id, status);
            return 4;
        }
        unsigned int in = 0, out = 0;
        get_u32(st->effect, "num_input_samples_per_frame", &in);
        get_u32(st->effect, "num_output_samples_per_frame", &out);
        if (!in || in != out || (frame && in != frame)) {
            fprintf(stderr, "%s: frame size %u in, %u out; the chain needs %u\n", st->name, in, out,
                    frame ? frame : in);
            return 5;
        }
        frame = in;
        st->out = calloc(frame, sizeof(float));
        if (i)
            strncat(names, "+", sizeof(names) - strlen(names) - 1);
        strncat(names, st->name, sizeof(names) - strlen(names) - 1);
    }
    float *in_buf = calloc(frame, sizeof(float));
    float *silence = calloc(frame, sizeof(float));
    /* the first runs of each model are several times slower; take that hit before audio flows */
    for (int n = 0; n < WARMUP_FRAMES; n++)
        run_chain(stages, count, run, silence, frame);
    struct keepawake keepawake = {0};
    if (keep_awake && keepawake_start(&keepawake)) {
        keepawake.memset_async = NULL;
        fprintf(stderr, "Could not start the GPU keep-awake; runs may be slow while the GPU idles\n");
    }
    fprintf(stderr, "%s ready; %u samples per frame at 48 kHz mono f32\n", names, frame);

    LARGE_INTEGER freq, t0, t1;
    QueryPerformanceFrequency(&freq);
    unsigned long frames = 0, failures = 0, dropped = 0;

    if (!offline) {
        dropped = drop_backlog(in_buf, frame);
        fprintf(stderr, "Dropped %lu stale frames queued during startup\n", dropped);
        dropped = 0;
    }

    for (;;) {
        /* With no input until the model unloads, run on silence so the GPU stays awake.
         * An idle GPU stalls 20 to 30 ms when audio returns. */
        if (keepawake.memset_async) {
            struct pollfd input = {fileno(stdin), POLLIN, 0};
            for (int polls = 1; poll(&input, 1, 40) == 0; polls++) {
                if (polls >= IDLE_POLLS) {
                    run_chain(stages, count, run, silence, frame);
                    keepawake.memset_async(keepawake.buf, 0, KEEPAWAKE_BYTES / 4, keepawake.stream);
                }
            }
        }
        if (!read_full(in_buf, frame))
            break;
        if (!offline)
            dropped += drop_backlog(in_buf, frame);
        const float *cur = in_buf;
        for (int i = 0; i < count; i++) {
            struct stage *st = &stages[i];
            const float *in_ptrs[1] = {cur};
            float *out_ptrs[1] = {st->out};
            QueryPerformanceCounter(&t0);
            status = run(st->effect, in_ptrs, out_ptrs, frame, 1);
            QueryPerformanceCounter(&t1);
            double ms = (t1.QuadPart - t0.QuadPart) * 1000.0 / freq.QuadPart;
            if (ms > st->max_ms)
                st->max_ms = ms;
            if (status) {
                if (!failures++)
                    fprintf(stderr, "Run(%s) failed at frame %lu: %d\n", st->name, frames, status);
                memcpy(st->out, cur, frame * sizeof(float));
            }
            cur = st->out;
        }
        if (fwrite(cur, sizeof(float), frame, stdout) != frame)
            break;
        fflush(stdout);
        if (keepawake.memset_async)
            keepawake.memset_async(keepawake.buf, 0, KEEPAWAKE_BYTES / 4, keepawake.stream);
        if (++frames % 250 == 0) {
            fprintf(stderr, "Processed %lu frames, failures %lu, dropped %lu, max run", frames,
                    failures, dropped);
            for (int i = 0; i < count; i++) {
                fprintf(stderr, " %s %.2f ms", stages[i].name, stages[i].max_ms);
                stages[i].max_ms = 0;
            }
            fprintf(stderr, "\n");
        }
    }

    fprintf(stderr, "Stopped after %lu frames, failures %lu\n", frames, failures);
    for (int i = 0; i < count; i++)
        destroy(stages[i].effect);
    FreeLibrary(afx);
    return 0;
}
