CARGO ?= cargo
COVERAGE_MIN ?= 80
COVERAGE_IGNORE := '(^|/)bin/'

.PHONY: help fmt fmt-check check clippy test doc coverage coverage-html deny machete ci

help: ## List targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F ':.*## ' '{printf "  %-14s %s\n", $$1, $$2}'

fmt: ## Format all code
	$(CARGO) fmt --all

fmt-check: ## Check formatting
	$(CARGO) fmt --all -- --check

check: ## Type-check every target
	$(CARGO) check --workspace --all-targets --locked

clippy: ## Lint with warnings as errors
	$(CARGO) clippy --workspace --all-targets --locked -- -D warnings

test: ## Run the test suite
	$(CARGO) nextest run --workspace --locked

doc: ## Build docs with warnings as errors
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --workspace --no-deps --locked

coverage: ## Enforce the line-coverage threshold
	$(CARGO) llvm-cov nextest --workspace --locked \
		--ignore-filename-regex $(COVERAGE_IGNORE) \
		--fail-under-lines $(COVERAGE_MIN)

coverage-html: ## Write an HTML coverage report to target/llvm-cov/html
	$(CARGO) llvm-cov nextest --workspace --locked \
		--ignore-filename-regex $(COVERAGE_IGNORE) --html

deny: ## Check advisories, licenses, bans, and sources
	$(CARGO) deny check

machete: ## Find unused dependencies
	$(CARGO) machete

ci: fmt-check check clippy test doc coverage deny machete ## Run every quality gate
