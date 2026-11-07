/* Fixed benign boundary probe. No repository code, commands or host paths are accepted. */
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/statvfs.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int check(int ok, const char *name) {
    if (!ok) { fprintf(stderr, "fixed probe failed: %s (errno=%d)\n", name, errno); return 0; }
    return 1;
}
static long long number(const char *path) {
    FILE *f = fopen(path, "r"); long long value = -1;
    if (!f) return -1;
    if (fscanf(f, "%lld", &value) != 1) value = -1;
    fclose(f); return value;
}
static long long throttled(void) {
    FILE *f = fopen("/sys/fs/cgroup/cpu.stat", "r");
    char key[64]; long long value = 0;
    if (!f) return -1;
    while (fscanf(f, "%63s %lld", key, &value) == 2) {
        if (!strcmp(key, "nr_throttled")) { fclose(f); return value; }
    }
    fclose(f); return -1;
}
static double now(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts)) return 0;
    return ts.tv_sec + ts.tv_nsec / 1000000000.0;
}
static int network(void) {
    int fd = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    if (!check(fd >= 0, "TCP socket")) return 0;
    struct sockaddr_in address = {.sin_family=AF_INET, .sin_port=htons(9)};
    /* RFC 5737 documentation address; no packet leaves a network=none namespace. */
    inet_pton(AF_INET, "192.0.2.1", &address.sin_addr);
    int result = connect(fd, (struct sockaddr *)&address, sizeof(address));
    int saved = errno; close(fd);
    if (!check(result == -1 && (saved == ENETUNREACH || saved == EHOSTUNREACH), "outbound route denied")) return 0;
    fd = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP); saved = errno;
    if (fd >= 0) close(fd);
    return check(fd < 0 && saved == EPERM, "raw socket capability denied");
}
static int filesystem(void) {
    if (!check(getuid() == 65534 && getgid() == 65534, "unprivileged identity")) return 0;
    struct statvfs fs;
    if (!check(!statvfs("/candidate", &fs) && (fs.f_flag & ST_RDONLY), "read-only rootfs")) return 0;
    FILE *f = fopen("/candidate/fixture.txt", "r"); char buf[64] = {0};
    if (!check(f != NULL, "fixture readable")) return 0;
    int valid = fgets(buf, sizeof(buf), f) && !strcmp(buf, "agenct benign fixture\n");
    fclose(f); if (!check(valid, "exact fixture contents")) return 0;
    if (!check(access("/candidate/.git", F_OK) != 0 && access("/var/run/docker.sock", F_OK) != 0, "no git metadata or daemon socket")) return 0;
    int fd = open("/candidate/fixture.txt", O_WRONLY | O_TRUNC);
    if (fd >= 0) { close(fd); return check(0, "source write denied"); }
    fd = open("/host-marker", O_WRONLY | O_CREAT | O_EXCL, 0600);
    if (fd >= 0) { close(fd); return check(0, "root write denied"); }
    const char *paths[] = {"/scratch/probe", "/tmp/probe"};
    for (int i = 0; i < 2; i++) {
        fd = open(paths[i], O_WRONLY | O_CREAT | O_EXCL, 0700);
        if (!check(fd >= 0, "bounded scratch writable")) return 0;
        if (!check(write(fd, "fixed data", 10) == 10 && !close(fd), "scratch write")) return 0;
    }
    pid_t pid = fork();
    if (pid == 0) {
        char *args[] = {"/tmp/probe", NULL}; char *env[] = {NULL};
        execve(args[0], args, env); _exit(errno == EACCES ? 0 : 1);
    }
    int status = -1;
    if (!check(pid > 0 && waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0, "tmp noexec enforced")) return 0;
    unlink(paths[0]); unlink(paths[1]);
    if (!check(!statvfs("/scratch", &fs) && fs.f_blocks * fs.f_frsize <= 134217728, "scratch capacity")) return 0;
    if (!check(!statvfs("/tmp", &fs) && fs.f_blocks * fs.f_frsize <= 33554432, "tmp capacity")) return 0;
    /* At most 33 MiB is attempted; ENOSPC must occur at the 32 MiB tmpfs bound. */
    fd = open("/tmp/capacity", O_WRONLY | O_CREAT | O_EXCL, 0600);
    if (!check(fd >= 0, "capacity fixture open")) return 0;
    char block[65536]; memset(block, 'x', sizeof(block));
    size_t total = 0; int exhausted = 0;
    while (total < 33 * 1024 * 1024) {
        ssize_t wrote = write(fd, block, sizeof(block));
        if (wrote < 0) { exhausted = errno == ENOSPC; break; }
        if (wrote == 0) break;
        total += (size_t)wrote;
    }
    close(fd); unlink("/tmp/capacity");
    return check(exhausted && total <= 33554432, "tmpfs write quota enforced");
}
static int resources(void) {
    /* Require actual cgroup v2 controls in the container, not merely CLI flags. */
    if (!check(number("/sys/fs/cgroup/memory.max") == 536870912 && number("/sys/fs/cgroup/memory.swap.max") == 0, "memory/swap cgroup limits")) return 0;
    if (!check(number("/sys/fs/cgroup/pids.max") == 64, "PID cgroup limit")) return 0;
    FILE *f = fopen("/sys/fs/cgroup/cpu.max", "r"); long long quota = 0, period = 0;
    if (!check(f && fscanf(f, "%lld %lld", &quota, &period) == 2 && quota == period && period > 0, "CPU cgroup quota")) { if (f) fclose(f); return 0; }
    fclose(f);
    pid_t children[80]; size_t count = 0; int denied = 0;
    for (; count < 80; count++) {
        pid_t pid = fork();
        if (pid == 0) { sleep(8); _exit(0); }
        if (pid < 0) { denied = errno == EAGAIN; break; }
        children[count] = pid;
    }
    for (size_t i = 0; i < count; i++) kill(children[i], SIGKILL);
    for (size_t i = 0; i < count; i++) waitpid(children[i], NULL, 0);
    if (!check(denied && count > 0 && count < 64, "PID quota enforced")) return 0;
    long long before = throttled();
    for (size_t i = 0; i < 2; i++) {
        children[i] = fork();
        if (children[i] == 0) { double end = now() + 1.5; volatile unsigned long value = 1;
            while (now() < end) { for (int n = 0; n < 10000; n++) value = value * 1664525 + 1013904223; }
            _exit(0);
        }
        if (!check(children[i] > 0, "CPU fixture fork")) return 0;
    }
    for (size_t i = 0; i < 2; i++) waitpid(children[i], NULL, 0);
    return check(before >= 0 && throttled() > before, "CPU throttling observed");
}
int main(int argc, char **argv) {
    (void)argc;
    if (strstr(argv[0], "/memory") != NULL) {
        /* Fixed 600 MiB allocation exceeds only this container's 512 MiB cgroup. */
        size_t size = 600UL * 1024 * 1024;
        volatile char *allocation = malloc(size);
        if (!allocation) return 86;
        for (size_t offset = 0; offset < size; offset += 4096) allocation[offset] = 1;
        free((void *)allocation); return 87; /* Must be OOM-killed, never complete. */
    }
    if (strstr(argv[0], "/hang") != NULL) { sleep(300); return 99; }
    return filesystem() && network() && resources() ? 0 : 1;
}
