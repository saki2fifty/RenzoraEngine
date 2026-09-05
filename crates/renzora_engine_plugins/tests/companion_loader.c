#include <dlfcn.h>
#include <stdio.h>

/* No Rust runtime: a companion cannot borrow symbols from this test host. */
int main(int argc, char **argv) {
    int failed = 0;
    for (int i = 1; i < argc; ++i) {
        void *library = dlopen(argv[i], RTLD_NOW | RTLD_LOCAL);
        if (!library) {
            fprintf(stderr, "%s: %s\n", argv[i], dlerror());
            failed = 1;
            continue;
        }
        if (!dlsym(library, "renzora_plugin_init")) {
            fprintf(stderr, "%s: missing plugin entry\n", argv[i]);
            failed = 1;
        }
        dlclose(library);
    }
    return failed;
}
