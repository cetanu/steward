clean:
	docker-compose kill
	docker-compose down --remove-orphans

daemonize:
	docker-compose up --detach --force-recreate --build server envoy

tavern:
	docker-compose build tavern
	docker-compose run --rm tavern

logs:
	docker-compose logs server

run:
	docker-compose up --detach --build envoy
	docker-compose up --build server

watch-envoy:
	docker-compose up --build envoy \
		&& docker-compose logs --no-log-prefix --no-color --follow envoy \
		| jq --sort-keys -R 'fromjson?'


pcap-redis:
	docker run -v `pwd`:/tmp/pcap -it --rm --net container:limiter_server_1 nicolaka/netshoot tcpdump -n -w /tmp/pcap/capture.pcap port 6379


test: daemonize tavern
