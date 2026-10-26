//! Профили устройств: ALSA-устройство и микшер, адрес локального IPC Алисы.
//! Формат файлов совместим с профилями spotiya (`audio`, `voice`); блок `controls` читается, но
//! игнорируется: кнопки, световое кольцо и дисплей в этой версии не поддерживаются.

use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read},
    net::SocketAddr,
    path::{Path, PathBuf},
};

const BUILTIN: &[(&str, &str)] = &[
    ("mini1", include_str!("../profiles/mini1.json")),
    ("mini2", include_str!("../profiles/mini2.json")),
    ("generic", include_str!("../profiles/generic.json")),
];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Audio {
    pub device: String,
    #[serde(default = "default_format")]
    pub format: String,
    pub mixer_device: String,
    pub mixer_control: String,
    #[serde(default)]
    pub mixer_index: u32,
    #[serde(default = "default_channel")]
    pub mixer_channel: String,
    /// Частота вывода, Гц. Источники с другой частотой передискретизируются.
    #[serde(default = "default_rate")]
    pub rate: u32,
}
fn default_format() -> String {
    "S16".into()
}
fn default_channel() -> String {
    "front_left".into()
}
fn default_rate() -> u32 {
    44_100
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Voice {
    pub protocol: String,
    pub address: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub version: u32,
    pub id: String,
    pub name: String,
    pub audio: Audio,
    #[serde(default)]
    pub voice: Option<Voice>,
    #[serde(default)]
    pub controls: Option<serde_json::Value>,
}

impl Profile {
    pub fn builtin(id: &str) -> Option<Profile> {
        BUILTIN.iter().find(|(name, _)| *name == id).and_then(|(_, json)| serde_json::from_str(json).ok())
    }

    pub fn builtin_ids() -> Vec<&'static str> {
        BUILTIN.iter().map(|(id, _)| *id).collect()
    }

    pub fn from_file(path: &Path) -> io::Result<Profile> {
        let mut text = String::new();
        fs::File::open(path)?.take(256 * 1024).read_to_string(&mut text)?;
        let profile: Profile = serde_json::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))?;
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> io::Result<()> {
        let bad = |msg: &str| Err(io::Error::new(io::ErrorKind::InvalidData, format!("профиль {}: {msg}", self.id)));
        if self.version != 1 {
            return bad("поддерживается только version 1");
        }
        if self.id.is_empty() || self.id.contains(['/', '\\']) {
            return bad("некорректный id");
        }
        if self.audio.format != "S16" {
            return bad("поддерживается только формат S16");
        }
        if !(8_000..=192_000).contains(&self.audio.rate) {
            return bad("rate вне диапазона 8000–192000");
        }
        if let Some(voice) = &self.voice {
            if voice.protocol != "quasar" {
                return bad("поддерживается только протокол quasar");
            }
            if voice.address.parse::<SocketAddr>().map(|a| !a.ip().is_loopback()).unwrap_or(true) {
                return bad("адрес голоса должен быть локальным host:port (loopback)");
            }
        }
        Ok(())
    }

    pub fn voice_address(&self) -> Option<SocketAddr> {
        self.voice.as_ref().and_then(|v| v.address.parse().ok())
    }
}

/// Путь к сохранённому выбору профиля.
pub fn selection_path(state_dir: &Path) -> PathBuf {
    state_dir.join("profile.txt")
}

/// Читает сохранённый выбор: `auto`, id встроенного профиля или путь к JSON.
pub fn selection(state_dir: &Path) -> String {
    fs::read_to_string(selection_path(state_dir))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "auto".into())
}

pub fn save_selection(state_dir: &Path, value: &str) -> io::Result<()> {
    crate::util::write_atomic(&selection_path(state_dir), format!("{value}\n").as_bytes(), 0o600)
}

/// Определяет модель по файловой системе станции (`root` = `/`, в тестах — каталог-макет).
pub fn detect(root: &Path) -> &'static str {
    let rooted = |p: &str| root.join(p.trim_start_matches('/'));
    if let Ok(file) = fs::File::open(rooted("/system/vendor/quasar/quasar.cfg")) {
        if let Ok(config) = serde_json::from_reader::<_, serde_json::Value>(file.take(256 * 1024)) {
            match config.pointer("/common/deviceType").and_then(|v| v.as_str()) {
                Some("yandexmini") => return "mini1",
                Some("yandexmini_2") => return "mini2",
                _ => {}
            }
        }
    }
    let compatible = fs::read(rooted("/proc/device-tree/compatible")).unwrap_or_default();
    let model = fs::read(rooted("/proc/device-tree/model")).unwrap_or_default();
    if compatible.split(|b| *b == 0).any(|s| s == b"yandex,station-mini2")
        || model.split(|b| *b == 0).next() == Some(b"Yandex Station Mini 2".as_slice())
    {
        "mini2"
    } else {
        "generic"
    }
}

