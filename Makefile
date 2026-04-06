INFERNO_DIR ?= ../inferno

.PHONY: build test clean inferno2pipe

## Build the bridge Docker image
build:
	docker build -t sendspin-bridge .

## Build the inferno2pipe image (required once before tests)
inferno2pipe:
	cd $(INFERNO_DIR) && git submodule update --init --recursive
	docker build -f $(INFERNO_DIR)/Dockerfile.alpine-i2pipe -t inferno_aoip:alpine-i2pipe $(INFERNO_DIR)

## Run the E2E test suite
test: build inferno2pipe
	cd test && docker compose down --remove-orphans 2>/dev/null; \
	docker compose up --build --abort-on-container-exit control_and_test; \
	result=$$?; \
	docker compose down --remove-orphans; \
	exit $$result

## Remove test containers and images
clean:
	cd test && docker compose down --remove-orphans --volumes 2>/dev/null || true
