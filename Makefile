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


test: daemonize tavern
