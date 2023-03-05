[x] set up redis
[x] use redis crate to store basic integers in redis
[x] get service to talk to redis upon requests
[x] use redis to track limits
[x] enforce rate limiting on requests
[x] added integration tests
[x] added CICD
[x] retrieve configuration from somewhere, to set the limits
[x] make config retrieval customizable (file, http, s3)
[x] figure out how to make rate limit expiry customizable
[x] customize port and addr to bind to
[ ] metrics
[ ] descriptive logs
[ ] figure out how to allow multiple ttls
[ ] figure out how to scale RPS per unit (in service.rs)
