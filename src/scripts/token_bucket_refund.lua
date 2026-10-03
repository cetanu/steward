local now = tonumber(ARGV[1])
local capacity = tonumber(ARGV[2])
local refill_per_ms = tonumber(ARGV[3])
local refund = tonumber(ARGV[4])
local window_ms = tonumber(ARGV[5])
local state = redis.call('HMGET', KEYS[1], 'tokens', 'timestamp_ms')
local tokens = tonumber(state[1])
local last = tonumber(state[2])

if tokens == nil then
  tokens = capacity
  last = now
else
  tokens = math.min(capacity, tokens + math.max(0, now - last) * refill_per_ms)
end

tokens = math.min(capacity, tokens + refund)

redis.call('HSET', KEYS[1], 'tokens', tokens, 'timestamp_ms', now)
redis.call('PEXPIRE', KEYS[1], window_ms * 2)
return { 1, math.floor(tokens) }
