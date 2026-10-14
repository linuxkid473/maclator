#include <errno.h>
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "child") == 0) {
        sleep(3);
        return 7;
    }
    pid_t child;
    char *child_args[] = {argv[0], "child", NULL};
    int error = posix_spawn(&child, argv[0], NULL, NULL, child_args, environ);
    if (error) {
        fprintf(stderr, "posix_spawn: %s\n", strerror(error));
        return 1;
    }
    printf("child=%d\n", child);
    fflush(stdout);
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 7)
        return 2;
    puts("PASS: guest child arguments and exit status");
    return 0;
}
