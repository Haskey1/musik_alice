use super::{Mixer, Output};
use crate::profile::Audio;
use alsa::{
    mixer::{Mixer as AlsaMixerHandle, SelemChannelId, SelemId},
    pcm::{Access, Format, HwParams, PCM},
    Direction, ValueOr,
};

pub struct AlsaOutput {
    pcm: PCM,
    rate: u32,
}

impl AlsaOutput {
    pub fn open(audio: &Audio) -> Result<Self, String> {
        let pcm = PCM::new(&audio.device, Direction::Playback, false)
            .map_err(|e| format!("не удалось открыть звуковое устройство «{}»: {e}", audio.device))?;
        let rate;
        {
            let hw = HwParams::any(&pcm).map_err(|e| format!("ALSA hw params: {e}"))?;
            hw.set_channels(2).map_err(|e| format!("ALSA: нужны 2 канала: {e}"))?;
            hw.set_rate(audio.rate, ValueOr::Nearest).map_err(|e| format!("ALSA rate: {e}"))?;
            hw.set_format(Format::s16()).map_err(|e| format!("ALSA: формат S16: {e}"))?;
            hw.set_access(Access::RWInterleaved).map_err(|e| format!("ALSA access: {e}"))?;
            // Небольшой буфер (~250 мс): быстрая пауза и скип, но запас против разрывов.
            // Точный размер без «near»-функций: у них на Mini 1 устаревшая версия ABI. Если устройство
            // такой размер не поддерживает, остаётся буфер по умолчанию.
            hw.set_buffer_size((audio.rate / 4) as alsa::pcm::Frames).ok();
            pcm.hw_params(&hw).map_err(|e| format!("ALSA hw_params: {e}"))?;
            rate = hw.get_rate().unwrap_or(audio.rate);
        }
        pcm.prepare().ok();
        Ok(Self { pcm, rate })
    }
}

impl Output for AlsaOutput {
    fn rate(&self) -> u32 {
        self.rate
    }

    fn write(&mut self, interleaved: &[i16]) -> Result<(), String> {
        let io = self.pcm.io_i16().map_err(|e| format!("ALSA io: {e}"))?;
        let mut rest = interleaved;
        let mut retries = 0;
        while !rest.is_empty() {
            match io.writei(rest) {
                Ok(frames) => rest = &rest[frames * 2..],
                Err(e) => {
                    retries += 1;
                    if retries > 5 {
                        return Err(format!("ALSA write: {e}"));
                    }
                    // Underrun/suspend: восстанавливаемся и пишем дальше.
                    self.pcm.try_recover(e, true).map_err(|e| format!("ALSA recover: {e}"))?;
                }
            }
        }
        Ok(())
    }

    fn drain(&mut self) {
        let _ = self.pcm.drain();
        let _ = self.pcm.prepare();
    }

    fn flush(&mut self) {
        let _ = self.pcm.drop();
        let _ = self.pcm.prepare();
    }
}

pub struct AlsaMixer {
    device: String,
    control: String,
    index: u32,
    channel: SelemChannelId,
}

impl AlsaMixer {
    pub fn new(audio: &Audio) -> Self {
        let channel = match audio.mixer_channel.as_str() {
            "front_right" => SelemChannelId::FrontRight,
            "mono" => SelemChannelId::mono(),
            _ => SelemChannelId::FrontLeft,
        };
        Self {
            device: audio.mixer_device.clone(),
            control: audio.mixer_control.clone(),
            index: audio.mixer_index,
            channel,
        }
    }

    fn with<T>(&self, f: impl FnOnce(&alsa::mixer::Selem<'_>) -> Result<T, String>) -> Result<T, String> {
        let mixer = AlsaMixerHandle::new(&self.device, false).map_err(|e| format!("ALSA mixer: {e}"))?;
        let id = SelemId::new(&self.control, self.index);
        let selem = mixer.find_selem(&id).ok_or_else(|| format!("нет регулятора «{}»", self.control))?;
        f(&selem)
    }
}

impl Mixer for AlsaMixer {
    fn get(&self) -> Result<u8, String> {
        self.with(|s| {
            let (min, max) = s.get_playback_volume_range();
            let value = s.get_playback_volume(self.channel).map_err(|e| e.to_string())?;
            if max <= min {
                return Ok(100);
            }
            Ok((((value - min) as f64 / (max - min) as f64) * 100.0).round() as u8)
        })
    }

    fn set(&self, percent: u8) -> Result<(), String> {
        self.with(|s| {
            let (min, max) = s.get_playback_volume_range();
            let value = min + ((max - min) as f64 * f64::from(percent.min(100)) / 100.0).round() as i64;
            s.set_playback_volume_all(value).map_err(|e| e.to_string())
        })
    }
}
