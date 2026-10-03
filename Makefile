BIN  := cc-stats
TARGET := target/release/$(BIN)
# Extra CLI args, e.g. `make run ARGS="--days 90 --open"`
ARGS ?=

.PHONY: build run test clean

build:
	cargo build --release

run: build
	./$(TARGET) $(ARGS)

test:
	cargo test

clean:
	cargo clean
