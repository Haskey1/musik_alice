//! Кнопки, световое кольцо и часы Мини 2.
//!
//! Пока musik играет или стоит на паузе, сенсорные кнопки работают на него (жесты — в [`gesture`]), кольцо
//! коротко подтверждает нажатие, а на часах на 2 секунды видна громкость (или постоянно — время трека,
//! `display position`). Как только musik отпускает колонку, кнопки и светодиоды снова целиком у Алисы.

pub mod gesture;
pub mod input;
pub mod leds;

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;

use crate::{
    announce,
    config::Display,
    engine::{State, Status},
    intent::Intent,
};
use gesture::{Action, Gesture, Keys};
use input::{Event, Input};
use leds::{Clock, Effect, Ring};

/// Шаг громкости кнопками, в процентах Master.
pub const VOLUME_STEP: u8 = 5;
const PROBE: Duration = Duration::from_millis(300);

/// Блок `controls` профиля станции (формат Spotiya).
#[derive(Clone, Debug, Deserialize)]
pub struct Settings {
    pub driver: String,
    pub input_device: Option<PathBuf>,
    pub keys: Option<KeyCodes>,
    pub ring_prefix: Option<String>,
    pub display_prefix: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct KeyCodes {
    pub alice: u16,
    pub volume_up: u16,
    pub volume_down: u16,
}

impl Settings {
    /// Настройки, если профиль описывает поддерживаемые органы управления.
    pub fn from_profile(controls: Option<&serde_json::Value>) -> Option<Self> {
        let settings: Settings = serde_json::from_value(controls?.clone()).ok()?;
        (settings.driver == "mini2").then_some(settings)
    }
}

/// То, чем контроллер управляет: служба musik (в тестах — подделка).
pub trait Remote: Send + Sync {
    fn status(&self) -> Option<Status>;
    fn request(&self, intent: Intent);
}

impl Remote for crate::engine::Handle {
    fn status(&self) -> Option<Status> {
        crate::engine::Handle::status(self)
    }
    fn request(&self, intent: Intent) {
        let _ = crate::engine::Handle::request(self, intent, false);
    }
}

/// Снимок состояния musik, нужный кнопкам.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct View {
    /// musik занимает колонку: кнопки и светодиоды наши.
    pub active: bool,
    pub paused: bool,
    pub volume: u8,
    pub buttons: bool,
    pub show_position: bool,
    pub position_ms: u64,
    pub playing: bool,
}

impl View {
    pub fn of(status: &Status) -> Self {
        let active = !status.session.is_empty() && status.state != State::Idle;
        Self {
            active,
            paused: status.state == State::Paused,
            volume: status.volume.unwrap_or(50),
            buttons: status.buttons,
            show_position: status.display == Display::Position,
            position_ms: status.position_ms,
            playing: status.state == State::Playing,
        }
    }
}

/// Что сделать по жесту: команда службе и отклик на кольце и часах.
#[derive(Debug, PartialEq, Eq)]
pub struct Reaction {
    pub intent: Option<Intent>,
    pub ring: Option<Effect>,
    /// Новая громкость, чтобы показать её на часах.
    pub volume: Option<u8>,
    pub listen: bool,
}

pub fn react(action: Action, view: &View) -> Reaction {
    let none = Reaction { intent: None, ring: None, volume: None, listen: false };
    match action {
        Action::Toggle => {
            Reaction { intent: Some(Intent::Toggle), ring: Some(Effect::Toggle { paused: !view.paused }), ..none }
        }
        Action::Next => Reaction { intent: Some(Intent::Next), ring: Some(Effect::Next), ..none },
        Action::Previous => Reaction { intent: Some(Intent::Previous), ring: Some(Effect::Previous), ..none },
        Action::Louder | Action::Quieter => {
            let level = if action == Action::Louder {
                view.volume.saturating_add(VOLUME_STEP).min(100)
            } else {
                view.volume.saturating_sub(VOLUME_STEP)
            };
            // Ровная сетка шагов: 47 → 50, а не 52.
            let level =
                (u16::from(level) + u16::from(VOLUME_STEP) / 2) / u16::from(VOLUME_STEP) * u16::from(VOLUME_STEP);
            let level = level.min(100) as u8;
            Reaction {
                intent: Some(Intent::Volume(level)),
                ring: Some(Effect::Volume(level)),
                volume: Some(level),
                ..none
            }
        }
        Action::Listen => Reaction { listen: true, ..none },
    }
}

