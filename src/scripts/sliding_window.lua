local redis_time = redis.call('TIME')
local now_sec = tonumber(redis_time[1])
local now_usec = tonumber(redis_time[2])
local now_ms = (now_sec * 1000) + math.floor(now_usec / 1000)

local window_ms = tonumber(ARGV[1])
local limit = tonumber(ARGV[2])
local hits = tonumber(ARGV[3])
local nonce = ARGV[4]

if hits == nil or hits > 100 or hits < 1 then
  return { 0, 0 }
end

redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', now_ms - window_ms)
local current = redis.call('ZCARD', KEYS[1])
local allowed = 0
if current + hits <= limit then
  for i = 1, hits do
    redis.call('ZADD', KEYS[1], now_ms, now_usec .. ':' .. nonce .. ':' .. i)
  end
  current = current + hits
  allowed = 1

  if current > 10000 then
    redis.call('ZREMRANGEBYRANK', KEYS[1], 0, current - 10001)
    current = 10000
  end
end

redis.call('PEXPIRE', KEYS[1], window_ms * 2)
return { allowed, current }
