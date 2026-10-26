//! Настройки клиента: адрес сервера musik, токен устройства, качество потока, поведение голоса.
//! Хранятся в `<state-dir>/config.json` с правами 0600 (внутри токен).

use crate::announce::Announce;
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub const BASE: &str = "/data/opt/musik-alice";
pub const CONTROL: &str = "/run/musikalice.control";

/// Какой поток запрашивать у сервера (`/api/stream/{id}?q=…`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    /// Сервер перекодирует в MP3: работает всегда и не требует перемотки в контейнере.
    Mp3,
    Aac,
    /// Мобильный профиль сервера.
    Mobile,
    /// Оригинальный файл без перекодирования (mp3/flac/ogg; для m4a нужен moov в начале файла).
    Original,
}
impl Quality {
    pub fn query(self) -> Option<&'static str> {
        match self {
            Quality::Mp3 => Some("mp3"),
            Quality::Aac => Some("aac"),
            Quality::Mobile => Some("mobile"),
            Quality::Original => None,
        }
    }
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_lowercase().as_str() {
            "mp3" => Some(Quality::Mp3),
            "aac" => Some(Quality::Aac),
            "mobile" => Some(Quality::Mobile),
            "original" | "orig" => Some(Quality::Original),
            _ => None,
        }
    }
}

/// Что делать на голосовую просьбу «включи <трек>».
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TrackMode {
    /// Радио от выбранного трека: он играет первым, дальше подборка сервера по вкусу.
    Radio,
    /// Только этот трек, затем тишина.
    Single,
}
impl TrackMode {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_lowercase().as_str() {
            "radio" => Some(TrackMode::Radio),
            "single" => Some(TrackMode::Single),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    /// Базовый адрес, например `http://192.168.3.141:8787` или `https://music.example.org`.
    pub server: String,
    /// Bearer-токен устройства (выпускается `musikalice login` или в веб-интерфейсе).
    pub token: String,
    /// Как колонка называется в списке устройств сервера.
    pub device_name: String,
    pub quality: Quality,
    /// Перехватывать музыкальные фразы Алисы.
    pub voice: bool,
    /// «Включи музыку» без активной сессии запускает радио (иначе фразу обработает сама Алиса).
    pub voice_radio_fallback: bool,
    pub track_mode: TrackMode,
    /// Перемешивать список при запуске артиста или альбома по голосу.
    pub shuffle_artist: bool,
    /// Доля громкости музыки, пока Алиса слушает или отвечает (0.05–1.0).
    pub duck_level: f32,
    /// Желаемая общая громкость 0–100; восстанавливается после диалога с Алисой.
    pub volume: Option<u8>,
    /// Ставить штатный плеер Алисы на паузу, пока играет musik.
    pub native_pause: bool,
    /// Порог похожести названия при голосовом поиске (0.5–1.0).
    pub search_threshold: f32,
    /// Звуковые сигналы вместо голосовых ответов: «не нашёл», «лайк».
    pub earcons: bool,
    /// Алиса называет трек: `off`, `voice` (после голосовой команды) или `always` (каждый трек).
    pub announce: Announce,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            server: String::new(),
            token: String::new(),
            device_name: String::new(),
            quality: Quality::Mp3,
            voice: true,
            voice_radio_fallback: true,
            track_mode: TrackMode::Radio,
            shuffle_artist: true,
            duck_level: 0.2,
            volume: None,
            native_pause: true,
            search_threshold: 0.72,
            earcons: true,
            announce: Announce::Voice,
        }
    }
}

impl Config {
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("config.json")
    }

    /// Загружает конфиг; отсутствие файла — это пустые настройки по умолчанию.
    pub fn load(state_dir: &Path) -> io::Result<Self> {
        match fs::read(Self::path(state_dir)) {
            Ok(bytes) => {
                let mut cfg: Config = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("config.json: {e}")))?;
                cfg.normalize();
                Ok(cfg)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, state_dir: &Path) -> io::Result<()> {
        let mut data = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        data.push(b'\n');
        crate::util::write_atomic(&Self::path(state_dir), &data, 0o600)
    }

    /// Приводит значения к допустимым границам и чистит адрес сервера.
    pub fn normalize(&mut self) {
        self.server = normalize_server(&self.server);
        self.token = self.token.trim().to_owned();
        self.duck_level = if self.duck_level.is_finite() { self.duck_level.clamp(0.05, 1.0) } else { 0.2 };
        self.search_threshold =
            if self.search_threshold.is_finite() { self.search_threshold.clamp(0.5, 1.0) } else { 0.72 };
        if let Some(v) = self.volume {
            self.volume = Some(v.min(100));
        }
    }

    pub fn configured(&self) -> bool {
        !self.server.is_empty() && !self.token.is_empty()
    }

    /// Полный URL потока для трека: относительный `stream` сервера или `/api/stream/{id}`.
    pub fn stream_url(&self, track_id: i64, stream: Option<&str>) -> String {
        let path = match stream {
            Some(s) if s.starts_with('/') => s.to_owned(),
            Some(s) if s.starts_with("http://") || s.starts_with("https://") => return with_quality(s, self.quality),
            _ => format!("/api/stream/{track_id}"),
        };
        with_quality(&format!("{}{}", self.server, path), self.quality)
    }
}

fn with_quality(url: &str, quality: Quality) -> String {
    match quality.query() {
        None => url.to_owned(),
        Some(q) if url.contains('?') => format!("{url}&q={q}"),
        Some(q) => format!("{url}?q={q}"),
    }
}

