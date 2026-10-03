# Symphony developer tasks. CI (.github/workflows/ci.yml) calls these same targets.
#
#   make            # = make all: format check, clippy, tests, web checks
#   make build      # web bundle + release binary (target/release/symphony)
#   make run        # build, then run against ./WORKFLOW.md with the dashboard on :4000
#   make docker     # local image for this machine's architecture
#
# Requirements: Rust (pinned by rust-toolchain.toml), Node >= 22.12 with pnpm (corepack enable),
# Docker with buildx for the docker targets.

CARGO    ?= cargo
PNPM     ?= pnpm
DOCKER   ?= docker
# Cargo resolution flags; `make lint LOCKED=` while you are still adding dependencies.
LOCKED   ?= --locked

WORKFLOW ?= ./WORKFLOW.md
PORT     ?= 4000
RUN_ARGS ?=

IMAGE     ?= symphony
TAG       ?= dev
PLATFORMS ?= linux/amd64,linux/arm64
# docker-multiarch output: cache only by default (verifies both architectures build);
# `make docker-multiarch IMAGE=ghcr.io/<owner>/<repo> TAG=x PUSH=1` pushes instead.
PUSH ?=
DOCKER_BUILD_ARGS ?=
VERSION  := $(shell sed -nE 's/^version = "([^"]+)"/\1/p' Cargo.toml | head -n 1)
REVISION := $(shell git rev-parse HEAD 2>/dev/null || echo unknown)

WEB_DEPS := web/node_modules/.modules.yaml

.DEFAULT_GOAL := all

.PHONY: help all ci setup fmt fmt-check lint test web web-deps web-check build run \
        docker docker-multiarch pr-body-check clean

help:
	@echo "Targets: all (fmt-check lint test web-check), fmt, fmt-check, lint, test, web, web-check,"
	@echo "         build, run, docker, docker-multiarch, pr-body-check FILE=<path>, setup, clean"

all: fmt-check lint test web-check

ci: all

setup: web-deps
	$(CARGO) fetch $(LOCKED)

# --- Rust ---------------------------------------------------------------------------------------

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

lint:
	$(CARGO) clippy --workspace --all-targets --all-features $(LOCKED) -- -D warnings

test:
	$(CARGO) test --workspace --all-features $(LOCKED)

# --- Web dashboard ------------------------------------------------------------------------------

$(WEB_DEPS): web/package.json web/pnpm-lock.yaml
	$(PNPM) --dir web install --frozen-lockfile
	@touch $@

web-deps: $(WEB_DEPS)

# Builds web/dist, which symphony-server embeds into the binary at compile time.
web: web-deps
	$(PNPM) --dir web build

# typecheck + oxlint + vitest
web-check: web-deps
	$(PNPM) --dir web check

# --- Binary -------------------------------------------------------------------------------------

build: web
	$(CARGO) build --release $(LOCKED) -p symphony

run: build
	./target/release/symphony --i-understand-that-this-will-be-running-without-the-usual-guardrails \
		--port $(PORT) $(RUN_ARGS) $(WORKFLOW)

pr-body-check:
	@test -n "$(FILE)" || { echo "usage: make pr-body-check FILE=/path/to/pr_body.md" >&2; exit 2; }
	$(CARGO) run --quiet $(LOCKED) -p xtask -- pr-body-check --file $(FILE)

# --- Docker -------------------------------------------------------------------------------------

DOCKER_LABEL_ARGS = --build-arg VERSION=$(VERSION) --build-arg REVISION=$(REVISION)

docker:
	$(DOCKER) buildx build -f docker/Dockerfile $(DOCKER_LABEL_ARGS) $(DOCKER_BUILD_ARGS) \
		-t $(IMAGE):$(TAG) --load .

docker-multiarch:
	$(DOCKER) buildx build -f docker/Dockerfile --platform $(PLATFORMS) \
		$(DOCKER_LABEL_ARGS) $(DOCKER_BUILD_ARGS) -t $(IMAGE):$(TAG) \
		$(if $(PUSH),--push,--output type=cacheonly) .

# --- Housekeeping -------------------------------------------------------------------------------

clean:
	$(CARGO) clean
	rm -rf web/dist web/coverage
