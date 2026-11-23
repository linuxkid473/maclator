// posix_spawn(2) and execve(2) given a path, argument vector, argument or environment vector the
// guest cannot read must fail with EFAULT, as they do natively, instead of crashing the runtime;
// arguments larger than ARG_MAX in total must fail with E2BIG. As in XNU, each argument costs its
// bytes as passed (row 12), its NUL and an 8-byte pointer, and is read no further than the space
// left, so running out of space comes before a fault past it (rows 8 and 10; row 9 the reverse).
// Rows 1 and 11 check that ordinary spawns still reach the child, with every argument. Each row
// runs in its own child process so a crash is reported as that row's failure; a row whose child
// cannot be started or collected also fails. Exit status 0 when every row matches native.
#include <errno.h>
#include <limits.h>
#include <mach-o/dyld.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static char self[4096];  // this program's path
static char *guard;      // a PROT_NONE page, right after ARG_MAX bytes of 'a'

// The environment the rows pass: a small fixed one, so that no row depends on the size of the
// runner's, holding the marker that makes a child started by mistake exit 99 at once.
static char *env[] = {"BAD_SPAWN_POINTERS_CHILD=1", NULL};

// waitpid, retried while a signal interrupts it.
static pid_t wait_for(pid_t pid, int *status) {
    pid_t w;
    do
        w = waitpid(pid, status, 0);
    while (w == -1 && errno == EINTR);
    return w;
}

// posix_spawn's error as -1 with errno set, or 0 once the program it started has exited with
// status 7 (1 if it exited otherwise).
static int spawn(const char *path, char *const argv[], char *const envp[]) {
    pid_t pid;
    int st, e = posix_spawn(&pid, path, NULL, NULL, argv, envp);
    if (e) {
        errno = e;
        return -1;
    }
    return wait_for(pid, &st) == pid && WIFEXITED(st) && WEXITSTATUS(st) == 7 ? 0 : 1;
}

// `n` arguments: self, then `first` (if set), then `rest` repeated, then `last` (if set).
static char **args(int n, char *first, char *rest, char *last) {
    char **v = calloc(n + 1, sizeof *v);
    if (!v) return NULL;
    for (int i = 0; i < n; i++) v[i] = rest;
    v[0] = self;
    if (first) v[1] = first;
    if (last) v[n - 1] = last;
    return v;
}

static int row(int id) {
    // The space left for argv[1] once argv[0] (self, its NUL and its pointer) is charged.
    size_t space = ARG_MAX - (strlen(self) + 1) - 8;
    char **v;
    switch (id) {
    case 1: return spawn(self, args(2, "exit7", NULL, NULL), env);
    case 2: return spawn(guard, args(1, NULL, NULL, NULL), env);
    case 3: return spawn(self, (char **)guard, env);
    case 4: return spawn(self, args(2, guard, NULL, NULL), env);
    case 5: return execve(self, (char **)0x1000, env);
    // The runtime reads envp only with a GPU shim on its search path (for example
    // MACLATOR_GPU_DIR), so only then does row 6 reach runtime code; otherwise the host kernel
    // answers it.
    case 6: return spawn(self, args(2, "exit7", NULL, NULL), (char **)guard);
    case 7: {
        char *s = malloc(ARG_MAX + 1);
        if (!s) return -1;
        memset(s, 'a', ARG_MAX);
        s[ARG_MAX] = 0;
        return spawn(self, args(2, s, NULL, NULL), env);
    }
    case 8: return spawn(self, args(2, guard - space, NULL, NULL), env);        // no NUL within the space
    case 9: return spawn(self, args(2, guard - (space - 1), NULL, NULL), env);  // the fault comes first
    case 10:  // 120000 empty arguments: 1 byte each fits, 9 with their pointers does not
        return (v = args(120002, NULL, "", guard)) ? spawn(self, v, env) : -1;
    case 11: return (v = args(5000, "count", "x", NULL)) ? spawn(self, v, env) : -1;
    case 12: {  // ARG_MAX / 2 bytes of 0xff fit; converted to UTF-8 (3 bytes each) they would not
        char *s = malloc(ARG_MAX / 2 + 1);
        if (!s) return -1;
        memset(s, 0xff, ARG_MAX / 2);
        s[ARG_MAX / 2] = 0;
        return spawn(self, args(3, s, NULL, guard), env);
    }
    }
    return -2;
}

static const struct { const char *what; int err; } rows[] = {
    {0, 0},
    {"posix_spawn(self, \"exit7\") exits 7", 0},
    {"posix_spawn(<unreadable path>)", EFAULT},
    {"posix_spawn(self, <unreadable argv>)", EFAULT},
    {"posix_spawn(self, argv[1] = <unreadable page>)", EFAULT},
    {"execve(self, <unmapped argv>)", EFAULT},
    {"posix_spawn(self, <unreadable envp>)", EFAULT},
    {"posix_spawn(self, argv[1] = ARG_MAX bytes and a NUL)", E2BIG},
    {"posix_spawn(self, argv[1] = <the space left, then an unreadable page>)", E2BIG},
    {"posix_spawn(self, argv[1] = <one byte less, then an unreadable page>)", EFAULT},
    {"posix_spawn(self, 120000 empty arguments, <unreadable page>)", E2BIG},
    {"posix_spawn(self, 5000 arguments) reaches the child with all 5000", 0},
    {"posix_spawn(self, argv[1] = ARG_MAX / 2 bytes of 0xff, argv[2] = <unreadable page>)", EFAULT},
};

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "exit7") == 0) return 7;
    if (argc > 1 && strcmp(argv[1], "count") == 0) return argc == 5000 ? 7 : 8;
    if (getenv("BAD_SPAWN_POINTERS_CHILD")) return 99;  // a spawn or exec that should have failed
    uint32_t n = sizeof self;
    long pg = sysconf(_SC_PAGESIZE);
    char *m = mmap(NULL, ARG_MAX + pg, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
    if (_NSGetExecutablePath(self, &n) != 0 || m == MAP_FAILED || mprotect(m + ARG_MAX, pg, PROT_NONE) != 0) {
        perror("setup");
        return 2;
    }
    memset(m, 'a', ARG_MAX);
    guard = m + ARG_MAX;
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
            _exit(r == -1 && errno ? (errno & 0xff) : r == 0 ? 0 : 99);  // 99: a wrong answer
        }
        int status;
        pid_t w = wait_for(pid, &status);
        if (w != pid) {
            printf("FAIL %s: waitpid: %s\n", rows[id].what, w == -1 ? strerror(errno) : "wrong child");
            failed = 1;
            continue;
        }
        int ok = WIFEXITED(status) && WEXITSTATUS(status) == rows[id].err;
        if (WIFSIGNALED(status))
            printf("FAIL %s: killed by signal %d\n", rows[id].what, WTERMSIG(status));
        else if (!ok && WEXITSTATUS(status) == 99)
            printf("FAIL %s: succeeded with the wrong answer\n", rows[id].what);
        else if (!ok)
            printf("FAIL %s: errno %d, expected %d\n", rows[id].what, WEXITSTATUS(status), rows[id].err);
        else
            printf("PASS %s\n", rows[id].what);
        failed |= !ok;
    }
    return failed;
}
