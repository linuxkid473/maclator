// Mapping a file executable at offset 0, as dyld maps a library's __TEXT, must not crash the
// runtime, which reads the start of such a mapping as a Mach-O header to register it with the
// translation cache. Each row maps a 4 KiB file holding an arm64 header that claims more load
// commands than the file holds, in its own child process, so a crash in one row is reported as
// that row's failure; a row whose child cannot be started or collected, or whose setup fails, also
// fails. The child prints what mmap answered, so the output can be compared with a native run, and
// the row passes only when the child then exits with status 0. Run it with the translation cache
// on (the default; with --no-aot the runtime does not read these headers), MACLATOR_CACHE set to a
// temporary directory and MACLATOR_NO_AOT_BUILD=1: a run writes profile files into the cache.
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static int fd;  // the 4 KiB file

static int row(int id) {
    switch (id) {
    case 1: return mmap(NULL, 4096, PROT_EXEC, MAP_PRIVATE, fd, 0) == MAP_FAILED ? -1 : 0;
    case 2: {  // 4 KiB of the file, then address space that cannot be read
        char *r = mmap(NULL, 0x10000, PROT_NONE, MAP_ANON | MAP_PRIVATE, -1, 0);
        if (r == MAP_FAILED) return -2;
        return mmap(r, 4096, PROT_READ | PROT_EXEC, MAP_FIXED | MAP_PRIVATE, fd, 0) == MAP_FAILED ? -1 : 0;
    }
    case 3:  // only an Intel host can fault here: on Apple Silicon the rest of the 16 KiB page reads as zeros
        return mmap(NULL, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd, 0) == MAP_FAILED ? -1 : 0;
    }
    return -2;
}

static const char *rows[] = {
    0,
    "mmap(PROT_EXEC) of a header that cannot be read",
    "mmap(MAP_FIXED) of a header whose load commands run into an unreadable page",
    "mmap of a header whose load commands run past the end of the file",
};

int main(void) {
    char path[] = "/tmp/bad_aot_header.XXXXXX";
    uint32_t h[4096 / 4] = {0xfeedfacf, 0x0100000c, 0, 6 /* MH_DYLIB */, 2 /* ncmds */, 0x3100 /* sizeofcmds */};
    h[8] = 0x19;    // first command: LC_SEGMENT_64...
    h[9] = 0x3000;  // ...whose size puts the second one at byte 0x3020, past the end of the file
    fd = mkstemp(path);
    if (fd < 0 || unlink(path) != 0 || write(fd, h, sizeof h) != sizeof h) {
        perror("setup");
        return 2;
    }
    int failed = 0;
    for (int id = 1; id < (int)(sizeof rows / sizeof rows[0]); id++) {
        fflush(stdout);
        pid_t pid = fork();
        if (pid == -1) {
            printf("FAIL %s: fork: %s\n", rows[id], strerror(errno));
            failed = 1;
            continue;
        }
        if (pid == 0) {
            errno = 0;
            int r = row(id), e = errno;
            if (r == -2) _exit(255);
            if (r == 0) printf("%s: mapped\n", rows[id]);
            else printf("%s: errno %d (%s)\n", rows[id], e, strerror(e));
            fflush(stdout);
            _exit(0);
        }
        int status;
        pid_t w;
        do
            w = waitpid(pid, &status, 0);
        while (w == -1 && errno == EINTR);
        if (w != pid) {
            printf("FAIL %s: waitpid: %s\n", rows[id], w == -1 ? strerror(errno) : "wrong child");
            failed = 1;
        } else if (WIFSIGNALED(status)) {
            printf("FAIL %s: killed by signal %d\n", rows[id], WTERMSIG(status));
            failed = 1;
        } else if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
            printf("FAIL %s: exit status %d\n", rows[id], WIFEXITED(status) ? WEXITSTATUS(status) : -1);
            failed = 1;
        } else {
            printf("PASS %s\n", rows[id]);
        }
    }
    return failed;
}
