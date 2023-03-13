from fastapi import FastAPI

app = FastAPI()

@app.get("/api/rate_limits")
async def get_configs():
    return {
        "default": [
            {
                "key": "protect_the_headers_api",
                "value": "1",
                "rate_limit": {
                    "unit": "seconds",
                    "requests_per_unit": 5
                }
            },
            {
                "key": "protect_the_headers_api",
                "value": "1",
                "rate_limit": {
                    "unit": "minutes",
                    "requests_per_unit": 100
                }
            },
        ]
    }
