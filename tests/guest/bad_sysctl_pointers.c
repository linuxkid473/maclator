// sysctlbyname(2) and sysctl(2) given a name or MIB the guest cannot read must fail with EFAULT,
// as they do natively, instead of crashing the runtime. Rows 14 to 23 check that ordinary lookups
// still reach the host and that the names the runtime answers itself behave as natively (a write
// is EPERM and changes nothing, newp or newlen alone is still a read, the MIB count is 32 bits,
// one trailing dot is dropped). The raw system calls are used because libc's sysctlbyname()
// measures the name with strlen() in the guest first. Each row runs in its own child process so a
// crash is reported as that row's failure; a row whose child cannot be started or collected also
// fails. Exit status 0 when every row matches native.
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/sysctl.h>
#include <sys/wait.h>
#include <unistd.h>
#pragma clang diagnostic ignored "-Wdeprecated-declarations"

static long pg;
static char *guard;  // a PROT_NONE page, right after a read-write one

static void *ending_at_guard(const void *bytes, size_t n) {
    memcpy(guard - n, bytes, n);
    return guard - n;
}

// A successful lookup must give `want`: "arm64" for hw.machine, the arm64 machine the guest sees
// (a lookup that reaches the x86_64 host succeeds too, with "x86_64"), or "Darwin" for kern.ostype,
// which only the host can answer. Returns -1 with errno set when the call fails, 0 on success with
// `want`, 1 on anything else (another answer, or a result that is neither 0 nor -1).
static const char *want = "arm64";
static int checked(long r, const char *buf) {
    if (r == -1) return -1;
    return r == 0 && strcmp(buf, want) == 0 ? 0 : 1;
}

static int byname(const void *name, size_t namelen) {
    char buf[64] = {0};
    size_t len = sizeof buf;
    return checked(syscall(SYS_sysctlbyname, name, namelen, buf, &len, NULL, 0), buf);
}

static int bymib(const void *mib, unsigned long n) {
    char buf[64] = {0};
    size_t len = sizeof buf;
    return checked(syscall(SYS_sysctl, mib, n, buf, &len, NULL, 0), buf);
}

// A write (newp and newlen both set) to a read-only name: -1 with errno set, or 1 if it succeeds.
static int write_byname(const void *name, size_t namelen) {
    int one = 1;
    return syscall(SYS_sysctlbyname, name, namelen, NULL, NULL, &one, sizeof one) == -1 ? -1 : 1;
}

static const int hw_machine[2] = {CTL_HW, HW_MACHINE}, kern_ostype[2] = {CTL_KERN, KERN_OSTYPE};

static long hw_machine_call(int mib, char *buf, size_t *len, const void *newp, size_t newlen) {
    return mib ? syscall(SYS_sysctl, hw_machine, 2UL, buf, len, newp, newlen)
               : syscall(SYS_sysctlbyname, "hw.machine", 10UL, buf, len, newp, newlen);
}

// newp without newlen, and newlen without newp, are not writes: both are reads.
static int read_with_half_a_write(int mib) {
    int one = 1;
    for (int half = 0; half < 2; half++) {
        char buf[64] = {0};
        size_t len = sizeof buf;
        long r = hw_machine_call(mib, buf, &len, half ? NULL : &one, half ? sizeof one : 0);
        if (checked(r, buf) != 0) return checked(r, buf);
    }
    return 0;
}

// A write given oldp and oldlenp too: -1 with errno set, or 1 if it succeeds or changes either.
static int write_keeps_old(int mib) {
    char buf[64], same[sizeof buf];
    size_t len = sizeof buf;
    int one = 1;
    memset(buf, 0x5a, sizeof buf);
    memset(same, 0x5a, sizeof same);
    long r = hw_machine_call(mib, buf, &len, &one, sizeof one);
    return r == -1 && memcmp(buf, same, sizeof buf) == 0 && len == sizeof buf ? -1 : 1;
}

// Rows 10 to 13: while one thread looks up hw.machine 20000 times, another keeps taking away and
// restoring access to the page that holds the name or MIB, or keeps changing it to one that names
// nothing ("hw.machinX", {CTL_HW, 99}). The kernel copies it once per call, so every call must
// answer "arm64" or fail with `other` (EFAULT or ENOENT). A runtime that checks one copy and then
// lets the host kernel read guest memory again can answer with the x86_64 host's machine instead.
// The lookups start only once the other thread has made its first change.
static atomic_int racing, started, failed_errno;
static unsigned *word, flip;

static void *flip_access(void *page) {
    while (atomic_load(&racing)) {
        if (mprotect(page, pg, PROT_NONE) != 0 || mprotect(page, pg, PROT_READ | PROT_WRITE) != 0) {
            atomic_store(&failed_errno, errno);
            break;
        }
        atomic_store(&started, 1);
    }
    atomic_store(&started, 1);
    return NULL;
}

static void *flip_word(void *unused) {
    while (atomic_load(&racing)) {
        *(volatile unsigned *)word ^= flip;
        atomic_store(&started, 1);
    }
    return NULL;
}

