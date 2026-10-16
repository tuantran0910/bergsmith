CARGO ?= cargo
COVERAGE_MIN ?= 80
COVERAGE_IGNORE := '(^|/)bin/'
COMPOSE := docker compose --file deploy/docker/compose.yaml

# Tests that need PostgreSQL read DATABASE_URL. When it is not set, the test
# targets start the Compose database first; set it to use your own server.
ifeq ($(origin DATABASE_URL),undefined)
DATABASE_URL := postgres://bergsmith:bergsmith@127.0.0.1:55432/bergsmith
DB_PREREQ := db-up
endif
export DATABASE_URL

.PHONY: help fmt fmt-check check clippy test doc coverage coverage-html deny machete ci db-up db-down

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

db-up: ## Start the test PostgreSQL (Docker Compose)
	$(COMPOSE) up --detach --wait postgres

db-down: ## Stop the test PostgreSQL and discard its data
	$(COMPOSE) down

test: $(DB_PREREQ) ## Run the test suite
	$(CARGO) nextest run --workspace --locked

doc: ## Build docs with warnings as errors
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --workspace --no-deps --locked

coverage: $(DB_PREREQ) ## Enforce the line-coverage threshold
	$(CARGO) llvm-cov nextest --workspace --locked \
		--ignore-filename-regex $(COVERAGE_IGNORE) \
		--fail-under-lines $(COVERAGE_MIN)

coverage-html: $(DB_PREREQ) ## Write an HTML coverage report to target/llvm-cov/html
	$(CARGO) llvm-cov nextest --workspace --locked \
		--ignore-filename-regex $(COVERAGE_IGNORE) --html

deny: ## Check advisories, licenses, bans, and sources
	$(CARGO) deny check

machete: ## Find unused dependencies
	$(CARGO) machete

ci: fmt-check check clippy test doc coverage deny machete ## Run every quality gate
