//! Вывод звука и системная громкость. Реальный бэкенд — ALSA (фича `alsa-backend`),
//! без неё и в тестах используется заглушка, которая копит сэмплы в памяти.

use crate::profile::Audio;
use std::sync::Arc;

#[cfg(feature = "alsa-backend")]
mod alsa_backend;
mod null;

#[allow(unused_imports)]
pub use null::{NullMixer, NullOutput};

/// Приёмник PCM: стерео S16 чередующийся, частота `rate()`.
pub trait Output: Send {
    fn rate(&self) -> u32;
    /// Блокируется, пока аппаратный буфер не примет данные (так плеер получает темп воспроизведения).
    fn write(&mut self, interleaved: &[i16]) -> Result<(), String>;
    /// Дожидается проигрывания всего, что уже записано.
    fn drain(&mut self);
    /// Мгновенно отбрасывает всё недоигранное (пауза, пропуск трека).
    fn flush(&mut self);
}

/// Системная громкость, 0–100.
pub trait Mixer: Send + Sync {
    fn get(&self) -> Result<u8, String>;
    fn set(&self, percent: u8) -> Result<(), String>;
}

pub fn open_output(audio: &Audio) -> Result<Box<dyn Output>, String> {
    #[cfg(feature = "alsa-backend")]
    {
        alsa_backend::AlsaOutput::open(audio).map(|o| Box::new(o) as Box<dyn Output>)
    }
    #[cfg(not(feature = "alsa-backend"))]
    {
        let _ = audio;
        Ok(Box::new(NullOutput::new(audio_rate(audio), Arc::new(std::sync::Mutex::new(Vec::new())))))
    }
}

pub fn open_mixer(audio: &Audio) -> Arc<dyn Mixer> {
    #[cfg(feature = "alsa-backend")]
    {
        Arc::new(alsa_backend::AlsaMixer::new(audio))
    }
    #[cfg(not(feature = "alsa-backend"))]
    {
        let _ = audio;
        Arc::new(NullMixer::new(50))
    }
}

#[cfg(not(feature = "alsa-backend"))]
fn audio_rate(audio: &Audio) -> u32 {
    audio.rate
}
