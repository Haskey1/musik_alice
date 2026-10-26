//! Охранник громкости.
//!
//! Во время диалога штатная система Алисы временно меняет общую громкость станции (Master), чтобы
//! речь звучала нужным образом. Если после диалога оставить так как есть, музыка станет громче
//! или тише, чем выбрал пользователь. Охранник через 750 мс после конца диалога возвращает
//! выбранный уровень. Исключение — фраза «громче/тише/громкость…»: тогда новый уровень, который
//! выставила Алиса, принимается как выбор пользователя.

const SETTLE_MS: u64 = 750;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardAction {
    /// Вернуть выбранную громкость, если Master от неё отличается.
    Restore,
    /// Прочитать Master и считать его новым выбором пользователя.
    Adopt,
}

#[derive(Default)]
pub struct VolumeGuard {
    enabled: bool,
    busy: bool,
    holding: bool,
    native_command: bool,
    release_at: Option<u64>,
}

impl VolumeGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Охранник работает, пока у musik есть активная сессия.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.holding = false;
            self.release_at = None;
        } else if self.busy {
            self.holding = true;
        }
    }

    /// Изменение активности Алисы. `now_ms` — любые монотонные миллисекунды.
    pub fn activity(&mut self, busy: bool, now_ms: u64) {
        let previous = std::mem::replace(&mut self.busy, busy);
        if busy {
            if !previous {
                self.native_command = false;
            }
            if self.enabled {
                self.holding = true;
            }
            self.release_at = None;
        } else if previous && self.holding {
            self.release_at = Some(now_ms + SETTLE_MS);
        }
    }

    /// Фраза была штатной командой громкости.
    pub fn native_command(&mut self) {
        self.native_command = true;
    }

    /// Пользователь сам поменял громкость (кнопка, команда, TUI): ничего откатывать не нужно.
    pub fn user_set(&mut self) {
        self.native_command = false;
    }

    #[cfg(test)]
    pub fn holding(&self) -> bool {
        self.holding
    }

    /// Вызывается периодически; возвращает действие, когда пришло время.
    pub fn poll(&mut self, now_ms: u64) -> Option<GuardAction> {
        let at = self.release_at?;
        if now_ms < at {
            return None;
        }
        self.release_at = None;
        self.holding = false;
        Some(if std::mem::take(&mut self.native_command) { GuardAction::Adopt } else { GuardAction::Restore })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_after_the_dialog_settles() {
        let mut guard = VolumeGuard::new();
        guard.set_enabled(true);
        guard.activity(true, 0);
        assert!(guard.holding());
        assert_eq!(guard.poll(10_000), None, "пока диалог идёт, ничего не трогаем");
        guard.activity(false, 5_000);
        assert_eq!(guard.poll(5_700), None);
        assert_eq!(guard.poll(5_750), Some(GuardAction::Restore));
        assert_eq!(guard.poll(9_000), None, "действие выдаётся один раз");
        assert!(!guard.holding());
    }

    #[test]
    fn native_volume_phrase_adopts_the_new_level() {
        let mut guard = VolumeGuard::new();
        guard.set_enabled(true);
        guard.activity(true, 0);
        guard.native_command();
        guard.activity(false, 1_000);
        assert_eq!(guard.poll(1_750), Some(GuardAction::Adopt));
        guard.activity(true, 3_000);
        guard.activity(false, 4_000);
        assert_eq!(guard.poll(4_750), Some(GuardAction::Restore), "флаг сбрасывается с новым диалогом");
    }

    #[test]
    fn a_new_dialog_cancels_a_pending_restore() {
        let mut guard = VolumeGuard::new();
        guard.set_enabled(true);
        guard.activity(true, 0);
        guard.activity(false, 100);
        guard.activity(true, 300);
        assert_eq!(guard.poll(2_000), None);
        guard.activity(false, 2_100);
        assert_eq!(guard.poll(2_850), Some(GuardAction::Restore));
    }

    #[test]
    fn disabled_guard_never_acts() {
        let mut guard = VolumeGuard::new();
        guard.activity(true, 0);
        guard.activity(false, 100);
        assert_eq!(guard.poll(10_000), None);
        guard.set_enabled(true);
        guard.activity(true, 20_000);
        guard.set_enabled(false);
        guard.activity(false, 21_000);
        assert_eq!(guard.poll(30_000), None);
    }

    #[test]
    fn enabling_during_a_dialog_starts_holding() {
        let mut guard = VolumeGuard::new();
        guard.activity(true, 0);
        assert!(!guard.holding());
        guard.set_enabled(true);
        assert!(guard.holding());
        guard.activity(false, 500);
        assert_eq!(guard.poll(1_250), Some(GuardAction::Restore));
    }
}
