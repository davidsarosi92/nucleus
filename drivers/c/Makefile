# libnucleus: `make` builds $(OUT)/libnucleus.a, `make test` the test program.
# `make TLS=openssl` builds it with TLS (OpenSSL, found by pkg-config); a
# program linking it then needs `pkg-config --libs openssl` too.
CC ?= cc
CFLAGS ?= -std=c99 -O2 -Wall -Wextra -Werror
OUT ?= build
ifeq ($(TLS),openssl)
CFLAGS += -DNC_WITH_OPENSSL $(shell pkg-config --cflags openssl)
LDLIBS += $(shell pkg-config --libs openssl)
endif

all: $(OUT)/libnucleus.a

$(OUT)/nucleus.o: nucleus.c nucleus.h
	mkdir -p $(OUT)
	$(CC) $(CFLAGS) -c -o $@ nucleus.c

$(OUT)/libnucleus.a: $(OUT)/nucleus.o
	ar rcs $@ $^

test: $(OUT)/test_nucleus

$(OUT)/test_nucleus: test/test_nucleus.c $(OUT)/libnucleus.a
	$(CC) $(CFLAGS) -I. -o $@ test/test_nucleus.c $(OUT)/libnucleus.a $(LDLIBS)

clean:
	rm -rf $(OUT)

.PHONY: all test clean
