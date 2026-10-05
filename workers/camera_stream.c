#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <windows.h>
#include "nvVideoEffects.h"

#define DEGREES (3.14159265f / 180.0f)

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
typedef void (WINAPI *image_free_fn)(NvCVImage *);
typedef int (WINAPI *image_transfer_fn)(const NvCVImage *, NvCVImage *, float, CUstream, NvCVImage *);
typedef int (WINAPI *composite_fn)(const NvCVImage *, const NvCVImage *, const NvCVImage *, NvCVImage *,
                                   CUstream);

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

static void usage(void) {
    fprintf(stderr,
            "Usage: camera_stream.exe GREENSCREEN_MODEL_DIR --size WxH [--denoise MODEL_DIR [--denoise-strength 0|1]]\n"
            "           [--background FILE.bgr | --blur 0..1 | --remove-background]\n"
            "           [--relight MODEL_DIR --hdr FILE.hdr [--strength 0..1]]\n"
            "           < input.bgr > output.bgr\n"
            "Frames and the background are BGR24 at --size. Removal fills the background black.\n");
}

int main(int argc, char **argv) {
    const char *gs_dir = NULL, *background_path = NULL, *relight_dir = NULL, *hdr_path = NULL,
               *denoise_dir = NULL;
    float strength = 1, denoise_strength = 1;
    float blur_strength = 0.5f;
    int blur_enabled = 0, remove_background = 0;
    unsigned width = 0, height = 0;
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
        else if (!strcmp(a, "--size") && next && sscanf(argv[++i], "%ux%u", &width, &height) == 2) {}
        else if (a[0] != '-' && !gs_dir) gs_dir = a;
        else {
            usage();
            return 2;
        }
    }
    if (!gs_dir || !width || !height || width % 2 || !relight_dir != !hdr_path) {
        usage();
        return 2;
    }
    if (!!background_path + blur_enabled + remove_background > 1) {
        fprintf(stderr, "Use only one of --background, --blur or --remove-background\n");
        return 2;
    }
    /* NVIDIA's filter still blurs at strength zero; make zero a true passthrough. */
    if (blur_strength == 0) blur_enabled = 0;

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
    RESOLVE(image_free_fn, image_free, cv, "NvCVImage_Dealloc");
    RESOLVE(image_transfer_fn, image_transfer, cv, "NvCVImage_Transfer");
    RESOLVE(composite_fn, composite, cv, "NvCVImage_Composite");
    if (!create || !destroy || !set_string || !set_u32 || !set_f32 || !set_stream || !stream_create ||
        !stream_destroy || !stream_sync || !load || !alloc_state || !free_state || !set_states ||
        !set_image || !run || !image_alloc || !image_free || !image_transfer || !composite) {
        fprintf(stderr, "A required VFX export is missing\n");
        return 1;
    }

    NvVFX_Handle gs = NULL, relight = NULL, denoise = NULL, blur = NULL;
    NvVFX_StateObjectHandle state = NULL, denoise_state = NULL;
    const size_t frame_bytes = (size_t)width * height * 3;
    const int need_mask = background_path || blur_enabled || remove_background || relight_dir;
    CUstream stream = NULL;
    NvCVImage src_cpu = {0}, src_gpu = {0}, src_rgb = {0}, mask = {0}, relit = {0}, projected = {0},
              hdr = {0}, light_mask = {0}, scaled_mask = {0}, bg_cpu = {0}, bg_gpu = {0}, out_gpu = {0}, out_cpu = {0},
              dn_in = {0}, dn_out = {0}, blur_in = {0}, blur_out = {0}, tmp = {0};
    uint8_t *raw = malloc(frame_bytes);
    float *hdr_pixels = NULL;
    int status = 0;
    if (!raw) {
        fprintf(stderr, "Out of host memory\n");
        return 1;
    }

    CHECK("CreateStream", stream_create(&stream));
    CHECK("Alloc src CPU", image_alloc(&src_cpu, width, height, NVCV_BGR, NVCV_U8, NVCV_CHUNKY, NVCV_CPU, 1));
    CHECK("Alloc src GPU", image_alloc(&src_gpu, width, height, NVCV_BGR, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 1));
    CHECK("Alloc src RGB", image_alloc(&src_rgb, width, height, NVCV_RGB, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 1));
    CHECK("Alloc mask", image_alloc(&mask, width, height, NVCV_A, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 1));
    CHECK("Alloc out GPU", image_alloc(&out_gpu, width, height, NVCV_RGB, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 1));
    CHECK("Alloc out CPU", image_alloc(&out_cpu, width, height, NVCV_BGR, NVCV_U8, NVCV_CHUNKY, NVCV_CPU, 1));

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
        if (background_path) {
            FILE *bg_file = fopen(background_path, "rb");
            if (!bg_file || fread(raw, 1, frame_bytes, bg_file) != frame_bytes) {
                fprintf(stderr, "Could not read %ux%u BGR24 background: %s\n", width, height, background_path);
                if (bg_file) fclose(bg_file);
                status = 1;
                goto cleanup;
            }
            fclose(bg_file);
        } else {
            memset(raw, 0, frame_bytes);
        }
        CHECK("Alloc bg CPU", image_alloc(&bg_cpu, width, height, NVCV_BGR, NVCV_U8, NVCV_CHUNKY, NVCV_CPU, 1));
        CHECK("Alloc bg GPU", image_alloc(&bg_gpu, width, height, NVCV_RGB, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 1));
        for (unsigned y = 0; y < height; ++y)
            memcpy((uint8_t *)bg_cpu.pixels + y * bg_cpu.pitch, raw + y * width * 3, width * 3);
        CHECK("Upload background", image_transfer(&bg_cpu, &bg_gpu, 1.0f, stream, &tmp));
    }
    const NvCVImage *fg = relight ? &relit : &src_rgb;
    const NvCVImage *bg = background_path || remove_background ? &bg_gpu : &src_rgb;
    fprintf(stderr, "Camera effects ready (%s%s%s%s%s); reading %ux%u BGR24 frames\n",
            denoise ? "noise removal " : "", background_path ? "background " : "",
            blur ? "background blur " : "", remove_background ? "background removal " : "",
            relight ? "Studio Light" : "", width, height);

    unsigned long frames = 0;
    ULONGLONG started = GetTickCount64();
    for (;;) {
        size_t got = fread(raw, 1, frame_bytes, stdin);
        if (got == 0 && feof(stdin)) break;
        if (got != frame_bytes) {
            fprintf(stderr, "Incomplete input frame after %lu frames (%zu bytes)\n", frames, got);
            status = 1;
            goto cleanup;
        }
        for (unsigned y = 0; y < height; ++y)
            memcpy((uint8_t *)src_cpu.pixels + y * src_cpu.pitch, raw + y * width * 3, width * 3);
        CHECK("Transfer input", image_transfer(&src_cpu, &src_gpu, 1.0f, stream, &tmp));
        if (denoise) {
            CHECK("Denoise input", image_transfer(&src_gpu, &dn_in, 1.0f / 255.0f, stream, &tmp));
            CHECK("Run(Denoising)", run(denoise, 0));
            CHECK("Denoise output", image_transfer(&dn_out, &src_gpu, 255.0f, stream, &tmp));
        }
        if (!need_mask) {
            CHECK("Transfer output", image_transfer(&src_gpu, &out_cpu, 1.0f, stream, &tmp));
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
            CHECK("Transfer output", image_transfer(&blur_out, &out_cpu, 1.0f, stream, &tmp));
        } else {
            CHECK("Transfer output", image_transfer(&out_gpu, &out_cpu, 1.0f, stream, &tmp));
        }
    write:
        CHECK("Synchronize", stream_sync(stream));
        for (unsigned y = 0; y < height; ++y)
            memcpy(raw + y * width * 3, (uint8_t *)out_cpu.pixels + y * out_cpu.pitch, width * 3);
        if (fwrite(raw, 1, frame_bytes, stdout) != frame_bytes || fflush(stdout) != 0) {
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
        NvCVImage *images[] = {&src_cpu, &src_gpu, &src_rgb, &mask, &relit, &projected, &hdr, &light_mask, &scaled_mask,
                               &bg_cpu, &bg_gpu, &out_gpu, &out_cpu, &dn_in, &dn_out, &blur_in, &blur_out, &tmp};
        for (size_t i = 0; i < sizeof images / sizeof *images; i++)
            if (images[i]->pixels) image_free(images[i]);
    }
    if (state) free_state(gs, state);
    if (gs) destroy(gs);
    if (relight) destroy(relight);
    if (denoise_state) free_state(denoise, denoise_state);
    if (denoise) destroy(denoise);
    if (blur) destroy(blur);
    if (stream) stream_destroy(stream);
    free(hdr_pixels);
    free(raw);
    FreeLibrary(cv);
    FreeLibrary(vfx);
    return status == 0 ? 0 : 1;
}
