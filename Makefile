DESTDIR ?=
PREFIX ?= /usr
LIBEXECDIR ?= ${PREFIX}/libexec
RELEASE ?= 1

ifeq ($(RELEASE),1)
	PROFILE ?= release
	CARGO_ARGS = --release
else
	PROFILE ?= debug
	CARGO_ARGS =
endif

.PHONY: all
all:
	cargo build ${CARGO_ARGS}

.PHONY: install
install:
	mkdir -p "${DESTDIR}$(PREFIX)/bin" "${DESTDIR}$(LIBEXECDIR)"
	install -D -t "${DESTDIR}$(PREFIX)/bin" target/${PROFILE}/ostree-ext-cli

install-tests:
	install -D -m 0755 "${DESTDIR}$(PREFIX)/lib/coreos-assembler/tests/kola"
	rsync -rlv tests/kolainst "${DESTDIR}$(PREFIX)/lib/coreos-assembler/tests/kola"