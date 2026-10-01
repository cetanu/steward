COMPOSE ?= docker compose

clean:
	$(COMPOSE) down --rmi local -v --remove-orphans --timeout 1

daemonize:
	$(COMPOSE) up --detach --force-recreate --build server envoy

logs:
	$(COMPOSE) logs server

run: clean
	$(COMPOSE) up --detach --build envoy
	$(COMPOSE) up --build --force-recreate server

watch-envoy:
	$(COMPOSE) up --build envoy \
		&& $(COMPOSE) logs --no-log-prefix --no-color --follow envoy \
		| jq --sort-keys -R 'fromjson?'


pcap-redis:
	docker run -v "$$(pwd)":/tmp/pcap -it --rm --net container:steward-server-1 nicolaka/netshoot tcpdump -n -w /tmp/pcap/capture.pcap port 6379


test: daemonize
	cargo test --test rate_limit
