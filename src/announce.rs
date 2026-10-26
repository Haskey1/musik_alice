//! Голосовое объявление трека голосом самой Алисы.
//!
//! У прошивки есть штатная консоль `maind --cli`; её команда `q` (Text Input Query) отправляет Алисе текстовый
//! запрос, как будто его произнесли. На запрос «повтори за мной …» Алиса читает фразу своим голосом. Всё
//! происходит на колонке: ни Home Assistant, ни токенов, ни внешних сервисов. Нужен только интернет, без которого
//! Алиса не работает в любом случае.

use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::api::Track;

/// Префикс запроса. По нему же движок узнаёт собственное эхо и не принимает его за команду.
pub const PREFIX: &str = "повтори за мной";

const MAIND: &str = "/system/vendor/quasar/maind";
const LIBS: &str = "/system/vendor/quasar/libs";
/// Сколько ждать, пока консоль подключится к службам Алисы, прежде чем печатать запрос.
const CONNECT_WAIT: Duration = Duration::from_millis(800);
/// Сколько держать консоль открытой после запроса, чтобы он точно ушёл.
const SEND_WAIT: Duration = Duration::from_millis(1500);
const EXIT_WAIT: Duration = Duration::from_secs(3);
const MAX_CHARS: usize = 120;

/// Когда объявлять.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Announce {
    /// Никогда.
    Off,
    /// Когда музыку запустили голосом (по умолчанию).
    Voice,
    /// Каждый новый трек.
    Always,
}

impl Announce {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "off" | "no" | "нет" | "выкл" => Some(Self::Off),
            "voice" | "голос" => Some(Self::Voice),
            "always" | "все" | "всегда" => Some(Self::Always),
            _ => None,
        }
    }
}

/// Фраза для колонки: «Radiohead, Creep». Служебные скобки вроде «(Remastered 2009)» Алиса читала бы зря.
pub fn phrase(track: &Track) -> String {
    let title = strip_notes(&track.title);
    let text = match (clean(&track.artist).as_str(), title.as_str()) {
        ("", t) => t.to_owned(),
        (a, "") => a.to_owned(),
        (a, t) => format!("{a}, {t}"),
    };
    text.chars().take(MAX_CHARS).collect()
}

/// Одна строка без управляющих символов: консоль читает запрос построчно.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_notes(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut depth = 0u32;
    for c in title.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let out = clean(&out);
    if out.is_empty() {
        clean(title)
    } else {
        out
    }
}

/// Наш ли это запрос: Алиса присылает его текст в событиях так же, как распознанную речь.
pub fn is_echo(normalized: &str) -> bool {
    normalized.starts_with(PREFIX)
}

/// Путь к консоли прошивки; `MUSIKALICE_MAIND` подменяет его (тесты, нестандартные прошивки).
fn program() -> PathBuf {
    std::env::var_os("MUSIKALICE_MAIND").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(MAIND))
}

/// Есть ли на этой колонке консоль Алисы.
pub fn available() -> bool {
    program().is_file()
}

/// Просит Алису произнести `text`. Блокирует поток на пару секунд: вызывайте в отдельном потоке.
pub fn speak(text: &str) -> Result<(), String> {
    let text = clean(text);
    if text.is_empty() {
        return Ok(());
    }
    let program = program();
    let mut child = Command::new(&program)
        .arg("--cli")
        .env("LD_LIBRARY_PATH", LIBS)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("не запустить {}: {e}", program.display()))?;
    let mut stdin = child.stdin.take().ok_or("нет stdin у консоли")?;
    thread::sleep(CONNECT_WAIT);
    let sent = stdin.write_all(format!("q\n{PREFIX} {text}\n").as_bytes()).and_then(|_| stdin.flush());
    thread::sleep(SEND_WAIT);
    let _ = stdin.write_all(b"x\n");
    drop(stdin);
    let deadline = Instant::now() + EXIT_WAIT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    sent.map_err(|e| format!("консоль не приняла запрос: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(artist: &str, title: &str) -> Track {
        serde_json::from_value(serde_json::json!({"id": 1, "artist": artist, "title": title})).unwrap()
    }

    #[test]
    fn phrase_joins_artist_and_title() {
        assert_eq!(phrase(&track("Radiohead", "Creep")), "Radiohead, Creep");
        assert_eq!(phrase(&track("", "Creep")), "Creep");
        assert_eq!(phrase(&track("Radiohead", "")), "Radiohead");
    }

    #[test]
    fn phrase_is_one_clean_line() {
        assert_eq!(phrase(&track("Queen", "Bohemian Rhapsody (Remastered 2011)")), "Queen, Bohemian Rhapsody");
        assert_eq!(phrase(&track("A", "(Intro)")), "A, (Intro)");
        assert_eq!(phrase(&track("A\nB", "x\ty")), "A B, x y");
        assert_eq!(phrase(&track("A", &"я".repeat(500))).chars().count(), MAX_CHARS);
    }

    #[test]
    fn echo_is_recognized() {
        assert!(is_echo("повтори за мной radiohead creep"));
        assert!(!is_echo("повтори трек"));
    }

    #[test]
    fn parse_modes() {
        assert_eq!(Announce::parse("voice"), Some(Announce::Voice));
        assert_eq!(Announce::parse("ВСЕГДА"), Some(Announce::Always));
        assert_eq!(Announce::parse("x"), None);
    }
}
