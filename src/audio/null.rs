#![allow(dead_code)]

use super::{Mixer, Output};
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc, Mutex,
};

/// Копит сэмплы в общем буфере. Для тестов и сборки без ALSA.
pub struct NullOutput {
    rate: u32,
    sink: Arc<Mutex<Vec<i16>>>,
    /// Имитировать реальное время записи (спит пропорционально длине блока).
    pace: bool,
}

impl NullOutput {
    pub fn new(rate: u32, sink: Arc<Mutex<Vec<i16>>>) -> Self {
        Self { rate, sink, pace: false }
    }
    #[allow(dead_code)]
    pub fn paced(rate: u32, sink: Arc<Mutex<Vec<i16>>>) -> Self {
        Self { rate, sink, pace: true }
    }
}

impl Output for NullOutput {
    fn rate(&self) -> u32 {
        self.rate
    }
    fn write(&mut self, interleaved: &[i16]) -> Result<(), String> {
        self.sink.lock().unwrap().extend_from_slice(interleaved);
        if self.pace {
            let micros = interleaved.len() as u64 / 2 * 1_000_000 / u64::from(self.rate);
            std::thread::sleep(std::time::Duration::from_micros(micros));
        }
        Ok(())
    }
    fn drain(&mut self) {}
    fn flush(&mut self) {}
}

pub struct NullMixer(AtomicU8);

impl NullMixer {
    pub fn new(initial: u8) -> Self {
        Self(AtomicU8::new(initial))
    }
}

impl Mixer for NullMixer {
    fn get(&self) -> Result<u8, String> {
        Ok(self.0.load(Ordering::Relaxed))
    }
    fn set(&self, percent: u8) -> Result<(), String> {
        self.0.store(percent.min(100), Ordering::Relaxed);
        Ok(())
    }
}
