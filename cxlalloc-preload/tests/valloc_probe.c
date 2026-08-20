#define _GNU_SOURCE

#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int mapping_for(const void *pointer, char *line_out, size_t line_out_size)
{
    FILE *maps = fopen("/proc/self/maps", "r");
    if (maps == NULL) {
        perror("fopen /proc/self/maps");
        return -1;
    }

    char *line = NULL;
    size_t capacity = 0;
    const uintptr_t address = (uintptr_t)pointer;
    int found = 0;

    while (getline(&line, &capacity, maps) >= 0) {
        uintptr_t start = 0;
        uintptr_t end = 0;
        if (sscanf(line, "%" SCNxPTR "-%" SCNxPTR, &start, &end) == 2 && address >= start && address < end) {
            snprintf(line_out, line_out_size, "%s", line);
            found = 1;
            break;
        }
    }

    free(line);
    fclose(maps);
    return found ? 0 : -1;
}

int main(void)
{
    const char *size_text = getenv("CXLALLOC_PROBE_SIZE");
    errno = 0;
    char *end = NULL;
    const unsigned long long parsed = strtoull(size_text != NULL ? size_text : "134217728", &end, 10);
    if (errno != 0 || end == (size_text != NULL ? size_text : "134217728") || *end != '\0' || parsed < 16 ||
        parsed > SIZE_MAX) {
        fprintf(stderr, "invalid CXLALLOC_PROBE_SIZE\n");
        return 2;
    }

    const size_t large_size = (size_t)parsed;
    volatile uint64_t *large = valloc(large_size);
    void *small = malloc(4096);
    if (large == NULL || small == NULL) {
        fprintf(stderr, "allocation failed: large=%p small=%p errno=%d\n", (void *)large, small, errno);
        free((void *)large);
        free(small);
        return 3;
    }

    large[0] = UINT64_C(0x1122334455667788);
    large[(large_size / sizeof(*large)) - 1] = UINT64_C(0x8877665544332211);
    if (large[0] != UINT64_C(0x1122334455667788) ||
        large[(large_size / sizeof(*large)) - 1] != UINT64_C(0x8877665544332211)) {
        fprintf(stderr, "scalar DAX readback failed\n");
        return 4;
    }

    char large_map[1024] = {0};
    char small_map[1024] = {0};
    if (mapping_for((const void *)large, large_map, sizeof(large_map)) != 0 ||
        mapping_for(small, small_map, sizeof(small_map)) != 0) {
        fprintf(stderr, "could not locate allocation mapping\n");
        return 5;
    }

    printf("large=%p map=%s", (void *)large, large_map);
    printf("small=%p map=%s", small, small_map);

    const int large_is_dax = strstr(large_map, "/dev/dax0.0") != NULL;
    const int small_is_dax = strstr(small_map, "/dev/dax0.0") != NULL;
    free((void *)large);
    free(small);

    if (!large_is_dax || small_is_dax) {
        fprintf(stderr, "placement mismatch: large_is_dax=%d small_is_dax=%d\n", large_is_dax, small_is_dax);
        return 6;
    }

    puts("placement=PASS");
    return 0;
}