/// `192.168.3.141:8787` → `http://192.168.3.141:8787`; хвостовые слэши убираются.
pub fn normalize_server(raw: &str) -> String {
    let raw = raw.trim().trim_end_matches('/');
    if raw.is_empty() {
        return String::new();
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_owned()
    } else {
        format!("http://{raw}")
    }
}

/// Значение для `config set KEY VALUE`; возвращает ошибку с понятным текстом.
pub fn set_key(cfg: &mut Config, key: &str, value: &str) -> Result<(), String> {
    let bool_of = |v: &str| match v.trim().to_lowercase().as_str() {
        "1" | "true" | "on" | "yes" | "да" | "вкл" => Ok(true),
        "0" | "false" | "off" | "no" | "нет" | "выкл" => Ok(false),
        _ => Err(format!("ожидалось on/off, получено «{v}»")),
    };
    match key {
        "server" => cfg.server = normalize_server(value),
        "token" => cfg.token = value.trim().to_owned(),
        "device_name" => cfg.device_name = value.trim().to_owned(),
        "quality" => cfg.quality = Quality::parse(value).ok_or("quality: mp3, aac, mobile или original")?,
        "voice" => cfg.voice = bool_of(value)?,
        "voice_radio_fallback" => cfg.voice_radio_fallback = bool_of(value)?,
        "track_mode" => cfg.track_mode = TrackMode::parse(value).ok_or("track_mode: radio или single")?,
        "shuffle_artist" => cfg.shuffle_artist = bool_of(value)?,
        "native_pause" => cfg.native_pause = bool_of(value)?,
        "earcons" => cfg.earcons = bool_of(value)?,
        "announce" => cfg.announce = Announce::parse(value).ok_or("announce: off, voice или always")?,
        "duck_level" => cfg.duck_level = value.trim().parse().map_err(|_| "duck_level: число от 0.05 до 1.0")?,
        "search_threshold" => {
            cfg.search_threshold = value.trim().parse().map_err(|_| "search_threshold: число от 0.5 до 1.0")?
        }
        "volume" => {
            cfg.volume = if value.trim() == "auto" {
                None
            } else {
                Some(value.trim().parse::<u8>().map_err(|_| "volume: 0–100 или auto")?)
            }
        }
        _ => return Err(format!("неизвестный параметр «{key}»")),
    }
    cfg.normalize();
    Ok(())
}

/// Конфиг для вывода пользователю: токен скрыт.
pub fn redacted_json(cfg: &Config) -> String {
    let mut shown = cfg.clone();
    if !shown.token.is_empty() {
        let tail: String = shown.token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
        shown.token = format!("…{tail}");
    }

    serde_json::to_string_pretty(&shown).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_is_normalized() {
        assert_eq!(normalize_server(" 192.168.3.141:8787/ "), "http://192.168.3.141:8787");
        assert_eq!(normalize_server("https://music.example.org//"), "https://music.example.org");
        assert_eq!(normalize_server(""), "");
    }

    #[test]
    fn stream_urls_follow_quality() {
        let mut cfg = Config { server: "http://h:8787".into(), ..Config::default() };
        assert_eq!(cfg.stream_url(7, Some("/api/stream/7")), "http://h:8787/api/stream/7?q=mp3");
        assert_eq!(cfg.stream_url(7, None), "http://h:8787/api/stream/7?q=mp3");
        cfg.quality = Quality::Original;
        assert_eq!(cfg.stream_url(7, None), "http://h:8787/api/stream/7");
        cfg.quality = Quality::Aac;
        assert_eq!(cfg.stream_url(7, Some("/api/stream/7?x=1")), "http://h:8787/api/stream/7?x=1&q=aac");
    }

    #[test]
    fn save_and_load_round_trip_with_private_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("musikalice-cfg-{}", std::process::id()));
        let cfg = Config { server: "http://h:1".into(), token: "secret".into(), volume: Some(40), ..Config::default() };
        cfg.save(&dir).unwrap();
        assert_eq!(Config::load(&dir).unwrap(), cfg);
        let mode = fs::metadata(Config::path(&dir)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_gives_defaults_and_bad_json_is_an_error() {
        let dir = std::env::temp_dir().join(format!("musikalice-cfg2-{}", std::process::id()));
        assert!(!Config::load(&dir).unwrap().configured());
        fs::create_dir_all(&dir).unwrap();
        fs::write(Config::path(&dir), b"{not json").unwrap();
        assert!(Config::load(&dir).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_key_validates_values() {
        let mut cfg = Config::default();
        set_key(&mut cfg, "server", "10.0.0.2:8787").unwrap();
        assert_eq!(cfg.server, "http://10.0.0.2:8787");
        set_key(&mut cfg, "quality", "original").unwrap();
        assert_eq!(cfg.quality, Quality::Original);
        set_key(&mut cfg, "voice", "off").unwrap();
        assert!(!cfg.voice);
        set_key(&mut cfg, "duck_level", "5").unwrap();
        assert_eq!(cfg.duck_level, 1.0);
        set_key(&mut cfg, "volume", "35").unwrap();
        assert_eq!(cfg.volume, Some(35));
        set_key(&mut cfg, "volume", "auto").unwrap();
        assert_eq!(cfg.volume, None);
        assert!(set_key(&mut cfg, "quality", "flac").is_err());
        assert!(set_key(&mut cfg, "voice", "maybe").is_err());
        assert!(set_key(&mut cfg, "nope", "1").is_err());
    }

    #[test]
    fn token_is_hidden_in_output() {
        let cfg = Config { token: "abcdefgh1234".into(), ..Config::default() };
        let shown = redacted_json(&cfg);
        assert!(shown.contains("…1234"));
        assert!(!shown.contains("abcdefgh"));
    }
}
