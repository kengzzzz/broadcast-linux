#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <unistd.h>
#include <windows.h>
#include "nvVideoEffects.h"

#define DEGREES (3.14159265f / 180.0f)
#define CU_CTX_SCHED_BLOCKING_SYNC 0x04
#define MAX_SLOTS 3

typedef int (WINAPI *create_fn)(const char *, NvVFX_Handle *);
typedef void (WINAPI *destroy_fn)(NvVFX_Handle);
typedef int (WINAPI *set_string_fn)(NvVFX_Handle, const char *, const char *);
typedef int (WINAPI *set_u32_fn)(NvVFX_Handle, const char *, unsigned);
typedef int (WINAPI *set_f32_fn)(NvVFX_Handle, const char *, float);
typedef int (WINAPI *set_stream_fn)(NvVFX_Handle, const char *, CUstream);
typedef int (WINAPI *stream_create_fn)(CUstream *);
typedef int (WINAPI *stream_destroy_fn)(CUstream);
typedef int (WINAPI *stream_sync_fn)(CUstream);
typedef int (WINAPI *load_fn)(NvVFX_Handle);
typedef int (WINAPI *alloc_state_fn)(NvVFX_Handle, NvVFX_StateObjectHandle *);
typedef int (WINAPI *free_state_fn)(NvVFX_Handle, NvVFX_StateObjectHandle);
typedef int (WINAPI *set_states_fn)(NvVFX_Handle, const char *, NvVFX_StateObjectHandle *);
typedef int (WINAPI *set_image_fn)(NvVFX_Handle, const char *, NvCVImage *);
typedef int (WINAPI *run_fn)(NvVFX_Handle, int);
typedef int (WINAPI *image_alloc_fn)(NvCVImage *, unsigned, unsigned, NvCVImage_PixelFormat,
                                     NvCVImage_ComponentType, unsigned, unsigned, unsigned);
typedef int (WINAPI *image_init_fn)(NvCVImage *, unsigned, unsigned, int, void *, NvCVImage_PixelFormat,
                                    NvCVImage_ComponentType, unsigned, unsigned);
typedef void (WINAPI *image_free_fn)(NvCVImage *);
typedef int (WINAPI *image_transfer_fn)(const NvCVImage *, NvCVImage *, float, CUstream, NvCVImage *);
typedef int (WINAPI *composite_fn)(const NvCVImage *, const NvCVImage *, const NvCVImage *, NvCVImage *,
                                   CUstream);

/* The few AR SDK (nvAR_defs.h) definitions used here; Broadcast ships the AR runtime in nvARPose.dll. */
typedef void *NvAR_Handle;
typedef struct { float x, y, width, height; } NvAR_Rect;
typedef struct { NvAR_Rect *boxes; uint8_t num_boxes, max_boxes; } NvAR_BBoxes;
#define AR_CONFIG(name) "NvAR_Parameter_Config_" #name
#define AR_INPUT(name) "NvAR_Parameter_Input_" #name
#define AR_OUTPUT(name) "NvAR_Parameter_Output_" #name
#define MAX_FACES 8

typedef int (WINAPI *ar_create_fn)(const char *, NvAR_Handle *);
typedef int (WINAPI *ar_destroy_fn)(NvAR_Handle);
typedef int (WINAPI *ar_load_fn)(NvAR_Handle);
typedef int (WINAPI *ar_run_fn)(NvAR_Handle);
typedef int (WINAPI *ar_set_string_fn)(NvAR_Handle, const char *, const char *);
typedef int (WINAPI *ar_set_u32_fn)(NvAR_Handle, const char *, unsigned);
typedef int (WINAPI *ar_set_s32_fn)(NvAR_Handle, const char *, int);
typedef int (WINAPI *ar_set_stream_fn)(NvAR_Handle, const char *, CUstream);
typedef int (WINAPI *ar_set_object_fn)(NvAR_Handle, const char *, void *, unsigned long);

typedef struct { int width, height; } NppiSize;
typedef struct { int x, y, width, height; } NppiRect;
#define NPPI_INTER_CUBIC 4
typedef int (WINAPI *npp_set_stream_fn)(CUstream);
typedef int (WINAPI *npp_resize_fn)(const uint8_t *, NppiSize, int, NppiRect, uint8_t *, int, NppiRect, double,
                                    double, double, double, int);

#define RESOLVE(type, var, dll, name) type var = (type)GetProcAddress(dll, name)
#define CHECK(name, expr) do { \
    status = (expr); \
    if (status != 0) { \
        fprintf(stderr, "%s failed: %d\n", name, status); \
        goto cleanup; \
    } \
} while (0)

