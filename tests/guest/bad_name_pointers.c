// Path-taking system calls given a name the guest cannot read must fail with EFAULT, as they do
// natively, instead of crashing the runtime. Each row runs in its own child process so a crash in
// one row is reported as that row's failure; a row whose child cannot be started or collected
// also fails. Exit status 0 when every row matches native.
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static char *readable;  // a read-write page
static char *guard;     // the PROT_NONE page right after it

// The last `n` bytes of the readable page, ending where the guard page begins.
static char *ending_at_guard(const char *bytes, size_t n) {
    memcpy(guard - n, bytes, n);
    return guard - n;
}

static int row(int id) {
    struct stat st;
    switch (id) {
    case 1: return open(guard, O_RDONLY);
    case 2: return openat(AT_FDCWD, guard, O_RDONLY);
    case 3: return stat(guard, &st);
    case 4: return access(guard, F_OK);
    case 5: return open((const char *)0x1000, O_RDONLY);
    case 6: return open(ending_at_guard("aaaaaaaa", 8), O_RDONLY);           // no NUL before the guard
    case 7: return open(ending_at_guard("/maclator-no-such-file", 23), O_RDONLY);  // NUL is the last readable byte
    }
    return -2;
}

static const struct { const char *what; int err; } rows[] = {
    {0, 0},
    {"open(<unreadable page>)", EFAULT},
    {"openat(AT_FDCWD, <unreadable page>)", EFAULT},
    {"stat(<unreadable page>)", EFAULT},
    {"access(<unreadable page>)", EFAULT},
    {"open(<unmapped address>)", EFAULT},
    {"open(<name running into an unreadable page>)", EFAULT},
    {"open(<name ending just before an unreadable page>)", ENOENT},
};

int main(void) {
    long pg = sysconf(_SC_PAGESIZE);
    readable = mmap(NULL, 2 * pg, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
    if (readable == MAP_FAILED || mprotect(readable + pg, pg, PROT_NONE) != 0) {
        perror("mmap/mprotect");
        return 2;
    }
    guard = readable + pg;
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
