INFERNO_DIR ?= ../inferno

.PHONY: build test test-multi clean

## Build the bridge Docker image
build:
	docker build -t sendspin-bridge .

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

## Remove all test containers, networks, and volumes
clean:
	cd test && docker compose down --remove-orphans --volumes 2>/dev/null || true
	cd test && docker compose -f docker-compose.multi.yml down --remove-orphans --volumes 2>/dev/null || true
