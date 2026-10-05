/* Signed integer overflow: UndefinedBehaviorSanitizer must report it. */
#include <limits.h>

int main(int argc, char **argv) {
	(void)argv;
	int x = INT_MAX;
	x += argc;
	return x == 0;
}
