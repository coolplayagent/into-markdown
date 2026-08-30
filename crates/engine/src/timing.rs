//! Monotonic processing timers shared by the preparation and execution phases.

use std::time::{Duration, Instant};

pub(crate) trait MonotonicClock {
    type Mark: Copy;

    fn now(&self) -> Self::Mark;
    fn elapsed(&self, start: Self::Mark, end: Self::Mark) -> Duration;
}

#[derive(Clone, Copy, Default)]
pub(crate) struct SystemClock;

impl MonotonicClock for SystemClock {
    type Mark = Instant;

    fn now(&self) -> Self::Mark {
        Instant::now()
    }

    fn elapsed(&self, start: Self::Mark, end: Self::Mark) -> Duration {
        end.saturating_duration_since(start)
    }
}

pub(crate) struct ProcessingTimer<C: MonotonicClock = SystemClock> {
    clock: C,
    started: C::Mark,
}

impl ProcessingTimer<SystemClock> {
    pub(crate) fn start() -> Self {
        Self::start_with(SystemClock)
    }
}

impl<C: MonotonicClock> ProcessingTimer<C> {
    fn start_with(clock: C) -> Self {
        let started = clock.now();
        Self { clock, started }
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.clock.elapsed(self.started, self.clock.now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct FakeClock {
        now_micros: Cell<u64>,
    }

    impl MonotonicClock for FakeClock {
        type Mark = u64;

        fn now(&self) -> Self::Mark {
            let value = self.now_micros.get();
            self.now_micros.set(value + 250);
            value
        }

        fn elapsed(&self, start: Self::Mark, end: Self::Mark) -> Duration {
            Duration::from_micros(end.saturating_sub(start))
        }
    }

    #[test]
    fn injected_monotonic_clock_controls_elapsed_time() {
        let timer = ProcessingTimer::start_with(FakeClock { now_micros: Cell::new(1_000) });

        assert_eq!(timer.elapsed(), Duration::from_micros(250));
        assert_eq!(timer.elapsed(), Duration::from_micros(500));
    }
}
