/* Asks the Linux CUDA driver for an export table by UUID and prints its size and
 * function pointers. Use it when the Wine relay logs "Unknown UUID": build with
 * `cc -D_GNU_SOURCE -o probe tools/cuda_export_table_probe.c -ldl` after editing
 * the UUID bytes below. */
#include <dlfcn.h>
#include <stdio.h>
#include <string.h>

typedef int (*cuInit_t)(unsigned);
typedef int (*cuGetExportTable_t)(const void **, const unsigned char *);

int main(void)
{
    static const unsigned char uuid[16] = {0xd2, 0x68, 0x8b, 0xf2, 0x83, 0x71, 0xe2, 0x4f,
                                           0x94, 0x69, 0x81, 0x77, 0xc8, 0xcc, 0xec, 0x27};
    void *lib = dlopen("libcuda.so.1", RTLD_NOW);
    if (!lib) { fprintf(stderr, "%s\n", dlerror()); return 1; }
    cuInit_t init = (cuInit_t)dlsym(lib, "cuInit");
    cuGetExportTable_t get = (cuGetExportTable_t)dlsym(lib, "cuGetExportTable");
    printf("cuInit=%d\n", init(0));
    const void *table = NULL;
    int r = get(&table, uuid);
    printf("cuGetExportTable=%d table=%p\n", r, table);
    if (r || !table) return 2;
    const unsigned long *words = table;
    printf("size field (first 8 bytes)=%lu (0x%lx)\n", words[0], words[0]);
    unsigned long n = words[0] / 8;
    if (n > 64) n = 64;
    for (unsigned long i = 1; i < n + 2; i++) {
        Dl_info info;
        memset(&info, 0, sizeof(info));
        int ok = dladdr((void *)words[i], &info);
        printf("[%2lu] %#018lx %s+%#lx\n", i, words[i], ok && info.dli_fname ? info.dli_fname : "?",
               ok ? words[i] - (unsigned long)info.dli_fbase : 0UL);
    }
    return 0;
}
