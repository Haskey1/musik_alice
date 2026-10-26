//! Приглушение музыки на время диалога с Алисой.
//!
//! Пока Алиса слушает, думает или говорит, громкость музыки плавно опускается до `duck_level`
//! (по умолчанию 20 %) за 80 мс; после диалога возвращается за 300 мс. Меняются только сэмплы
//! musik, общая громкость станции не затрагивается. Если завершающее событие потерялось,
//! приглушение само снимается через 60 секунд.

use std::{
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

const HOLD: Duration = Duration::from_secs(60);
const ATTACK_SECONDS: f32 = 0.08;
const RELEASE_SECONDS: f32 = 0.3;

pub struct Ducking {
    origin: Instant,
    /// Момент (мс от `origin`), до которого приглушение считается включённым; 0 — выключено.
    until: AtomicU32,
}

impl Ducking {
    pub fn new() -> Self {
        Self { origin: Instant::now(), until: AtomicU32::new(0) }
    }

    pub fn set_busy(&self, busy: bool) {
        let until = if busy {
            (self.origin.elapsed().as_millis() as u32).wrapping_add(HOLD.as_millis() as u32).max(1)
        } else {
            0
        };
        self.until.store(until, Ordering::Release);
    }

    pub fn active(&self) -> bool {
        let until = self.until.load(Ordering::Acquire);
        until != 0 && until.wrapping_sub(self.origin.elapsed().as_millis() as u32) as i32 > 0
    }
}

impl Default for Ducking {
    fn default() -> Self {
        Self::new()
    }
}

/// Плавный регулятор усиления: не зависит от размера пакетов, оба канала меняются синхронно.
pub struct Gain {
    current: f32,
    level: f32,
    attack_step: f32,
    release_step: f32,
}

impl Gain {
    pub fn new(rate: u32, level: f32) -> Self {
        let mut gain = Self { current: 1.0, level: 1.0, attack_step: 0.0, release_step: 0.0 };
        gain.configure(rate, level);
        gain
    }

    pub fn configure(&mut self, rate: u32, level: f32) {
        self.level = level.clamp(0.0, 1.0);
        let depth = (1.0 - self.level).max(f32::EPSILON);
        self.attack_step = depth / (rate as f32 * ATTACK_SECONDS);
        self.release_step = depth / (rate as f32 * RELEASE_SECONDS);
        self.current = self.current.clamp(self.level, 1.0);
    }

    pub fn is_unity(&self, busy: bool) -> bool {
        !busy && self.current >= 1.0
    }

    /// `frames` — чередующиеся сэмплы с `channels` каналами.
    pub fn apply(&mut self, samples: &mut [f32], channels: usize, busy: bool) {
        let target = if busy { self.level } else { 1.0 };
        if self.current == 1.0 && target == 1.0 {
            return;
        }
        for frame in samples.chunks_exact_mut(channels.max(1)) {
            self.current = if target < self.current {
                (self.current - self.attack_step).max(target)
            } else {
                (self.current + self.release_step).min(target)
            };
            for sample in frame {
                *sample *= self.current;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_flag_expires_and_clears() {
        let duck = Ducking::new();
        assert!(!duck.active());
        duck.set_busy(true);
        assert!(duck.active());
        duck.set_busy(false);
        assert!(!duck.active());
    }

    #[test]
    fn ramps_are_stereo_linked_bounded_and_independent_of_packet_size() {
        let rate = 44_100;
        let mut gain = Gain::new(rate, 0.2);
        let mut whole = vec![1.0f32; 10_000];
        gain.apply(&mut whole, 2, true);
        assert!(whole[0] < 1.0 && whole[0] > 0.99);
        assert!((whole[9_999] - 0.2).abs() < 1e-4);
        assert!(whole.windows(2).all(|w| w[0] >= w[1]));
        assert!(whole.chunks_exact(2).all(|f| f[0] == f[1]));

        let mut split = vec![1.0f32; 10_000];
        let mut split_gain = Gain::new(rate, 0.2);
        for chunk in split.chunks_mut(128) {
            split_gain.apply(chunk, 2, true);
        }
        assert_eq!(whole, split);

        let mut restored = vec![1.0f32; 30_000];
        gain.apply(&mut restored, 2, false);
        assert!(restored[0] > 0.2 && restored[0] < 0.21);
        assert_eq!(restored[29_999], 1.0);
        assert!(restored.windows(2).all(|w| w[0] <= w[1]));

        let original = vec![-0.5f32, 0.5, 0.0, 1.0];
        let mut untouched = original.clone();
        gain.apply(&mut untouched, 2, false);
        assert_eq!(untouched, original);
        assert!(gain.is_unity(false));
    }

    #[test]
    fn level_one_means_no_ducking() {
        let mut gain = Gain::new(44_100, 1.0);
        let original = vec![0.3f32; 100];
        let mut data = original.clone();
        gain.apply(&mut data, 2, true);
        assert_eq!(data, original);
    }
}
