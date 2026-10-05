/* Heap use after free: AddressSanitizer must stop it. */
#include <stdlib.h>

int main(int argc, char **argv) {
	(void)argv;
	int *p = malloc(sizeof *p);
	if (p == NULL)
		return 2;
	*p = argc;
	free(p);
	return *p;
}
