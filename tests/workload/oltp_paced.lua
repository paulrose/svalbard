-- The transactions of sysbench's oltp_read_write at a fixed rate for each thread. The latency
-- counts from the scheduled start, so time spent waiting for a stalled server counts in full.
-- sysbench's own latency starts only when a thread takes the event, and leaves the wait out.
--
-- Each thread writes `<finished µs since the epoch> <latency µs>` lines to
-- `<log-prefix>.<thread>`. With --thread-rate=0 the threads do not wait.

require("oltp_read_write")

local ffi = require("ffi")
ffi.cdef [[
struct svalbard_timespec { long tv_sec; long tv_nsec; };
int clock_gettime(int clock, struct svalbard_timespec *now);
int usleep(unsigned int microseconds);
]]

sysbench.cmdline.options.thread_rate = { "Transactions per second for each thread", 0 }
sysbench.cmdline.options.log_prefix = { "Prefix of the per-thread latency logs", "" }

local now = ffi.new("struct svalbard_timespec")
local function microseconds()
  ffi.C.clock_gettime(0, now) -- CLOCK_REALTIME, like the logs of pgbench.
  return tonumber(now.tv_sec) * 1000000 + math.floor(tonumber(now.tv_nsec) / 1000)
end

local transaction, init, done = event, thread_init, thread_done
local interval, scheduled, log

function thread_init()
  init()
  local rate = sysbench.opt.thread_rate
  interval = rate > 0 and 1000000 / rate or 0
  -- Spread the threads over the first interval.
  scheduled = microseconds() + math.random() * interval
  log = assert(io.open(string.format("%s.%d", sysbench.opt.log_prefix, sysbench.tid), "w"))
end

function thread_done()
  log:close()
  done()
end

-- sysbench calls this again after an ignored error, so the retry keeps the scheduled start.
function event()
  if interval == 0 then
    scheduled = microseconds()
  end
  local wait = scheduled - microseconds()
  if wait > 0 then
    ffi.C.usleep(wait)
  end
  transaction()
  local finished = microseconds()
  log:write(string.format("%d %d\n", finished, math.floor(finished - scheduled)))
  scheduled = scheduled + interval
end
