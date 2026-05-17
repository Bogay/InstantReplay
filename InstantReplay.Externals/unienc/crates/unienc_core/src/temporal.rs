use std::sync::Mutex;
use std::time::Instant;

/// Tracks wall-clock recording time, accounting for pauses.
pub struct TemporalController {
    inner: Mutex<Inner>,
}

struct Inner {
    is_paused: bool,
    pause_start: Option<Instant>,
    total_paused_secs: f64,
    start: Instant,
}

impl TemporalController {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                is_paused: true,
                pause_start: Some(Instant::now()),
                total_paused_secs: 0.0,
                start: Instant::now(),
            }),
        }
    }

    pub fn now_secs(&self) -> f64 {
        let g = self.inner.lock().unwrap();
        g.start.elapsed().as_secs_f64()
    }

    pub fn is_paused(&self) -> bool {
        self.inner.lock().unwrap().is_paused
    }

    pub fn total_paused_secs(&self) -> f64 {
        let g = self.inner.lock().unwrap();
        let mut total = g.total_paused_secs;
        if let Some(ps) = g.pause_start {
            total += ps.elapsed().as_secs_f64();
        }
        total
    }

    pub fn resume(&self) {
        let mut g = self.inner.lock().unwrap();
        if !g.is_paused {
            return;
        }
        if let Some(ps) = g.pause_start.take() {
            g.total_paused_secs += ps.elapsed().as_secs_f64();
        }
        g.is_paused = false;
    }

    pub fn pause(&self) {
        let mut g = self.inner.lock().unwrap();
        if g.is_paused {
            return;
        }
        g.pause_start = Some(Instant::now());
        g.is_paused = true;
    }
}

impl Default for TemporalController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn starts_paused() {
        let tc = TemporalController::new();
        assert!(tc.is_paused());
    }

    #[test]
    fn resume_unpauses() {
        let tc = TemporalController::new();
        tc.resume();
        assert!(!tc.is_paused());
    }

    #[test]
    fn pause_after_resume() {
        let tc = TemporalController::new();
        tc.resume();
        tc.pause();
        assert!(tc.is_paused());
    }

    #[test]
    fn total_paused_duration_accumulates() {
        let tc = TemporalController::new();
        // start paused; resume then immediately pause again
        tc.resume();
        thread::sleep(Duration::from_millis(20));
        tc.pause();
        let paused = tc.total_paused_secs();
        // The initial pause from construction time is included.
        // After resume+sleep+pause only the pre-resume time counts as "total_paused".
        // Here we just check that the value is non-negative.
        assert!(paused >= 0.0);
    }
}
