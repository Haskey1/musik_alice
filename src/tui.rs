//! Терминальный интерфейс поверх управляющего сокета: статус, прогресс, горячие клавиши.
//! Работает по `adb shell`/ssh и не требует ничего, кроме терминала с ANSI.

use crate::{control, util};
use std::{
    io::{self, Read, Write},
    os::fd::AsRawFd,
    path::Path,
    time::{Duration, Instant},
};

/// Действие по нажатию клавиши.
#[derive(Debug, PartialEq, Eq)]
pub enum Key {
    Command(&'static str),
    Quit,
    /// Открыть строку ввода запроса («/»).
    Search,
    Nothing,
}

pub fn map_key(byte: u8) -> Key {
    match byte {
        b' ' => Key::Command("TOGGLE"),
        b'n' | b'N' => Key::Command("NEXT"),
        b'p' | b'P' | b'b' => Key::Command("PREVIOUS"),
        b'l' | b'L' => Key::Command("LIKE"),
        b'd' | b'D' => Key::Command("DISLIKE"),
        b'r' | b'R' => Key::Command("RADIO"),
        b'v' | b'V' => Key::Command("VOICE_TOGGLE"),
        b'+' | b'=' => Key::Command("VOLUME_UP"),
        b'-' | b'_' => Key::Command("VOLUME_DOWN"),
        b'/' | b's' => Key::Search,
        b'q' | b'Q' | 3 | 27 => Key::Quit,
        _ => Key::Nothing,
    }
}

/// Строки экрана для ширины `width`.
pub fn render(status: &serde_json::Value, width: usize, note: &str) -> Vec<String> {
    let text = |k: &str| status[k].as_str().unwrap_or("").to_owned();
    let number = |k: &str| status[k].as_u64().unwrap_or(0);
    let state = match status["state"].as_str().unwrap_or("idle") {
        "playing" => "▶ играет",
        "paused" => "⏸ пауза",
        "loading" => "… загрузка",
        _ => "■ свободно",
    };
    let (artist, title) = (text("artist"), text("title"));
    let name = match (artist.is_empty(), title.is_empty()) {
        (_, true) if artist.is_empty() => "—".to_owned(),
        (true, false) => title,
        (false, true) => artist,
        _ => format!("{artist} — {title}"),
    };
    let (pos, dur) = (number("position_ms"), number("duration_ms"));
    let bar_width = width.saturating_sub(16).clamp(8, 60);
    let filled = (pos.min(dur) * bar_width as u64).checked_div(dur).unwrap_or(0) as usize;
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_width - filled));
    let volume = status["volume"].as_u64().map_or("—".to_owned(), |v| format!("{v}%"));
    let voice = if status["voice"].as_bool().unwrap_or(false) { "вкл" } else { "выкл" };
    let busy = if status["busy"].as_bool().unwrap_or(false) { "  Алиса слушает" } else { "" };
    let mut lines = vec![
        format!("musik-alice  {}", util::truncate(&text("server"), width.saturating_sub(14))),
        String::new(),
        format!("{state}   {}", util::truncate(&text("session"), width.saturating_sub(14))),
        util::truncate(&name, width),
        util::truncate(&text("album"), width),
        format!("{bar} {} / {}", util::clock(pos), util::clock(dur)),
        String::new(),
        format!("громкость {volume}   голос {voice}   каталог {} треков{busy}", number("library")),
        util::truncate(&text("message"), width),
        String::new(),
        "пробел пауза · n дальше · p назад · l лайк · d дизлайк · r радио".into(),
        "+/- громкость · v голос · / поиск · q выход".into(),
    ];
    if !note.is_empty() {
        lines.push(note.to_owned());
    }
    lines.into_iter().map(|l| util::truncate(&l, width.max(2))).collect()
}

struct RawMode {
    original: libc::termios,
}

impl RawMode {
    fn enter() -> io::Result<Self> {
        let fd = io::stdin().as_raw_fd();
        let mut term = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(fd, &mut term) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let original = term;
        term.c_lflag &= !(libc::ICANON | libc::ECHO);
        term.c_cc[libc::VMIN] = 0;
        term.c_cc[libc::VTIME] = 2; // 200 мс ожидания клавиши
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(io::stdin().as_raw_fd(), libc::TCSANOW, &self.original) };
        println!("\x1b[?25h\x1b[0m");
        let _ = io::stdout().flush();
    }
}

fn term_width() -> usize {
    let mut size = unsafe { std::mem::zeroed::<libc::winsize>() };
    if unsafe { libc::ioctl(io::stdout().as_raw_fd(), libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_col > 0 {
        usize::from(size.ws_col)
    } else {
        80
    }
}

/// Читает строку с терминала без эха (для пароля).
pub fn read_password(prompt: &str) -> io::Result<String> {
    eprint!("{prompt}");
    let fd = io::stdin().as_raw_fd();
    let mut term = unsafe { std::mem::zeroed::<libc::termios>() };
    let have_tty = unsafe { libc::tcgetattr(fd, &mut term) } == 0;
    let original = term;
    if have_tty {
        term.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) };
    }
    let mut line = String::new();
    let result = io::stdin().read_line(&mut line);
    if have_tty {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &original) };
        eprintln!();
    }
    result?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

