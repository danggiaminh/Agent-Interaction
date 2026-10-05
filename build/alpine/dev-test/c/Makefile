# Minimal C project: make drives cc (gcc), ld and the musl headers and libc.
CFLAGS = -O2 -std=c11 -Wall -Wextra -Werror

hello: main.o util.o
	$(CC) $(LDFLAGS) -o $@ main.o util.o

main.o util.o: util.h

clean:
	rm -f hello main.o util.o
.PHONY: clean
