BIN  := cc-stats
TARGET := target/release/$(BIN)
# Extra CLI args, e.g. `make run ARGS="--days 90 --open"`
ARGS ?=

.DEFAULT_GOAL := help
.PHONY: help build run test clean

help: ## Show this help
	@grep -E '^[a-z]+:.*##' $(MAKEFILE_LIST) | awk -F ':.*## ' '{printf "  make %-8s %s\n", $$1, $$2}'
	@echo '  (pass flags with ARGS="...", e.g. make run ARGS="--days 90 --open")'

build: ## Build the release binary
	cargo build --release

run: build ## Build and run the binary
	./$(TARGET) $(ARGS)

test: ## Run unit tests
	cargo test

clean: ## Remove build artifacts
	cargo clean