pub fn run(socket: &Path) -> io::Result<()> {
    let _raw = RawMode::enter().map_err(|e| io::Error::new(e.kind(), format!("нужен интерактивный терминал: {e}")))?;
    print!("\x1b[?25l");
    let mut note = String::new();
    let mut typing: Option<String> = None;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let mut volume = 50i64;
    loop {
        if last_draw.elapsed() >= Duration::from_millis(500) {
            let status = control::request(socket, "STATUS")
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
            let mut lines = match &status {
                Some(s) => {
                    volume = s["volume"].as_i64().unwrap_or(volume);
                    render(s, term_width(), &note)
                }
                None => vec!["musik-alice не запущен (musikalice start)".to_owned()],
            };
            if let Some(input) = &typing {
                lines.push(format!("найти: {input}_"));
            }
            print!("\x1b[H\x1b[2J{}", lines.join("\r\n"));
            let _ = io::stdout().flush();
            last_draw = Instant::now();
        }
        let mut byte = [0u8; 1];
        if io::stdin().read(&mut byte)? == 0 {
            continue;
        }
        let byte = byte[0];
        if let Some(input) = typing.as_mut() {
            match byte {
                b'\r' | b'\n' => {
                    let query = std::mem::take(input);
                    typing = None;
                    if !query.trim().is_empty() {
                        note = control::request(socket, &format!("PLAY {}", query.trim()))
                            .unwrap_or_else(|e| e.to_string());
                    }
                }
                27 => typing = None,
                127 | 8 => {
                    // Удаляем последний символ UTF-8 целиком.
                    input.pop();
                }
                b if b >= 0x20 => {
                    // Собираем многобайтный символ: первый байт задаёт длину.
                    let extra = match b {
                        0xC0..=0xDF => 1,
                        0xE0..=0xEF => 2,
                        0xF0..=0xF7 => 3,
                        _ => 0,
                    };
                    let mut bytes = vec![b];
                    for _ in 0..extra {
                        let mut next = [0u8; 1];
                        while io::stdin().read(&mut next)? == 0 {}
                        bytes.push(next[0]);
                    }
                    if let Ok(s) = String::from_utf8(bytes) {
                        input.push_str(&s);
                    }
                }
                _ => {}
            }
            last_draw = Instant::now() - Duration::from_secs(1);
            continue;
        }
        match map_key(byte) {
            Key::Quit => return Ok(()),
            Key::Search => typing = Some(String::new()),
            Key::Command("VOLUME_UP") => {
                volume = (volume + 5).min(100);
                note = control::request(socket, &format!("VOLUME {volume}")).unwrap_or_default();
            }
            Key::Command("VOLUME_DOWN") => {
                volume = (volume - 5).max(0);
                note = control::request(socket, &format!("VOLUME {volume}")).unwrap_or_default();
            }
            Key::Command("VOICE_TOGGLE") => {
                let on = control::request(socket, "STATUS")
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    .and_then(|s| s["voice"].as_bool())
                    .unwrap_or(true);
                note = control::request(socket, if on { "VOICE OFF" } else { "VOICE ON" }).unwrap_or_default();
            }
            Key::Command(line) => note = control::request(socket, line).unwrap_or_else(|e| e.to_string()),
            Key::Nothing => {}
        }
        last_draw = Instant::now() - Duration::from_secs(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_map_to_control_lines() {
        assert_eq!(map_key(b' '), Key::Command("TOGGLE"));
        assert_eq!(map_key(b'n'), Key::Command("NEXT"));
        assert_eq!(map_key(b'/'), Key::Search);
        assert_eq!(map_key(b'q'), Key::Quit);
        assert_eq!(map_key(b'x'), Key::Nothing);
    }

    #[test]
    fn screen_shows_track_progress_and_state() {
        let status = json!({
            "state": "playing", "artist": "Radiohead", "title": "Creep", "album": "Pablo Honey",
            "position_ms": 60_000, "duration_ms": 240_000, "session": "radio", "volume": 40,
            "voice": true, "busy": true, "library": 1234, "message": "Включаю радио", "server": "http://nas:8787"
        });
        let screen = render(&status, 80, "ok").join("\n");
        assert!(screen.contains("▶ играет"));
        assert!(screen.contains("Radiohead — Creep"));
        assert!(screen.contains("1:00 / 4:00"));
        assert!(screen.contains("громкость 40%"));
        assert!(screen.contains("Алиса слушает"));
        assert!(screen.contains("1234 треков"));
    }

    #[test]
    fn idle_screen_and_narrow_terminal_do_not_panic() {
        let lines = render(&json!({"state": "idle"}), 20, "");
        assert!(lines.iter().any(|l| l.contains("свободно")));
        assert!(lines.iter().all(|l| l.chars().count() <= 20));
        render(&json!({}), 1, "очень длинное сообщение для узкого терминала");
    }
}
