//! Световое кольцо и часы Мини 2 через sysfs (`/sys/class/leds/mini2:*`).
//!
//! Логика перенесена из Spotiya (`controls/mini2/lights.h`, `display.h`, MIT) и следует тем же правилам
//! вежливости: светодиоды принадлежат Алисе. Кольцо берём, только если оно погашено, а при любой чужой записи
//! сразу уступаем. С часов сохраняем исходную картинку и возвращаем её на место.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Один светодиод: файл `brightness`.
pub struct Channel {
    file: File,
    /// Обычный файл (тесты) нужно обрезать перед записью, sysfs — нет.
    truncate: bool,
}

impl Channel {
    pub fn open(path: &Path) -> Option<Self> {
        let file = OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC).open(path).ok()?;
        Some(Self { file, truncate: !path.starts_with("/sys") })
    }

    pub fn read(&mut self) -> Option<u8> {
        let mut text = [0u8; 16];
        self.file.seek(SeekFrom::Start(0)).ok()?;
        let n = self.file.read(&mut text).ok()?;
        let digits: Vec<u8> = text[..n].iter().copied().take_while(u8::is_ascii_digit).collect();
        if digits.is_empty() {
            return None;
        }
        std::str::from_utf8(&digits).ok()?.parse::<u16>().ok().filter(|v| *v <= 255).map(|v| v as u8)
    }

    pub fn write(&mut self, value: u8) -> bool {
        if self.file.seek(SeekFrom::Start(0)).is_err() {
            return false;
        }
        if self.truncate {
            let _ = self.file.set_len(0);
        }
        self.file.write_all(format!("{value}\n").as_bytes()).is_ok()
    }
}

// ---------------------------------------------------------------- кольцо

const RING_LEDS: usize = 18;

/// Короткий отклик на кольце.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Next,
    Previous,
    /// Пауза/продолжить; `paused` — состояние после нажатия.
    Toggle {
        paused: bool,
    },
    /// Громкость 0–100.
    Volume(u8),
}

pub struct Ring {
    prefix: String,
    channels: Vec<Channel>,
    last: [u8; RING_LEDS],
    effect: Effect,
    started: Instant,
    next_frame: Instant,
    yield_until: Instant,
}

impl Ring {
    pub fn new(prefix: &str) -> Self {
        let now = Instant::now();
        Self {
            prefix: prefix.to_owned(),
            channels: Vec::new(),
            last: [0; RING_LEDS],
            effect: Effect::Next,
            started: now,
            next_frame: now,
            yield_until: now,
        }
    }

    pub fn active(&self) -> bool {
        !self.channels.is_empty()
    }

    /// Порядок каналов: красные сегменты 1–6, затем зелёные, затем синие.
    fn path(&self, index: usize) -> PathBuf {
        let color = ["red", "green", "blue"][index / 6];
        PathBuf::from(format!("{}{color}:ring{}/brightness", self.prefix, index % 6 + 1))
    }

    fn open(&mut self) -> bool {
        let mut channels = Vec::with_capacity(RING_LEDS);
        for i in 0..RING_LEDS {
            let Some(mut channel) = Channel::open(&self.path(i)) else { return false };
            // Горит — значит кольцом пользуется Алиса.
            if channel.read() != Some(0) {
                return false;
            }
            channels.push(channel);
        }
        self.channels = channels;
        self.last = [0; RING_LEDS];
        true
    }

    fn owned(&mut self) -> bool {
        let last = self.last;
        self.channels.iter_mut().zip(last).all(|(c, v)| c.read() == Some(v))
    }

    fn close(&mut self, restore: bool) {
        if !self.active() {
            return;
        }
        // Изменённый кем-то канал принадлежит Алисе: её кадр не трогаем.
        if restore && self.owned() {
            for (c, v) in self.channels.iter_mut().zip(self.last) {
                if v != 0 {
                    c.write(0);
                }
            }
        }
        self.channels.clear();
    }

