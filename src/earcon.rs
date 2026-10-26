//! Короткие звуковые сигналы вместо голосовых ответов: колонка не умеет говорить от имени
//! musik, а сигнал сразу сообщает, что команда принята или не выполнена.

use std::f32::consts::TAU;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Earcon {
    /// Команда принята.
    Ok,
    /// Ничего не найдено или сервер недоступен.
    Fail,
}

fn tone(out: &mut Vec<i16>, rate: u32, freq: f32, millis: u32, amp: f32) {
    let frames = (rate as u64 * u64::from(millis) / 1000) as usize;
    let fade = (rate as usize / 200).max(1); // 5 мс, чтобы не щёлкало
    for i in 0..frames {
        let edge = i.min(frames - 1 - i);
        let envelope = if edge < fade { edge as f32 / fade as f32 } else { 1.0 };
        let value = (TAU * freq * i as f32 / rate as f32).sin() * amp * envelope;
        let sample = (value * 32767.0) as i16;
        out.push(sample);
        out.push(sample);
    }
}

fn silence(out: &mut Vec<i16>, rate: u32, millis: u32) {
    out.extend(std::iter::repeat_n(0, (rate as u64 * u64::from(millis) / 1000) as usize * 2));
}

/// Стерео S16 для устройства с частотой `rate`.
pub fn render(kind: Earcon, rate: u32) -> Vec<i16> {
    let mut out = Vec::new();
    match kind {
        Earcon::Ok => {
            tone(&mut out, rate, 660.0, 70, 0.25);
            tone(&mut out, rate, 880.0, 90, 0.25);
        }
        Earcon::Fail => {
            tone(&mut out, rate, 330.0, 120, 0.25);
            silence(&mut out, rate, 30);
            tone(&mut out, rate, 220.0, 160, 0.25);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_are_short_stereo_and_distinct() {
        let ok = render(Earcon::Ok, 44100);
        let fail = render(Earcon::Fail, 44100);
        assert_eq!(ok.len() % 2, 0);
        assert!((0.1..0.3).contains(&(ok.len() as f64 / 2.0 / 44100.0)));
        assert!((0.2..0.5).contains(&(fail.len() as f64 / 2.0 / 44100.0)));
        assert_ne!(ok, fail);
        assert_eq!(ok[0], 0, "сигнал начинается с нуля без щелчка");
        assert!(ok.iter().any(|s| s.abs() > 3000));
    }
}
