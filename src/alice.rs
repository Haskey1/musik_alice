//! Локальный IPC Алисы на станции (Quasar, `127.0.0.1:9875`).
//!
//! Кадр: 4 байта длины (LE) + сообщение protobuf без схемы: поле 253 — тип, поле 252 — вложенная
//! полезная нагрузка. Тип 3 — `AliceState`: из него берутся активность Алисы (слушает, думает,
//! говорит) и окончательный текст распознавания. Команды уходят на тот же порт кадром типа 210
//! (`RemotingMessage`): отменить диалог Алисы и поставить на паузу штатный плеер.
//!
//! Формат кадров описан по наблюдениям проекта spotiya (MIT, https://github.com/klockky/spotiya);
//! код здесь написан заново.

use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

pub const MAX_FRAME: usize = 512 * 1024;
const TYPE_ALICE_STATE: u64 = 3;
const TYPE_REMOTING: u64 = 210;

// ---------------------------------------------------------------- protobuf без схемы

fn varint(data: &[u8], offset: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *data.get(*offset)?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Some(value);
        }
    }
    None
}

#[derive(Clone, Copy)]
enum Value<'a> {
    Number(u64),
    Bytes(&'a [u8]),
}

/// Находит первое поле с номером `wanted`. Нераспознанная разметка — `None`, паники исключены.
fn field(data: &[u8], wanted: u64) -> Option<Value<'_>> {
    let mut offset = 0;
    while offset < data.len() {
        let key = varint(data, &mut offset)?;
        if key >> 3 == 0 {
            return None;
        }
        let value = match key & 7 {
            0 => Value::Number(varint(data, &mut offset)?),
            2 => {
                let len = usize::try_from(varint(data, &mut offset)?).ok()?;
                let end = offset.checked_add(len)?;
                let bytes = data.get(offset..end)?;
                offset = end;
                Value::Bytes(bytes)
            }
            1 | 5 => {
                let len = if key & 7 == 1 { 8 } else { 4 };
                offset = offset.checked_add(len)?;
                if offset > data.len() {
                    return None;
                }
                continue;
            }
            _ => return None,
        };
        if key >> 3 == wanted {
            return Some(value);
        }
    }
    None
}

fn bytes(data: &[u8], number: u64) -> Option<&[u8]> {
    match field(data, number)? {
        Value::Bytes(v) => Some(v),
        Value::Number(_) => None,
    }
}

fn number(data: &[u8], field_number: u64) -> Option<u64> {
    match field(data, field_number)? {
        Value::Number(v) => Some(v),
        Value::Bytes(_) => None,
    }
}

/// Все поля сообщения подряд; `None`, если разметка не похожа на protobuf.
fn fields(data: &[u8]) -> Option<Vec<(u64, Value<'_>)>> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let key = varint(data, &mut offset)?;
        if key >> 3 == 0 {
            return None;
        }
        let value = match key & 7 {
            0 => Value::Number(varint(data, &mut offset)?),
            2 => {
                let len = usize::try_from(varint(data, &mut offset)?).ok()?;
                let end = offset.checked_add(len)?;
                let bytes = data.get(offset..end)?;
                offset = end;
                Value::Bytes(bytes)
            }
            1 | 5 => {
                let len = if key & 7 == 1 { 8 } else { 4 };
                let end = offset.checked_add(len)?;
                let raw = data.get(offset..end)?;
                offset = end;
                Value::Bytes(raw)
            }
            _ => return None,
        };
        out.push((key >> 3, value));
    }
    Some(out)
}

