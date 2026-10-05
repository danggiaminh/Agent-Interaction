/* Opens /dev/null until the process runs out of file descriptors; reports how it ended. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>

int main(void) {
	int n = 0;
	for (;;) {
		if (open("/dev/null", O_RDONLY) < 0)
			break;
		n++;
	}
	printf("%s opened=%d\n", errno == EMFILE ? "EMFILE" : strerror(errno), n);
	return errno == EMFILE ? 0 : 1;
}