/* Radiance .hdr (RGBE, flat or new-style run-length scanlines) to chunky RGB floats. */
static float *read_hdr(const char *path, unsigned *width, unsigned *height) {
    FILE *f = fopen(path, "rb");
    if (!f)
        return NULL;
    char line[256];
    int in_header = 1;
    while (in_header && fgets(line, sizeof line, f))
        in_header = line[0] != '\n';
    int w = 0, h = 0;
    if (in_header || !fgets(line, sizeof line, f) || sscanf(line, "-Y %d +X %d", &h, &w) != 2 ||
        w <= 0 || h <= 0) {
        fclose(f);
        return NULL;
    }
    float *rgb = malloc((size_t)w * h * 3 * sizeof(float));
    uint8_t *scan = malloc((size_t)w * 4);
    for (int y = 0; rgb && scan && y < h; y++) {
        uint8_t head[4];
        if (fread(head, 1, 4, f) != 4)
            goto fail;
        if (head[0] == 2 && head[1] == 2 && ((head[2] << 8) | head[3]) == w) {
            for (int c = 0; c < 4; c++) {
                for (int x = 0; x < w;) {
                    int count = fgetc(f);
                    if (count == EOF)
                        goto fail;
                    if (count > 128) {
                        count -= 128;
                        int value = fgetc(f);
                        if (value == EOF || x + count > w)
                            goto fail;
                        while (count--)
                            scan[4 * x++ + c] = (uint8_t)value;
                    } else {
                        if (count == 0 || x + count > w)
                            goto fail;
                        while (count--) {
                            int value = fgetc(f);
                            if (value == EOF)
                                goto fail;
                            scan[4 * x++ + c] = (uint8_t)value;
                        }
                    }
                }
            }
        } else {
            memcpy(scan, head, 4);
            if (fread(scan + 4, 4, w - 1, f) != (size_t)(w - 1))
                goto fail;
        }
        for (int x = 0; x < w; x++) {
            const uint8_t *p = scan + 4 * x;
            float scale = p[3] ? ldexpf(1.0f, p[3] - 136) : 0.0f;
            for (int c = 0; c < 3; c++)
                rgb[((size_t)y * w + x) * 3 + c] = p[c] * scale;
        }
    }
    free(scan);
    fclose(f);
    *width = (unsigned)w;
    *height = (unsigned)h;
    return rgb;
fail:
    free(scan);
    free(rgb);
    fclose(f);
    return NULL;
}

/* NVIDIA's effects synchronize internally; the default spin-wait burns a core while the GPU works.
 * Primary context flags only apply before the context starts, so this runs before any effect. */
static void use_blocking_sync(void) {
    typedef int (WINAPI *init_fn)(unsigned);
    typedef int (WINAPI *count_fn)(int *);
    typedef int (WINAPI *set_flags_fn)(int, unsigned);
    HMODULE cuda = LoadLibraryA("nvcuda.dll");
    if (!cuda)
        return;
    RESOLVE(init_fn, init, cuda, "cuInit");
    RESOLVE(count_fn, count, cuda, "cuDeviceGetCount");
    RESOLVE(set_flags_fn, set_flags, cuda, "cuDevicePrimaryCtxSetFlags");
    int devices = 0;
    if (!init || !count || !set_flags || init(0) || count(&devices))
        return;
    for (int d = 0; d < devices; d++)
        if (set_flags(d, CU_CTX_SCHED_BLOCKING_SYNC))
            fprintf(stderr, "Could not set blocking sync on GPU %d; the worker will spin while waiting\n", d);
}

/* Page-locks shared frame memory so uploads and downloads skip CUDA's staging copy. */
static int pin_host(void *pixels, size_t bytes, int pin) {
    typedef int (WINAPI *register_fn)(void *, size_t, unsigned);
    typedef int (WINAPI *unregister_fn)(void *);
    HMODULE cuda = GetModuleHandleA("nvcuda.dll");
    if (!cuda)
        return -1;
    if (!pin) {
        RESOLVE(unregister_fn, unregister, cuda, "cuMemHostUnregister");
        return unregister ? unregister(pixels) : -1;
    }
    RESOLVE(register_fn, reg, cuda, "cuMemHostRegister_v2");
    return reg ? reg(pixels, bytes, 0) : -1;
}

struct framing {
    float cx, cy, height;
    float target_cx, target_cy, target_height;
    ULONGLONG last_face, last_step;
};

#define FRAME_FACE_SCALE 2.6f     /* crop height per face height */
#define FRAME_FACE_LEVEL 0.42f    /* face centre, as a fraction down the crop */
#define FRAME_MAX_ZOOM 2.0f
#define FRAME_DEADZONE 0.08f      /* fraction of the crop height the face may move before the crop follows */
#define FRAME_SETTLE_SECONDS 0.35f
#define FRAME_LOST_MS 2000

/* Picks the largest face and eases the crop toward framing it. The crop only retargets
 * when the face leaves a dead zone, so small head movements leave the picture still. */
static void update_framing(struct framing *f, const NvAR_BBoxes *faces, unsigned width, unsigned height) {
    ULONGLONG now = GetTickCount64();
    const NvAR_Rect *face = NULL;
    for (unsigned i = 0; i < faces->num_boxes; i++)
        if (!face || faces->boxes[i].width * faces->boxes[i].height > face->width * face->height)
            face = &faces->boxes[i];
    if (face && face->height > 0) {
        f->last_face = now;
        float h = fminf(fmaxf(face->height * FRAME_FACE_SCALE, height / FRAME_MAX_ZOOM), (float)height);
        float x = face->x + face->width / 2;
        float y = face->y + face->height / 2 + (0.5f - FRAME_FACE_LEVEL) * h;
        float limit = FRAME_DEADZONE * f->target_height;
        if (fabsf(x - f->target_cx) > limit || fabsf(y - f->target_cy) > limit ||
            fabsf(h - f->target_height) > 2 * limit) {
            f->target_cx = x;
            f->target_cy = y;
            f->target_height = h;
        }
    } else if (now - f->last_face > FRAME_LOST_MS) {
        f->target_cx = width / 2.0f;
        f->target_cy = height / 2.0f;
        f->target_height = (float)height;
    }
    float dt = f->last_step ? (now - f->last_step) / 1000.0f : 0;
    f->last_step = now;
    float k = 1 - expf(-dt / FRAME_SETTLE_SECONDS);
    f->cx += (f->target_cx - f->cx) * k;
    f->cy += (f->target_cy - f->cy) * k;
    f->height += (f->target_height - f->height) * k;
    float half_h = f->height / 2, half_w = half_h * width / height;
    f->cx = fminf(fmaxf(f->cx, half_w), width - half_w);
    f->cy = fminf(fmaxf(f->cy, half_h), height - half_h);
}

