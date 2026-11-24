// libc's sysctlbyname() looks a name up with name2oid (sysctl {0, 3}) and then asks by number, and
// sysctlnametomib() is that lookup alone. The names the runtime answers itself
// (hw.optional.arm.FEAT_LSE2 and the rest) must be found that way too where the host kernel does not
// have them, and then give the answer the sysctlbyname system call gives; lookups of other names must
// still behave as they do natively. Each row runs in its own child process, under a 60-second alarm,
// so a crash or a hang is reported as that row's failure; a row whose child cannot be started or
// collected also fails. Exit status 0 when every row passes, as each does natively.
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/sysctl.h>
#include <sys/wait.h>
#include <unistd.h>
#pragma clang diagnostic ignored "-Wdeprecated-declarations"

static char *guard;  // a PROT_NONE page, right after a read-write one
static const int name2oid[2] = {0, 3};
static const char lse2[] = "hw.optional.arm.FEAT_LSE2";

static void *ending_at_guard(const void *bytes, size_t n) {
    memcpy(guard - n, bytes, n);
    return guard - n;
}

// An int sysctl asked for by libc's sysctlbyname (0), by sysctlnametomib and sysctl (1) or by the
// sysctlbyname system call (2): the result, errno, the length and the value.
struct answer { long rc; int err; size_t len; int val; };
static struct answer lookup(int route, const char *name) {
    struct answer a = {0, 0, sizeof(int), -1};
    int mib[CTL_MAXNAME];
    size_t n = CTL_MAXNAME;
    if (route == 0)
        a.rc = sysctlbyname(name, &a.val, &a.len, NULL, 0);
    else if (route == 1)
        a.rc = sysctlnametomib(name, mib, &n) == -1 ? -1 : sysctl(mib, (u_int)n, &a.val, &a.len, NULL, 0);
    else
        a.rc = syscall(SYS_sysctlbyname, name, strlen(name), &a.val, &a.len, NULL, 0);
    a.err = a.rc == -1 ? errno : 0;
    return a;
}

// Two failures with the same errno, or two successes with the same 4-byte value.
static int same(struct answer a, struct answer b) {
    if (a.rc == -1 || b.rc == -1) return a.rc == b.rc && a.err == b.err;
    return a.rc == 0 && b.rc == 0 && a.len == sizeof(int) && b.len == sizeof(int) && a.val == b.val;
}

// 0 if the three ways of asking give the same answer, else 1.
static int agree(const char *name) {
    struct answer a = lookup(0, name);
    return same(a, lookup(1, name)) && same(a, lookup(2, name)) ? 0 : 1;
}

// 0 for a 4-byte 0 or 1, -1 with errno set for a failure, 1 for anything else.
static int found(struct answer a) {
    if (a.rc == -1) return errno = a.err, -1;
    return a.rc == 0 && a.len == sizeof(int) && (a.val == 0 || a.val == 1) ? 0 : 1;
}

