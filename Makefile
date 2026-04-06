INFERNO_DIR ?= ../inferno
MUSIC_ASSISTANT_COMPOSE ?= test/music_assistant/docker-compose.yml
MUSIC_ASSISTANT_DOCKER_CONFIG ?= /tmp/music-assistant-docker-config

.PHONY: build test test-multi test-resilience test-ma-interactive ma-up ma-down ma-logs clean

## Build the bridge Docker image
## Requires inferno-fork symlink: ln -sf ../inferno-fork inferno-fork
build:
	@test -d inferno-fork/inferno_aoip || (echo "Error: inferno-fork not found. Run: ln -sf ../inferno-fork inferno-fork" && exit 1)
	docker build -t spin2dante .

## Build the inferno2pipe image (required once before tests)
inferno2pipe:
	cd $(INFERNO_DIR) && git submodule update --init --recursive
	docker build -f $(INFERNO_DIR)/Dockerfile.alpine-i2pipe -t inferno_aoip:alpine-i2pipe $(INFERNO_DIR)

## Run the single-stream E2E test
test: build inferno2pipe
	cd test && docker compose down --remove-orphans 2>/dev/null; \
	docker compose up --build --abort-on-container-exit control_and_test; \
	result=$$?; \
	docker compose down --remove-orphans; \
	exit $$result

## Run the multi-stream E2E test (16 synchronized streams)
test-multi: build inferno2pipe
	cd test && docker compose -f docker-compose.multi.yml down --remove-orphans 2>/dev/null; \
	docker compose -f docker-compose.multi.yml up --build --abort-on-container-exit control_and_test; \
	result=$$?; \
	docker compose -f docker-compose.multi.yml down --remove-orphans; \
	exit $$result

## Run the resilience test (stream stop/start, seek, server restart)
test-resilience: build
	cd test && docker compose -f docker-compose.resilience.yml down --remove-orphans 2>/dev/null; \
	docker compose -f docker-compose.resilience.yml up --build --abort-on-container-exit validator; \
	result=$$?; \
	docker compose -f docker-compose.resilience.yml down --remove-orphans; \
	exit $$result

## (All tests now use real PTPv2 — no separate PTP targets needed)

## Interactive Music Assistant test (2 bridges, full DANTE path, runs until ctrl-c)
## Set MA_HOST if Music Assistant is not on the default Docker gateway
MA_HOST ?= $(shell docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}' 2>/dev/null || echo 172.17.0.1)
test-ma-interactive: build inferno2pipe
	mkdir -p output
	rm -f output/bridge1.raw output/bridge2.raw
	MA_HOST=$(MA_HOST) cd test && docker compose -f docker-compose.ma-interactive.yml up --build; \
	docker compose -f docker-compose.ma-interactive.yml down --remove-orphans

## Start the Music Assistant test server with a clean GHCR Docker config
ma-up:
	mkdir -p $(MUSIC_ASSISTANT_DOCKER_CONFIG)
	test -f $(MUSIC_ASSISTANT_DOCKER_CONFIG)/config.json || printf '{"auths":{}}\n' > $(MUSIC_ASSISTANT_DOCKER_CONFIG)/config.json
	docker --config $(MUSIC_ASSISTANT_DOCKER_CONFIG) compose -f $(MUSIC_ASSISTANT_COMPOSE) up -d

## Stop the Music Assistant test server
ma-down:
	docker --config $(MUSIC_ASSISTANT_DOCKER_CONFIG) compose -f $(MUSIC_ASSISTANT_COMPOSE) down

## Tail Music Assistant logs
ma-logs:
	docker --config $(MUSIC_ASSISTANT_DOCKER_CONFIG) compose -f $(MUSIC_ASSISTANT_COMPOSE) logs -f --tail 200

## Remove all test containers, networks, and volumes
clean:
	cd test && docker compose down --remove-orphans --volumes 2>/dev/null || true
	cd test && docker compose -f docker-compose.multi.yml down --remove-orphans --volumes 2>/dev/null || true
	cd test && docker compose -f docker-compose.resilience.yml down --remove-orphans --volumes 2>/dev/null || true
	cd test && docker compose -f docker-compose.ma-interactive.yml down --remove-orphans --volumes 2>/dev/null || true