/* Input frame formats: webcam YUV is limited range, decoded MJPEG (jNNN) is full range. */
static const struct input_format {
    const char *name;
    NvCVImage_PixelFormat format;
    unsigned layout, colorspace, bits_per_pixel, row_bytes_per_pixel;
} input_formats[] = {
    {"bgr24", NVCV_BGR, NVCV_CHUNKY, 0, 24, 3},
    {"yuyv", NVCV_YUV422, NVCV_YUYV, NVCV_601 | NVCV_VIDEO_RANGE | NVCV_CHROMA_INTSTITIAL, 16, 2},
    {"nv12", NVCV_YUV420, NVCV_NV12, NVCV_601 | NVCV_VIDEO_RANGE | NVCV_CHROMA_INTSTITIAL, 12, 1},
    {"j420", NVCV_YUV420, NVCV_I420, NVCV_601 | NVCV_FULL_RANGE | NVCV_CHROMA_JPEG, 12, 1},
    {"j422", NVCV_YUV422, NVCV_YUV, NVCV_601 | NVCV_FULL_RANGE | NVCV_CHROMA_JPEG, 16, 1},
    {"j444", NVCV_YUV444, NVCV_YUV, NVCV_601 | NVCV_FULL_RANGE | NVCV_CHROMA_JPEG, 24, 1},
};

static void usage(void) {
    fprintf(stderr,
            "Usage: camera_stream.exe GREENSCREEN_MODEL_DIR --size WxH [--input FORMAT]\n"
            "           [--shm FILE [--out-device DEVICE --out-offsets OFFSET,...]]\n"
            "           [--denoise MODEL_DIR [--denoise-strength 0|1]]\n"
            "           [--eye-contact GAZE_MODEL_DIR] [--auto-frame FACE_MODEL_DIR]\n"
            "           [--background FILE.bgr | --blur 0..1 | --remove-background]\n"
            "           [--relight MODEL_DIR --hdr FILE.hdr [--strength 0..1]]\n"
            "           < input > output.yuyv\n"
            "FORMAT is bgr24 (default), yuyv, nv12, or planar full-range j420, j422 or j444.\n"
            "With --shm, FILE holds 2 input frames then 2 output frames, and stdin and stdout carry\n"
            "one byte per frame: the slot to process, then the slot that is done.\n"
            "With --out-device, output frames are the v4l2loopback buffers mapped from DEVICE at\n"
            "each OFFSET (up to 3), FILE holds only input frames, and there is one slot per buffer.\n"
            "The background is BGR24 at --size; output frames are YUYV (BT.601, limited range).\n"
            "Removal fills the background black.\n");
}

