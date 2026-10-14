#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;
static int clean(void) {
    const char *dyld = getenv("DYLD_INSERT_LIBRARIES");
    return !getenv("MACLATOR_APP_ICON") && (!dyld || !strstr(dyld, "libmclappicon.dylib"));
}
int main(int argc, char **argv) {
    if (argc > 1) { puts(clean() ? "PASS: ARM child shim removed" : "FAIL: child inherited icon"); return clean() ? 0 : 1; }
    if (!getenv("MACLATOR_APP_ICON")) return 2;
    pid_t child;
    char *args[] = {argv[0], "child", NULL};
    if (posix_spawn(&child, argv[0], NULL, NULL, args, environ)) return 3;
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status)) return 4;
    // A native helper must not ask the host dyld to load an ARM-only shim.
    char *native_args[] = {"/bin/sh", "-c", "test -z \"$MACLATOR_APP_ICON\" && case \"$DYLD_INSERT_LIBRARIES\" in *libmclappicon*) exit 1;; *) exit 0;; esac", NULL};
    if (posix_spawn(&child, "/bin/sh", NULL, NULL, native_args, environ)) return 5;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status)) return 6;
    puts("PASS: native child shim removed");
    return 0;
}
