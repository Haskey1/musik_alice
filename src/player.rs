//! Проигрыватель: поток загрузки+декодирования кормит поток вывода через ограниченную очередь.
//!
//! Поток вывода владеет звуковым устройством и отвечает на команды каждые ~50 мс, поэтому
//! пауза, пропуск и приглушение реагируют быстро, даже если сеть подвисла. Каждый запуск
//! трека получает номер поколения; события от устаревших поколений движок отбрасывает.

use crate::{
    api::Fetcher,
    audio::Output,
    decode::{hint_for, TrackDecoder},
    ducking::{Ducking, Gain},
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender},
        Arc,
    },
    thread,
    time::Duration,
};

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    /// Первые звуки трека пошли в устройство.
    Started { generation: u64 },
    /// Трек доигран до конца.
    Ended { generation: u64, seconds: f64 },
    /// Не удалось открыть или декодировать; `started` — звучал ли трек до сбоя.
    Failed { generation: u64, message: String, started: bool },
}

enum Command {
    Load { generation: u64, url: String },
    Pause,
    Resume,
    Stop,
    Earcon(Vec<i16>),
    Quit,
}

/// Общие наблюдаемые значения.
#[derive(Default)]
pub struct Shared {
    /// Позиция в миллисекундах от начала текущего трека.
    pub position_ms: AtomicU64,
    /// Длительность по заголовку контейнера, мс (0 — неизвестна).
    pub duration_ms: AtomicU64,
    pub playing: AtomicBool,
}

pub struct Player {
    tx: Sender<Command>,
    pub shared: Arc<Shared>,
    rate: u32,
}

enum Pipe {
    Info { duration: Option<f64> },
    Pcm(Vec<i16>),
    End,
    Fail(String),
}

const PIPE_DEPTH: usize = 40;

fn spawn_pipe(fetcher: Arc<dyn Fetcher>, url: String, rate: u32, cancel: Arc<AtomicBool>) -> Receiver<Pipe> {
    let (tx, rx): (SyncSender<Pipe>, Receiver<Pipe>) = mpsc::sync_channel(PIPE_DEPTH);
    thread::Builder::new()
        .name("pipe".into())
        .spawn(move || {
            let send = |p: Pipe| !cancel.load(Ordering::Relaxed) && tx.send(p).is_ok();
            let opened = match fetcher.open(&url) {
                Ok(o) => o,
                Err(e) => {
                    send(Pipe::Fail(e.to_string()));
                    return;
                }
            };
            let mut decoder = match TrackDecoder::open(opened.reader, &hint_for(opened.content_type.as_deref()), rate) {
                Ok(d) => d,
                Err(e) => {
                    send(Pipe::Fail(e));
                    return;
                }
            };
            if !send(Pipe::Info { duration: decoder.duration_hint }) {
                return;
            }
            let mut batch: Vec<i16> = Vec::new();
            let flush_at = rate as usize / 10 * 2; // 100 мс
            loop {
                match decoder.next_chunk(&mut batch) {
                    Ok(true) => {
                        if batch.len() >= flush_at && !send(Pipe::Pcm(std::mem::take(&mut batch))) {
                            return;
                        }
                    }
                    Ok(false) => {
                        if !batch.is_empty() && !send(Pipe::Pcm(std::mem::take(&mut batch))) {
                            return;
                        }
                        send(Pipe::End);
                        return;
                    }
                    Err(e) => {
                        if !batch.is_empty() {
                            send(Pipe::Pcm(std::mem::take(&mut batch)));
                        }
                        send(Pipe::Fail(e));
                        return;
                    }
                }
            }
        })
        .expect("не удалось запустить поток загрузки");
    rx
}

struct Current {
    generation: u64,
    rx: Receiver<Pipe>,
    cancel: Arc<AtomicBool>,
    frames: u64,
    started: bool,
    paused: bool,
}

impl Current {
    fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Player {
    pub fn start(
        output: Box<dyn Output>,
        fetcher: Arc<dyn Fetcher>,
        ducking: Arc<Ducking>,
        duck_level: f32,
        emit: impl Fn(PlayerEvent) + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        let rate = output.rate();
        let worker =
            Worker { output, fetcher, ducking, gain: Gain::new(rate, duck_level), shared: shared.clone(), rate };
        thread::Builder::new()
            .name("player".into())
            .spawn(move || worker.run(rx, emit))
            .expect("не удалось запустить поток вывода");
        Self { tx, shared, rate }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }
    pub fn load(&self, generation: u64, url: String) {
        let _ = self.tx.send(Command::Load { generation, url });
    }
    pub fn pause(&self) {
        let _ = self.tx.send(Command::Pause);
    }
    pub fn resume(&self) {
        let _ = self.tx.send(Command::Resume);
    }
    pub fn stop(&self) {
        let _ = self.tx.send(Command::Stop);
    }
    pub fn earcon(&self, pcm: Vec<i16>) {
        let _ = self.tx.send(Command::Earcon(pcm));
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Quit);
    }
}