static int row(int id) {
    int mib[CTL_MAXNAME], mib2[CTL_MAXNAME], one = 1, v = 7;
    size_t n = CTL_MAXNAME, n2 = CTL_MAXNAME, len = sizeof mib;
    char buf[64] = {0};
    switch (id) {
    case 1: return found(lookup(0, lse2));
    case 2: {  // and if the MIB is one no kernel gives (a negative first entry), the runtime answered
               // the lookup itself, so the value must be its system call's
        struct answer a = lookup(1, lse2);
        if (found(a)) return found(a);
        if (!same(a, lookup(0, lse2))) return 1;
        if (sysctlnametomib(lse2, mib, &n) == -1) return -1;
        return mib[0] >= 0 || same(a, lookup(2, lse2)) ? 0 : 1;
    }
    case 3: return agree("hw.optional.arm64");
    case 4: return agree("hw.optional.AdvSIMD");
    case 5:  // the host's own MIB, which the kernel turns back into the name (oid2name, {0, 1, ...})
        n = CTL_MAXNAME - 2;
        if (sysctlnametomib("hw.pagesize", mib + 2, &n) == -1) return -1;
        mib[0] = 0, mib[1] = 1, len = sizeof buf;
        if (sysctl(mib, (u_int)n + 2, buf, &len, NULL, 0) == -1) return -1;
        return strcmp(buf, "hw.pagesize") == 0 ? 0 : 1;
    case 6: {
        len = sizeof v;
        int a = sysctlbyname("hw.optional.arm.FEAT_MACLATOR_NONE", &v, &len, NULL, 0) == -1 ? errno : 0;
        int b = sysctlnametomib("hw.optional.arm.FEAT_MACLATOR_NONE", mib, &n) == -1 ? errno : 0;
        return a == ENOENT && b == ENOENT ? (errno = ENOENT, -1) : 1;
    }
    case 7: return sysctl((int *)name2oid, 2, mib, &len, guard, 10) == -1 ? -1 : 1;
    case 8: return sysctl((int *)name2oid, 2, mib, &len, ending_at_guard(lse2, sizeof lse2 - 1), sizeof lse2) == -1 ? -1 : 1;
    case 9:  // no NUL: name2oid copies exactly newlen bytes
        if (sysctl((int *)name2oid, 2, mib, &len, ending_at_guard(lse2, sizeof lse2 - 1), sizeof lse2 - 1) == -1) return -1;
        return len == 4 * sizeof(int) ? 0 : 1;
    case 10: return sysctlnametomib(lse2, NULL, &n) == -1 ? -1 : n == 4 ? 0 : 1;
    case 11:  // too small for the whole MIB: nothing is copied and the size becomes 0
        memset(mib, 0x5a, sizeof mib), n = 2;
        if (sysctlnametomib(lse2, mib, &n) == 0) return 1;
        if (errno != ENOMEM) return -1;
        return n == 0 && mib[0] == 0x5a5a5a5a && mib[1] == 0x5a5a5a5a ? -1 : 1;
    case 12:  // a write, given oldp and oldlenp too, is refused and changes neither
        len = sizeof v;
        if (sysctlbyname(lse2, &v, &len, &one, sizeof one) == 0) return 1;
        if (errno != EPERM) return -1;
        return v == 7 && len == sizeof v ? -1 : 1;
    case 13:  // one trailing dot is dropped
        if (sysctlnametomib("hw.optional.arm.FEAT_LSE2.", mib, &n) == -1 || sysctlnametomib(lse2, mib2, &n2) == -1) return -1;
        return n == 4 && n2 == 4 && memcmp(mib, mib2, 4 * sizeof(int)) == 0 ? 0 : 1;
    case 14: return sysctl((int *)name2oid, 2, mib, &len, NULL, 10) == -1 ? -1 : 1;
    case 15: {  // a write to a name macOS does not have, through libc and through the system call
        int a = sysctlbyname("hw.optional.AdvSIMD", NULL, NULL, &one, sizeof one) == -1 ? errno : 0;
        int b = syscall(SYS_sysctlbyname, "hw.optional.AdvSIMD", 19UL, NULL, NULL, &one, sizeof one) == -1 ? errno : 0;
        return a == ENOENT && b == ENOENT ? (errno = ENOENT, -1) : 1;
    }
    }
    return -2;
}

static const struct { const char *what; int err; } rows[] = {
    {0, 0},
    {"sysctlbyname(hw.optional.arm.FEAT_LSE2) is found", 0},
    {"sysctlnametomib + sysctl(hw.optional.arm.FEAT_LSE2) give sysctlbyname's answer", 0},
    {"hw.optional.arm64: sysctlbyname, sysctlnametomib + sysctl and the system call agree", 0},
    {"hw.optional.AdvSIMD (not in macOS 27.0.1's kernel): the three agree", 0},
    {"sysctlnametomib(hw.pagesize) is the host's MIB for hw.pagesize", 0},
    {"a name that does not exist: ENOENT from sysctlbyname and sysctlnametomib", ENOENT},
    {"name2oid of a name on an unreadable page: EFAULT", EFAULT},
    {"name2oid of a name running into an unreadable page: EFAULT", EFAULT},
    {"name2oid of a name ending just before an unreadable page is found", 0},
    {"sysctlnametomib(hw.optional.arm.FEAT_LSE2, NULL) gives a size of 4", 0},
    {"sysctlnametomib with room for 2 entries: ENOMEM, size 0, nothing copied", ENOMEM},
    {"sysctlbyname write to hw.optional.arm.FEAT_LSE2: EPERM, old value untouched", EPERM},
    {"hw.optional.arm.FEAT_LSE2. (trailing dot) has the same MIB", 0},
    {"name2oid of a NULL name with newlen 10: ENOENT", ENOENT},
    {"hw.optional.AdvSIMD write: ENOENT from sysctlbyname and the system call", ENOENT},
};

int main(void) {
    long pg = sysconf(_SC_PAGESIZE);
    char *readable = mmap(NULL, 2 * pg, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
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
        if (pid == 0) {  // the alarm must fire even if SIGALRM came in ignored or blocked
            sigset_t alrm;
            sigemptyset(&alrm);
            sigaddset(&alrm, SIGALRM);
            signal(SIGALRM, SIG_DFL);
            sigprocmask(SIG_UNBLOCK, &alrm, NULL);
            alarm(60);
            errno = 0;
            int r = row(id);
            _exit(r == -1 ? (errno & 0xff) : r == 0 ? 0 : 255);
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
        else if (!ok && WEXITSTATUS(status) == 255)
            printf("FAIL %s: wrong answer\n", rows[id].what);
        else if (!ok)
            printf("FAIL %s: errno %d, expected %d\n", rows[id].what, WEXITSTATUS(status), rows[id].err);
        else
            printf("PASS %s\n", rows[id].what);
        failed |= !ok;
    }
    return failed;
}