// На 32-битной станции поля timespec — i32, на 64-битной машине сборки — уже i64.
#[allow(clippy::useless_conversion)]
fn now_us(monotonic: bool) -> i64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    let clock = if monotonic { libc::CLOCK_MONOTONIC } else { libc::CLOCK_REALTIME };
    // SAFETY: корректный указатель на timespec.
    unsafe { libc::clock_gettime(clock, &mut t) };
    i64::from(t.tv_sec) * 1_000_000 + i64::from(t.tv_nsec) / 1000
}

/// Запускает поток управления. Возвращает `None`, если у станции нет поддерживаемых органов управления.
pub fn spawn(settings: Settings, remote: Arc<dyn Remote>, stop: Arc<AtomicBool>) -> Option<thread::JoinHandle<()>> {
    thread::Builder::new().name("controls".into()).spawn(move || run(settings, remote, stop)).ok()
}

fn run(settings: Settings, remote: Arc<dyn Remote>, stop: Arc<AtomicBool>) {
    let keys = settings.keys.map(|k| Keys { alice: k.alice, volume_up: k.volume_up, volume_down: k.volume_down });
    let mut input = match (&settings.input_device, keys) {
        (Some(path), Some(_)) => match Input::open(path) {
            Ok(input) => Some(input),
            Err(e) => {
                eprintln!("[кнопки] {} недоступно: {e}; работают только кольцо и часы", path.display());
                None
            }
        },
        _ => None,
    };
    let mut gesture = keys.map(Gesture::new);
    let mut ring = settings.ring_prefix.as_deref().map(Ring::new);
    let mut clock = settings.display_prefix.as_deref().map(Clock::new);
    eprintln!(
        "[кнопки] готово: кнопки {}, кольцо {}, часы {}",
        if input.is_some() { "да" } else { "нет" },
        if ring.is_some() { "да" } else { "нет" },
        if clock.is_some() { "да" } else { "нет" }
    );

    let mut view = View::default();
    let mut probed = Instant::now();
    let mut probe_at = Instant::now();
    let mut held = false;
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= probe_at {
            if let Some(status) = remote.status() {
                view = View::of(&status);
                probed = now;
            }
            probe_at = now + PROBE;
        }
        let wanted = view.active && view.buttons;

        // События ввода.
        if let (Some(dev), Some(g)) = (input.as_mut(), gesture.as_mut()) {
            match dev.read() {
                Ok(events) => {
                    for event in events {
                        match event {
                            Event::Dropped => {
                                g.reset();
                                held = dev.any_key_down();
                            }
                            Event::Key { code, value, at } => {
                                held = value != 0 || dev.any_key_down();
                                if dev.grabbed() && wanted {
                                    g.key(code, value, at);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[кнопки] чтение не удалось: {e}; кнопки остаются у Алисы");
                    input = None;
                    continue;
                }
            }
            // Захват меняем, только когда ничего не зажато: иначе Алиса увидит «нажатие без отпускания».
            if dev.grabbed() != wanted && !held && !dev.any_key_down() {
                match dev.grab(wanted) {
                    Ok(()) => {
                        g.reset();
                        eprintln!("[кнопки] {}", if wanted { "управляют musik" } else { "вернулись Алисе" });
                    }
                    Err(e) => {
                        eprintln!("[кнопки] захват не удался: {e}; кнопки остаются у Алисы");
                        input = None;
                        continue;
                    }
                }
            }
            if dev.grabbed() && wanted {
                g.tick(now_us(dev.monotonic));
            }
            for action in g.take() {
                let reaction = react(action, &view);
                eprintln!("[кнопки] {action:?}");
                if let Some(intent) = reaction.intent {
                    remote.request(intent);
                }
                if reaction.listen {
                    let _ = thread::Builder::new().name("alice-listen".into()).spawn(|| {
                        if let Err(e) = announce::listen() {
                            eprintln!("[кнопки] Алиса не откликнулась: {e}");
                        }
                    });
                }
                let now = Instant::now();
                if let (Some(ring), Some(effect)) = (ring.as_mut(), reaction.ring) {
                    ring.begin(effect, now);
                }
                if let Some(level) = reaction.volume {
                    view.volume = level;
                    if let Some(clock) = clock.as_mut() {
                        clock.volume(level, now);
                    }
                }
                if matches!(action, Action::Toggle) {
                    view.paused = !view.paused;
                    view.playing = !view.paused;
                }
                // Состояние после команды перечитаем сразу.
                probe_at = now;
            }
        }

        // Светодиоды.
        let now = Instant::now();
        let mut wake = probe_at;
        let mut earliest = |at: Option<Instant>| {
            if let Some(at) = at {
                wake = wake.min(at);
            }
        };
        if let Some(ring) = ring.as_mut() {
            earliest(ring.tick(now));
        }
        if let Some(clock) = clock.as_mut() {
            let position = (view.active && view.show_position).then(|| {
                let extra = if view.playing { now.duration_since(probed).as_millis() as u64 } else { 0 };
                (view.position_ms + extra) / 1000
            });
            clock.set_position(position);
            earliest(clock.tick(now));
        }
        if let (Some(dev), Some(g)) = (input.as_ref(), gesture.as_ref()) {
            let deadline = g.deadline();
            if deadline != 0 {
                let left = (deadline - now_us(dev.monotonic)).max(0) as u64;
                earliest(Some(now + Duration::from_micros(left)));
            }
        }

        let wait = wake.saturating_duration_since(Instant::now()).min(PROBE);
        let ms = wait.as_millis().max(1) as i32;
        match input.as_ref() {
            Some(dev) => dev.wait(ms),
            None => thread::sleep(Duration::from_millis(ms as u64)),
        }
    }
    if let Some(mut dev) = input {
        let _ = dev.grab(false);
    }
    if let Some(mut clock) = clock {
        clock.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(volume: u8, paused: bool) -> View {
        View { active: true, paused, volume, buttons: true, ..View::default() }
    }

    #[test]
    fn gestures_become_commands_and_feedback() {
        assert_eq!(react(Action::Next, &view(50, false)).intent, Some(Intent::Next));
        assert_eq!(react(Action::Previous, &view(50, false)).ring, Some(Effect::Previous));
        let toggle = react(Action::Toggle, &view(50, false));
        assert_eq!((toggle.intent, toggle.ring), (Some(Intent::Toggle), Some(Effect::Toggle { paused: true })));
        assert!(react(Action::Listen, &view(50, false)).listen);
    }

    #[test]
    fn volume_moves_on_a_grid_and_stays_in_range() {
        assert_eq!(react(Action::Louder, &view(47, false)).volume, Some(50));
        assert_eq!(react(Action::Louder, &view(50, false)).volume, Some(55));
        assert_eq!(react(Action::Quieter, &view(3, false)).volume, Some(0));
        assert_eq!(react(Action::Louder, &view(98, false)).volume, Some(100));
        assert_eq!(react(Action::Quieter, &view(0, false)).intent, Some(Intent::Volume(0)));
    }

    #[test]
    fn settings_come_from_the_spotiya_profile_format() {
        let json = serde_json::json!({
            "driver": "mini2",
            "input_device": "/dev/input/event0",
            "keys": { "alice": 582, "volume_up": 115, "volume_down": 114 },
            "ring_prefix": "/sys/class/leds/mini2:",
            "display_prefix": "/sys/class/leds/mini2:white:clock"
        });
        let s = Settings::from_profile(Some(&json)).unwrap();
        assert_eq!(s.keys.unwrap().alice, 582);
        assert!(Settings::from_profile(Some(&serde_json::json!({"driver": "other"}))).is_none());
        assert!(Settings::from_profile(None).is_none());
    }
}
