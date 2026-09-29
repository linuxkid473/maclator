#include <dispatch/dispatch.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/wait.h>
int main(int argc, char **argv) {
    // Make the process multithreaded with libdispatch active, like Chromium.
    dispatch_async(dispatch_get_global_queue(0, 0), ^{ usleep(1000); });
    usleep(200000);
    pid_t p = fork();
    if (p == 0) {
        if (argc > 1) {
            execl(argv[1], argv[1], "child-exec", (char *)0);
            _exit(99);
        }
        write(1, "child alive\n", 12);
        _exit(7);
    }
    int st = 0;
    waitpid(p, &st, 0);
    printf("parent: child exited status=%d\n", WEXITSTATUS(st));
    return 0;
}
