// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::sync::OnceLock;
use std::time::Instant;

pub fn mono_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Client view of the server monotonic clock, sampled from response envelopes.
#[derive(Clone, Debug)]
pub struct Clock {
    server_base: u64,
    local_base: Instant,
}

impl Clock {
    pub fn new(server_now: u64) -> Self {
        Self {
            server_base: server_now,
            local_base: Instant::now(),
        }
    }

    pub fn server_now(&self) -> u64 {
        let elapsed = u64::try_from(self.local_base.elapsed().as_millis()).unwrap_or(0);
        self.server_base.saturating_add(elapsed)
    }

    pub fn observe(&mut self, server_now: u64) {
        self.server_base = server_now;
        self.local_base = Instant::now();
    }
}