    pub fn begin(&mut self, effect: Effect, now: Instant) {
        if now < self.yield_until {
            return;
        }
        if self.active() && !self.owned() {
            self.close(false);
            self.yield_until = now + Duration::from_secs(1);
            return;
        }
        if !self.active() && !self.open() {
            return;
        }
        self.effect = effect;
        self.started = now;
        self.next_frame = now;
    }

    /// Следующий кадр; возвращает, когда позвать снова.
    pub fn tick(&mut self, now: Instant) -> Option<Instant> {
        if !self.active() {
            return None;
        }
        if now < self.next_frame {
            return Some(self.next_frame);
        }
        if !self.owned() {
            self.close(false);
            self.yield_until = now + Duration::from_secs(1);
            return None;
        }
        let elapsed = now.duration_since(self.started).as_millis() as i32;
        let Some(frame) = ring_frame(self.effect, elapsed) else {
            self.close(true);
            return None;
        };
        for (i, value) in frame.into_iter().enumerate() {
            if value != self.last[i] {
                if !self.channels[i].write(value) {
                    self.close(true);
                    return None;
                }
                self.last[i] = value;
            }
        }
        self.next_frame = now + Duration::from_millis(25);
        Some(self.next_frame)
    }
}

/// Кадр эффекта через `elapsed` мс; `None` — эффект закончился. Встроенные эффекты Spotiya.
pub fn ring_frame(effect: Effect, elapsed: i32) -> Option<[u8; RING_LEDS]> {
    let duration = if matches!(effect, Effect::Next | Effect::Previous) { 480 } else { 380 };
    if elapsed >= duration {
        return None;
    }
    let mut frame = [0u8; RING_LEDS];
    match effect {
        Effect::Next | Effect::Previous => {
            // Бегущая точка по кругу: вперёд — по часовой, назад — против.
            let head = elapsed / 80;
            for i in 0..6 {
                let brightness = match head - i as i32 {
                    0 => 80,
                    1 => 24,
                    2 => 5,
                    _ => 0,
                };
                let segment = if effect == Effect::Next { i } else { 5 - i };
                frame[6 + segment] = brightness;
                frame[12 + segment] = brightness / 3;
            }
        }
        Effect::Toggle { .. } | Effect::Volume(_) => {
            let fade = if elapsed < 60 { elapsed + 20 } else { (380 - elapsed) * 80 / 320 }.clamp(0, 80);
            for i in 0..6 {
                let mut brightness = fade;
                if let Effect::Volume(level) = effect {
                    if level == 0 {
                        frame[i] = (fade / 3) as u8;
                        continue;
                    }
                    let amount = (i32::from(level) * 6 - i as i32 * 100).clamp(0, 100);
                    brightness = fade * amount / 100;
                }
                if matches!(effect, Effect::Toggle { paused: true }) {
                    frame[i] = brightness as u8;
                    frame[6 + i] = (brightness / 3) as u8;
                } else {
                    frame[i] = (brightness / 12) as u8;
                    frame[6 + i] = brightness as u8;
                    frame[12 + i] = (brightness / 24) as u8;
                }
            }
        }
    }
    Some(frame)
}

// ---------------------------------------------------------------- часы

const CLOCK_LEDS: usize = 34;
/// Двоеточие: PLUS1 и PLUS2 (так их зажигают штатные часы).
const COLON: [usize; 2] = [28, 29];
/// Сегменты A–G для цифр 0–9, бит 0 — A.
const DIGITS: [u8; 10] = [63, 6, 91, 79, 102, 109, 125, 7, 127, 111];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Громкость на 2 секунды.
    Volume,
    /// Время трека.
    Position,
}

pub struct Clock {
    prefix: String,
    channels: Vec<Channel>,
    brightness: u8,
    mode: Mode,
    saved: [u8; CLOCK_LEDS],
    last: [u8; CLOCK_LEDS],
    until: Instant,
    check_at: Instant,
    yield_until: Instant,
    /// Время трека в секундах, если его надо показывать.
    position: Option<u64>,
}

impl Clock {
    pub fn new(prefix: &str) -> Self {
        let now = Instant::now();
        Self {
            prefix: prefix.to_owned(),
            channels: Vec::new(),
            brightness: 22,
            mode: Mode::Volume,
            saved: [0; CLOCK_LEDS],
            last: [0; CLOCK_LEDS],
            until: now,
            check_at: now,
            yield_until: now,
            position: None,
        }
    }

