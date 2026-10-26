//! Короткие команды без «Алиса» («дальше», «назад»), которые прошивка слышит, но отбрасывает.
//!
//! Прошивка сама слушает несколько фраз без обращения (командный спотер). Часть из них («стоп», громкость) она
//! превращает в запрос к Алисе, и такой запрос musik-alice видит на локальном порту как обычно. А «дальше» и
//! «назад» прошивка выполняет, только пока играет её собственный плеер: когда играет musik, её плеер на паузе,
//! и фраза пропадает. Ни на порту, ни где-то ещё о ней не сообщается, кроме журнала самой прошивки:
//!
//! ```text
//! [sdk] [info] [command_spotter_capability.cpp:309] Command phrase spotted: дальше
//! ```
//!
//! Поэтому служба читает этот журнал (как `tail -F`): новые строки, переживая ротацию файла. Журнал лежит на
//! самой колонке, ничего внешнего не нужно.

use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

/// Журнал прошивки; `MUSIKALICE_SPOTTER_LOG` подменяет путь.
pub const LOG: &str = "/data/quasar/daemons_logs/maind.log";
const MARK: &str = "Command phrase spotted: ";
const POLL: Duration = Duration::from_millis(150);
const MAX_LINE: usize = 4096;

pub fn log_path() -> PathBuf {
    std::env::var_os("MUSIKALICE_SPOTTER_LOG").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(LOG))
}

/// Фраза из строки журнала, если это срабатывание командного спотера.
pub fn phrase(line: &str) -> Option<&str> {
    let start = line.find(MARK)? + MARK.len();
    let text = line[start..].trim();
    (!text.is_empty()).then_some(text)
}

/// Чтение дописываемого файла с переживанием ротации и обрезки.
pub struct Tail {
    path: PathBuf,
    file: Option<File>,
    inode: u64,
    pos: u64,
    partial: Vec<u8>,
    /// Первое открытие — с конца файла: старые строки не наши.
    from_end: bool,
}

impl Tail {
    pub fn new(path: &Path) -> Self {
        Self { path: path.to_owned(), file: None, inode: 0, pos: 0, partial: Vec::new(), from_end: true }
    }

    fn open(&mut self) -> bool {
        let Ok(mut file) = File::open(&self.path) else { return false };
        let Ok(meta) = file.metadata() else { return false };
        self.inode = meta.ino();
        self.pos = if self.from_end { meta.len() } else { 0 };
        self.from_end = false;
        if file.seek(SeekFrom::Start(self.pos)).is_err() {
            return false;
        }
        self.partial.clear();
        self.file = Some(file);
        true
    }

    fn drain(&mut self, out: &mut Vec<String>) {
        let Some(file) = self.file.as_mut() else { return };
        let mut buf = [0u8; 16 * 1024];
        loop {
            match file.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    self.pos += n as u64;
                    for &b in &buf[..n] {
                        if b == b'\n' {
                            out.push(String::from_utf8_lossy(&self.partial).into_owned());
                            self.partial.clear();
                        } else if self.partial.len() < MAX_LINE {
                            self.partial.push(b);
                        }
                    }
                }
            }
        }
    }

    /// Новые полные строки с прошлого вызова.
    pub fn poll(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if self.file.is_none() && !self.open() {
            return out;
        }
        self.drain(&mut out);
        match fs::metadata(&self.path) {
            // Файл переименовали в .1 и начали новый: старый дочитан выше, новый читаем с начала.
            Ok(meta) if meta.ino() != self.inode => {
                self.file = None;
                if self.open() {
                    self.drain(&mut out);
                }
            }
            // Файл обрезали на месте.
            Ok(meta) if meta.len() < self.pos => {
                if let Some(file) = self.file.as_mut() {
                    if file.seek(SeekFrom::Start(0)).is_ok() {
                        self.pos = 0;
                        self.partial.clear();
                        self.drain(&mut out);
                    }
                }
            }
            Ok(_) => {}
            Err(_) => self.file = None,
        }
        out
    }
}

/// Следит за журналом и отдаёт услышанные фразы, пока не поднят `stop`.
pub fn watch(path: PathBuf, stop: Arc<AtomicBool>, mut emit: impl FnMut(String)) {
    let mut tail = Tail::new(&path);
    while !stop.load(Ordering::Relaxed) {
        for line in tail.poll() {
            if let Some(text) = phrase(&line) {
                emit(text.to_owned());
            }
        }
        thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn phrase_is_taken_from_the_spotter_line() {
        let line = "[1740:1768] [2026-10-26 19:24:31.194] [sdk] [info] [command_spotter_capability.cpp:309] \
                    Command phrase spotted: дальше";
        assert_eq!(phrase(line), Some("дальше"));
        assert_eq!(phrase("[sdk] [info] routeDirective playback_pause.MusikAlice"), None);
    }

    fn append(path: &Path, text: &str) {
        fs::OpenOptions::new().create(true).append(true).open(path).unwrap().write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn tail_skips_history_and_survives_rotation_and_truncation() {
        let dir = std::env::temp_dir().join(format!("musikalice-tail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("maind.log");
        append(&log, "старое\n");
        let mut tail = Tail::new(&log);
        assert!(tail.poll().is_empty(), "история до запуска не читается");
        append(&log, "раз\nдв");
        assert_eq!(tail.poll(), ["раз"]);
        append(&log, "а\n");
        assert_eq!(tail.poll(), ["два"], "строка, дописанная частями, собирается");

        // Ротация: дописали в старый, переименовали, начали новый.
        append(&log, "последняя в старом\n");
        fs::rename(&log, dir.join("maind.log.1")).unwrap();
        append(&log, "первая в новом\n");
        assert_eq!(tail.poll(), ["последняя в старом", "первая в новом"]);

        // Обрезка на месте.
        fs::write(&log, "").unwrap();
        append(&log, "после обрезки\n");
        assert_eq!(tail.poll(), ["после обрезки"]);
    }
}
