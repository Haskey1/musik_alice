//! Жесты сенсорных кнопок Мини 2.
//!
//! Перенесено из Spotiya (`controls/mini2/buttons.c`, MIT): те же пороги и те же записанные с настоящей колонки
//! последовательности в тестах. Добавлено долгое нажатие кнопки Алисы — вызвать Алису, пока musik держит кнопки.
//!
//! | Жест | Действие |
//! |------|----------|
//! | короткое касание «Алисы» | пауза / продолжить |
//! | «Алиса», затем «+» (вместе или в пределах 250 мс) | следующий трек |
//! | «Алиса», затем «−» | предыдущий трек |
//! | «+» / «−» (удержание повторяет) | громкость |
//! | удержание «Алисы» 0,8 с | Алиса слушает |

/// Время на вторую кнопку после отпускания «Алисы», мкс.
pub const GESTURE_US: i64 = 250_000;
const REPEAT_DELAY_US: i64 = 450_000;
pub const REPEAT_US: i64 = 180_000;
/// Удержание «Алисы», после которого вызываем Алису, мкс.
pub const LISTEN_US: i64 = 800_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Toggle,
    Next,
    Previous,
    Louder,
    Quieter,
    Listen,
}

/// Коды клавиш из профиля станции.
#[derive(Clone, Copy, Debug)]
pub struct Keys {
    pub alice: u16,
    pub volume_up: u16,
    pub volume_down: u16,
}

#[derive(Default, Clone, Copy)]
struct Key {
    down: bool,
    suppressed: bool,
    since: i64,
    repeat_at: i64,
}

/// Состояние распознавания. Время — микросекунды монотонных часов событий.
pub struct Gesture {
    keys: Keys,
    /// 0 — «−», 1 — «+», 2 — «Алиса».
    state: [Key; 3],
    armed: bool,
    consumed: bool,
    pending: i64,
    out: Vec<Action>,
}

impl Gesture {
    pub fn new(keys: Keys) -> Self {
        Self { keys, state: [Key::default(); 3], armed: false, consumed: false, pending: 0, out: Vec::new() }
    }

    pub fn reset(&mut self) {
        *self = Self::new(self.keys);
    }

