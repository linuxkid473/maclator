// Read-only probe for optional Xcode component path redirection.
#include <errno.h>
#include <fcntl.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/attr.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static const char *guest_dir = "/Library/Developer/PrivateFrameworks/MaclatorProbe.framework";
static const char *guest_file = "/Library/Developer/PrivateFrameworks/MaclatorProbe.framework/probe.txt";
extern char **environ;

static int check_fd(int fd, const char *label) {
    char data[32] = {0};
    if (fd < 0) {
        printf("FAIL %s: %s\n", label, strerror(errno));
        return 1;
    }
    int ok = read(fd, data, sizeof(data) - 1) == 7 && strcmp(data, "overlay") == 0;
    close(fd);
    printf("%s %s\n", ok ? "PASS" : "FAIL", label);
    return !ok;
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "child") == 0)
        return check_fd(open(guest_file, O_RDONLY), "child inherited overlay");
    int failures = 0;
    failures += check_fd(open(guest_file, O_RDONLY), "absolute overlay open");
    int directory = open(guest_dir, O_RDONLY | O_DIRECTORY);
    if (directory < 0) {
        perror("overlay directory");
        return 2;
    }
    failures += check_fd(openat(directory, "probe.txt", O_RDONLY), "relative openat");
    struct stat st;
    int ok = fstatat(directory, "probe.txt", &st, 0) == 0 && st.st_size == 7;
    printf("%s relative fstatat\n", ok ? "PASS" : "FAIL");
    failures += !ok;
    char target[128] = {0};
    ssize_t length = readlinkat(directory, "relative-link", target, sizeof(target)-1);
    ok = length == 9 && strcmp(target, "probe.txt") == 0;
    printf("%s relative readlinkat\n", ok ? "PASS" : "FAIL");
    failures += !ok;
    failures += check_fd(openat(directory, "../MaclatorProbe.framework/probe.txt", O_RDONLY), "normalized parent within overlay");
    char path[1024] = {0};
    if (fcntl(directory, F_GETPATH, path) == 0) {
        printf("F_GETPATH=%s\n", path);
        ok = strcmp(path, guest_dir) == 0;
        printf("%s virtual F_GETPATH\n", ok ? "PASS" : "FAIL");
        failures += !ok;
    } else {
        puts("FAIL F_GETPATH");
        failures++;
    }
    char *real = realpath(guest_file, NULL);
    printf("realpath=%s\n", real ? real : strerror(errno));
    free(real);
    struct attrlist attrs = {0};
    attrs.bitmapcount = ATTR_BIT_MAP_COUNT;
    attrs.commonattr = ATTR_CMN_FULLPATH;
    struct { uint32_t size; attrreference_t reference; char bytes[2048]; } buffer = {0};
    if (getattrlist(guest_file, &attrs, &buffer, sizeof(buffer), 0) == 0) {
        const char *fullpath = (char *)&buffer.reference + buffer.reference.attr_dataoffset;
        printf("ATTR_CMN_FULLPATH=%s\n", fullpath);
        ok = strcmp(fullpath, guest_file) == 0;
        printf("%s virtual FULLPATH-only attribute\n", ok ? "PASS" : "FAIL");
        failures += !ok;
    } else {
        puts("FAIL FULLPATH-only attribute");
        failures++;
    }
    close(directory);
    // The payload shadows CoreTypes.bundle, but this existing host file is
    // deliberately absent from the tiny fixture: relative reads must fall back.
    directory = open("/System/Library/CoreServices/CoreTypes.bundle/Contents", O_RDONLY | O_DIRECTORY);
    if (directory < 0) {
        perror("CoreTypes directory");
        return 2;
    }
    int fd = openat(directory, "Info.plist", O_RDONLY);
    ok = fd >= 0;
    printf("%s relative host fallback\n", ok ? "PASS" : "FAIL");
    failures += !ok;
    if (fd >= 0) close(fd);
    int duplicate = dup(directory);
    fd = openat(duplicate, "Info.plist", O_RDONLY);
    ok = fd >= 0;
    printf("%s duplicated dirfd host fallback\n", ok ? "PASS" : "FAIL");
    failures += !ok;
    if (fd >= 0) close(fd);
    int replaced = open("/", O_RDONLY | O_DIRECTORY);
    if (dup2(directory, replaced) < 0) {
        puts("FAIL dup2");
        failures++;
    } else {
        fd = openat(replaced, "Info.plist", O_RDONLY);
        ok = fd >= 0;
        printf("%s dup2 dirfd host fallback\n", ok ? "PASS" : "FAIL");
        failures += !ok;
        if (fd >= 0) close(fd);
    }
    close(replaced);
    int cloexec = fcntl(directory, F_DUPFD_CLOEXEC, 20);
    fd = openat(cloexec, "Info.plist", O_RDONLY);
    ok = fd >= 0;
    printf("%s F_DUPFD_CLOEXEC host fallback\n", ok ? "PASS" : "FAIL");
    failures += !ok;
    if (fd >= 0) close(fd);
    if (cloexec >= 0) close(cloexec);
    close(duplicate);
    close(directory);
    pid_t child;
    char *child_args[] = {argv[0], "child", NULL};
    int error = posix_spawn(&child, argv[0], NULL, NULL, child_args, environ);
    int status = 0;
    ok = error == 0 && waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0;
    printf("%s spawned child overlay propagation\n", ok ? "PASS" : "FAIL");
    failures += !ok;
    return failures ? 1 : 0;
}
