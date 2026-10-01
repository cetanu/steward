local now = tonumber(ARGV[1])
local window_ms = tonumber(ARGV[2])
local limit = tonumber(ARGV[3])
local hits = tonumber(ARGV[4])
local nonce = ARGV[5]

redis.call('ZREMRANGEBYSCORE', KEYS[1], 0, now - window_ms)
local current = redis.call('ZCARD', KEYS[1])
local allowed = 0
if current + hits <= limit then
  for i = 1, hits do
    redis.call('ZADD', KEYS[1], now, nonce .. '-' .. i)
  end
  current = current + hits
  allowed = 1
end

redis.call('PEXPIRE', KEYS[1], window_ms * 2)
return { allowed, current }
