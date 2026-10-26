//! Супервизор: держит рабочий процесс живым, перезапуская его с нарастающей паузой.
//! Нужен на станции, где нет systemd: если звуковое устройство занято Алисой или процесс упал,
//! через пару секунд он поднимется сам.

use std::time::Duration;

const MIN_DELAY: Duration = Duration::from_secs(2);
const MAX_DELAY: Duration = Duration::from_secs(60);
const HEALTHY_AFTER: Duration = Duration::from_secs(60);

/// Пауза перед следующим запуском по длительности последней работы и предыдущей паузе.
pub fn next_delay(uptime: Duration, previous: Duration) -> Duration {
    if uptime >= HEALTHY_AFTER {
        MIN_DELAY
    } else {
        (previous * 2).clamp(MIN_DELAY, MAX_DELAY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_grows_on_crash_loops_and_resets_after_healthy_run() {
        let mut delay = MIN_DELAY;
        let mut seen = Vec::new();
        for _ in 0..8 {
            delay = next_delay(Duration::from_secs(1), delay);
            seen.push(delay.as_secs());
        }
        assert_eq!(seen, vec![4, 8, 16, 32, 60, 60, 60, 60]);
        assert_eq!(next_delay(Duration::from_secs(300), MAX_DELAY), MIN_DELAY);
    }
}
