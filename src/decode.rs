//! Декодирование потока (mp3/aac/flac/ogg/wav) в чередующийся стерео S16 нужной частоты.
//!
//! Поток по HTTP не поддерживает перемотку, поэтому используется `ReadOnlySource`.
//! Симфония выдаёт любое число каналов и частоту; здесь всё приводится к стерео и частоте
//! устройства линейной интерполяцией (для музыки на колонке этого достаточно).

use std::io::Read;
use symphonia::core::{
    audio::SampleBuffer,
    codecs::{DecoderOptions, CODEC_TYPE_NULL},
    errors::Error as SymError,
    formats::{FormatOptions, FormatReader},
    io::{MediaSourceStream, ReadOnlySource},
    meta::MetadataOptions,
    probe::Hint,
};

pub struct TrackDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track_id: u32,
    resampler: Resampler,
    scratch: Option<SampleBuffer<f32>>,
    /// Длительность по заголовку, если контейнер её знает.
    pub duration_hint: Option<f64>,
}

/// Подсказка формата по Content-Type ответа сервера.
pub fn hint_for(content_type: Option<&str>) -> Hint {
    let mut hint = Hint::new();
    if let Some(ct) = content_type {
        hint.mime_type(ct);
        let ext = match ct {
            "audio/mpeg" | "audio/mp3" => Some("mp3"),
            "audio/aac" | "audio/aacp" => Some("aac"),
            "audio/mp4" | "audio/x-m4a" | "audio/m4a" => Some("m4a"),
            "audio/flac" | "audio/x-flac" => Some("flac"),
            "audio/ogg" | "application/ogg" => Some("ogg"),
            "audio/wav" | "audio/x-wav" | "audio/wave" => Some("wav"),
            _ => None,
        };
        if let Some(ext) = ext {
            hint.with_extension(ext);
        }
    }
    hint
}

impl TrackDecoder {
    pub fn open(reader: Box<dyn Read + Send>, hint: &Hint, out_rate: u32) -> Result<Self, String> {
        struct Src(std::sync::Mutex<Box<dyn Read + Send>>);
        impl Read for Src {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0.get_mut().unwrap_or_else(|e| e.into_inner()).read(buf)
            }
        }
        let source = ReadOnlySource::new(Src(std::sync::Mutex::new(reader)));
        let stream = MediaSourceStream::new(Box::new(source), Default::default());
        let probed = symphonia::default::get_probe()
            .format(hint, stream, &FormatOptions::default(), &MetadataOptions::default())
            .map_err(|e| format!("формат не распознан: {e}"))?;
        let format = probed.format;
        let track =
            format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL).ok_or("в потоке нет аудио")?;
        let track_id = track.id;
        let params = track.codec_params.clone();
        let duration_hint = match (params.n_frames, params.sample_rate) {
            (Some(n), Some(rate)) if rate > 0 => Some(n as f64 / f64::from(rate)),
            _ => None,
        };
        let decoder = symphonia::default::get_codecs()
            .make(&params, &DecoderOptions::default())
            .map_err(|e| format!("кодек не поддерживается: {e}"))?;
        Ok(Self { format, decoder, track_id, resampler: Resampler::new(out_rate), scratch: None, duration_hint })
    }

    /// Добавляет в `out` следующую порцию стерео-сэмплов. `Ok(false)` — поток закончился.
    pub fn next_chunk(&mut self, out: &mut Vec<i16>) -> Result<bool, String> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(SymError::ResetRequired) => return Ok(false),
                Err(SymError::IoError(e)) => return Err(format!("обрыв потока: {e}")),
                Err(e) => return Err(format!("ошибка чтения: {e}")),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let spec = *decoded.spec();
                    let channels = spec.channels.count();
                    if channels == 0 || decoded.frames() == 0 {
                        continue;
                    }
                    let needed = decoded.capacity() as u64;
                    if self.scratch.as_ref().is_none_or(|b| (b.capacity() as u64) < needed * channels as u64) {
                        self.scratch = Some(SampleBuffer::new(needed, spec));
                    }
                    let buffer = self.scratch.as_mut().expect("буфер создан выше");
                    buffer.copy_interleaved_ref(decoded);
                    self.resampler.push(buffer.samples(), channels, spec.rate, out);
                    return Ok(true);
                }
                // Битый кадр: пропускаем, как делают плееры.
                Err(SymError::DecodeError(_)) => continue,
                Err(SymError::IoError(_)) => continue,
                Err(e) => return Err(format!("ошибка декодера: {e}")),
            }
        }
    }
}

/// Потоковый ресемплер в стерео S16 (линейная интерполяция).
struct Resampler {
    out_rate: u32,
    in_rate: u32,
    /// Дробная позиция следующего выходного кадра относительно `prev`.
    pos: f64,
    prev: [f32; 2],
    have_prev: bool,
}

impl Resampler {
    fn new(out_rate: u32) -> Self {
        Self { out_rate, in_rate: 0, pos: 0.0, prev: [0.0; 2], have_prev: false }
    }