struct Worker {
    output: Box<dyn Output>,
    fetcher: Arc<dyn Fetcher>,
    ducking: Arc<Ducking>,
    gain: Gain,
    shared: Arc<Shared>,
    rate: u32,
}

impl Worker {
    fn run(mut self, commands: Receiver<Command>, emit: impl Fn(PlayerEvent)) {
        let mut current: Option<Current> = None;
        loop {
            let idle = current.as_ref().is_none_or(|c| c.paused);
            let command = if idle {
                match commands.recv_timeout(Duration::from_millis(200)) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                commands.try_recv().ok()
            };
            if let Some(command) = command {
                match command {
                    Command::Quit => {
                        if let Some(c) = &current {
                            c.cancel();
                        }
                        self.output.flush();
                        return;
                    }
                    Command::Load { generation, url } => {
                        if let Some(old) = current.take() {
                            old.cancel();
                        }
                        self.output.flush();
                        self.shared.position_ms.store(0, Ordering::Relaxed);
                        self.shared.duration_ms.store(0, Ordering::Relaxed);
                        let cancel = Arc::new(AtomicBool::new(false));
                        let rx = spawn_pipe(self.fetcher.clone(), url, self.rate, cancel.clone());
                        current = Some(Current { generation, rx, cancel, frames: 0, started: false, paused: false });
                        self.shared.playing.store(true, Ordering::Relaxed);
                    }
                    Command::Pause => {
                        if let Some(c) = current.as_mut() {
                            if !c.paused {
                                c.paused = true;
                                self.output.flush();
                                self.shared.playing.store(false, Ordering::Relaxed);
                            }
                        }
                    }
                    Command::Resume => {
                        if let Some(c) = current.as_mut() {
                            c.paused = false;
                            self.shared.playing.store(true, Ordering::Relaxed);
                        }
                    }
                    Command::Stop => {
                        if let Some(old) = current.take() {
                            old.cancel();
                        }
                        self.output.flush();
                        self.shared.playing.store(false, Ordering::Relaxed);
                        self.shared.position_ms.store(0, Ordering::Relaxed);
                    }
                    Command::Earcon(pcm) => {
                        // Сигнал звучит поверх паузы: музыка на это время ждёт.
                        self.output.flush();
                        let _ = self.output.write(&pcm);
                        self.output.drain();
                    }
                }
                continue;
            }
            let Some(cur) = current.as_mut() else { continue };
            if cur.paused {
                continue;
            }
            match cur.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Pipe::Info { duration }) => {
                    self.shared.duration_ms.store(duration.map_or(0, |d| (d * 1000.0) as u64), Ordering::Relaxed);
                }
                Ok(Pipe::Pcm(mut pcm)) => {
                    self.apply_gain(&mut pcm);
                    if let Err(message) = self.output.write(&pcm) {
                        let generation = cur.generation;
                        let started = cur.started;
                        cur.cancel();
                        current = None;
                        self.shared.playing.store(false, Ordering::Relaxed);
                        emit(PlayerEvent::Failed { generation, message, started });
                        continue;
                    }
                    if !cur.started {
                        cur.started = true;
                        emit(PlayerEvent::Started { generation: cur.generation });
                    }
                    cur.frames += pcm.len() as u64 / 2;
                    self.shared.position_ms.store(cur.frames * 1000 / u64::from(self.rate), Ordering::Relaxed);
                }
                Ok(Pipe::End) => {
                    self.output.drain();
                    let (generation, seconds) = (cur.generation, cur.frames as f64 / f64::from(self.rate));
                    current = None;
                    self.shared.playing.store(false, Ordering::Relaxed);
                    emit(PlayerEvent::Ended { generation, seconds });
                }
                Ok(Pipe::Fail(message)) => {
                    let (generation, started) = (cur.generation, cur.started);
                    current = None;
                    self.shared.playing.store(false, Ordering::Relaxed);
                    emit(PlayerEvent::Failed { generation, message, started });
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    let (generation, started) = (cur.generation, cur.started);
                    current = None;
                    self.shared.playing.store(false, Ordering::Relaxed);
                    emit(PlayerEvent::Failed {
                        generation, message: "поток загрузки оборвался".into(), started
                    });
                }
            }
        }
    }

    fn apply_gain(&mut self, pcm: &mut [i16]) {
        let busy = self.ducking.active();
        if self.gain.is_unity(busy) {
            return;
        }
        let mut floats: Vec<f32> = pcm.iter().map(|&s| f32::from(s)).collect();
        self.gain.apply(&mut floats, 2, busy);
        for (dst, src) in pcm.iter_mut().zip(floats) {
            *dst = src.round().clamp(-32768.0, 32767.0) as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::{ApiError, Opened},
        audio::NullOutput,
    };
    use std::sync::Mutex;

    struct FixtureFetcher;
    impl Fetcher for FixtureFetcher {
        fn open(&self, url: &str) -> Result<Opened, ApiError> {
            match url {
                "good" => Ok(Opened {
                    reader: Box::new(std::io::Cursor::new(include_bytes!("../tests/fixtures/tone.mp3").as_slice())),
                    content_type: Some("audio/mpeg".into()),
                }),
                "junk" => Ok(Opened { reader: Box::new(std::io::Cursor::new(vec![7u8; 5000])), content_type: None }),
                _ => Err(ApiError { status: 404, code: "not_found".into(), message: "нет трека".into() }),
            }
        }
    }

    struct Rig {
        player: Player,
        events: Receiver<PlayerEvent>,
        sink: Arc<Mutex<Vec<i16>>>,
        ducking: Arc<Ducking>,
    }

    fn rig() -> Rig {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let (etx, events) = mpsc::channel();
        let etx = Mutex::new(etx);
        let ducking = Arc::new(Ducking::new());
        let output = Box::new(NullOutput::new(44100, sink.clone()));
        let player = Player::start(output, Arc::new(FixtureFetcher), ducking.clone(), 0.2, move |e| {
            let _ = etx.lock().unwrap().send(e);
        });
        Rig { player, events, sink, ducking }
    }

    fn next(rig: &Rig) -> PlayerEvent {
        rig.events.recv_timeout(Duration::from_secs(5)).expect("событие плеера")
    }

    #[test]
    fn plays_a_track_to_the_end() {
        let rig = rig();
        rig.player.load(1, "good".into());
        assert_eq!(next(&rig), PlayerEvent::Started { generation: 1 });
        match next(&rig) {
            PlayerEvent::Ended { generation: 1, seconds } => assert!((0.9..1.2).contains(&seconds), "{seconds}"),
            other => panic!("{other:?}"),
        }
        let samples = rig.sink.lock().unwrap().len();
        assert!(samples > 44100 * 2 * 9 / 10);
        assert!(rig.player.shared.position_ms.load(Ordering::Relaxed) > 900);
        assert!(!rig.player.shared.playing.load(Ordering::Relaxed));
    }

    #[test]
    fn open_errors_and_bad_data_become_failures() {
        let rig = rig();
        rig.player.load(2, "missing".into());
        match next(&rig) {
            PlayerEvent::Failed { generation: 2, message, started: false } => assert!(message.contains("нет трека")),
            other => panic!("{other:?}"),
        }
        rig.player.load(3, "junk".into());
        assert!(matches!(next(&rig), PlayerEvent::Failed { generation: 3, started: false, .. }));
    }

    #[test]
    fn stop_silences_and_suppresses_the_end_event() {
        let rig = rig();
        rig.player.load(1, "good".into());
        rig.player.stop();
        thread::sleep(Duration::from_millis(400));
        while let Ok(event) = rig.events.try_recv() {
            assert!(!matches!(event, PlayerEvent::Ended { .. }), "{event:?}");
        }
        assert!(!rig.player.shared.playing.load(Ordering::Relaxed));
    }

    #[test]
    fn a_new_load_replaces_the_old_generation() {
        let rig = rig();
        rig.player.load(1, "good".into());
        rig.player.load(2, "good".into());
        let mut ended = Vec::new();
        while let Ok(event) = rig.events.recv_timeout(Duration::from_secs(2)) {
            if let PlayerEvent::Ended { generation, .. } = event {
                ended.push(generation);
            }
        }
        assert_eq!(ended, vec![2]);
    }

    #[test]
    fn ducking_lowers_the_level_of_the_stream() {
        let plain = rig();
        plain.player.load(1, "good".into());
        while !matches!(next(&plain), PlayerEvent::Ended { .. }) {}
        let quiet = rig();
        quiet.ducking.set_busy(true);
        quiet.player.load(1, "good".into());
        while !matches!(next(&quiet), PlayerEvent::Ended { .. }) {}
        let tail = |s: &Arc<Mutex<Vec<i16>>>| {
            let v = s.lock().unwrap();
            v[v.len() / 2..].iter().map(|x| i32::from(x.unsigned_abs())).max().unwrap()
        };
        let (loud, soft) = (tail(&plain.sink), tail(&quiet.sink));
        assert!(f64::from(soft) < f64::from(loud) * 0.35, "{soft} vs {loud}");
    }

    #[test]
    fn earcons_are_written_directly() {
        let rig = rig();
        rig.player.earcon(vec![100; 64]);
        thread::sleep(Duration::from_millis(300));
        assert_eq!(rig.sink.lock().unwrap().len(), 64);
    }
}
