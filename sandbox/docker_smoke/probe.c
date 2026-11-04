/* Fixed benign fixture. Never executes repository code or accepts a command. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/statvfs.h>
#include <unistd.h>

int main(int argc, char **argv) {
    (void)argc;
    if (strstr(argv[0], "/hang") != NULL) {
        sleep(300);
        return 99;
    }
    if (getuid() != 65534 || getgid() != 65534) return 10;
    struct statvfs fs;
    if (statvfs("/candidate", &fs) || !(fs.f_flag & ST_RDONLY)) return 11;
    FILE *f = fopen("/candidate/fixture.txt", "r");
    char buf[64] = {0};
    if (!f || !fgets(buf, sizeof(buf), f) || strcmp(buf, "agenct benign fixture\n")) return 12;
    fclose(f);
    if (access("/candidate/.git", F_OK) == 0) return 13;
    int fd = open("/candidate/fixture.txt", O_WRONLY | O_TRUNC);
    if (fd >= 0) { close(fd); return 14; }
    const char *paths[] = {"/scratch/probe", "/tmp/probe"};
    for (int i = 0; i < 2; i++) {
        fd = open(paths[i], O_WRONLY | O_CREAT | O_EXCL, 0600);
        if (fd < 0) return 15;
        if (write(fd, "ok", 2) != 2 || close(fd) || unlink(paths[i])) return 16;
    }
    return 0;
}