/// Загружает профиль по выбору: `auto` определяет модель, остальное — id или путь к файлу.
/// Пользовательские профили лежат в `<base>/profiles/`; встроенные id в приоритете.
pub fn resolve(choice: &str, root: &Path, base: &Path) -> io::Result<Profile> {
    let id = if choice == "auto" { detect(root) } else { choice };
    if let Some(p) = Profile::builtin(id) {
        return Ok(p);
    }
    let candidate = if id.contains('/') { PathBuf::from(id) } else { base.join("profiles").join(format!("{id}.json")) };
    Profile::from_file(&candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_profiles_are_valid() {
        for id in Profile::builtin_ids() {
            let p = Profile::builtin(id).unwrap_or_else(|| panic!("{id} не разобран"));
            p.validate().unwrap();
            assert_eq!(p.id, id);
        }
        assert!(Profile::builtin("generic").unwrap().voice.is_none());
        assert_eq!(Profile::builtin("mini2").unwrap().voice_address(), Some("127.0.0.1:9875".parse().unwrap()));
    }

    #[test]
    fn unknown_fields_and_bad_values_are_rejected() {
        let json = r#"{"version":1,"id":"x","name":"X","audio":{"device":"d","mixer_device":"d","mixer_control":"PCM","oops":1}}"#;
        assert!(serde_json::from_str::<Profile>(json).is_err());
        let mut p = Profile::builtin("mini2").unwrap();
        p.voice = Some(Voice { protocol: "quasar".into(), address: "8.8.8.8:9875".into() });
        assert!(p.validate().is_err());
        p.voice = Some(Voice { protocol: "other".into(), address: "127.0.0.1:1".into() });
        assert!(p.validate().is_err());
        let mut p = Profile::builtin("mini2").unwrap();
        p.audio.format = "S32".into();
        assert!(p.validate().is_err());
    }

    fn fake_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("musikalice-prof-{name}-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn detects_models_from_firmware_files() {
        let root = fake_root("a");
        assert_eq!(detect(&root), "generic");
        fs::create_dir_all(root.join("system/vendor/quasar")).unwrap();
        fs::write(root.join("system/vendor/quasar/quasar.cfg"), r#"{"common":{"deviceType":"yandexmini_2"}}"#).unwrap();
        assert_eq!(detect(&root), "mini2");
        fs::write(root.join("system/vendor/quasar/quasar.cfg"), r#"{"common":{"deviceType":"yandexmini"}}"#).unwrap();
        assert_eq!(detect(&root), "mini1");
        fs::remove_dir_all(&root).ok();

        let root = fake_root("b");
        fs::create_dir_all(root.join("proc/device-tree")).unwrap();
        fs::write(root.join("proc/device-tree/model"), b"Yandex Station Mini 2\0").unwrap();
        assert_eq!(detect(&root), "mini2");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolves_builtin_auto_and_custom_files() {
        let root = fake_root("c");
        let base = root.join("base");
        fs::create_dir_all(base.join("profiles")).unwrap();
        assert_eq!(resolve("auto", &root, &base).unwrap().id, "generic");
        assert_eq!(resolve("mini1", &root, &base).unwrap().id, "mini1");
        let custom = r#"{"version":1,"id":"mine","name":"Mine","audio":{"device":"plughw:0,0","mixer_device":"hw:0","mixer_control":"PCM"}}"#;
        fs::write(base.join("profiles/mine.json"), custom).unwrap();
        let p = resolve("mine", &root, &base).unwrap();
        assert_eq!(p.audio.device, "plughw:0,0");
        assert_eq!(p.audio.rate, 44_100);
        assert!(resolve("missing", &root, &base).is_err());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn selection_defaults_to_auto_and_persists() {
        let dir = fake_root("d");
        assert_eq!(selection(&dir), "auto");
        save_selection(&dir, "mini2").unwrap();
        assert_eq!(selection(&dir), "mini2");
        fs::remove_dir_all(&dir).ok();
    }
}
