// The code-signing fcntls given a struct the guest cannot read must fail as they do natively,
// instead of crashing the runtime: EFAULT, or EBADF for an invalid descriptor or one that is not
// a vnode (a pipe or a socket, say), which XNU checks before it copies the struct in. Every row
// is refused before the kernel looks for a signature, so none is added to any file. Each row runs
// in its own child process so a crash is reported as that row's failure; a row whose child cannot
// be started or collected also fails.
// Exit status 0 when every row matches native.
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static long pg;
static char *guard;     // a PROT_NONE page between two read-write ones
static int file;        // a file under /System, which the runtime answers F_GETSIGSINFO for itself
static int not_a_file;  // the read end of a pipe

// The first `n` bytes of an fsignatures_t, as the last bytes before the unreadable page. They name
// no signature: a start far past the end of the file and, in row 6's 24 bytes, a size as large.
static void *ending_at_guard(size_t n) {
    fsignatures_t fs = {.fs_file_start = 1LL << 40, .fs_blob_size = 1UL << 40};
    memcpy(guard - n, &fs, n);
    return guard - n;
}

// An fgetsigsinfo_t whose fg_file_start is on the unreadable page and the rest on the next one.
static void *starting_in_guard(void) {
    int *rest = (int *)(guard + pg);
    rest[0] = GETSIGSINFO_PLATFORM_BINARY;
    rest[1] = 0;
    return guard + pg - 8;
}

static int row(int id) {
    switch (id) {
    case 1: return fcntl(file, F_ADDFILESIGS_RETURN, guard);
    case 2: return fcntl(file, F_ADDFILESIGS_RETURN, (void *)0x1000);
    case 3: return fcntl(file, F_ADDFILESIGS_RETURN, NULL);
    case 4: return fcntl(file, F_ADDFILESIGS_RETURN, ending_at_guard(16));  // 24 bytes are copied in
    case 5: return fcntl(file, F_ADDFILESIGS_INFO, guard);
    case 6: return fcntl(file, F_ADDFILESIGS_INFO, ending_at_guard(24));    // 32 bytes are copied in
    case 7: return fcntl(file, F_GETSIGSINFO, guard);
    case 8: return fcntl(file, F_GETSIGSINFO, starting_in_guard());
    case 9: return fcntl(-1, F_ADDFILESIGS_RETURN, guard);
    case 10: return fcntl(-1, F_ADDFILESIGS_RETURN, NULL);
    case 11: return fcntl(not_a_file, F_ADDFILESIGS_RETURN, guard);
    case 12: return fcntl(-1, F_CHECK_LV, NULL);
    }
    return -2;
}

static const struct { const char *what; int err; } rows[] = {
    {0, 0},
    {"fcntl(<a file>, F_ADDFILESIGS_RETURN, <unreadable page>)", EFAULT},
    {"fcntl(<a file>, F_ADDFILESIGS_RETURN, <unmapped address>)", EFAULT},
    {"fcntl(<a file>, F_ADDFILESIGS_RETURN, NULL)", EFAULT},
    {"fcntl(<a file>, F_ADDFILESIGS_RETURN, <struct running into an unreadable page after 16 bytes>)", EFAULT},
    {"fcntl(<a file>, F_ADDFILESIGS_INFO, <unreadable page>)", EFAULT},
    {"fcntl(<a file>, F_ADDFILESIGS_INFO, <struct running into an unreadable page after 24 bytes>)", EFAULT},
    {"fcntl(<a /System file>, F_GETSIGSINFO, <unreadable page>)", EFAULT},
    {"fcntl(<a /System file>, F_GETSIGSINFO, <struct starting on an unreadable page>)", EFAULT},
    {"fcntl(<bad descriptor>, F_ADDFILESIGS_RETURN, <unreadable page>)", EBADF},
    {"fcntl(<bad descriptor>, F_ADDFILESIGS_RETURN, NULL)", EBADF},
    {"fcntl(<a pipe>, F_ADDFILESIGS_RETURN, <unreadable page>)", EBADF},
    {"fcntl(<bad descriptor>, F_CHECK_LV, NULL)", EBADF},
};

int main(void) {
    pg = sysconf(_SC_PAGESIZE);
    char *m = mmap(NULL, 3 * pg, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
    int p[2];
    if (m == MAP_FAILED || mprotect(m + pg, pg, PROT_NONE) != 0) {
        perror("mmap/mprotect");
        return 2;
    }
    guard = m + pg;
    file = open("/System/Library/CoreServices/SystemVersion.plist", O_RDONLY);
    if (file < 0 || pipe(p) != 0) {
        perror("open/pipe");
        return 2;
    }
    not_a_file = p[0];
    int failed = 0;
    for (int id = 1; id < (int)(sizeof rows / sizeof rows[0]); id++) {
        fflush(stdout);
        pid_t pid = fork();
        if (pid == -1) {
            printf("FAIL %s: fork: %s\n", rows[id].what, strerror(errno));
            failed = 1;
            continue;
        }
        if (pid == 0) {
            errno = 0;
            int r = row(id);
            _exit(r == -1 ? (errno & 0xff) : 0);
        }
        int status;
        pid_t w;
        do
            w = waitpid(pid, &status, 0);
        while (w == -1 && errno == EINTR);
        if (w != pid) {
            printf("FAIL %s: waitpid: %s\n", rows[id].what, w == -1 ? strerror(errno) : "wrong child");
            failed = 1;
            continue;
        }
        int ok = WIFEXITED(status) && WEXITSTATUS(status) == rows[id].err;
        if (WIFSIGNALED(status))
            printf("FAIL %s: killed by signal %d\n", rows[id].what, WTERMSIG(status));
        else if (!ok)
            printf("FAIL %s: errno %d, expected %d\n", rows[id].what, WEXITSTATUS(status), rows[id].err);
        else
            printf("PASS %s\n", rows[id].what);
        failed |= !ok;
    }
    return failed;
}
