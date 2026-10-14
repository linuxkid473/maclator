#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
extern char **environ;

int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "child")) {
#ifdef __aarch64__
        puts("child-architecture=arm64");
#else
        puts("child-architecture=x86_64");
#endif
        return 7;
    }
    if (argc != 2) return 2;
    char *args[] = {argv[1], "child", NULL};
    pid_t pid;
    int error = posix_spawn(&pid, argv[1], NULL, NULL, args, environ);
    if (error) { fprintf(stderr, "spawn: %d\n", error); return 3; }
    int status;
    if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status) || WEXITSTATUS(status) != 7) return 4;
    puts("PASS: architecture routing and child exit status");
    return 0;
}
