/* Small parent so wait4's child peak RSS does not include Python's pre-exec
 * heap. This reports the workload's resources, not fixture/hash construction. */
#define _DEFAULT_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc < 3) return 2;
    struct timespec started, finished;
    if (clock_gettime(CLOCK_MONOTONIC, &started)) return 2;
    pid_t child = fork();
    if (child < 0) { perror("fork"); return 2; }
    if (child == 0) {
        execvp(argv[2], argv + 2);
        perror("execvp");
        _exit(127);
    }
    struct rusage usage;
    int status;
    while (wait4(child, &status, 0, &usage) < 0) {
        if (errno == EINTR) continue;
        perror("wait4");
        return 2;
    }
    if (clock_gettime(CLOCK_MONOTONIC, &finished)) return 2;
    FILE *metrics = fopen(argv[1], "w");
    if (!metrics) { perror("metrics"); return 2; }
    int written = fprintf(metrics, "%.6f %.6f %ld %.6f\n",
        usage.ru_utime.tv_sec + usage.ru_utime.tv_usec / 1000000.0,
        usage.ru_stime.tv_sec + usage.ru_stime.tv_usec / 1000000.0,
        usage.ru_maxrss,
        (finished.tv_sec - started.tv_sec) * 1000.0 + (finished.tv_nsec - started.tv_nsec) / 1000000.0);
    if (fclose(metrics) || written < 0) return 2;
    return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
}
