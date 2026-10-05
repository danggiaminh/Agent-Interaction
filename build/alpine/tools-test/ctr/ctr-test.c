/* One static binary for the container tests: the image holds nothing else (FROM scratch). */
#include <netdb.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static void slurp(const char *path) {
	char buf[4096];
	ssize_t n;
	int fd = open(path, O_RDONLY);
	if (fd < 0) {
		printf("open %s: %s\n", path, strerror(errno));
		return;
	}
	while ((n = read(fd, buf, sizeof buf)) > 0)
		fwrite(buf, 1, (size_t)n, stdout);
	close(fd);
}

static double now(clockid_t c) {
	struct timespec t;
	clock_gettime(c, &t);
	return (double)t.tv_sec + (double)t.tv_nsec / 1e9;
}

int main(int argc, char **argv) {
	const char *mode = argc > 1 ? argv[1] : "echo";
	setvbuf(stdout, NULL, _IOLBF, 0);
	if (!strcmp(mode, "echo")) {
		for (int i = 2; i < argc; i++)
			printf("%s%s", argv[i], i + 1 < argc ? " " : "\n");
	} else if (!strcmp(mode, "mem")) { /* mem MiB: allocate, touch, hold */
		size_t mb = (size_t)atol(argv[2]);
		for (size_t i = 0; i < mb; i++) {
			char *p = malloc(1 << 20);
			if (!p) {
				puts("mem-failed");
				return 1;
			}
			memset(p, 1, 1 << 20);
		}
		puts("mem-ok");
		sleep(2);
	} else if (!strcmp(mode, "fork")) { /* fork N children that sleep; report how many succeeded */
		int n = atoi(argv[2]), made = 0;
		for (int i = 0; i < n; i++) {
			pid_t p = fork();
			if (p == 0) {
				sleep(5);
				_exit(0);
			}
			if (p < 0)
				break;
			made++;
		}
		printf("forked=%d of %d%s\n", made, n, made < n ? " (refused)" : "");
		for (int i = 0; i < made; i++)
			wait(NULL);
	} else if (!strcmp(mode, "cat")) {
		slurp(argv[2]);
	} else if (!strcmp(mode, "write")) {
		int fd = open(argv[2], O_WRONLY | O_CREAT, 0644);
		if (fd < 0)
			printf("write %s: %s\n", argv[2], strerror(errno));
		else
			printf("write %s: ok\n", argv[2]);
	} else if (!strcmp(mode, "caps")) {
		FILE *f = fopen("/proc/self/status", "r");
		char line[256];
		while (f && fgets(line, sizeof line, f))
			if (!strncmp(line, "CapEff:", 7) || !strncmp(line, "NoNewPrivs:", 11))
				fputs(line, stdout);
	} else if (!strcmp(mode, "ifaces")) {
		FILE *f = fopen("/proc/net/dev", "r");
		char line[512];
		for (int i = 0; f && fgets(line, sizeof line, f); i++)
			if (i >= 2) {
				char *c = strchr(line, ':');
				if (c)
					*c = 0;
				char *s = line;
				while (*s == ' ')
					s++;
				printf("%s ", s);
			}
		puts("");
	} else if (!strcmp(mode, "listen")) { /* listen PORT: answer one connection with a line */
		int s = socket(AF_INET, SOCK_STREAM, 0), one = 1;
		struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons((unsigned short)atoi(argv[2]))};
		setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
		if (bind(s, (struct sockaddr *)&a, sizeof a) || listen(s, 1)) {
			perror("listen");
			return 1;
		}
		puts("listening");
		int c = accept(s, NULL, NULL);
		(void)!write(c, "pong\n", 5);
		close(c);
	} else if (!strcmp(mode, "dial")) { /* dial HOST PORT: resolve, connect (retrying for 15 s), print the line the peer sends */
		struct addrinfo hints = {.ai_family = AF_INET, .ai_socktype = SOCK_STREAM}, *res = NULL;
		char buf[64];
		int s = -1;
		for (int i = 0; i < 30 && s < 0; i++) {
			if (getaddrinfo(argv[2], argv[3], &hints, &res) == 0) {
				s = socket(AF_INET, SOCK_STREAM, 0);
				if (connect(s, res->ai_addr, res->ai_addrlen)) {
					close(s);
					s = -1;
				}
				freeaddrinfo(res);
			}
			if (s < 0)
				usleep(500000);
		}
		if (s < 0) {
			puts("dial: failed");
			return 1;
		}
		ssize_t n = read(s, buf, sizeof buf - 1);
		buf[n > 0 ? n : 0] = 0;
		printf("dial got %s", buf);
	} else if (!strcmp(mode, "cpu")) { /* cpu SECONDS: burn, report cpu time / wall time */
		double w0 = now(CLOCK_MONOTONIC), c0 = now(CLOCK_PROCESS_CPUTIME_ID), secs = atof(argv[2]);
		volatile unsigned long x = 0;
		while (now(CLOCK_MONOTONIC) - w0 < secs)
			for (int i = 0; i < 100000; i++)
				x += (unsigned long)i;
		printf("cpu-ratio=%.2f\n", (now(CLOCK_PROCESS_CPUTIME_ID) - c0) / (now(CLOCK_MONOTONIC) - w0));
	} else {
		fprintf(stderr, "unknown mode %s\n", mode);
		return 2;
	}
	return 0;
}
