CARGO ?= cargo
COVERAGE_MIN ?= 80
COVERAGE_IGNORE := '(^|/)bin/'
COMPOSE := docker compose --file deploy/docker/compose.yaml
# buf runs as the invoking user: `buf breaking` clones `.git`, and git refuses
# a repository owned by another user ("dubious ownership"). The cache moves
# to /tmp because that user has no home directory in the image.
BUF := docker run --rm --user "$$(id -u):$$(id -g)" --env BUF_CACHE_DIR=/tmp/buf \
	--volume "$(CURDIR)":/workspace --workdir /workspace bufbuild/buf:1.50.0
# The ref protobuf changes are checked against for breaking changes.
PROTO_BASE ?= main

# Integration tests reach the Compose services through these variables.
DATABASE_URL ?= postgres://bergsmith:bergsmith@127.0.0.1:55432/bergsmith
BERGSMITH_TEST_S3_ENDPOINT ?= http://127.0.0.1:59000
BERGSMITH_TEST_CATALOG_URL ?= http://127.0.0.1:58181
export DATABASE_URL BERGSMITH_TEST_S3_ENDPOINT BERGSMITH_TEST_CATALOG_URL
NEXTEST_PROFILE ?= default

.PHONY: help fmt fmt-check check clippy test doc coverage coverage-html deny machete ci \
	services-up services-down proto-lint proto-breaking

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

services-up: ## Start PostgreSQL, RustFS, and Lakekeeper (Docker Compose)
	$(COMPOSE) up --detach --wait postgres rustfs lakekeeper

services-down: ## Stop the test services and discard their data
	$(COMPOSE) down

test: services-up ## Run the test suite
	$(CARGO) nextest run --workspace --locked --profile $(NEXTEST_PROFILE)

doc: ## Build docs with warnings as errors
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --workspace --no-deps --locked

coverage: services-up ## Enforce the line-coverage threshold
	$(CARGO) llvm-cov nextest --workspace --locked \
		--ignore-filename-regex $(COVERAGE_IGNORE) \
		--fail-under-lines $(COVERAGE_MIN)

coverage-html: services-up ## Write an HTML coverage report to target/llvm-cov/html
	$(CARGO) llvm-cov nextest --workspace --locked \
		--ignore-filename-regex $(COVERAGE_IGNORE) --html

deny: ## Check advisories, licenses, bans, and sources
	$(CARGO) deny check

machete: ## Find unused dependencies
	$(CARGO) machete

proto-lint: ## Lint protobuf files (buf, via Docker)
	$(BUF) lint

proto-breaking: ## Check protobuf changes against PROTO_BASE (buf, via Docker)
	@if git cat-file -e $(PROTO_BASE):buf.yaml 2>/dev/null; then \
		$(BUF) breaking --against '.git#ref=$(PROTO_BASE)'; \
	else \
		echo "$(PROTO_BASE) has no buf.yaml yet; nothing to compare against"; \
	fi

ci: fmt-check proto-lint proto-breaking check clippy test doc coverage deny machete ## Run every quality gate