/// Читаемый разбор кадра без схемы: номера полей, числа, строки и вложенные сообщения. Для диагностики.
pub fn dump(data: &[u8]) -> String {
    fn walk(data: &[u8], depth: usize, out: &mut String) {
        let Some(list) = fields(data) else {
            out.push_str(&format!("{}<{} байт>\n", "  ".repeat(depth), data.len()));
            return;
        };
        for (number, value) in list {
            let pad = "  ".repeat(depth);
            match value {
                Value::Number(v) => out.push_str(&format!("{pad}{number}: {v}\n")),
                Value::Bytes(b) => {
                    let text = std::str::from_utf8(b)
                        .ok()
                        .filter(|t| !t.is_empty() && t.chars().all(|c| !c.is_control() || c == '\n' || c == '\t'));
                    match text {
                        Some(t) if depth >= 12 || fields(b).is_none() || !t.is_ascii() => {
                            out.push_str(&format!("{pad}{number}: «{}»\n", t.chars().take(300).collect::<String>()))
                        }
                        _ if b.is_empty() => out.push_str(&format!("{pad}{number}: {{}}\n")),
                        _ if depth < 12 && fields(b).is_some() => {
                            out.push_str(&format!("{pad}{number} {{\n"));
                            walk(b, depth + 1, out);
                            out.push_str(&format!("{pad}}}\n"));
                        }
                        Some(t) => out.push_str(&format!("{pad}{number}: «{t}»\n")),
                        None => out.push_str(&format!("{pad}{number}: <{} байт>\n", b.len())),
                    }
                }
            }
        }
    }
    let mut out = String::new();
    walk(data, 0, &mut out);
    out
}

/// Печатает все кадры Алисы `seconds` секунд подряд (команда `musikalice alice-dump`).
pub fn dump_stream(address: SocketAddr, seconds: u64, mut print: impl FnMut(&str)) -> io::Result<usize> {
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    socket.set_read_timeout(Some(Duration::from_millis(300)))?;
    let started = std::time::Instant::now();
    let mut reader = FrameReader::default();
    let mut count = 0;
    while started.elapsed() < Duration::from_secs(seconds) {
        if let Some(frame) = reader.poll(&mut socket)? {
            count += 1;
            print(&format!(
                "--- кадр {count}, {:.1} с, {} байт\n{}",
                started.elapsed().as_secs_f32(),
                frame.len(),
                dump(&frame)
            ));
        }
    }
    Ok(count)
}

/// `Some(true)` — Алиса слушает/думает/говорит; `Some(false)` — ждёт; `None` — это не состояние Алисы.
pub fn alice_busy(data: &[u8]) -> Option<bool> {
    if number(data, 253) != Some(TYPE_ALICE_STATE) {
        return None;
    }
    let state = bytes(data, 252)?;
    match number(state, 1).unwrap_or(0) {
        0 | 6 => Some(false),        // IDLE / NONE
        1 | 2 | 3 | 5 => Some(true), // LISTENING / SPEAKING / BUSY / SHAZAM
        _ => None,
    }
}

/// Окончательная фраза пользователя: `(идентификатор запроса, текст)`.
///
/// Алиса помечает фразу окончательной ещё в состоянии LISTENING, примерно за секунду до BUSY.
/// Реагировать нужно сразу: отмена успевает дойти до сервера раньше, чем Алиса ответит сама.
pub fn recognized(data: &[u8]) -> Option<(String, String)> {
    if number(data, 253) != Some(TYPE_ALICE_STATE) {
        return None;
    }
    let state = bytes(data, 252)?;
    if !matches!(number(state, 1), Some(1 | 3)) {
        return None;
    }
    let result = bytes(state, 15)?;
    if number(result, 2) != Some(1) || number(result, 4) != Some(1) {
        return None;
    }
    let text = std::str::from_utf8(bytes(result, 1)?).ok()?;
    if text.is_empty() || text.len() > 512 || Some(text.as_bytes()) != bytes(state, 2) {
        return None;
    }
    let request = std::str::from_utf8(bytes(state, 7)?).ok()?;
    if request.len() != 36 {
        return None;
    }
    Some((request.to_owned(), text.to_owned()))
}

