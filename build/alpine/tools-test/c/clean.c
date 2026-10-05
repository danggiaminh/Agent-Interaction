/* Correct program: valgrind, gdb, strace, ltrace, perf and the sanitizers all run it. */
#include <stdio.h>
#include <stdlib.h>

int add(int a, int b) { return a + b; }

int main(void) {
	char *p = malloc(16);
	if (p == NULL)
		return 2;
	snprintf(p, 16, "%d", add(40, 2));
	puts(p);
	free(p);
	return 0;
}
