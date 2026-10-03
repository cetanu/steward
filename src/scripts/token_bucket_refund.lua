local redis_time = redis.call('TIME')
local now_sec = tonumber(redis_time[1])
local now_usec = tonumber(redis_time[2])
local now_ms = (now_sec * 1000) + math.floor(now_usec / 1000)

local capacity = tonumber(ARGV[1])
local refill_per_ms = tonumber(ARGV[2])
local refund = tonumber(ARGV[3])
local window_ms = tonumber(ARGV[4])

local state = redis.call('HMGET', KEYS[1], 'tokens', 'timestamp_ms')
local tokens = tonumber(state[1])
local last = tonumber(state[2])

if last ~= nil and now_ms < last then
  now_ms = last
end

if tokens == nil then
  tokens = capacity
  last = now_ms
else
  local elapsed = math.max(0, now_ms - (last or now_ms))
  tokens = math.min(capacity, tokens + elapsed * refill_per_ms)
end

tokens = math.min(capacity, tokens + refund)

redis.call('HSET', KEYS[1], 'tokens', tokens, 'timestamp_ms', now_ms)
redis.call('PEXPIRE', KEYS[1], window_ms * 2)
return { 1, math.floor(tokens) }
