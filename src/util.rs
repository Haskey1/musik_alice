//! Мелкие общие помощники: идентификаторы, время, атомарная запись файлов, генератор случайных чисел.

use serde::Deserialize;
use std::{
    fs,
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

/// Для полей JSON, которые сервер может прислать как `null`: вместо ошибки берётся значение по умолчанию.
pub fn nullable<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

pub fn unix_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `n` случайных байтов из ядра; если /dev/urandom недоступен, используется время и адрес стека.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    if fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut out)).is_ok() {
        return out;
    }
    let mut x = unix_millis() ^ (std::process::id() as u64).rotate_left(32) ^ (&out as *const _ as u64);
    for byte in &mut out {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *byte = (x >> 24) as u8;
    }
    out
}

/// Идентификатор события для `/api/events`: 128 бит в hex. При повторе запроса передаётся тот же.
pub fn new_event_id() -> String {
    random_bytes(16).iter().map(|b| format!("{b:02x}")).collect()
}

/// Маленький xorshift для перемешивания списков. Криптостойкость не нужна.
pub struct Rng(u64);
impl Rng {
    pub fn new() -> Self {
        let bytes = random_bytes(8);
        let mut seed = 0u64;
        for b in bytes {
            seed = (seed << 8) | u64::from(b);
        }
        Self(seed | 1)
    }
    #[cfg(test)]
    pub fn from_seed(seed: u64) -> Self {
        Self(seed | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }
}
impl Default for Rng {
    fn default() -> Self {
        Self::new()
    }
}

/// Записывает файл целиком: сначала во временный, затем переименованием, с режимом `mode`.
pub fn write_atomic(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&tmp)?;
        file.write_all(data)?;
        file.sync_all().ok();
    }
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path)
}

/// Длительность в виде `м:сс` для TUI и логов.
pub fn clock(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Обрезает строку до `max` символов, добавляя «…».
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Процентное кодирование для значений query-строки.
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(byte)),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_ids_are_unique_hex() {
        let a = new_event_id();
        let b = new_event_id();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn shuffle_keeps_all_items() {
        let mut rng = Rng::from_seed(42);
        let mut items: Vec<u32> = (0..100).collect();
        rng.shuffle(&mut items);
        let mut sorted = items.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..100).collect::<Vec<_>>());
        assert_ne!(items, (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn clock_and_truncate() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(65_000), "1:05");
        assert_eq!(clock(3_725_000), "62:05");
        assert_eq!(truncate("абвгд", 10), "абвгд");
        assert_eq!(truncate("абвгдеёжз", 5), "абвг…");
    }

    #[test]
    fn percent_encoding_handles_utf8_and_spaces() {
        assert_eq!(percent_encode("a b/я"), "a%20b%2F%D1%8F");
        assert_eq!(percent_encode("AZaz09-_.~"), "AZaz09-_.~");
    }

    #[test]
    fn atomic_write_sets_mode_and_replaces() {
        let dir = std::env::temp_dir().join(format!("musikalice-util-{}", std::process::id()));
        let path = dir.join("nested").join("file.json");
        write_atomic(&path, b"one", 0o600).unwrap();
        write_atomic(&path, b"two", 0o600).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(&dir).ok();
    }
}
