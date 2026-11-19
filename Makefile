TARGET = meowsh
PREFIX ?= /usr/local

# On Linux, build for musl by default, matched to the host architecture,
# so the binary is static and runs on any distribution. Override with
# TRIPLE=<triple>, or TRIPLE= for a plain host build.
ifeq ($(shell uname -s),Linux)
TRIPLE ?= $(shell uname -m)-unknown-linux-musl
endif

ifneq ($(TRIPLE),)
CARGO_FLAGS = --target $(TRIPLE)
BIN = target/$(TRIPLE)/release/meowsh
else
BIN = target/release/meowsh
endif

all: $(TARGET)

$(TARGET): Cargo.toml Cargo.lock $(shell find src -name '*.rs')
	cargo build --release $(CARGO_FLAGS)
	cp $(BIN) $(TARGET)

install: $(TARGET)
	install -D -m 755 $(TARGET) $(DESTDIR)$(PREFIX)/bin/$(TARGET)

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/$(TARGET)

clean:
	rm -f $(TARGET)

.PHONY: all clean install uninstall
