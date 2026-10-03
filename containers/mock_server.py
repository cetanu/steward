from fastapi import FastAPI

app = FastAPI()


@app.get("/api/rate_limits")
async def get_configs():
    return {
        "default": [
            {
                "key": "remote_address",
                # Wildcard: value omitted to dynamically match and isolate any client IP
                "rate_limit": {
                    "unit": "seconds",
                    "requests_per_unit": 50,
                },
            },
            {
                "key": "protect_the_headers_api",
                "value": "1",
                "rate_limits": [
                    {
                        "unit": "seconds",
                        "requests_per_unit": 5,
                    },
                    {
                        "unit": "minutes",
                        "requests_per_unit": 100,
                    },
                ],
            },
            {
                "key": "disallow_spammy_GETs",
                "value": "1",
                "rate_limit": {
                    "unit": "minutes",
                    "requests_per_unit": 30,
                },
            },
        ]
    }