    pub fn active(&self) -> bool {
        !self.channels.is_empty()
    }

    fn path(&self, index: usize) -> PathBuf {
        let name = if index < 28 {
            format!("{}{}", (b'A' + (index % 7) as u8) as char, index / 7 + 1)
        } else {
            ["PLUS1", "PLUS2", "PLUS3", "PLUS4", "MINUS", "DEGREE"][index - 28].to_owned()
        };
        PathBuf::from(format!("{}{name}/brightness", self.prefix))
    }

    fn read_all(&mut self) -> Option<[u8; CLOCK_LEDS]> {
        let mut out = [0u8; CLOCK_LEDS];
        for (i, c) in self.channels.iter_mut().enumerate() {
            out[i] = c.read().filter(|v| *v <= 170)?;
        }
        Some(out)
    }

    fn open(&mut self) -> bool {
        let mut channels = Vec::with_capacity(CLOCK_LEDS);
        for i in 0..CLOCK_LEDS {
            let Some(c) = Channel::open(&self.path(i)) else { return false };
            channels.push(c);
        }
        self.channels = channels;
        let Some(current) = self.read_all() else {
            self.channels.clear();
            return false;
        };
        self.saved = current;
        self.last = current;
        self.brightness = current.iter().copied().max().filter(|b| *b > 0).unwrap_or(22);
        true
    }

    /// Вернуть часам их картинку: только те каналы, где ещё лежит наша запись.
    pub fn close(&mut self) {
        if !self.active() {
            return;
        }
        let (saved, last) = (self.saved, self.last);
        for (i, c) in self.channels.iter_mut().enumerate() {
            if last[i] != saved[i] && c.read() == Some(last[i]) {
                c.write(saved[i]);
            }
        }
        self.channels.clear();
    }

    /// Не изменил ли кто-то наши каналы. Мигание двоеточия штатных часов запоминаем и гасим обратно.
    fn unchanged(&mut self) -> bool {
        let Some(current) = self.read_all() else { return false };
        let mut ok = true;
        for (i, value) in current.into_iter().enumerate() {
            if value == self.last[i] {
                continue;
            }
            if COLON.contains(&i) {
                self.saved[i] = value;
                if !self.channels[i].write(self.last[i]) {
                    return false;
                }
            } else {
                ok = false;
            }
        }
        ok
    }

    /// Кто-то пишет в часы: уступаем. Если это штатные часы сменили минуту, запоминаем новое время.
    fn yield_to_owner(&mut self, now: Instant) {
        if let Some(current) = self.read_all() {
            self.saved = merge_clock(&self.saved, &self.last, &current);
        }
        self.close();
        self.yield_until = now + Duration::from_millis(1500);
    }

    fn draw(&mut self, frame: &[u8; CLOCK_LEDS]) -> bool {
        for (i, value) in frame.iter().copied().enumerate() {
            if value != self.last[i] {
                if !self.channels[i].write(value) {
                    self.close();
                    return false;
                }
                self.last[i] = value;
            }
        }
        true
    }

    /// Показать громкость на 2 секунды.
    pub fn volume(&mut self, percent: u8, now: Instant) {
        if self.active() && !self.unchanged() {
            self.yield_to_owner(now);
            return;
        }
        if now < self.yield_until || (!self.active() && !self.open()) {
            return;
        }
        if self.draw(&volume_frame(percent, self.brightness)) {
            self.mode = Mode::Volume;
            self.until = now + Duration::from_secs(2);
            self.check_at = now + Duration::from_millis(50);
        }
    }

    /// Какое время трека показывать (`None` — не показывать, вернуть часы).
    pub fn set_position(&mut self, seconds: Option<u64>) {
        self.position = seconds;
    }

