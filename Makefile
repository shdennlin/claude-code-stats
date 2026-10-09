BIN  := cc-stats
TARGET := target/release/$(BIN)
# Extra CLI args, e.g. `make run ARGS="--days 90"` (--open is always passed)
ARGS ?=

.DEFAULT_GOAL := help
.PHONY: help build run test clean

help: ## Show this help
	@grep -E '^[a-z]+:.*##' $(MAKEFILE_LIST) | awk -F ':.*## ' '{printf "  make %-8s %s\n", $$1, $$2}'
	@echo '  (pass flags with ARGS="...", e.g. make run ARGS="--days 90")'

build: ## Build the release binary
	cargo build --release

run: build ## Build, run, and open the report
	./$(TARGET) --open $(ARGS)

test: ## Run unit and report regression tests
	cargo test
	cargo build
	python3 tests/usage_regression.py
	node tests/report_dates.cjs

clean: ## Remove build artifacts
	cargo clean
