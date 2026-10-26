//! Устройство ввода (evdev) с кнопками станции.
//!
//! Пока musik активен, устройство захватывается (`EVIOCGRAB`): касания получаем только мы, а Алиса их не видит.
//! Когда musik отпускает колонку, захват снимается, и кнопки снова работают штатно.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    mem::size_of,
    os::unix::{fs::OpenOptionsExt, io::AsRawFd},
    path::Path,
};

const EVIOCGRAB: u32 = 0x4004_4590;
/// `EVIOCGKEY(96)`: битовая карта нажатых клавиш.
const EVIOCGKEY96: u32 = 0x8060_4518;
const EVIOCSCLOCKID: u32 = 0x4004_45a0;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const SYN_DROPPED: u16 = 3;

/// Событие ядра в разборе, не зависящем от разрядности `timeval`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Клавиша: код, значение (1 — нажата, 0 — отпущена, 2 — повтор), время в микросекундах.
    Key { code: u16, value: i32, at: i64 },
    /// Ядро потеряло события: состояние кнопок надо перечитать.
    Dropped,
}

pub struct Input {
    file: File,
    grabbed: bool,
    /// Часы событий монотонные (иначе — реального времени, и сравнивать их надо с ними же).
    pub monotonic: bool,
}

impl Input {
    pub fn open(path: &Path) -> io::Result<Self> {
        // O_CLOEXEC: дочерние процессы (консоль Алисы) не должны унаследовать захват.
        let file = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(path)?;
        let clock: libc::c_int = libc::CLOCK_MONOTONIC;
        // SAFETY: ioctl с указателем на локальное int, как требует EVIOCSCLOCKID.
        let monotonic = unsafe { libc::ioctl(file.as_raw_fd(), EVIOCSCLOCKID as _, &clock) } >= 0;
        Ok(Self { file, grabbed: false, monotonic })
    }

    pub fn grabbed(&self) -> bool {
        self.grabbed
    }

    /// Захватить или отпустить устройство.
    pub fn grab(&mut self, on: bool) -> io::Result<()> {
        if on == self.grabbed {
            return Ok(());
        }
        let arg: libc::c_int = on.into();
        // SAFETY: EVIOCGRAB принимает целое значение, а не указатель.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), EVIOCGRAB as _, arg) } < 0 {
            return Err(io::Error::last_os_error());
        }
        self.grabbed = on;
        Ok(())
    }

    /// Нажата ли сейчас хоть одна клавиша (по данным ядра).
    pub fn any_key_down(&self) -> bool {
        let mut keys = [0u8; 96];
        // SAFETY: буфер ровно того размера, что закодирован в EVIOCGKEY96.
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), EVIOCGKEY96 as _, keys.as_mut_ptr()) };
        rc >= 0 && keys.iter().any(|b| *b != 0)
    }

    /// Все события, накопившиеся к этому моменту (без ожидания).
    pub fn read(&mut self) -> io::Result<Vec<Event>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 64 * size_of::<libc::input_event>()];
        loop {
            match self.file.read(&mut buf) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "устройство ввода закрыто")),
                Ok(n) => {
                    if n % size_of::<libc::input_event>() != 0 {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "размер события ввода не совпал"));
                    }
                    out.extend(parse(&buf[..n]));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(out),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Ждать событий не дольше `timeout_ms`.
    pub fn wait(&self, timeout_ms: i32) {
        let mut pfd = libc::pollfd { fd: self.file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: один корректный pollfd.
        unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        let _ = self.grab(false);
    }
}

/// Разбор сырого буфера событий ядра.
pub fn parse(bytes: &[u8]) -> Vec<Event> {
    bytes
        .chunks_exact(size_of::<libc::input_event>())
        .filter_map(|chunk| {
            // SAFETY: длина куска равна размеру input_event; read_unaligned не требует выравнивания.
            let e: libc::input_event = unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast()) };
            match (e.type_, e.code) {
                (EV_SYN, SYN_DROPPED) => Some(Event::Dropped),
                // На 32-битной станции поля timeval — i32, на 64-битной машине сборки — уже i64.
                #[allow(clippy::useless_conversion)]
                (EV_KEY, code) => Some(Event::Key {
                    code,
                    value: e.value,
                    at: i64::from(e.time.tv_sec) * 1_000_000 + i64::from(e.time.tv_usec),
                }),
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(type_: u16, code: u16, value: i32, sec: i64, usec: i64) -> Vec<u8> {
        let e = libc::input_event { time: libc::timeval { tv_sec: sec as _, tv_usec: usec as _ }, type_, code, value };
        // SAFETY: input_event — простая C-структура.
        unsafe {
            std::slice::from_raw_parts((&e as *const libc::input_event).cast::<u8>(), size_of::<libc::input_event>())
        }
        .to_vec()
    }

    #[test]
    fn parses_keys_and_drops_and_skips_sync() {
        let mut bytes = raw(EV_KEY, 582, 1, 12, 345_678);
        bytes.extend(raw(EV_SYN, 0, 0, 12, 345_678));
        bytes.extend(raw(EV_SYN, SYN_DROPPED, 0, 12, 400_000));
        bytes.extend(raw(4, 4, 7, 12, 400_000)); // EV_MSC
        assert_eq!(parse(&bytes), [Event::Key { code: 582, value: 1, at: 12_345_678 }, Event::Dropped]);
    }
}