int main(int argc, char **argv) {
    const char *gs_dir = NULL, *background_path = NULL, *relight_dir = NULL, *hdr_path = NULL,
               *denoise_dir = NULL, *shm_path = NULL, *gaze_dir = NULL, *frame_dir = NULL,
               *out_device = NULL;
    unsigned long out_offsets[MAX_SLOTS];
    int out_count = 0;
    float strength = 1, denoise_strength = 1;
    float blur_strength = 0.5f;
    int blur_enabled = 0, remove_background = 0;
    unsigned width = 0, height = 0;
    const struct input_format *input = &input_formats[0];
    for (int i = 1; i < argc; i++) {
        const char *a = argv[i];
        const char *next = i + 1 < argc ? argv[i + 1] : NULL;
        if (!strcmp(a, "--background") && next) background_path = argv[++i];
        else if (!strcmp(a, "--remove-background")) remove_background = 1;
        else if (!strcmp(a, "--blur") && next) {
            char *end;
            blur_strength = strtof(argv[++i], &end);
            if (end == argv[i] || *end || !isfinite(blur_strength) || blur_strength < 0 || blur_strength > 1) {
                fprintf(stderr, "Blur strength must be between 0.0 and 1.0\n");
                return 2;
            }
            blur_enabled = 1;
        } else if (!strcmp(a, "--relight") && next) relight_dir = argv[++i];
        else if (!strcmp(a, "--hdr") && next) hdr_path = argv[++i];
        else if (!strcmp(a, "--strength") && next) strength = (float)atof(argv[++i]);
        else if (!strcmp(a, "--denoise") && next) denoise_dir = argv[++i];
        else if (!strcmp(a, "--denoise-strength") && next) denoise_strength = (float)atof(argv[++i]);
        else if (!strcmp(a, "--eye-contact") && next) gaze_dir = argv[++i];
        else if (!strcmp(a, "--auto-frame") && next) frame_dir = argv[++i];
        else if (!strcmp(a, "--size") && next && sscanf(argv[++i], "%ux%u", &width, &height) == 2) {}
        else if (!strcmp(a, "--shm") && next) shm_path = argv[++i];
        else if (!strcmp(a, "--out-device") && next) out_device = argv[++i];
        else if (!strcmp(a, "--out-offsets") && next) {
            char *p = argv[++i], *end = p;
            for (out_count = 0; out_count < MAX_SLOTS; p = end + 1) {
                out_offsets[out_count++] = strtoul(p, &end, 10);
                if (end == p || *end != ',') break;
            }
            if (end == p || *end) {
                usage();
                return 2;
            }
        }
        else if (!strcmp(a, "--input") && next) {
            const char *name = argv[++i];
            input = NULL;
            for (size_t f = 0; f < sizeof input_formats / sizeof *input_formats; f++)
                if (!strcmp(name, input_formats[f].name)) input = &input_formats[f];
            if (!input) {
                usage();
                return 2;
            }
        }
        else if (a[0] != '-' && !gs_dir) gs_dir = a;
        else {
            usage();
            return 2;
        }
    }
    if (!gs_dir || !width || !height || width % 2 || (input->bits_per_pixel == 12 && height % 2) ||
        !relight_dir != !hdr_path || !out_device != !out_count || (out_device && !shm_path)) {
        usage();
        return 2;
    }
    if (!!background_path + blur_enabled + remove_background > 1) {
        fprintf(stderr, "Use only one of --background, --blur or --remove-background\n");
        return 2;
    }
    const int slots = out_device ? out_count : 2;
    /* NVIDIA's filter still blurs at strength zero; make zero a true passthrough. */
    if (blur_strength == 0) blur_enabled = 0;

    use_blocking_sync();
    HMODULE vfx = LoadLibraryA("NVVideoEffects.dll");
    HMODULE cv = LoadLibraryA("NVCVImage.dll");
    if (!vfx || !cv) {
        fprintf(stderr, "Could not load VFX DLLs: %u\n", GetLastError());
        return 1;
    }

    RESOLVE(create_fn, create, vfx, "NvVFX_CreateEffect");
    RESOLVE(destroy_fn, destroy, vfx, "NvVFX_DestroyEffect");
    RESOLVE(set_string_fn, set_string, vfx, "NvVFX_SetString");
    RESOLVE(set_u32_fn, set_u32, vfx, "NvVFX_SetU32");
    RESOLVE(set_f32_fn, set_f32, vfx, "NvVFX_SetF32");
    RESOLVE(set_stream_fn, set_stream, vfx, "NvVFX_SetCudaStream");
    RESOLVE(stream_create_fn, stream_create, vfx, "NvVFX_CudaStreamCreate");
    RESOLVE(stream_destroy_fn, stream_destroy, vfx, "NvVFX_CudaStreamDestroy");
    RESOLVE(stream_sync_fn, stream_sync, vfx, "NvVFX_CudaStreamSynchronize");
    RESOLVE(load_fn, load, vfx, "NvVFX_Load");
    RESOLVE(alloc_state_fn, alloc_state, vfx, "NvVFX_AllocateState");
    RESOLVE(free_state_fn, free_state, vfx, "NvVFX_DeallocateState");
    RESOLVE(set_states_fn, set_states, vfx, "NvVFX_SetStateObjectHandleArray");
    RESOLVE(set_image_fn, set_image, vfx, "NvVFX_SetImage");
    RESOLVE(run_fn, run, vfx, "NvVFX_Run");
    RESOLVE(image_alloc_fn, image_alloc, cv, "NvCVImage_Alloc");
    RESOLVE(image_init_fn, image_init, cv, "NvCVImage_Init");
    RESOLVE(image_free_fn, image_free, cv, "NvCVImage_Dealloc");
    RESOLVE(image_transfer_fn, image_transfer, cv, "NvCVImage_Transfer");
    RESOLVE(composite_fn, composite, cv, "NvCVImage_Composite");
    if (!create || !destroy || !set_string || !set_u32 || !set_f32 || !set_stream || !stream_create ||
        !stream_destroy || !stream_sync || !load || !alloc_state || !free_state || !set_states ||
        !set_image || !run || !image_alloc || !image_init || !image_free || !image_transfer || !composite) {
        fprintf(stderr, "A required VFX export is missing\n");
        return 1;
    }
    HMODULE ar = NULL, npp = NULL;
    ar_create_fn ar_create = NULL;
    ar_destroy_fn ar_destroy = NULL;
    ar_load_fn ar_load = NULL;
    ar_run_fn ar_run = NULL;
    ar_set_string_fn ar_set_string = NULL;
    ar_set_u32_fn ar_set_u32 = NULL;
    ar_set_s32_fn ar_set_s32 = NULL;
    ar_set_stream_fn ar_set_stream = NULL;
    ar_set_object_fn ar_set_object = NULL;
    npp_set_stream_fn npp_set_stream = NULL;
    npp_resize_fn npp_resize = NULL;
    if (gaze_dir || frame_dir) {
        ar = LoadLibraryA("nvARPose.dll");
        if (!ar) {
            fprintf(stderr, "Could not load nvARPose.dll: %u\n", GetLastError());
            return 1;
        }
        ar_create = (ar_create_fn)GetProcAddress(ar, "NvAR_Create");
        ar_destroy = (ar_destroy_fn)GetProcAddress(ar, "NvAR_Destroy");
        ar_load = (ar_load_fn)GetProcAddress(ar, "NvAR_Load");
        ar_run = (ar_run_fn)GetProcAddress(ar, "NvAR_Run");
        ar_set_string = (ar_set_string_fn)GetProcAddress(ar, "NvAR_SetString");
        ar_set_u32 = (ar_set_u32_fn)GetProcAddress(ar, "NvAR_SetU32");
        ar_set_s32 = (ar_set_s32_fn)GetProcAddress(ar, "NvAR_SetS32");
        ar_set_stream = (ar_set_stream_fn)GetProcAddress(ar, "NvAR_SetCudaStream");
        ar_set_object = (ar_set_object_fn)GetProcAddress(ar, "NvAR_SetObject");
        if (!ar_create || !ar_destroy || !ar_load || !ar_run || !ar_set_string || !ar_set_u32 || !ar_set_s32 ||
            !ar_set_stream || !ar_set_object) {
            fprintf(stderr, "A required AR export is missing\n");
            return 1;
        }
    }
    if (frame_dir) {
        npp = LoadLibraryA("nppig64_12.dll");
        HMODULE npp_core = GetModuleHandleA("nppc64_12.dll");
        if (npp && npp_core) {
            npp_resize = (npp_resize_fn)GetProcAddress(npp, "nppiResizeSqrPixel_8u_C3R");
            npp_set_stream = (npp_set_stream_fn)GetProcAddress(npp_core, "nppSetStream");
        }
        if (!npp_resize || !npp_set_stream) {
            fprintf(stderr, "Could not load NPP resize for Auto Frame\n");
            return 1;
        }
    }

    NvVFX_Handle gs = NULL, relight = NULL, denoise = NULL, blur = NULL;
    NvAR_Handle gaze = NULL, faces = NULL;
    NvAR_Rect face_rects[MAX_FACES] = {{0}};
    NvAR_BBoxes face_boxes = {face_rects, 0, MAX_FACES};
    struct framing framing = {width / 2.0f, height / 2.0f, (float)height,
                              width / 2.0f, height / 2.0f, (float)height, 0, 0};
    NvVFX_StateObjectHandle state = NULL, denoise_state = NULL;
    const size_t frame_bytes = (size_t)width * height * 3, out_bytes = (size_t)width * height * 2,
                 in_bytes = (size_t)width * height * input->bits_per_pixel / 8;
    const int need_mask = background_path || blur_enabled || remove_background || relight_dir;
    CUstream stream = NULL;
    NvCVImage src_slots[MAX_SLOTS] = {{0}}, out_slots[MAX_SLOTS] = {{0}};
    NvCVImage src_gpu = {0}, src_rgb = {0}, mask = {0}, relit = {0}, projected = {0},
              hdr = {0}, light_mask = {0}, scaled_mask = {0}, bg_cpu = {0}, bg_gpu = {0}, out_gpu = {0},
              dn_in = {0}, dn_out = {0}, blur_in = {0}, blur_out = {0}, gaze_out = {0}, framed = {0}, tmp = {0};
    float *hdr_pixels = NULL;
    uint8_t *shm = MAP_FAILED, *out_maps[MAX_SLOTS];
    const size_t shm_bytes = slots * (in_bytes + (out_device ? 0 : out_bytes));
    int status = 0, pinned = 0, out_pinned[MAX_SLOTS] = {0}, mapped_outs = 0;

    CHECK("CreateStream", stream_create(&stream));
    if (shm_path) {
        int fd = open(shm_path, O_RDWR);
        if (fd >= 0) {
            shm = mmap(NULL, shm_bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
            close(fd);
        }
        if (shm == MAP_FAILED) {
            fprintf(stderr, "Could not map %s\n", shm_path);
            status = 1;
            goto cleanup;
        }
        pinned = pin_host(shm, shm_bytes, 1) == 0;
        if (out_device) {
            fd = open(out_device, O_RDWR);
            for (; fd >= 0 && mapped_outs < slots; mapped_outs++) {
                out_maps[mapped_outs] = mmap(NULL, out_bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd,
                                             (off_t)out_offsets[mapped_outs]);
                if (out_maps[mapped_outs] == MAP_FAILED) break;
                out_pinned[mapped_outs] = pin_host(out_maps[mapped_outs], out_bytes, 1) == 0;
            }
            if (fd >= 0) close(fd);
            if (mapped_outs < slots) {
                fprintf(stderr, "Could not map the buffers of %s\n", out_device);
                status = 1;
                goto cleanup;
            }
        }
        int all_pinned = pinned;
        for (int k = 0; k < mapped_outs; k++) all_pinned &= out_pinned[k];
        if (!all_pinned)
            fprintf(stderr, "Could not pin shared frames; transfers will be slower\n");
        for (int k = 0; k < slots; k++) {
            uint8_t *out = out_device ? out_maps[k] : shm + slots * in_bytes + k * out_bytes;
            const int out_is_pinned = out_device ? out_pinned[k] : pinned;
            CHECK("Init src slot", image_init(&src_slots[k], width, height, (int)(width * input->row_bytes_per_pixel),
                                              shm + k * in_bytes, input->format, NVCV_U8, input->layout,
                                              pinned ? NVCV_CPU_PINNED : NVCV_CPU));
            CHECK("Init out slot", image_init(&out_slots[k], width, height, (int)(width * 2), out, NVCV_YUV422,
                                              NVCV_U8, NVCV_YUYV, out_is_pinned ? NVCV_CPU_PINNED : NVCV_CPU));
        }
    } else {
        if (image_alloc(&src_slots[0], width, height, input->format, NVCV_U8, input->layout, NVCV_CPU_PINNED, 1))
            CHECK("Alloc src CPU", image_alloc(&src_slots[0], width, height, input->format, NVCV_U8, input->layout, NVCV_CPU, 1));
        if (image_alloc(&out_slots[0], width, height, NVCV_YUV422, NVCV_U8, NVCV_YUYV, NVCV_CPU_PINNED, 1))
            CHECK("Alloc out CPU", image_alloc(&out_slots[0], width, height, NVCV_YUV422, NVCV_U8, NVCV_YUYV, NVCV_CPU, 1));
        /* Frames are read and written whole, so the buffers must be tightly packed. */
        if (src_slots[0].bufferBytes != in_bytes || out_slots[0].bufferBytes != out_bytes) {
            fprintf(stderr, "Unexpected frame layout: %llu/%llu bytes\n", src_slots[0].bufferBytes,
                    out_slots[0].bufferBytes);
            status = 1;
            goto cleanup;
        }
    }
    for (int k = 0; k < slots; k++) {
        src_slots[k].colorspace = (unsigned char)input->colorspace;
        out_slots[k].colorspace = NVCV_601 | NVCV_VIDEO_RANGE | NVCV_CHROMA_INTSTITIAL;
    }
    CHECK("Alloc src GPU", image_alloc(&src_gpu, width, height, NVCV_BGR, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 1));
    CHECK("Alloc src RGB", image_alloc(&src_rgb, width, height, NVCV_RGB, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 1));
    CHECK("Alloc mask", image_alloc(&mask, width, height, NVCV_A, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 1));
    CHECK("Alloc out GPU", image_alloc(&out_gpu, width, height, NVCV_RGB, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 1));

    if (denoise_dir) {
        CHECK("Alloc denoise in", image_alloc(&dn_in, width, height, NVCV_BGR, NVCV_F32, NVCV_PLANAR, NVCV_GPU, 1));
        CHECK("Alloc denoise out", image_alloc(&dn_out, width, height, NVCV_BGR, NVCV_F32, NVCV_PLANAR, NVCV_GPU, 1));
        CHECK("CreateEffect(Denoising)", create("Denoising", &denoise));
        CHECK("Set Stream", set_stream(denoise, "CudaStream", stream));
        CHECK("Set ModelDir", set_string(denoise, "ModelDir", denoise_dir));
        CHECK("Set SrcImage0", set_image(denoise, "SrcImage0", &dn_in));
        CHECK("Set DstImage0", set_image(denoise, "DstImage0", &dn_out));
        CHECK("Set Strength", set_f32(denoise, "Strength", denoise_strength));
        CHECK("AllocateState", alloc_state(denoise, &denoise_state));
        CHECK("Set State", set_states(denoise, "State", &denoise_state));
        CHECK("Load(Denoising)", load(denoise));
    }

    if (gaze_dir) {
        CHECK("Alloc eye contact output", image_alloc(&gaze_out, width, height, NVCV_BGR, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 1));
        CHECK("NvAR_Create(GazeRedirection)", ar_create("GazeRedirection", &gaze));
        CHECK("Set ModelDir", ar_set_string(gaze, AR_CONFIG(ModelDir), gaze_dir));
        CHECK("Set Stream", ar_set_stream(gaze, AR_CONFIG(CUDAStream), stream));
        CHECK("Set Temporal", ar_set_u32(gaze, AR_CONFIG(Temporal), 0xffffffff));
        CHECK("Set GazeRedirect", ar_set_u32(gaze, AR_CONFIG(GazeRedirect), 1));
        CHECK("NvAR_Load(GazeRedirection)", ar_load(gaze));
        CHECK("Set Input Image", ar_set_object(gaze, AR_INPUT(Image), &src_gpu, sizeof(NvCVImage)));
        CHECK("Set Output Image", ar_set_object(gaze, AR_OUTPUT(Image), &gaze_out, sizeof(NvCVImage)));
        CHECK("Set Input Width", ar_set_s32(gaze, AR_INPUT(Width), (int)width));
        CHECK("Set Input Height", ar_set_s32(gaze, AR_INPUT(Height), (int)height));
    }

    if (frame_dir) {
        CHECK("Alloc framed", image_alloc(&framed, width, height, NVCV_BGR, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 1));
        CHECK("NvAR_Create(FaceBoxDetection)", ar_create("FaceBoxDetection", &faces));
        CHECK("Set ModelDir", ar_set_string(faces, AR_CONFIG(ModelDir), frame_dir));
        CHECK("Set Stream", ar_set_stream(faces, AR_CONFIG(CUDAStream), stream));
        CHECK("Set Temporal", ar_set_u32(faces, AR_CONFIG(Temporal), 0xffffffff));
        CHECK("NvAR_Load(FaceBoxDetection)", ar_load(faces));
        CHECK("Set Input Image", ar_set_object(faces, AR_INPUT(Image), &src_gpu, sizeof(NvCVImage)));
        CHECK("Set BoundingBoxes", ar_set_object(faces, AR_OUTPUT(BoundingBoxes), &face_boxes, sizeof face_boxes));
        CHECK("Set NPP stream", npp_set_stream(stream));
    }

    if (need_mask) {
        CHECK("CreateEffect(GreenScreen)", create("GreenScreen", &gs));
        CHECK("Set ModelDir", set_string(gs, "ModelDir", gs_dir));
        CHECK("Set Mode", set_u32(gs, "Mode", 0));
        CHECK("Set Stream", set_stream(gs, "CudaStream", stream));
        CHECK("Set MaxInputWidth", set_u32(gs, "MaxInputWidth", width));
        CHECK("Set MaxInputHeight", set_u32(gs, "MaxInputHeight", height));
        CHECK("Set MaxNumberStreams", set_u32(gs, "MaxNumberStreams", 1));
        CHECK("Load(GreenScreen)", load(gs));
        CHECK("AllocateState", alloc_state(gs, &state));
        CHECK("Set SrcImage0", set_image(gs, "SrcImage0", &src_gpu));
        CHECK("Set DstImage0", set_image(gs, "DstImage0", &mask));
        CHECK("Set State", set_states(gs, "State", &state));
    }

    if (blur_enabled) {
        CHECK("Alloc blur input", image_alloc(&blur_in, width, height, NVCV_BGR, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 1));
        CHECK("Alloc blur output", image_alloc(&blur_out, width, height, NVCV_BGR, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 1));
        CHECK("CreateEffect(BackgroundBlur)", create("BackgroundBlur", &blur));
        CHECK("Set Stream", set_stream(blur, "CudaStream", stream));
        CHECK("Set SrcImage0", set_image(blur, "SrcImage0", &blur_in));
        CHECK("Set SrcImage1", set_image(blur, "SrcImage1", &mask));
        CHECK("Set DstImage0", set_image(blur, "DstImage0", &blur_out));
        CHECK("Set Strength", set_f32(blur, "Strength", blur_strength));
        CHECK("Load(BackgroundBlur)", load(blur));
    }

    if (relight_dir) {
        unsigned hw = 0, hh = 0;
        hdr_pixels = read_hdr(hdr_path, &hw, &hh);
        if (!hdr_pixels) {
            fprintf(stderr, "Could not read Radiance HDR image: %s\n", hdr_path);
            status = 1;
            goto cleanup;
        }
        CHECK("Alloc HDR", image_alloc(&hdr, hw, hh, NVCV_RGB, NVCV_F32, NVCV_CHUNKY, NVCV_CPU, 0));
        for (unsigned y = 0; y < hh; y++)
            memcpy((uint8_t *)hdr.pixels + y * hdr.pitch, hdr_pixels + (size_t)y * hw * 3,
                   hw * 3 * sizeof(float));
        CHECK("Alloc relit", image_alloc(&relit, width, height, NVCV_RGB, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 1));
        CHECK("Alloc projected", image_alloc(&projected, width, height, NVCV_RGB, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 1));
        CHECK("CreateEffect(Relighting)", create("Relighting", &relight));
        CHECK("Set Stream", set_stream(relight, "CudaStream", stream));
        CHECK("Set ModelDir", set_string(relight, "ModelDir", relight_dir));
        CHECK("Set SrcImage0", set_image(relight, "SrcImage0", &src_gpu));
        CHECK("Set SrcImage1", set_image(relight, "SrcImage1", &mask));
        CHECK("Set SrcImage2", set_image(relight, "SrcImage2", &hdr));
        CHECK("Set DstImage0", set_image(relight, "DstImage0", &relit));
        CHECK("Set DstImage1", set_image(relight, "DstImage1", &projected));
        /* the sample app's defaults; Broadcast exposes neither */
        CHECK("Set AnglePan", set_f32(relight, "AnglePan", -90 * DEGREES));
        CHECK("Set AngleVFOV", set_f32(relight, "AngleVFOV", 60 * DEGREES));
        CHECK("Load(Relighting)", load(relight));
        if (strength < 1) {
            CHECK("Alloc light mask", image_alloc(&light_mask, width, height, NVCV_A, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 1));
            CHECK("Alloc scaled mask", image_alloc(&scaled_mask, width, height, NVCV_A, NVCV_F32, NVCV_CHUNKY, NVCV_GPU, 1));
        }
    }

    if (background_path || remove_background) {
        CHECK("Alloc bg CPU", image_alloc(&bg_cpu, width, height, NVCV_BGR, NVCV_U8, NVCV_CHUNKY, NVCV_CPU, 1));
        CHECK("Alloc bg GPU", image_alloc(&bg_gpu, width, height, NVCV_RGB, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 1));
        if (background_path) {
            FILE *bg_file = fopen(background_path, "rb");
            if (!bg_file || fread(bg_cpu.pixels, 1, frame_bytes, bg_file) != frame_bytes) {
                fprintf(stderr, "Could not read %ux%u BGR24 background: %s\n", width, height, background_path);
                if (bg_file) fclose(bg_file);
                status = 1;
                goto cleanup;
            }
            fclose(bg_file);
        } else {
            memset(bg_cpu.pixels, 0, frame_bytes);
        }
        CHECK("Upload background", image_transfer(&bg_cpu, &bg_gpu, 1.0f, stream, &tmp));
    }
    const NvCVImage *fg = relight ? &relit : &src_rgb;
    const NvCVImage *bg = background_path || remove_background ? &bg_gpu : &src_rgb;
    fprintf(stderr, "Camera effects ready (%s%s%s%s%s%s%s); reading %ux%u %s frames, writing YUYV\n",
            denoise ? "noise removal " : "", gaze ? "Eye Contact " : "", faces ? "Auto Frame " : "",
            background_path ? "background " : "",
            blur ? "background blur " : "", remove_background ? "background removal " : "",
            relight ? "Studio Light" : "", width, height, input->name);

    unsigned long frames = 0;
    ULONGLONG started = GetTickCount64();
    for (;;) {
        int k = 0;
        if (shm != MAP_FAILED) {
            k = getchar();
            if (k == EOF) break;
            if (k >= slots) {
                fprintf(stderr, "Bad frame slot %d\n", k);
                status = 1;
                goto cleanup;
            }
        } else {
            size_t got = fread(src_slots[0].pixels, 1, in_bytes, stdin);
            if (got == 0 && feof(stdin)) break;
            if (got != in_bytes) {
                fprintf(stderr, "Incomplete input frame after %lu frames (%zu bytes)\n", frames, got);
                status = 1;
                goto cleanup;
            }
        }
        NvCVImage *src_cpu = &src_slots[k], *out_cpu = &out_slots[k];
        CHECK("Transfer input", image_transfer(src_cpu, &src_gpu, 1.0f, stream, &tmp));
        if (denoise) {
            CHECK("Denoise input", image_transfer(&src_gpu, &dn_in, 1.0f / 255.0f, stream, &tmp));
            CHECK("Run(Denoising)", run(denoise, 0));
            CHECK("Denoise output", image_transfer(&dn_out, &src_gpu, 255.0f, stream, &tmp));
        }
        if (gaze) {
            CHECK("Run(GazeRedirection)", ar_run(gaze));
            CHECK("Eye contact output", image_transfer(&gaze_out, &src_gpu, 1.0f, stream, &tmp));
        }
        if (faces) {
            face_boxes.num_boxes = 0;
            CHECK("Run(FaceBoxDetection)", ar_run(faces));
            update_framing(&framing, &face_boxes, width, height);
            if (framing.height < height - 0.5f) {
                const double zoom = height / framing.height;
                const double left = framing.cx - framing.height * width / height / 2,
                             top = framing.cy - framing.height / 2;
                const NppiSize size = {(int)width, (int)height};
                const NppiRect whole = {0, 0, (int)width, (int)height};
                CHECK("Resize(AutoFrame)", npp_resize(src_gpu.pixels, size, src_gpu.pitch, whole, framed.pixels,
                                                      framed.pitch, whole, zoom, zoom, -left * zoom, -top * zoom,
                                                      NPPI_INTER_CUBIC));
                CHECK("Auto Frame output", image_transfer(&framed, &src_gpu, 1.0f, stream, &tmp));
            }
        }
        if (!need_mask) {
            CHECK("Transfer output", image_transfer(&src_gpu, out_cpu, 1.0f, stream, &tmp));
            goto write;
        }
        CHECK("Run(GreenScreen)", run(gs, 0));
        CHECK("Convert input", image_transfer(&src_gpu, &src_rgb, 1.0f, stream, &tmp));
        if (relight)
            CHECK("Run(Relighting)", run(relight, 1));
        if (light_mask.pixels) {
            /* Strength only scales highlights, so blend relit and unlit instead.
             * Transfer only scales when one side is float. */
            CHECK("Scale mask", image_transfer(&mask, &scaled_mask, strength, stream, &tmp));
            CHECK("Convert mask", image_transfer(&scaled_mask, &light_mask, 1.0f, stream, &tmp));
            CHECK("Blend light", composite(&relit, &src_rgb, &light_mask, &relit, stream));
        }
        CHECK("Composite", composite(fg, bg, &mask, &out_gpu, stream));
        if (blur) {
            CHECK("Convert blur input", image_transfer(&out_gpu, &blur_in, 1.0f, stream, &tmp));
            CHECK("Run(BackgroundBlur)", run(blur, 0));
            CHECK("Transfer output", image_transfer(&blur_out, out_cpu, 1.0f, stream, &tmp));
        } else {
            CHECK("Transfer output", image_transfer(&out_gpu, out_cpu, 1.0f, stream, &tmp));
        }
    write:
        CHECK("Synchronize", stream_sync(stream));
        if ((shm != MAP_FAILED ? putchar(k) == EOF : fwrite(out_cpu->pixels, 1, out_bytes, stdout) != out_bytes) ||
            fflush(stdout) != 0) {
            fprintf(stderr, "Output pipe closed after %lu frames\n", frames);
            status = 1;
            goto cleanup;
        }
        ++frames;
        if (frames % 30 == 0) {
            ULONGLONG ms = GetTickCount64() - started;
            fprintf(stderr, "Processed %lu frames in %.2f s (%.1f fps)\n",
                    frames, ms / 1000.0, ms ? frames * 1000.0 / ms : 0.0);
        }
    }
    if (frames == 0) status = 1;

cleanup:
    {
        NvCVImage *images[] = {&src_gpu, &src_rgb, &mask, &relit, &projected, &hdr, &light_mask, &scaled_mask,
                               &bg_cpu, &bg_gpu, &out_gpu, &dn_in, &dn_out, &blur_in, &blur_out, &gaze_out,
                               &framed, &tmp};
        for (size_t i = 0; i < sizeof images / sizeof *images; i++)
            if (images[i]->pixels) image_free(images[i]);
        for (int k = 0; k < slots; k++) {
            if (src_slots[k].deletePtr) image_free(&src_slots[k]);
            if (out_slots[k].deletePtr) image_free(&out_slots[k]);
        }
    }
    if (shm != MAP_FAILED) {
        if (pinned) pin_host(shm, shm_bytes, 0);
        munmap(shm, shm_bytes);
    }
    for (int k = 0; k < mapped_outs; k++) {
        if (out_pinned[k]) pin_host(out_maps[k], out_bytes, 0);
        munmap(out_maps[k], out_bytes);
    }
    if (state) free_state(gs, state);
    if (gs) destroy(gs);
    if (relight) destroy(relight);
    if (denoise_state) free_state(denoise, denoise_state);
    if (denoise) destroy(denoise);
    if (blur) destroy(blur);
    if (gaze) ar_destroy(gaze);
    if (faces) ar_destroy(faces);
    if (stream) stream_destroy(stream);
    free(hdr_pixels);
    if (npp) FreeLibrary(npp);
    if (ar) FreeLibrary(ar);
    FreeLibrary(cv);
    FreeLibrary(vfx);
    return status == 0 ? 0 : 1;
}