    /// Накопленные действия.
    pub fn take(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.out)
    }

    fn index(&self, code: u16) -> Option<usize> {
        if code == self.keys.volume_down {
            Some(0)
        } else if code == self.keys.volume_up {
            Some(1)
        } else if code == self.keys.alice {
            Some(2)
        } else {
            None
        }
    }

    fn emit(&mut self, action: Action) {
        if self.out.len() < 16 {
            self.out.push(action);
        }
    }

    pub fn tick(&mut self, now: i64) {
        if self.pending != 0 && now >= self.pending {
            self.emit(Action::Toggle);
            self.pending = 0;
            self.armed = false;
        }
        let alice = self.state[2];
        if alice.down && self.armed && !self.consumed && now >= alice.since + LISTEN_US {
            self.consumed = true;
            self.emit(Action::Listen);
        }
        for i in 0..2 {
            let k = self.state[i];
            if k.down && !k.suppressed && now >= k.repeat_at {
                self.emit(if i == 1 { Action::Louder } else { Action::Quieter });
                self.state[i].repeat_at = now + REPEAT_US;
            }
        }
    }

    /// Ближайший момент, когда `tick` что-то сделает; 0 — ждать только событий.
    pub fn deadline(&self) -> i64 {
        let mut next = self.pending;
        let mut take = |at: i64| {
            if next == 0 || at < next {
                next = at;
            }
        };
        for k in &self.state[..2] {
            if k.down && !k.suppressed {
                take(k.repeat_at);
            }
        }
        let alice = self.state[2];
        if alice.down && self.armed && !self.consumed {
            take(alice.since + LISTEN_US);
        }
        next
    }

    /// Событие клавиши: `value` 1 — нажата, 0 — отпущена, 2 — автоповтор (игнорируется).
    pub fn key(&mut self, code: u16, value: i32, now: i64) {
        self.tick(now);
        let Some(i) = self.index(code) else { return };
        if value == 2 {
            return;
        }
        if value == 0 {
            if !self.state[i].down {
                return;
            }
            self.state[i] = Key::default();
            if i == 2 && self.armed && !self.consumed {
                self.pending = now + GESTURE_US;
            }
            if self.state.iter().all(|k| !k.down) && self.consumed {
                self.armed = false;
                self.consumed = false;
            }
            return;
        }
        if value != 1 || self.state[i].down {
            return;
        }
        self.state[i].down = true;
        self.state[i].since = now;
        if i == 2 {
            if self.pending != 0 {
                self.emit(Action::Toggle);
            }
            self.pending = 0;
            self.armed = true;
            self.consumed = false;
            self.state[i].suppressed = true;
        } else if self.armed && (self.state[2].down || self.pending != 0) {
            self.pending = 0;
            self.armed = false;
            self.consumed = true;
            self.state[i].suppressed = true;
            self.emit(if i == 1 { Action::Next } else { Action::Previous });
        } else if self.state[2].down && self.consumed {
            self.state[i].suppressed = true;
        } else {
            self.state[i].repeat_at = now + REPEAT_DELAY_US;
            self.emit(if i == 1 { Action::Louder } else { Action::Quieter });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: Keys = Keys { alice: 582, volume_up: 115, volume_down: 114 };

    fn replay(events: &[(i64, u16, i32)]) -> Vec<Action> {
        let mut g = Gesture::new(KEYS);
        for &(at, code, value) in events {
            g.key(code, value, at);
        }
        g.tick(events.last().unwrap().0 + 1_000_000);
        g.take()
    }

    #[test]
    fn recorded_mini2_traces() {
        // Настоящие записи с Мини 2: датчики перекрываются, отпускание раньше второго касания, зазор 26 мс.
        assert_eq!(replay(&[(240317, 582, 1), (473913, 115, 1), (473964, 582, 0), (525313, 115, 0)]), [Action::Next]);
        assert_eq!(
            replay(&[(146527, 582, 1), (378154, 582, 0), (378208, 114, 1), (429659, 114, 0)]),
            [Action::Previous]
        );
        assert_eq!(replay(&[(691053, 582, 1), (922581, 582, 0), (948322, 115, 1), (973993, 115, 0)]), [Action::Next]);
        assert_eq!(replay(&[(346055, 582, 1), (423140, 582, 0)]), [Action::Toggle]);
    }

    #[test]
    fn tap_waits_for_a_possible_second_key() {
        let mut g = Gesture::new(KEYS);
        g.key(582, 1, 1000);
        g.key(582, 0, 2000);
        assert_eq!(g.deadline(), 2000 + GESTURE_US);
    }

    #[test]
    fn separate_taps_stay_separate() {
        let mut g = Gesture::new(KEYS);
        g.key(582, 1, 192947);
        g.key(582, 0, 321617);
        g.key(114, 1, 1117756);
        g.key(114, 0, 1220380);
        assert_eq!(g.take(), [Action::Toggle, Action::Quieter]);
    }

    #[test]
    fn holding_volume_repeats() {
        let mut g = Gesture::new(KEYS);
        assert_eq!(g.deadline(), 0);
        g.key(115, 1, 1000);
        assert_eq!(g.deadline(), 451000);
        g.tick(450999);
        assert_eq!(g.take(), [Action::Louder]);
        g.tick(451000);
        assert_eq!(g.take(), [Action::Louder]);
        assert_eq!(g.deadline(), 451000 + REPEAT_US);
        g.key(115, 0, 452000);
        assert_eq!(g.deadline(), 0);
        g.tick(999000);
        assert!(g.take().is_empty());
    }

    #[test]
    fn long_alice_press_calls_alice_instead_of_pausing() {
        let mut g = Gesture::new(KEYS);
        g.key(582, 1, 1000);
        assert_eq!(g.deadline(), 1000 + LISTEN_US);
        g.tick(1000 + LISTEN_US);
        assert_eq!(g.take(), [Action::Listen]);
        g.key(582, 0, 2_000_000);
        g.tick(3_000_000);
        assert!(g.take().is_empty(), "отпускание после вызова Алисы не ставит паузу");
        // Следующее касание снова работает как обычно.
        g.key(582, 1, 4_000_000);
        g.key(582, 0, 4_100_000);
        g.tick(5_000_000);
        assert_eq!(g.take(), [Action::Toggle]);
    }

    #[test]
    fn other_keys_and_remapping() {
        let mut g = Gesture::new(KEYS);
        g.key(248, 1, 1000);
        g.key(248, 0, 2000);
        g.tick(3_000_000);
        assert!(g.take().is_empty(), "кнопка микрофона не наша");
        let mut g = Gesture::new(Keys { alice: 200, volume_up: 201, volume_down: 202 });
        for (at, code, value) in [(1000, 200, 1), (2000, 201, 1), (3000, 200, 0), (4000, 201, 0)] {
            g.key(code, value, at);
        }
        g.tick(2_000_000);
        assert_eq!(g.take(), [Action::Next]);
    }
}
