/* Leaks 16 bytes: valgrind --leak-check=full must report them. */
#include <stdio.h>
#include <stdlib.h>

int main(void) {
	char *p = malloc(16);
	if (p == NULL)
		return 2;
	snprintf(p, 16, "leak");
	puts(p);
	return 0;
}
