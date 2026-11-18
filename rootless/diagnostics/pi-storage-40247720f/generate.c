#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
int main(int argc, char **argv) {
    if (argc != 3) return 2;
    uint64_t state = strtoull(argv[1], NULL, 10);
    size_t left = strtoull(argv[2], NULL, 10);
    uint64_t block[512];
    while (left >= sizeof(block)) {
        for (int i = 0; i < 512; i++) {
            uint64_t z = (state += UINT64_C(0x9e3779b97f4a7c15));
            z = (z ^ (z >> 30)) * UINT64_C(0xbf58476d1ce4e5b9);
            z = (z ^ (z >> 27)) * UINT64_C(0x94d049bb133111eb);
            block[i] = z ^ (z >> 31);
        }
        if (fwrite(block, 1, sizeof(block), stdout) != sizeof(block)) return 1;
        left -= sizeof(block);
    }
    return left || fclose(stdout);
}