    fn push(&mut self, samples: &[f32], channels: usize, in_rate: u32, out: &mut Vec<i16>) {
        if in_rate != self.in_rate {
            self.in_rate = in_rate;
            self.pos = 0.0;
        }
        let frame = |i: usize| -> [f32; 2] {
            let s = &samples[i * channels..(i + 1) * channels];
            match channels {
                1 => [s[0], s[0]],
                _ => [s[0], s[1]],
            }
        };
        let frames = samples.len() / channels;
        let to_i16 = |v: f32| (v.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        if in_rate == self.out_rate {
            out.reserve(frames * 2);
            for i in 0..frames {
                let f = frame(i);
                out.push(to_i16(f[0]));
                out.push(to_i16(f[1]));
            }
            self.have_prev = false;
            return;
        }
        let step = f64::from(in_rate) / f64::from(self.out_rate);
        out.reserve((frames as f64 / step) as usize * 2 + 4);
        for i in 0..frames {
            let cur = frame(i);
            if !self.have_prev {
                self.prev = cur;
                self.have_prev = true;
                continue;
            }
            // Между prev (позиция 0) и cur (позиция 1).
            while self.pos < 1.0 {
                let t = self.pos as f32;
                out.push(to_i16(self.prev[0] + (cur[0] - self.prev[0]) * t));
                out.push(to_i16(self.prev[1] + (cur[1] - self.prev[1]) * t));
                self.pos += step;
            }
            self.pos -= 1.0;
            self.prev = cur;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(bytes: &'static [u8], content_type: &str, rate: u32) -> (Vec<i16>, Option<f64>) {
        let mut dec =
            TrackDecoder::open(Box::new(std::io::Cursor::new(bytes)), &hint_for(Some(content_type)), rate).unwrap();
        let hint = dec.duration_hint;
        let mut out = Vec::new();
        while dec.next_chunk(&mut out).unwrap() {}
        (out, hint)
    }

    fn peak(samples: &[i16]) -> i16 {
        samples.iter().map(|s| s.saturating_abs()).max().unwrap_or(0)
    }

    /// Частота по числу переходов через ноль в левом канале.
    fn frequency(samples: &[i16], rate: u32) -> f64 {
        let left: Vec<i16> = samples.iter().step_by(2).copied().collect();
        let crossings = left.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
        crossings as f64 / (left.len() as f64 / f64::from(rate))
    }

    #[test]
    fn mp3_decodes_to_stereo_at_the_native_rate() {
        let (pcm, _) = decode_all(include_bytes!("../tests/fixtures/tone.mp3"), "audio/mpeg", 44100);
        assert_eq!(pcm.len() % 2, 0);
        let seconds = pcm.len() as f64 / 2.0 / 44100.0;
        assert!((0.9..1.2).contains(&seconds), "{seconds}");
        assert!(peak(&pcm) > 2000);
        let f = frequency(&pcm, 44100);
        assert!((f - 440.0).abs() < 15.0, "{f}");
    }

    #[test]
    fn resampling_keeps_pitch_and_duration() {
        let (pcm, _) = decode_all(include_bytes!("../tests/fixtures/tone.mp3"), "audio/mpeg", 48000);
        let seconds = pcm.len() as f64 / 2.0 / 48000.0;
        assert!((0.9..1.2).contains(&seconds), "{seconds}");
        let f = frequency(&pcm, 48000);
        assert!((f - 440.0).abs() < 15.0, "{f}");
    }

    #[test]
    fn mono_flac_is_duplicated_to_both_channels_and_upsampled() {
        let (pcm, hint) = decode_all(include_bytes!("../tests/fixtures/tone_mono.flac"), "audio/flac", 44100);
        assert!((hint.unwrap() - 1.0).abs() < 0.01);
        assert!(pcm.chunks(2).all(|f| f[0] == f[1]));
        let f = frequency(&pcm, 44100);
        assert!((f - 440.0).abs() < 15.0, "{f}");
        let seconds = pcm.len() as f64 / 2.0 / 44100.0;
        assert!((0.95..1.05).contains(&seconds), "{seconds}");
    }

    #[test]
    fn aac_in_mp4_is_a_supported_container() {
        let reader = std::io::Cursor::new(include_bytes!("../tests/fixtures/tone.m4a").as_slice());
        // MP4 ищет moov по потоку; для ReadOnlySource файл с moov в начале читается последовательно.
        match TrackDecoder::open(Box::new(reader), &hint_for(Some("audio/mp4")), 44100) {
            Ok(mut dec) => {
                let mut out = Vec::new();
                while dec.next_chunk(&mut out).unwrap_or(false) {}
                assert!(peak(&out) > 2000);
            }
            Err(e) => panic!("m4a не открылся: {e}"),
        }
    }

    #[test]
    fn garbage_is_rejected_with_a_message() {
        let junk: Vec<u8> = (0..4096u32).map(|i| (i * 31 % 251) as u8).collect();
        let err = TrackDecoder::open(Box::new(std::io::Cursor::new(junk)), &Hint::new(), 44100).err().unwrap();
        assert!(!err.is_empty());
    }

    #[test]
    fn truncated_stream_ends_cleanly() {
        let bytes: &'static [u8] = &include_bytes!("../tests/fixtures/tone.mp3")[..4000];
        let (pcm, _) = decode_all(bytes, "audio/mpeg", 44100);
        assert!(!pcm.is_empty());
    }
}