static int race(int mib, int access, int other) {
    unsigned *p = mmap(NULL, pg, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
    if (p == MAP_FAILED) return -1;
    memcpy(p, mib ? (const void *)hw_machine : "hw.machine", mib ? 8 : 10);
    word = mib ? &p[1] : &p[2];  // HW_MACHINE, or the bytes "ne\0\0"
    flip = mib ? 1 ^ 99 : ('e' ^ 'X') << 8;
    pthread_t t;
    atomic_store(&racing, 1);
    if ((errno = pthread_create(&t, NULL, access ? flip_access : flip_word, p)) != 0) return -1;
    while (!atomic_load(&started)) sched_yield();
    int r = 0;
    for (int i = 0; i < 20000 && r == 0 && !atomic_load(&failed_errno); i++) {
        errno = 0;
        r = mib ? bymib(p, 2) : byname(p, 10);
        if (r == -1 && errno == other) r = 0;
    }
    int e = errno;
    atomic_store(&racing, 0);
    pthread_join(t, NULL);
    errno = atomic_load(&failed_errno) ? atomic_load(&failed_errno) : e;
    return atomic_load(&failed_errno) ? -1 : r;
}

static int row(int id) {
    switch (id) {
    case 1: return byname("hw.machine", 10);
    case 2: return bymib(hw_machine, 2);
    case 3: return byname(guard, 10);
    case 4: return byname((const void *)0x1000, 10);
    case 5: return byname(ending_at_guard("hw.machine", 10), 11);  // one byte past the readable page
    case 6: return byname(ending_at_guard("hw.machine", 10), 10);  // exactly up to it
    case 7: return bymib(guard, 2);
    case 8: return bymib((const void *)0x1000, 2);
    case 9: return bymib(ending_at_guard(hw_machine, 4), 2);      // second entry in the unreadable page
    case 10: return race(0, 1, EFAULT);
    case 11: return race(1, 1, EFAULT);
    case 12: return race(0, 0, ENOENT);
    case 13: return race(1, 0, ENOENT);
    case 14: want = "Darwin"; return byname("kern.ostype", 11);
    case 15: want = "Darwin"; return bymib(kern_ostype, 2);
    case 16: return bymib(hw_machine, 0x100000002UL);  // XNU reads the count as a 32-bit u_int
    case 17: return byname("hw.machine.", 11);         // XNU drops one trailing dot
    case 18: return write_byname("hw.machineX", 10);
    case 19: return write_keeps_old(1);
    case 20: return write_byname("hw.optional.arm64", 17);
    case 21: return read_with_half_a_write(0);
    case 22: return read_with_half_a_write(1);
    case 23: return write_keeps_old(0);
    }
    return -2;
}

static const struct { const char *what; int err; } rows[] = {
    {0, 0},
    {"sysctlbyname(\"hw.machine\") is arm64", 0},
    {"sysctl({CTL_HW, HW_MACHINE}) is arm64", 0},
    {"sysctlbyname(<unreadable page>)", EFAULT},
    {"sysctlbyname(<unmapped address>)", EFAULT},
    {"sysctlbyname(<name running into an unreadable page>)", EFAULT},
    {"sysctlbyname(<name ending just before an unreadable page>) is arm64", 0},
    {"sysctl(<MIB on an unreadable page>)", EFAULT},
    {"sysctl(<unmapped MIB>)", EFAULT},
    {"sysctl(<MIB running into an unreadable page>)", EFAULT},
    {"sysctlbyname(\"hw.machine\") while its page loses and regains access is arm64 or EFAULT", 0},
    {"sysctl({CTL_HW, HW_MACHINE}) while its page loses and regains access is arm64 or EFAULT", 0},
    {"sysctlbyname(\"hw.machine\") while it changes to \"hw.machinX\" is arm64 or ENOENT", 0},
    {"sysctl({CTL_HW, HW_MACHINE}) while it changes to {CTL_HW, 99} is arm64 or ENOENT", 0},
    {"sysctlbyname(\"kern.ostype\") is Darwin", 0},
    {"sysctl({CTL_KERN, KERN_OSTYPE}) is Darwin", 0},
    {"sysctl({CTL_HW, HW_MACHINE}) with 0x100000002 in the count register is arm64", 0},
    {"sysctlbyname(\"hw.machine.\") is arm64", 0},
    {"sysctlbyname(\"hw.machineX\", namelen 10) write", EPERM},
    {"sysctl({CTL_HW, HW_MACHINE}) write leaves oldp and oldlenp untouched", EPERM},
    {"sysctlbyname(\"hw.optional.arm64\") write", EPERM},
    {"sysctlbyname(\"hw.machine\") with only newp, or only newlen, is a read: arm64", 0},
    {"sysctl({CTL_HW, HW_MACHINE}) with only newp, or only newlen, is a read: arm64", 0},
    {"sysctlbyname(\"hw.machine\") write leaves oldp and oldlenp untouched", EPERM},
};

int main(void) {
    pg = sysconf(_SC_PAGESIZE);
    char *m = mmap(NULL, 2 * pg, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
    if (m == MAP_FAILED || mprotect(m + pg, pg, PROT_NONE) != 0) {
        perror("mmap/mprotect");
        return 2;
    }
    guard = m + pg;
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
        else if (!ok && WEXITSTATUS(status) == 99)
            printf("FAIL %s: a wrong answer\n", rows[id].what);
        else if (!ok)
            printf("FAIL %s: errno %d, expected %d\n", rows[id].what, WEXITSTATUS(status), rows[id].err);
        else
            printf("PASS %s\n", rows[id].what);
        failed |= !ok;
    }
    return failed;
}
