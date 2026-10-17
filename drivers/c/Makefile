# libnucleus: `make` builds $(OUT)/libnucleus.a, `make test` the test program.
CC ?= cc
CFLAGS ?= -std=c99 -O2 -Wall -Wextra -Werror
OUT ?= build

all: $(OUT)/libnucleus.a

$(OUT)/nucleus.o: nucleus.c nucleus.h
	mkdir -p $(OUT)
	$(CC) $(CFLAGS) -c -o $@ nucleus.c

$(OUT)/libnucleus.a: $(OUT)/nucleus.o
	ar rcs $@ $^

test: $(OUT)/test_nucleus

$(OUT)/test_nucleus: test/test_nucleus.c $(OUT)/libnucleus.a
	$(CC) $(CFLAGS) -I. -o $@ test/test_nucleus.c $(OUT)/libnucleus.a

clean:
	rm -rf $(OUT)

.PHONY: all test clean