fn put_var(out: &mut Vec<u8>, mut v: u64) {
    while v >= 128 {
        out.push((v as u8 & 127) | 128);
        v >>= 7;
    }
    out.push(v as u8);
}
fn put_number(out: &mut Vec<u8>, field: u64, value: u64) {
    put_var(out, field << 3);
    put_var(out, value);
}
fn put_bytes(out: &mut Vec<u8>, field: u64, value: &[u8]) {
    put_var(out, (field << 3) | 2);
    put_var(out, value.len() as u64);
    out.extend_from_slice(value);
}

fn rpc(capability: &str, field: u64, method: &[u8]) -> Vec<u8> {
    let mut remoting = Vec::new();
    put_bytes(&mut remoting, 7, capability.as_bytes());
    put_bytes(&mut remoting, field, method);
    let mut message = Vec::new();
    put_bytes(&mut message, 252, &remoting);
    put_number(&mut message, 253, TYPE_REMOTING);
    let mut frame = (message.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(&message);
    frame
}

/// Кадр «отменить текущий диалог Алисы».
pub fn cancel_frame() -> Vec<u8> {
    rpc("AliceCapability", 8, &[8, 3])
}

/// Кадр «поставить на паузу штатный плеер». `request_id` — свежий UUID-подобный идентификатор.
pub fn pause_frame(request_id: &str) -> Vec<u8> {
    let mut source = Vec::new();
    put_number(&mut source, 1, 14);
    put_number(&mut source, 2, 3);
    put_number(&mut source, 3, 1);
    put_bytes(&mut source, 4, request_id.as_bytes());
    put_bytes(&mut source, 6, b"MusikAlice");
    let mut method = vec![8, 1];
    put_bytes(&mut method, 5, &source);
    put_number(&mut method, 7, 0);
    rpc("PlaybackControlCapability", 13, &method)
}

/// 128 случайных бит в виде строки UUID v4.
pub fn uuid_like() -> String {
    let mut b = crate::util::random_bytes(16);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

// ---------------------------------------------------------------- отправка команд

fn send_native(address: SocketAddr, frame: &[u8]) -> io::Result<()> {
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_millis(500))?;
    socket.set_write_timeout(Some(Duration::from_millis(500)))?;
    socket.write_all(frame)?;
    socket.shutdown(std::net::Shutdown::Write)
}

/// Отправка команд Алисе. Трейт позволяет подменить её в тестах движка.
pub trait Native: Send + Sync {
    /// Отменить текущий диалог (если фраза пришла голосом) и, если `pause`, поставить штатный плеер на паузу.
    fn prepare_playback(&self, from_voice: bool, pause: bool);
    /// Повторная пауза штатного плеера: Алиса может запустить свою музыку уже после диалога.
    fn pause_native(&self);
    /// Прервать текущий ответ Алисы (когда на фразу отвечаем мы).
    fn cancel(&self);
}

pub struct NativeClient {
    address: SocketAddr,
}
impl NativeClient {
    pub fn new(address: SocketAddr) -> Self {
        Self { address }
    }
}
impl Native for NativeClient {
    fn prepare_playback(&self, from_voice: bool, pause: bool) {
        let address = self.address;
        // Отдельный поток: соединение с Алисой не должно задерживать цикл движка.
        let _ = thread::Builder::new().name("alice-cmd".into()).stack_size(128 * 1024).spawn(move || {
            if from_voice {
                let _ = send_native(address, &cancel_frame());
            }
            if pause {
                let _ = send_native(address, &pause_frame(&uuid_like()));
            }
        });
    }
    fn pause_native(&self) {
        let address = self.address;
        let _ = thread::Builder::new().name("alice-pause".into()).stack_size(128 * 1024).spawn(move || {
            let _ = send_native(address, &pause_frame(&uuid_like()));
        });
    }
    fn cancel(&self) {
        let address = self.address;
        let _ = thread::Builder::new().name("alice-cancel".into()).stack_size(128 * 1024).spawn(move || {
            let _ = send_native(address, &cancel_frame());
        });
    }
}

/// Заглушка для профилей без голоса.
pub struct NoNative;
impl Native for NoNative {
    fn prepare_playback(&self, _from_voice: bool, _pause: bool) {}
    fn pause_native(&self) {}
    fn cancel(&self) {}
}

// ---------------------------------------------------------------- чтение событий

/// Накапливает байты и выдаёт полные кадры. Устойчив к таймаутам чтения и дроблению.
#[derive(Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}
impl FrameReader {
    /// Читает не более одного вызова `read`; возвращает кадр, если он уже собран.
    pub fn poll(&mut self, source: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
        if let Some(frame) = self.take()? {
            return Ok(Some(frame));
        }
        let mut chunk = [0u8; 4096];
        match source.read(&mut chunk) {
            Ok(0) => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Алиса закрыла соединение")),
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                self.take()
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    fn take(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let size = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if size == 0 || size > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "некорректная длина кадра"));
        }
        if self.buf.len() < 4 + size {
            return Ok(None);
        }
        let frame = self.buf[4..4 + size].to_vec();
        self.buf.drain(..4 + size);
        if self.buf.capacity() > 64 * 1024 && self.buf.is_empty() {
            self.buf = Vec::new();
        }
        Ok(Some(frame))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AliceEvent {
    Busy(bool),
    Recognized { text: String },
}

/// Разбирает кадры и вызывает `emit`. Повторные кадры одного запроса отбрасываются.
pub struct Decoder {
    seen: VecDeque<String>,
}
impl Decoder {
    pub fn new() -> Self {
        Self { seen: VecDeque::new() }
    }
    pub fn feed(&mut self, frame: &[u8], emit: &mut dyn FnMut(AliceEvent)) {
        if let Some(busy) = alice_busy(frame) {
            emit(AliceEvent::Busy(busy));
        }
        if let Some((request, text)) = recognized(frame) {
            if !self.seen.contains(&request) {
                self.seen.push_back(request);
                if self.seen.len() > 64 {
                    self.seen.pop_front();
                }
                emit(AliceEvent::Recognized { text });
            }
        }
    }
}
impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Поток-наблюдатель: подключается к Алисе, раздаёт события, переподключается через 2 с.
/// Перед выходом всегда сообщает `Busy(false)`, чтобы не оставить музыку приглушённой.
pub fn watch(address: SocketAddr, stop: Arc<AtomicBool>, mut emit: impl FnMut(AliceEvent) + Send + 'static) {
    let mut decoder = Decoder::new();
    while !stop.load(Ordering::Relaxed) {
        let result: io::Result<()> = (|| {
            let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
            socket.set_read_timeout(Some(Duration::from_millis(500)))?;
            eprintln!("voice: подключено к событиям Алисы");
            let mut reader = FrameReader::default();
            while !stop.load(Ordering::Relaxed) {
                if let Some(frame) = reader.poll(&mut socket)? {
                    decoder.feed(&frame, &mut emit);
                }
            }
            Ok(())
        })();
        emit(AliceEvent::Busy(false));
        if let Err(e) = result {
            eprintln!("voice: соединение с Алисой потеряно ({e}); повтор через 2 с");
        }
        for _ in 0..20 {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &[u8; 36] = b"12345678-1234-1234-1234-123456789012";

    fn state_message(state: Option<u64>, kind: u64) -> Vec<u8> {
        let mut body = Vec::new();
        if let Some(state) = state {
            put_number(&mut body, 1, state);
        }
        let mut msg = Vec::new();
        put_bytes(&mut msg, 252, &body);
        put_number(&mut msg, 253, kind);
        msg
    }

    fn phrase(alice_state: u64, text: &str, partial: bool) -> Vec<u8> {
        let mut asr = Vec::new();
        put_bytes(&mut asr, 1, text.as_bytes());
        put_number(&mut asr, 2, u64::from(!partial));
        put_number(&mut asr, 4, 1);
        let mut state = Vec::new();
        put_number(&mut state, 1, alice_state);
        put_bytes(&mut state, 2, text.as_bytes());
        put_bytes(&mut state, 7, ID);
        put_bytes(&mut state, 15, &asr);
        let mut msg = Vec::new();
        put_bytes(&mut msg, 252, &state);
        put_number(&mut msg, 253, 3);
        msg
    }

    #[test]
    fn activity_covers_listening_thinking_and_speech_only() {
        for state in [0, 6] {
            assert_eq!(alice_busy(&state_message(Some(state), 3)), Some(false));
        }
        for state in [1, 2, 3, 5] {
            assert_eq!(alice_busy(&state_message(Some(state), 3)), Some(true));
        }
        assert_eq!(alice_busy(&state_message(None, 3)), Some(false));
        assert_eq!(alice_busy(&state_message(Some(99), 3)), None);
        assert_eq!(alice_busy(&state_message(Some(1), 210)), None);
        assert_eq!(alice_busy(&[]), None);
    }

    #[test]
    fn final_phrase_is_taken_while_listening_or_busy_only() {
        let msg = phrase(3, "Включи radiohead", false);
        assert_eq!(recognized(&msg), Some((String::from_utf8(ID.to_vec()).unwrap(), "Включи radiohead".into())));
        assert!(recognized(&phrase(1, "Включи radiohead", false)).is_some());
        assert_eq!(recognized(&phrase(1, "Включи radiohead", true)), None, "промежуточный результат");
        assert_eq!(recognized(&phrase(2, "Включи radiohead", false)), None, "Алиса уже говорит");
        assert_eq!(recognized(&phrase(0, "Включи radiohead", false)), None);
    }

    #[test]
    fn parser_survives_truncated_and_malformed_input() {
        let msg = phrase(3, "Включи radiohead", false);
        for n in 0..msg.len() {
            assert_eq!(recognized(&msg[..n]), None, "обрезка до {n}");
            let _ = alice_busy(&msg[..n]);
        }
        for malformed in [vec![0], vec![0xff; 12], vec![0x12, 0xff, 0x7f], vec![0xe8, 0x0f, 210, 1]] {
            assert_eq!(recognized(&malformed), None);
            let _ = alice_busy(&malformed);
        }
    }

    #[test]
    fn cancel_frame_matches_the_native_cli_bytes() {
        assert_eq!(
            &cancel_frame()[4..],
            &[
                0xe2, 0x0f, 0x15, 0x3a, 0x0f, b'A', b'l', b'i', b'c', b'e', b'C', b'a', b'p', b'a', b'b', b'i', b'l',
                b'i', b't', b'y', 0x42, 2, 8, 3, 0xe8, 0x0f, 0xd2, 1
            ]
        );
        let frame = cancel_frame();
        assert_eq!(u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize, frame.len() - 4);
    }

    #[test]
    fn pause_frame_names_the_playback_capability() {
        let frame = pause_frame(&uuid_like());
        let message = &frame[4..];
        assert_eq!(number(message, 253), Some(210));
        let remoting = bytes(message, 252).unwrap();
        assert_eq!(bytes(remoting, 7), Some(b"PlaybackControlCapability".as_slice()));
        assert!(bytes(remoting, 13).is_some());
    }

    #[test]
    fn uuid_has_canonical_shape() {
        let id = uuid_like();
        assert_eq!(id.len(), 36);
        assert_eq!(id.matches('-').count(), 4);
        assert_eq!(&id[14..15], "4");
    }

    #[test]
    fn frame_reader_reassembles_fragmented_frames() {
        struct Dribble<'a>(&'a [u8]);
        impl Read for Dribble<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = self.0.len().min(3).min(buf.len());
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                if n == 0 {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                Ok(n)
            }
        }
        let a = state_message(Some(1), 3);
        let b = state_message(Some(0), 3);
        let mut wire = (a.len() as u32).to_le_bytes().to_vec();
        wire.extend_from_slice(&a);
        wire.extend_from_slice(&(b.len() as u32).to_le_bytes());
        wire.extend_from_slice(&b);
        let mut source = Dribble(&wire);
        let mut reader = FrameReader::default();
        let mut frames = Vec::new();
        for _ in 0..200 {
            if let Some(frame) = reader.poll(&mut source).unwrap() {
                frames.push(frame);
            }
        }
        assert_eq!(frames, vec![a, b]);
    }

    #[test]
    fn frame_reader_rejects_bad_lengths_and_eof() {
        let mut reader = FrameReader::default();
        let zero = [0u8; 4];
        assert!(reader.poll(&mut &zero[..]).is_err());
        let mut reader = FrameReader::default();
        let huge = (MAX_FRAME as u32 + 1).to_le_bytes();
        assert!(reader.poll(&mut &huge[..]).is_err());
        let mut reader = FrameReader::default();
        let empty: &[u8] = &[];
        assert_eq!(reader.poll(&mut &*empty).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn decoder_emits_activity_and_deduplicates_requests() {
        let mut decoder = Decoder::new();
        let mut events = Vec::new();
        let mut emit = |e| events.push(e);
        decoder.feed(&state_message(Some(1), 3), &mut emit);
        decoder.feed(&phrase(1, "Включи radiohead", false), &mut emit);
        decoder.feed(&phrase(3, "Включи radiohead", false), &mut emit);
        decoder.feed(&state_message(Some(0), 3), &mut emit);
        assert_eq!(
            events,
            vec![
                AliceEvent::Busy(true),
                AliceEvent::Busy(true),
                AliceEvent::Recognized { text: "Включи radiohead".into() },
                AliceEvent::Busy(true),
                AliceEvent::Busy(false),
            ]
        );
    }

    #[test]
    fn watcher_talks_to_a_fake_alice_and_restores_activity_on_disconnect() {
        use std::sync::mpsc;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = thread::spawn(move || {
            watch(address, flag, move |e| {
                let _ = tx.send(e);
            })
        });
        let (mut conn, _) = listener.accept().unwrap();
        for msg in [state_message(Some(1), 3), phrase(1, "Следующий трек", false)] {
            conn.write_all(&(msg.len() as u32).to_le_bytes()).unwrap();
            conn.write_all(&msg).unwrap();
        }
        let mut got = Vec::new();
        while got.len() < 3 {
            got.push(rx.recv_timeout(Duration::from_secs(3)).expect("событие"));
        }
        assert_eq!(got[0], AliceEvent::Busy(true));
        assert!(got.contains(&AliceEvent::Recognized { text: "Следующий трек".into() }));
        drop(conn);
        loop {
            if rx.recv_timeout(Duration::from_secs(3)).expect("Busy(false) после обрыва") == AliceEvent::Busy(false)
            {
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
    }

    #[test]
    fn native_client_sends_cancel_then_pause() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = NativeClient::new(address);
        client.prepare_playback(true, true);
        let mut capabilities = Vec::new();
        for _ in 0..2 {
            let (mut conn, _) = listener.accept().unwrap();
            let mut data = Vec::new();
            conn.read_to_end(&mut data).unwrap();
            let remoting = bytes(&data[4..], 252).unwrap().to_vec();
            capabilities.push(String::from_utf8(bytes(&remoting, 7).unwrap().to_vec()).unwrap());
        }
        assert_eq!(capabilities, ["AliceCapability", "PlaybackControlCapability"]);
    }

    #[test]
    fn dump_shows_nested_text_and_numbers() {
        let mut inner = Vec::new();
        put_bytes(&mut inner, 1, "дальше".as_bytes());
        put_number(&mut inner, 2, 1);
        let mut msg = Vec::new();
        put_bytes(&mut msg, 7, &inner);
        put_number(&mut msg, 253, 42);
        let text = dump(&msg);
        assert!(text.contains("7 {"), "{text}");
        assert!(text.contains("1: «дальше»"), "{text}");
        assert!(text.contains("253: 42"), "{text}");
        assert!(dump(&[0xff, 0xff]).contains("<2 байт>"));
    }
}
