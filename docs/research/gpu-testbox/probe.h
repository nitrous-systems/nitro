#pragma once
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <poll.h>
#include <errno.h>
static long kv(const char *path, const char *key) {
    FILE *f = fopen(path, "r"); if (!f) return -1;
    char line[256]; size_t n = strlen(key); long v = -1;
    while (fgets(line, sizeof line, f)) if (!strncmp(line, key, n) && line[n] == ':') { v = atol(line + n + 1); break; }
    fclose(f); return v;
}
static double now_ms(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e3 + t.tv_nsec / 1e6; }
static void cp(const char *what) {   /* KiB */
    const char *s = "/proc/self/status", *r = "/proc/self/smaps_rollup";
    printf("CP %-22s rss=%6ld anon=%6ld file=%6ld shmem=%5ld | pss=%6ld pss_anon=%6ld pss_file=%6ld\n", what,
           kv(s,"VmRSS"), kv(s,"RssAnon"), kv(s,"RssFile"), kv(s,"RssShmem"), kv(r,"Pss"), kv(r,"Pss_Anon"), kv(r,"Pss_File"));
    fflush(stdout);
}
static int fd_signaled(int fd, int ms) { struct pollfd p = { .fd = fd, .events = POLLIN }; return poll(&p, 1, ms) == 1; }
#define RESULT(name, ok, ...) do { printf("RESULT %-34s %s  ", name, (ok) ? "PASS" : "FAIL"); printf(__VA_ARGS__); printf("\n"); fflush(stdout);} while (0)
#define TY 128
#define TU 100
#define TV 160
static void expected_rgb(float *o) {  /* BT.709 narrow */
    float y = (TY - 16) / 219.0f, cb = (TU - 128) / 224.0f, cr = (TV - 128) / 224.0f;
    o[0] = y + 1.5748f * cr; o[1] = y - 0.1873f * cb - 0.4681f * cr; o[2] = y + 1.8556f * cb;
}
