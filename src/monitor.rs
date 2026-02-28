use std::{
    io,
    os::fd::{AsFd, AsRawFd},
    time::{Duration, Instant},
};

use crate::os_sendq_bytes;

#[derive(Debug, Clone, Copy)]
pub struct PressureConfig {
    /// Under this threashold backpressure will be treated
    /// as a zero
    pub zero_epsilon_bytes: u32,
    /// non-zero longer than this means we have persistent backpressure
    pub max_nonzero_for: Duration,
    pub sample_every: Duration,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct PersistentBackPressure {
    /// How long the queue/pressure signal has remained non-zero without clearing.
    pub nonzero_for: Duration,
    /// Current observed queue depth at the time persistence is detected.
    pub q: u32,
    /// Highest observed queue depth during this non-zero period.
    pub peak_q: u32,
}

#[derive(Debug, Clone, Copy)]
pub enum PressureEvent {
    Cleared {
        nonzero_for: Duration,
        last_q: u32,
    },
    Persistent {
        nonzero_for: Duration,
        q: u32,
        peak_q: u32,
    },
    Sample {
        q: u32,
        nonzero_for: Duration,
        peak_q: u32,
    },
}

#[derive(Debug)]
pub struct PressureMonitor {
    pub cfg: PressureConfig,
    pub nonzero_since: Option<Instant>,
    pub peak_q: u32,
    pub last_q: u32,
    pub last_sample: Instant,
}

impl PressureMonitor {
    pub fn new(cfg: PressureConfig) -> Self {
        Self {
            cfg,
            nonzero_since: None,
            peak_q: 0,
            last_q: 0,
            last_sample: Instant::now() - cfg.sample_every,
        }
    }

    pub fn tick(&mut self, sock: impl AsFd, now: Instant) -> io::Result<Option<PressureEvent>> {
        if now.duration_since(self.last_sample) < self.cfg.sample_every {
            return Ok(None);
        }
        self.last_sample = now;

        let q = os_sendq_bytes(sock.as_fd().as_raw_fd())?;
        self.last_q = q;
        self.peak_q = self.peak_q.max(q);

        let is_zero = q <= self.cfg.zero_epsilon_bytes;

        match (is_zero, self.nonzero_since) {
            (true, None) => Ok(Some(PressureEvent::Sample {
                q,
                nonzero_for: Duration::ZERO,
                peak_q: self.peak_q,
            })),
            (true, Some(t0)) => {
                let d = now.duration_since(t0);
                self.nonzero_since = None;
                self.peak_q = 0;
                Ok(Some(PressureEvent::Cleared {
                    nonzero_for: d,
                    last_q: q,
                }))
            }
            (false, None) => {
                self.nonzero_since = Some(now);
                Ok(Some(PressureEvent::Sample {
                    q,
                    nonzero_for: Duration::ZERO,
                    peak_q: self.peak_q,
                }))
            }
            (false, Some(t0)) => {
                let d = now.duration_since(t0);
                if d >= self.cfg.max_nonzero_for {
                    Ok(Some(PressureEvent::Persistent {
                        nonzero_for: d,
                        q,
                        peak_q: self.peak_q,
                    }))
                } else {
                    Ok(Some(PressureEvent::Sample {
                        q,
                        nonzero_for: d,
                        peak_q: self.peak_q,
                    }))
                }
            }
        }
    }
}
