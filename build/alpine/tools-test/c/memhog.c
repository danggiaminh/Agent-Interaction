/* Allocates 8 MiB blocks until malloc fails (RLIMIT_AS), touching each block; reports how it ended. */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(void) {
	int n = 0;
	for (;;) {
		void *p = malloc(8u << 20);
		if (p == NULL)
			break;
		memset(p, 1, 8u << 20);
		n++;
	}
	printf("%s blocks=%d\n", errno == ENOMEM ? "ENOMEM" : strerror(errno), n);
	return errno == ENOMEM ? 0 : 1;
}
