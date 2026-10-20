// Linux-only syscall fault seam; used only against disposable test homes.
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void record(const char *event) {
    const char *path = getenv("STATE_CAP_TEST_TRACE");
    if (!path) return;
    FILE *trace = fopen(path, "a");
    if (trace) {
        fprintf(trace, "%s\n", event);
        fclose(trace);
    }
}

int fsync(int fd) {
    int (*real_fsync)(int) = dlsym(RTLD_NEXT, "fsync");
    const char *root = getenv("STATE_CAP_TEST_ROOT");
    const char *source = getenv("STATE_CAP_TEST_SOURCE");
    char descriptor[64], path[PATH_MAX], sessions[PATH_MAX];
    snprintf(descriptor, sizeof(descriptor), "/proc/self/fd/%d", fd);
    ssize_t length = readlink(descriptor, path, sizeof(path) - 1);
    if (!root || !source || length < 0) return real_fsync(fd);
    path[length] = '\0';
    snprintf(sessions, sizeof(sessions), "%s/sessions", root);
    const char *stage = NULL;
    if (strcmp(path, root) == 0) stage = "index";
    if (strcmp(path, sessions) == 0) stage = access(source, F_OK) == 0 ? "archive" : "source";
    if (stage) {
        const char *fail = getenv("STATE_CAP_TEST_FAIL");
        char event[64];
        int inject = fail && strcmp(stage, fail) == 0;
        snprintf(event, sizeof(event), "%s-%s", inject ? "fail" : "sync", stage);
        record(event);
        if (inject) {
            errno = EIO;
            return -1;
        }
    }
    return real_fsync(fd);
}

int unlink(const char *path) {
    int (*real_unlink)(const char *) = dlsym(RTLD_NEXT, "unlink");
    const char *source = getenv("STATE_CAP_TEST_SOURCE");
    if (source && strcmp(path, source) == 0) record("unlink-source");
    return real_unlink(path);
}
