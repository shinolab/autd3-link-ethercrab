use std::time::Duration;

use tokio::time::Instant;

pub(crate) trait Sleep {
    fn sleep_until(deadline: Instant);
}

pub(crate) struct StdSleep {}

impl Sleep for StdSleep {
    fn sleep_until(deadline: Instant) {
        let duration = deadline - Instant::now();
        if duration > Duration::ZERO {
            std::thread::sleep(duration)
        }
    }
}

pub(crate) struct BusyWait {}

impl Sleep for BusyWait {
    fn sleep_until(deadline: Instant) {
        while Instant::now() < deadline {
            std::hint::spin_loop();
        }
    }
}
