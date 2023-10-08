use std::cmp;
use std::time::Duration;

use rand::prelude::*;
use tokio::time::sleep;

pub struct WaitExponentialRandom {
    min: Duration,
    max: Duration,
    i: u64,
    multiplier: u64,
}
pub struct WaitExponentialRandomBuilder {
    inner: WaitExponentialRandom,
}

impl WaitExponentialRandomBuilder {
    pub fn build(self) -> WaitExponentialRandom {
        self.inner
    }
    pub fn min(mut self, duration: Duration) -> Self {
        self.inner.min = duration;
        self
    }
    pub fn max(mut self, duration: Duration) -> Self {
        self.inner.max = duration;
        self
    }
    pub fn multiplier(mut self, mul: u64) -> Self {
        self.inner.multiplier = mul;
        self
    }
}

impl WaitExponentialRandom {
    pub fn builder() -> WaitExponentialRandomBuilder {
        WaitExponentialRandomBuilder {
            inner: WaitExponentialRandom {
                min: Duration::ZERO,
                max: Duration::MAX,
                i: 0,
                multiplier: 1,
            },
        }
    }
    pub fn next_delay(&self) -> Duration {
        let secs = (1 << self.i) * self.multiplier;
        let secs = rand::thread_rng().gen_range(self.min.as_secs()..=secs);
        let dur = Duration::from_secs(secs);
        cmp::min(cmp::max(dur, self.min), self.max)
    }
    pub async fn wait(&mut self) {
        sleep(self.next_delay()).await;
        self.i += 1;
    }
}
