COMPOSE ?= docker compose

clean:
	$(COMPOSE) down --rmi local -v --remove-orphans --timeout 1

build:
	cargo build --release --locked --bin steward

daemonize:
	$(COMPOSE) up --detach --force-recreate --build server envoy

daemonize-prebuilt: build
	$(COMPOSE) -f docker-compose.yml -f docker-compose.prebuilt.yml up --detach --force-recreate --build server envoy

logs:
	$(COMPOSE) logs server envoy

run: clean
	$(COMPOSE) up --detach --build envoy
	$(COMPOSE) up --build --force-recreate server

run-prebuilt: clean build
	$(COMPOSE) -f docker-compose.yml -f docker-compose.prebuilt.yml up --detach --build envoy
	$(COMPOSE) -f docker-compose.yml -f docker-compose.prebuilt.yml up --build --force-recreate server

watch-envoy:
	$(COMPOSE) up --build envoy \
		&& $(COMPOSE) logs --no-log-prefix --no-color --follow envoy \
		| jq --sort-keys -R 'fromjson?'

pcap-redis:
	docker run -v "$$(pwd)":/tmp/pcap -it --rm --net container:steward-server-1 nicolaka/netshoot tcpdump -n -w /tmp/pcap/capture.pcap port 6379

test: daemonize
	cargo test --locked --test rate_limit

test-prebuilt: daemonize-prebuilt
	cargo test --locked --test rate_limit
