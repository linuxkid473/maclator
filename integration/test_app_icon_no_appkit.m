// Verify injection leaves a non-GUI guest without AppKit or NSApplication.
#import <Foundation/Foundation.h>
#include <objc/runtime.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>

int main(int argc, const char **argv) {
    @autoreleasepool {
        if (argc != 3) return 2;
        setenv("MACLATOR_APP_ICON", argv[2], 1);
        int before = objc_getClass("NSApplication") != Nil;
        void *shim = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
        if (!shim) { fprintf(stderr, "dlopen: %s\n", dlerror()); return 3; }
        int after = objc_getClass("NSApplication") != Nil;
        int (*applied)(void) = dlsym(shim, "mcl_application_icon_applied");
        int did_apply = applied ? applied() : -1;
        int failed = before || after || did_apply != 0;
        printf("ICON_NO_APPKIT before=%d after=%d applied=%d result=%s\n",
               before, after, did_apply, failed ? "FAIL" : "PASS");
        return failed;
    }
}
