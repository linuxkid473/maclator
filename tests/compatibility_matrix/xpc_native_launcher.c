// Experimental entry only for the runner's self-owned temporary .xpc bundles.
// No Foundation/libxpc initialization and no environment mutation before exec.
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#if !defined(MCL_XPC_RUNTIME) || !defined(MCL_XPC_SYSROOT) || !defined(MCL_XPC_PAYLOAD) || !defined(MCL_XPC_LOG)
#error "This probe requires explicit runtime, sysroot, payload and owned log paths"
#endif

extern char **environ;

int main(int argc, char **argv) {
    size_t entries = 0;
    for (char **p = environ; p && *p; ++p) ++entries;
    int fd = open(MCL_XPC_LOG, O_WRONLY | O_CREAT | O_APPEND, 0600);
    if (fd >= 0) {
        dprintf(fd, "native-wrapper-entry pid=%d argc=%d env_count=%zu xpc_service_name=%d xpc_flags=%d cf_bundle_id=%d\n",
                getpid(), argc, entries, getenv("XPC_SERVICE_NAME") != NULL,
                getenv("XPC_FLAGS") != NULL, getenv("__CFBundleIdentifier") != NULL);
        dprintf(fd, "native-wrapper-exec runtime=%s sysroot=%s payload=%s argv0=%s\n",
                MCL_XPC_RUNTIME, MCL_XPC_SYSROOT, MCL_XPC_PAYLOAD,
                argc ? argv[0] : "");
        close(fd);
    }
    // execl preserves PID and passes environ unchanged. The probe checks whether
    // the new runtime retains a usable service endpoint across this exec.
    // The service-listener probe takes no command-line arguments of its own.
    execl(MCL_XPC_RUNTIME, MCL_XPC_RUNTIME, "--no-aot", "--sysroot", MCL_XPC_SYSROOT,
          MCL_XPC_PAYLOAD, (char *)NULL);
    int saved = errno;
    fd = open(MCL_XPC_LOG, O_WRONLY | O_CREAT | O_APPEND, 0600);
    if (fd >= 0) {
        dprintf(fd, "native-wrapper-exec-failed errno=%d message=%s\n", saved, strerror(saved));
        close(fd);
    }
    return 126;
}
