#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <sys/ioctl.h>
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

int main(int argc, char **argv) {
    if (argc < 5 || (argc - 2) % 3) {
        fprintf(stderr, "Usage: %s CUSHION_MS EFFECT MODEL_PATH INTENSITY [EFFECT MODEL_PATH INTENSITY]...\n",
                argv[0]);
        return 2;
    }
    unsigned int cushion_ms = (unsigned int)atoi(argv[1]);
    int count = (argc - 2) / 3;
    setvbuf(stdin, NULL, _IONBF, 0);
    /* a small output pipe pushes any backlog back to stdin, where drop_backlog discards it */
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
        if ((status = load(st->effect))) {
            fprintf(stderr, "Load(%s) failed: %d\n", st->name, status);
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
    /* the first runs of each model are several times slower; take that hit before audio flows */
    for (int n = 0; n < WARMUP_FRAMES; n++) {
        const float *cur = in_buf;
        for (int i = 0; i < count; i++) {
            const float *in_ptrs[1] = {cur};
            float *out_ptrs[1] = {stages[i].out};
            run(stages[i].effect, in_ptrs, out_ptrs, frame, 1);
            cur = stages[i].out;
        }
    }
    fprintf(stderr, "%s ready; %u samples per frame at 48 kHz mono f32\n", names, frame);

    LARGE_INTEGER freq, t0, t1;
    QueryPerformanceFrequency(&freq);
    unsigned long frames = 0, failures = 0, dropped = 0;

    dropped = drop_backlog(in_buf, frame);
    fprintf(stderr, "Dropped %lu stale frames queued during startup\n", dropped);
    dropped = 0;
    unsigned int cushion = cushion_ms * 48;

    while (read_full(in_buf, frame)) {
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
        if (!frames && cushion) {
            /* output only arrives in whole frames; queue silence ahead of the first one so
             * the reader keeps a fixed slack that absorbs run-time jitter */
            float *silence = calloc(cushion, sizeof(float));
            fwrite(silence, sizeof(float), cushion, stdout);
            free(silence);
        }
        if (fwrite(cur, sizeof(float), frame, stdout) != frame)
            break;
        fflush(stdout);
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
