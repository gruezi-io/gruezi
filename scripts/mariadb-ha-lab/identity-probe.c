/* Disposable lab probe: prove no_new_privs survives exec of a setuid binary. */
#include <stdio.h>
#include <unistd.h>

int main(void) {
    printf("uid=%lu euid=%lu\n", (unsigned long)getuid(),
           (unsigned long)geteuid());
    return 0;
}