    pub fn tick(&mut self, now: Instant) -> Option<Instant> {
        if !self.active() {
            let seconds = self.position?;
            if now < self.yield_until {
                return Some(self.yield_until);
            }
            if !self.open() {
                self.yield_until = now + Duration::from_secs(1);
                return Some(self.yield_until);
            }
            // Поверх анимации Алисы не рисуем: только поверх обычных часов.
            if parse_clock(&self.saved).is_none() {
                self.close();
                self.yield_until = now + Duration::from_millis(1500);
                return Some(self.yield_until);
            }
            self.mode = Mode::Position;
            self.draw(&position_frame(seconds, self.brightness));
            self.check_at = now + Duration::from_millis(100);
            return Some(self.check_at);
        }
        if now < self.check_at {
            return Some(self.check_at);
        }
        if !self.unchanged() {
            self.yield_to_owner(now);
            return Some(self.yield_until);
        }
        match (self.mode, self.position) {
            (Mode::Volume, _) if now < self.until => {}
            (_, Some(seconds)) => {
                self.mode = Mode::Position;
                if !self.draw(&position_frame(seconds, self.brightness)) {
                    return None;
                }
            }
            (_, None) => {
                self.close();
                return None;
            }
        }
        self.check_at = now + Duration::from_millis(if self.mode == Mode::Volume { 50 } else { 100 });
        Some(self.check_at)
    }
}

fn digit_into(frame: &mut [u8; CLOCK_LEDS], position: usize, digit: u8, brightness: u8) {
    let mask = DIGITS[usize::from(digit % 10)];
    for segment in 0..7 {
        if mask & (1 << segment) != 0 {
            frame[position * 7 + segment] = brightness;
        }
    }
}

/// Громкость прижатыми вправо цифрами.
pub fn volume_frame(percent: u8, brightness: u8) -> [u8; CLOCK_LEDS] {
    let mut frame = [0u8; CLOCK_LEDS];
    let mut value = percent.min(100);
    for position in (1..=3).rev() {
        digit_into(&mut frame, position, value % 10, brightness);
        value /= 10;
        if value == 0 {
            break;
        }
    }
    frame
}

/// «м:сс» или «мм:сс» с горящим двоеточием; больше 99 минут — по модулю.
pub fn position_frame(seconds: u64, brightness: u8) -> [u8; CLOCK_LEDS] {
    let mut frame = [0u8; CLOCK_LEDS];
    let minutes = (seconds / 60) % 100;
    let secs = seconds % 60;
    if minutes >= 10 {
        digit_into(&mut frame, 0, (minutes / 10) as u8, brightness);
    }
    digit_into(&mut frame, 1, (minutes % 10) as u8, brightness);
    digit_into(&mut frame, 2, (secs / 10) as u8, brightness);
    digit_into(&mut frame, 3, (secs % 10) as u8, brightness);
    for i in COLON {
        frame[i] = brightness;
    }
    frame
}

/// Если кадр — обычные часы «ЧЧ:ММ» (первая цифра может быть погашена), вернуть часы, минуты и яркость.
pub fn parse_clock(frame: &[u8; CLOCK_LEDS]) -> Option<(u32, u32, u8, bool)> {
    if frame[30..].iter().any(|v| *v != 0) {
        return None;
    }
    let mut digits = [0u32; 4];
    let mut lit = false;
    let mut first_blank = false;
    let mut brightness = 0u8;
    for (position, digit) in digits.iter_mut().enumerate() {
        let mut mask = 0u8;
        for segment in 0..7 {
            let v = frame[position * 7 + segment];
            if v != 0 {
                mask |= 1 << segment;
                brightness = brightness.max(v);
            }
        }
        if position == 0 && mask == 0 {
            first_blank = true;
            *digit = 0;
            continue;
        }
        lit |= mask != 0;
        *digit = DIGITS.iter().position(|m| *m == mask)? as u32;
    }
    if !lit {
        return None;
    }
    let (hours, minutes) = (digits[0] * 10 + digits[1], digits[2] * 10 + digits[3]);
    (hours < 24 && minutes < 60).then_some((hours, minutes, brightness, first_blank))
}

