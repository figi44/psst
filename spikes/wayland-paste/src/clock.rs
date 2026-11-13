//! CLOCK_MONOTONIC in microseconds; comparable across processes.

use rustix::time::{ClockId, clock_gettime};

pub fn now_us() -> u64 {
    let t = clock_gettime(ClockId::Monotonic);
    t.tv_sec as u64 * 1_000_000 + t.tv_nsec as u64 / 1_000
}

pub fn now_ms_u32() -> u32 {
    (now_us() / 1_000) as u32
}
