# Single source of truth for the CI "Check & Lint" and "Test" legs.
#
# The workflows call these targets, so the commands live in exactly one place.
# The one leg that cannot call make is the Windows test runner (no make on the
# image); .github/scripts/ci-drift-check.sh fails if it drifts from `make test`.
#
# Never hardcode CARGO_TARGET_DIR here: a shared target dir is inherited from
# the environment.

CARGO ?= cargo

# CI lints and builds with rich-formats. A warning is only reachable under this
# feature, so dropping it lints weaker than CI.
LINT_FEATURES ?= --features rich-formats

# Feature config for one test leg. CI passes one per matrix runner; `test` runs
# every config CI covers.
TEST_FLAGS ?=

# The OS keychain has no headless path: it blocks on a GUI prompt mid-run.
SPELUNK_SECRET_STORE ?= file
export SPELUNK_SECRET_STORE

OPENAPI_SNAPSHOT := docs/openapi.json

# Gates must not interleave: a parallel run obscures which one failed.
.NOTPARALLEL:

.DEFAULT_GOAL := help

.PHONY: help check lint fmt fmt-check clippy cargo-check build test test-config \
        nextest doctest precommit audit deny openapi openapi-check ci-drift \
        require-nextest

help:
	@echo "make check      Run the CI Check & Lint + Test legs. Green means both would pass."
	@echo "make lint       fmt, clippy, check, build (rich-formats)."
	@echo "make test       nextest + doctests, for every feature config CI covers."
	@echo "make fmt        Reformat in place."
	@echo "make precommit  fmt + clippy only. Fast subset for a git pre-commit hook."
	@echo ""
	@echo "NOT covered by 'make check'. Run these yourself, or let CI do it:"
	@echo "  make audit          cargo audit                    (Security workflow)"
	@echo "  make deny           cargo deny                     (Security workflow)"
	@echo "  make openapi-check  OpenAPI snapshot is current    (CI workflow)"
	@echo "  make openapi        Regenerate the OpenAPI snapshot."
	@echo "  make ci-drift       Workflow and Makefile agree    (CI workflow)"
	@echo ""
	@echo "No local target: Windows tests, Docker image build, release scripts,"
	@echo "weekly fuzz. For those, run CI on your branch without a PR:"
	@echo "  gh workflow run ci.yml --ref \$$(git branch --show-current)"

# The two legs, in fail-fast order: the cheap gates before the slow ones.
check: lint test

lint: fmt-check clippy cargo-check build

fmt-check:
	@echo "==> fmt-check"
	$(CARGO) fmt --all -- --check

clippy:
	@echo "==> clippy"
	$(CARGO) clippy --all-targets $(LINT_FEATURES) -- -D warnings

cargo-check:
	@echo "==> cargo-check"
	$(CARGO) check --all-targets $(LINT_FEATURES)

build:
	@echo "==> build"
	$(CARGO) build --all-targets $(LINT_FEATURES)

fmt:
	$(CARGO) fmt --all

# CI runs one feature config per matrix runner; every one must pass.
test:
	@$(MAKE) test-config TEST_FLAGS=""
	@$(MAKE) test-config TEST_FLAGS="--no-default-features"

test-config: nextest doctest

# nextest does not run doctests, hence the separate doctest gate.
nextest: require-nextest
	@echo "==> nextest $(TEST_FLAGS)"
	$(CARGO) nextest run $(TEST_FLAGS)

doctest:
	@echo "==> doctest $(TEST_FLAGS)"
	$(CARGO) test --doc $(TEST_FLAGS)

precommit: fmt-check clippy

require-nextest:
	@command -v cargo-nextest >/dev/null 2>&1 || { \
		echo "error: cargo-nextest is not installed, and CI runs 'cargo nextest run'."; \
		echo "       install it with: cargo install cargo-nextest --locked"; \
		exit 1; \
	}

audit:
	$(CARGO) audit

deny:
	$(CARGO) deny check advisories licenses bans

openapi:
	$(CARGO) run -p spelunk-server -- --print-openapi > $(OPENAPI_SNAPSHOT)

openapi-check:
	@echo "==> openapi-check"
	@tmp=$$(mktemp); trap 'rm -f "$$tmp"' EXIT; \
	$(CARGO) run -p spelunk-server -- --print-openapi > "$$tmp" && \
	diff -w -u $(OPENAPI_SNAPSHOT) "$$tmp" || { \
		echo ""; \
		echo "ERROR: $(OPENAPI_SNAPSHOT) is out of date. Regenerate it with: make openapi"; \
		exit 1; \
	}

ci-drift:
	@echo "==> ci-drift"
	.github/scripts/ci-drift-check.sh
