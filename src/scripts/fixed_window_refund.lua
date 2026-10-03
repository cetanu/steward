local current = redis.call('GET', KEYS[1])
if not current then
  return 0
end
current = tonumber(current)
local new_val = math.max(0, current - tonumber(ARGV[1]))
redis.call('SET', KEYS[1], new_val, 'KEEPTTL')
return new_val