fn clock_frame(hours: u32, minutes: u32, brightness: u8, first_blank: bool, colon: &[u8]) -> [u8; CLOCK_LEDS] {
    let mut frame = [0u8; CLOCK_LEDS];
    if !(first_blank && hours < 10) {
        digit_into(&mut frame, 0, (hours / 10) as u8, brightness);
    }
    digit_into(&mut frame, 1, (hours % 10) as u8, brightness);
    digit_into(&mut frame, 2, (minutes / 10) as u8, brightness);
    digit_into(&mut frame, 3, (minutes % 10) as u8, brightness);
    frame[28] = colon[0];
    frame[29] = colon[1];
    frame
}

/// Что на самом деле нарисовали бы штатные часы после их записи поверх нашего кадра.
///
/// Часы пишут только изменившиеся каналы. Поэтому канал, совпадающий с нашим, неоднозначен: часы его либо
/// не трогали, либо записали то же значение. Чаще всего причина записи — смена минуты: если «сохранённое время
/// плюс минута» объясняет все замеченные изменения, берём его целиком. Иначе меняем только явно изменённые каналы.
pub fn merge_clock(saved: &[u8; CLOCK_LEDS], last: &[u8; CLOCK_LEDS], current: &[u8; CLOCK_LEDS]) -> [u8; CLOCK_LEDS] {
    let changed: Vec<usize> = (0..CLOCK_LEDS).filter(|i| current[*i] != last[*i] && !COLON.contains(i)).collect();
    let colon = [current[28], current[29]];
    if let Some((h, m, brightness, blank)) = parse_clock(saved) {
        for step in [1u32, 0] {
            let total = (h * 60 + m + step) % (24 * 60);
            let candidate = clock_frame(total / 60, total % 60, brightness, blank, &colon);
            if changed.iter().all(|i| candidate[*i] == current[*i]) {
                return candidate;
            }
        }
    }
    let mut merged = *saved;
    for i in changed {
        merged[i] = current[i];
    }
    merged[28] = colon[0];
    merged[29] = colon[1];
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_ring(dir: &Path) -> String {
        for color in ["red", "green", "blue"] {
            for n in 1..=6 {
                let d = dir.join(format!("mini2:{color}:ring{n}"));
                std::fs::create_dir_all(&d).unwrap();
                std::fs::write(d.join("brightness"), "0\n").unwrap();
            }
        }
        format!("{}/mini2:", dir.display())
    }

    fn fake_clock(dir: &Path, frame: &[u8; CLOCK_LEDS]) -> String {
        let prefix = format!("{}/mini2:white:clock", dir.display());
        let clock = Clock::new(&prefix);
        for (i, v) in frame.iter().enumerate() {
            let path = clock.path(i);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("{v}\n")).unwrap();
        }
        prefix
    }

    fn read_clock(prefix: &str) -> [u8; CLOCK_LEDS] {
        let clock = Clock::new(prefix);
        let mut out = [0u8; CLOCK_LEDS];
        for (i, v) in out.iter_mut().enumerate() {
            *v = std::fs::read_to_string(clock.path(i)).unwrap().trim().parse().unwrap();
        }
        out
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("musikalice-leds-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ring_effects_have_the_expected_shape() {
        let next = ring_frame(Effect::Next, 0).unwrap();
        assert_eq!(next[6], 80, "вперёд начинается с первого сегмента");
        let prev = ring_frame(Effect::Previous, 0).unwrap();
        assert_eq!(prev[11], 80, "назад — с последнего");
        assert!(ring_frame(Effect::Next, 480).is_none());
        let paused = ring_frame(Effect::Toggle { paused: true }, 60).unwrap();
        assert!(paused[0] > paused[6], "пауза — тёплый цвет");
        let playing = ring_frame(Effect::Toggle { paused: false }, 60).unwrap();
        assert!(playing[6] > playing[0], "игра — зелёный");
        let half = ring_frame(Effect::Volume(50), 60).unwrap();
        assert!(half[6 + 2] > 0 && half[6 + 3] == 0, "половина громкости — половина кольца: {half:?}");
        assert_eq!(ring_frame(Effect::Volume(0), 60).unwrap()[0], 26);
    }

    #[test]
    fn ring_plays_then_turns_off_and_yields_to_alice() {
        let dir = tmp("ring");
        let prefix = fake_ring(&dir);
        let mut ring = Ring::new(&prefix);
        let t0 = Instant::now();
        ring.begin(Effect::Next, t0);
        assert!(ring.active());
        ring.tick(t0);
        assert_eq!(std::fs::read_to_string(dir.join("mini2:green:ring1/brightness")).unwrap(), "80\n");
        ring.tick(t0 + Duration::from_millis(600));
        assert!(!ring.active());
        assert_eq!(std::fs::read_to_string(dir.join("mini2:green:ring1/brightness")).unwrap(), "0\n");

        // Алиса зажгла кольцо — не берём его.
        std::fs::write(dir.join("mini2:blue:ring3/brightness"), "40\n").unwrap();
        ring.begin(Effect::Next, t0 + Duration::from_secs(2));
        assert!(!ring.active());
        // Алиса перехватила кольцо посреди эффекта — уходим, её кадр не гасим.
        std::fs::write(dir.join("mini2:blue:ring3/brightness"), "0\n").unwrap();
        let t1 = t0 + Duration::from_secs(3);
        ring.begin(Effect::Previous, t1);
        ring.tick(t1);
        std::fs::write(dir.join("mini2:red:ring1/brightness"), "99\n").unwrap();
        ring.tick(t1 + Duration::from_millis(30));
        assert!(!ring.active());
        assert_eq!(std::fs::read_to_string(dir.join("mini2:red:ring1/brightness")).unwrap(), "99\n");
    }

    #[test]
    fn clock_frames() {
        assert_eq!(parse_clock(&clock_frame(9, 5, 22, true, &[0, 0])), Some((9, 5, 22, true)));
        assert_eq!(parse_clock(&clock_frame(23, 59, 22, false, &[22, 22])), Some((23, 59, 22, false)));
        assert_eq!(parse_clock(&volume_frame(45, 22)), None, "«  45» — не время");
        let p = position_frame(3 * 60 + 7, 30);
        assert_eq!(&p[7..14], &[30, 30, 30, 30, 0, 0, 30], "минуты: 3 — сегменты A, B, C, D, G");
        assert_eq!((p[28], p[29]), (30, 30), "двоеточие горит");
        assert!(p[..7].iter().all(|v| *v == 0), "ведущий ноль погашен");
    }

    #[test]
    fn minute_change_under_overlay_is_restored_correctly() {
        // До нас часы показывали 12:39, мы рисовали время трека, а часы переключились на 12:40.
        let before = clock_frame(12, 39, 22, false, &[22, 22]);
        let ours = position_frame(2 * 60 + 5, 22);
        let truth = clock_frame(12, 40, 22, false, &[0, 0]);
        // Часы пишут только разницу между своими кадрами.
        let mut current = ours;
        for i in 0..28 {
            if truth[i] != before[i] {
                current[i] = truth[i];
            }
        }
        current[28] = 0;
        current[29] = 0;
        assert_eq!(merge_clock(&before, &ours, &current), truth);
    }

    #[test]
    fn clock_shows_volume_and_position_then_gives_the_time_back() {
        let dir = tmp("clock");
        let time = clock_frame(7, 30, 22, true, &[22, 22]);
        let prefix = fake_clock(&dir, &time);
        let mut clock = Clock::new(&prefix);
        let t0 = Instant::now();
        clock.volume(45, t0);
        assert_eq!(read_clock(&prefix), volume_frame(45, 22));
        clock.tick(t0 + Duration::from_secs(3));
        assert_eq!(read_clock(&prefix), time, "через 2 секунды часы вернулись");

        clock.set_position(Some(65));
        clock.tick(t0 + Duration::from_secs(4));
        assert_eq!(read_clock(&prefix), position_frame(65, 22));
        clock.set_position(Some(66));
        clock.tick(t0 + Duration::from_secs(5));
        assert_eq!(read_clock(&prefix), position_frame(66, 22));
        clock.set_position(None);
        clock.tick(t0 + Duration::from_secs(6));
        assert!(!clock.active());
        assert_eq!(read_clock(&prefix), time);
    }
}
