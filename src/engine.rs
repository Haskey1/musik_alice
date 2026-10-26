//! Движок: единственный поток, который владеет состоянием сессии и принимает решения.
//!
//! Всё, что может блокироваться (HTTP), уходит в короткие потоки; их результат возвращается
//! в движок сообщением `Apply` и обрабатывается в его потоке, поэтому блокировок на состоянии нет.

use crate::{
    alice::{AliceEvent, Native},
    announce::{self, Announce},
    api::{ApiError, Event, EventResult, MusikApi, PlayRequest, SessionPayload, Track},
    audio::Mixer,
    config::Config,
    ducking::Ducking,
    earcon::{self, Earcon},
    intent::{self, Intent, Query},
    player::{Player, PlayerEvent},
    search::{Catalog, Target},
    util::{new_event_id, Rng},
    volume_guard::{GuardAction, VolumeGuard},
};
use serde::Serialize;
use std::{
    path::PathBuf,
    sync::{atomic::Ordering, mpsc, Arc},
    thread,
    time::{Duration, Instant},
};

pub type Apply = Box<dyn FnOnce(&mut Engine) + Send>;

pub enum Msg {
    Alice(AliceEvent),
    Player(PlayerEvent),
    Command {
        intent: Intent,
        from_voice: bool,
        reply: Option<mpsc::Sender<String>>,
    },
    Apply(Apply),
    Status(mpsc::Sender<Status>),
    /// Конфиг изменён снаружи (команда `config set`): подхватить без перезапуска.
    Reload(Box<Config>),
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Idle,
    Loading,
    Playing,
    Paused,
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub state: State,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub session: String,
    pub volume: Option<u8>,
    pub voice: bool,
    pub busy: bool,
    pub library: usize,
    pub message: String,
    pub server: String,
    /// Что показывать на часах Мини 2.
    pub display: crate::config::Display,
    /// Отдавать ли кнопки станции под управление musik.
    pub buttons: bool,
}

struct Session {
    id: String,
    mode: String,
    name: String,
}

struct Now {
    track: Track,
    generation: u64,
    started: bool,
}

pub struct Deps {
    pub config: Config,
    pub state_dir: PathBuf,
    pub api: Arc<dyn MusikApi>,
    pub player: Player,
    pub native: Arc<dyn Native>,
    pub mixer: Arc<dyn Mixer>,
    pub ducking: Arc<Ducking>,
}

pub struct Engine {
    cfg: Config,
    state_dir: PathBuf,
    api: Arc<dyn MusikApi>,
    player: Player,
    native: Arc<dyn Native>,
    mixer: Arc<dyn Mixer>,
    ducking: Arc<Ducking>,
    guard: VolumeGuard,
    catalog: Catalog,
    tx: mpsc::Sender<Msg>,
    session: Option<Session>,
    now: Option<Now>,
    state: State,
    generation: u64,
    /// Растёт с каждой пользовательской командой запуска: устаревшие ответы сети отбрасываются.
    ticket: u64,
    failures: u32,
    message: String,
    chosen_volume: Option<u8>,
    busy: bool,
    origin: Instant,
    next_library_ms: u64,
    last_progress_ms: u64,
    last_native_pause_ms: u64,
    rng: Rng,
    /// Следующий начавшийся трек нужно объявить голосом (музыку запустили голосовой командой).
    announce_next: bool,
    /// Последняя транспортная команда голосом и когда: короткая команда из журнала и тот же запрос Алисы
    /// («стоп» приходит обоими путями) не должны сработать дважды.
    last_transport: Option<(Intent, u64)>,
}

/// Повтор загрузки каталога: после успеха редко, после сбоя быстро.
const LIBRARY_REFRESH_MS: u64 = 30 * 60_000;
const LIBRARY_RETRY_MS: u64 = 30_000;
const PROGRESS_MS: u64 = 30_000;
const NATIVE_PAUSE_MS: u64 = 500;
const MAX_FAILURES: u32 = 3;

impl Engine {
    pub fn new(deps: Deps, tx: mpsc::Sender<Msg>) -> Self {
        let mut guard = VolumeGuard::new();
        guard.set_enabled(deps.config.voice);
        let chosen_volume = deps.config.volume;
        Self {
            cfg: deps.config,
            state_dir: deps.state_dir,
            api: deps.api,
            player: deps.player,
            native: deps.native,
            mixer: deps.mixer,
            ducking: deps.ducking,
            guard,
            catalog: Catalog::from_rows(Vec::new()),
            tx,
            session: None,
            now: None,
            state: State::Idle,
            generation: 0,
            ticket: 0,
            announce_next: false,
            last_transport: None,
            failures: 0,
            message: String::new(),
            chosen_volume,
            busy: false,
            origin: Instant::now(),
            next_library_ms: 0,
            last_progress_ms: 0,
            last_native_pause_ms: 0,
            rng: Rng::new(),
        }
    }

    pub fn run(mut self, rx: mpsc::Receiver<Msg>) {
        if let Some(v) = self.chosen_volume {
            let _ = self.mixer.set(v);
        }
        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Msg::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(msg) => self.handle(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            self.tick();
        }
        self.player.stop();
    }

    fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Alice(event) => self.on_alice(event),
            Msg::Player(event) => self.on_player(event),
            Msg::Command { intent, from_voice, reply } => {
                let text = self.command(intent, from_voice);
                if let Some(reply) = reply {
                    let _ = reply.send(text);
                }
            }
            Msg::Apply(apply) => apply(self),
            Msg::Status(reply) => {
                let _ = reply.send(self.status());
            }
            Msg::Reload(cfg) => {
                self.guard.set_enabled(cfg.voice);
                self.player_duck_level_changed(&cfg);
                self.api.configure(&cfg.server, &cfg.token, &cfg.device_name);
                self.cfg = *cfg;
                self.next_library_ms = 0;
            }
            Msg::Shutdown => {}
        }
    }

    fn player_duck_level_changed(&mut self, _new: &Config) {
        // Уровень приглушения читается плеером при запуске; изменение вступает в силу после перезапуска.
    }

    // ------------------------------------------------------------ фоновые задачи

    /// Выполняет `work` в отдельном потоке, а `apply` — потом в потоке движка.
    fn spawn<R: Send + 'static>(
        &self,
        work: impl FnOnce() -> R + Send + 'static,
        apply: impl FnOnce(&mut Engine, R) + Send + 'static,
    ) {
        let tx = self.tx.clone();
        let _ = thread::Builder::new().name("job".into()).spawn(move || {
            let result = work();
            let _ = tx.send(Msg::Apply(Box::new(move |engine| apply(engine, result))));
        });
    }

    /// Алиса называет начавшийся трек, если это включено и уместно.
    fn announce_track(&mut self) {
        let wanted = match self.cfg.announce {
            Announce::Off => false,
            Announce::Voice => self.announce_next,
            Announce::Always => true,
        };
        self.announce_next = false;
        if !wanted || !announce::available() {
            return;
        }
        let Some(now) = &self.now else { return };
        let text = announce::phrase(&now.track);
        if !text.is_empty() {
            self.speak(text);
        }
    }

    /// Просит Алису произнести фразу её голосом (в отдельном потоке: это занимает пару секунд).
    fn speak(&self, text: String) {
        if !announce::available() {
            eprintln!("[объявление] консоль Алисы не найдена, «{text}» не произнесено");
            return;
        }
        eprintln!("[объявление] {text}");
        let _ = thread::Builder::new().name("announce".into()).spawn(move || {
            if let Err(e) = announce::speak(&text) {
                eprintln!("[объявление] не получилось: {e}");
            }
        });
    }

    fn fire_and_forget(&self, event: Event) {
        let api = self.api.clone();
        let _ = thread::Builder::new().name("event".into()).spawn(move || {
            let _ = api.event(&event);
        });
    }

    fn tick(&mut self) {
        let now = self.now_ms();
        if self.cfg.configured() && now >= self.next_library_ms {
            self.next_library_ms = now + LIBRARY_RETRY_MS;
            let api = self.api.clone();
            self.spawn(
                move || api.library(),
                |engine, result| match result {
                    Ok(rows) => {
                        engine.catalog = Catalog::from_rows(rows);
                        engine.next_library_ms = engine.now_ms() + LIBRARY_REFRESH_MS;
                    }
                    Err(e) => engine.remember_error("каталог не загружен", &e),
                },
            );
        }
        match self.guard.poll(now) {
            Some(GuardAction::Restore) => {
                if let Some(v) = self.chosen_volume {
                    if !self.mixer.get().is_ok_and(|current| current == v) {
                        let _ = self.mixer.set(v);
                    }
                }
            }
            Some(GuardAction::Adopt) => {
                if let Ok(v) = self.mixer.get() {
                    self.choose_volume(v, false);
                }
            }
            None => {}
        }
        // Пока играет musik, штатный плеер Алисы держим на паузе: она запускает свою музыку с задержкой,
        // уже после диалога, и одного нажатия при команде не хватает.
        if self.cfg.native_pause && self.state == State::Playing && now >= self.last_native_pause_ms + NATIVE_PAUSE_MS {
            self.last_native_pause_ms = now;
            self.native.pause_native();
        }
        if self.state == State::Playing && now >= self.last_progress_ms + PROGRESS_MS {
            self.last_progress_ms = now;
            if let Some(event) = self.event_for("progress", None) {
                self.fire_and_forget(event);
            }
        }
    }

    // ------------------------------------------------------------ Алиса

    fn on_alice(&mut self, event: AliceEvent) {
        match event {
            AliceEvent::Busy(busy) => {
                if busy != self.busy {
                    eprintln!("[алиса] диалог: {}", if busy { "идёт" } else { "закончился" });
                }
                self.busy = busy;
                self.ducking.set_busy(busy);
                self.guard.activity(busy, self.now_ms());
            }
            AliceEvent::Recognized { text } => {
                if !self.cfg.voice {
                    eprintln!("[алиса] распознано «{text}» — голос выключен, пропускаю");
                    return;
                }
                if announce::is_echo(&intent::normalize(&text)) {
                    // Наше же «повтори за мной …» вернулось событием Алисы.
                    return;
                }
                if intent::native_volume_request(&text) {
                    eprintln!("[алиса] распознано «{text}» — громкость, остаётся Алисе");
                    self.guard.native_command();
                    return;
                }
                let Some(parsed) = intent::from_speech(&text) else {
                    eprintln!("[алиса] распознано «{text}» — не музыкальная команда, остаётся Алисе");
                    return;
                };
                if parsed.needs_active_session() && self.session.is_none() {
                    eprintln!("[алиса] распознано «{text}» — {parsed}, но musik сейчас не играет: остаётся Алисе");
                    return;
                }
                let mut parsed = parsed;
                if matches!(parsed, Intent::Resume) && self.state != State::Paused {
                    // «Включи музыку» без паузы — просьба поставить что-нибудь: отдаём её персональному радио.
                    // Голое «продолжи» остаётся Алисе.
                    if self.cfg.voice_radio_fallback && intent::normalize(&text).ends_with("музыку") {
                        parsed = Intent::Radio;
                    } else {
                        eprintln!("[алиса] распознано «{text}» — продолжать нечего: остаётся Алисе");
                        return;
                    }
                }
                if self.transport_duplicate(&parsed) {
                    eprintln!("[алиса] распознано «{text}» — уже выполнено по короткой команде");
                    return;
                }
                eprintln!("[алиса] распознано «{text}» → {parsed}");
                let reply = self.command(parsed, true);
                eprintln!("[алиса] ответ: {reply}");
                self.message = reply;
            }
            AliceEvent::Spotted { text } => {
                if !self.cfg.voice || !self.cfg.quick_commands {
                    return;
                }
                let parsed = intent::from_speech(&text);
                let Some(parsed) =
                    parsed.filter(|p| matches!(p, Intent::Next | Intent::Previous | Intent::Pause | Intent::Resume))
                else {
                    return;
                };
                if self.session.is_none() {
                    eprintln!("[алиса] без обращения «{text}» — musik не играет, остаётся Алисе");
                    return;
                }
                if matches!(parsed, Intent::Resume) && self.state != State::Paused {
                    return;
                }
                if self.transport_duplicate(&parsed) {
                    return;
                }
                eprintln!("[алиса] без обращения «{text}» → {parsed}");
                // Диалога нет — отменять у Алисы нечего.
                self.message = self.command(parsed, false);
            }
        }
    }

    /// Та же транспортная команда меньше чем через 2,5 с — повтор одной фразы, пришедшей двумя путями.
    fn transport_duplicate(&mut self, intent: &Intent) -> bool {
        if !matches!(intent, Intent::Next | Intent::Previous | Intent::Pause | Intent::Resume | Intent::Toggle) {
            return false;
        }
        let now = self.now_ms();
        if let Some((last, at)) = &self.last_transport {
            if last == intent && now.saturating_sub(*at) < 2500 {
                return true;
            }
        }
        self.last_transport = Some((intent.clone(), now));
        false
    }

    // ------------------------------------------------------------ плеер

    fn on_player(&mut self, event: PlayerEvent) {
        match event {
            PlayerEvent::Started { generation } => {
                let Some(now) = self.now.as_mut().filter(|n| n.generation == generation) else { return };
                now.started = true;
                self.state = State::Playing;
                self.failures = 0;
                self.last_progress_ms = self.now_ms();
                if let Some(event) = self.event_for("track_start", None) {
                    self.fire_and_forget(event);
                }
                self.announce_track();
            }
            PlayerEvent::Ended { generation, seconds } => {
                if !self.is_current(generation) {
                    return;
                }
                let Some(mut event) = self.event_for("track_end", Some("completed")) else { return };
                event.listened_sec = seconds;
                event.position_sec = seconds;
                self.advance(event, false);
            }
            PlayerEvent::Failed { generation, message, started } => {
                if !self.is_current(generation) {
                    return;
                }
                self.failures += 1;
                self.message = format!("не удалось проиграть: {message}");
                if self.failures >= MAX_FAILURES || started && self.failures > 1 {
                    self.finish("слишком много ошибок подряд, остановился");
                    self.signal(Earcon::Fail);
                    return;
                }
                // Битый трек пропускаем как обычный скип: сервер сам выберет следующий.
                if let Some(event) = self.event_for("skip", Some("skipped")) {
                    self.advance(event, false);
                }
            }
        }
    }

    fn is_current(&self, generation: u64) -> bool {
        self.now.as_ref().is_some_and(|n| n.generation == generation)
    }

    /// Отправляет событие конца/пропуска и по его ответу запускает следующий трек.
    fn advance(&mut self, event: Event, dislike_first: bool) {
        let ticket = self.ticket;
        let generation = self.now.as_ref().map(|n| n.generation);
        let api = self.api.clone();
        self.spawn(
            move || {
                if dislike_first {
                    let mut dislike = event.clone();
                    dislike.kind = "dislike";
                    dislike.event_id = new_event_id();
                    dislike.reason = None;
                    let _ = api.event(&dislike);
                }
                api.event(&event)
            },
            move |engine, result| {
                // Пока шёл запрос, пользователь мог запустить что-то другое.
                if engine.ticket != ticket || engine.now.as_ref().map(|n| n.generation) != generation {
                    return;
                }
                engine.after_event(result);
            },
        );
    }

    fn after_event(&mut self, result: Result<EventResult, ApiError>) {
        match result {
            Ok(EventResult { next: Some(track), session_id, .. }) => {
                if let Some(session) = self.session.as_mut() {
                    if !session_id.is_empty() {
                        session.id = session_id;
                    }
                }
                self.play_track(track);
            }
            Ok(_) => self.finish("очередь закончилась"),
            Err(e) => {
                self.remember_error("сервер не ответил", &e);
                self.finish("");
                self.signal(Earcon::Fail);
            }
        }
    }

    // ------------------------------------------------------------ команды

    pub fn command(&mut self, intent: Intent, from_voice: bool) -> String {
        if !self.cfg.configured() {
            return "musik не настроен: выполните «musikalice login»".into();
        }
        if intent.needs_active_session() && self.session.is_none() {
            return "сейчас ничего не играет".into();
        }
        let text = match intent {
            Intent::Play(query) => self.play_query(&query, from_voice),
            Intent::Radio => {
                self.begin(from_voice);
                let api = self.api.clone();
                self.start_session("радио", move || api.radio_start(None));
                "Включаю радио".into()
            }
            Intent::Mix(name) => self.play_mix(&name, from_voice),
            Intent::Resume => {
                if self.state == State::Paused {
                    self.resume(from_voice);
                    "Продолжаю".into()
                } else {
                    "нечего продолжать".into()
                }
            }
            Intent::Pause => {
                self.pause();
                "Пауза".into()
            }
            Intent::Toggle => {
                if self.state == State::Paused {
                    self.resume(from_voice);
                    "Продолжаю".into()
                } else {
                    self.pause();
                    "Пауза".into()
                }
            }
            Intent::Next => {
                self.skip(false);
                "Следующий".into()
            }
            Intent::Previous => {
                self.previous();
                "Предыдущий".into()
            }
            Intent::Volume(v) => {
                self.choose_volume(v.min(100), true);
                format!("Громкость {v}")
            }
            Intent::Like => {
                self.like();
                "В избранное".into()
            }
            Intent::Dislike => {
                self.skip(true);
                "Больше не включу".into()
            }
            Intent::Voice(on) => {
                self.cfg.voice = on;
                self.guard.set_enabled(on);
                self.save_config();
                if on {
                    "Голосовое управление включено"
                } else {
                    "Голосовое управление выключено"
                }
                .into()
            }
            Intent::Release => {
                self.finish("колонка освобождена");
                self.player.stop();
                "Колонка свободна для Алисы".into()
            }
            Intent::WhatsPlaying => match self.now.as_ref().map(|n| announce::phrase(&n.track)) {
                Some(text) if !text.is_empty() => {
                    if from_voice {
                        // Свой ответ Алисы («не знаю, что играет») обрываем и отвечаем её же голосом.
                        self.native.cancel();
                        self.speak(text.clone());
                    }
                    format!("Играет: {text}")
                }
                _ => "Сейчас ничего не играет".into(),
            },
            Intent::Retry => {
                self.next_library_ms = 0;
                "Обновляю каталог".into()
            }
        };
        self.message = text.clone();
        text
    }

    /// Общая подготовка к запуску нового источника музыки.
    fn begin(&mut self, from_voice: bool) {
        self.ticket += 1;
        self.failures = 0;
        self.native.prepare_playback(from_voice, self.cfg.native_pause);
        self.announce_next = from_voice;
        if let Some(now) = &self.now {
            // Закрываем прежний трек как пропущенный, чтобы вкус не терял сигнал.
            if now.started {
                if let Some(event) = self.event_for("skip", Some("skipped")) {
                    self.fire_and_forget(event);
                }
            }
        }
    }

    fn play_query(&mut self, query: &Query, from_voice: bool) -> String {
        // Голосовой запрос без совпадения молча остаётся Алисе: сигнал «не вышло» поверх её ответа только мешал бы.
        if self.catalog.is_empty() {
            if !from_voice {
                self.signal(Earcon::Fail);
            }
            return "каталог ещё не загружен, попробуйте через минуту".into();
        }
        let Some(found) = self.catalog.search(query, self.cfg.search_threshold) else {
            if !from_voice {
                self.signal(Earcon::Fail);
            }
            return format!("Не нашла «{}»", query.text);
        };
        self.begin(from_voice);
        let api = self.api.clone();
        match found.target {
            Target::Track(id) => {
                let label = self.catalog.row(id).map(|r| format!("{} — {}", r.artist, r.title)).unwrap_or_default();
                if self.cfg.track_mode == crate::config::TrackMode::Single {
                    let request = PlayRequest { track_ids: vec![id], name: label.clone(), start_track_id: Some(id) };
                    self.start_session("трек", move || api.play(&request));
                } else {
                    self.start_session("радио по треку", move || api.radio_start(Some(id)));
                }
                format!("Включаю {label}")
            }
            Target::Artist(name) => {
                let mut ids = self.catalog.artist_tracks(&name);
                if self.cfg.shuffle_artist {
                    self.rng.shuffle(&mut ids);
                }
                let request = PlayRequest { track_ids: ids, name: name.clone(), start_track_id: None };
                self.start_session("исполнитель", move || api.play(&request));
                format!("Включаю {name}")
            }
            Target::Album { artist, album } => {
                let ids = self.catalog.album_tracks(&artist, &album);
                let name = format!("{artist} — {album}");
                let request = PlayRequest { track_ids: ids, name: name.clone(), start_track_id: None };
                self.start_session("альбом", move || api.play(&request));
                format!("Включаю альбом {name}")
            }
        }
    }

    fn play_mix(&mut self, wanted: &str, from_voice: bool) -> String {
        self.begin(from_voice);
        let (api, wanted) = (self.api.clone(), wanted.to_owned());
        let label = wanted.clone();
        self.start_session("микс", move || {
            let kind = resolve_mix(&*api, &wanted)?;
            api.mix_play(&kind)
        });
        format!("Включаю {label}")
    }

    /// Запускает сетевой запрос сессии; по его результату начинает играть первый трек.
    fn start_session(
        &mut self,
        what: &'static str,
        work: impl FnOnce() -> Result<SessionPayload, ApiError> + Send + 'static,
    ) {
        let ticket = self.ticket;
        self.state = State::Loading;
        self.spawn(work, move |engine, result| {
            if engine.ticket != ticket {
                return;
            }
            match result {
                Ok(SessionPayload { session_id, mode, name, current: Some(track), .. }) => {
                    engine.session =
                        Some(Session { id: session_id, mode: if mode.is_empty() { what.into() } else { mode }, name });
                    engine.play_track(track);
                    engine.signal(Earcon::Ok);
                }
                Ok(_) => {
                    engine.message = format!("{what}: сервер не вернул треков");
                    engine.finish("нечего играть");
                    engine.signal(Earcon::Fail);
                }
                Err(e) => {
                    engine.remember_error(what, &e);
                    engine.finish("");
                    engine.signal(Earcon::Fail);
                }
            }
        });
    }

    fn play_track(&mut self, track: Track) {
        self.generation += 1;
        let url = self.cfg.stream_url(track.id, track.stream.as_deref());
        self.now = Some(Now { track, generation: self.generation, started: false });
        self.state = State::Loading;
        self.player.load(self.generation, url);
    }

    fn pause(&mut self) {
        if matches!(self.state, State::Playing | State::Loading) {
            self.player.pause();
            self.state = State::Paused;
        }
    }

    fn resume(&mut self, from_voice: bool) {
        self.native.prepare_playback(from_voice, self.cfg.native_pause);
        self.player.resume();
        self.state = if self.now.as_ref().is_some_and(|n| n.started) { State::Playing } else { State::Loading };
    }

    fn skip(&mut self, dislike: bool) {
        let Some(mut event) = self.event_for("skip", Some("skipped")) else { return };
        event.listened_sec = event.position_sec;
        self.ticket += 1;
        self.player.stop();
        self.state = State::Loading;
        self.advance(event, dislike);
    }

    fn previous(&mut self) {
        let Some(session) = self.session.as_ref() else { return };
        let id = session.id.clone();
        self.ticket += 1;
        let ticket = self.ticket;
        let api = self.api.clone();
        self.player.stop();
        self.state = State::Loading;
        self.spawn(
            move || api.session_back(&id),
            move |engine, result| {
                if engine.ticket != ticket {
                    return;
                }
                match result {
                    Ok(SessionPayload { current: Some(track), .. }) => engine.play_track(track),
                    Ok(_) => engine.message = "предыдущего трека нет".into(),
                    Err(e) => engine.remember_error("предыдущий трек", &e),
                }
            },
        );
    }

    fn like(&mut self) {
        let Some(track_id) = self.now.as_ref().map(|n| n.track.id) else { return };
        if let Some(event) = self.event_for("like", None) {
            self.fire_and_forget(event);
        }
        let api = self.api.clone();
        self.spawn(
            move || api.favorite_add(track_id),
            |engine, result| {
                if let Err(e) = result {
                    engine.remember_error("избранное", &e);
                }
            },
        );
    }

    fn choose_volume(&mut self, percent: u8, user: bool) {
        self.chosen_volume = Some(percent);
        self.cfg.volume = Some(percent);
        if user {
            self.guard.user_set();
            let _ = self.mixer.set(percent);
        }
        self.save_config();
    }

    fn save_config(&self) {
        if let Err(e) = self.cfg.save(&self.state_dir) {
            eprintln!("не удалось сохранить настройки: {e}");
        }
    }

    // ------------------------------------------------------------ вспомогательное

    fn finish(&mut self, why: &str) {
        self.ticket += 1;
        self.player.stop();
        self.session = None;
        self.now = None;
        self.state = State::Idle;
        if !why.is_empty() {
            self.message = why.to_owned();
        }
    }

    fn signal(&self, kind: Earcon) {
        if self.cfg.earcons {
            self.player.earcon(earcon::render(kind, self.player.rate()));
        }
    }

    fn remember_error(&mut self, what: &str, error: &ApiError) {
        self.message = if error.is_unauthorized() {
            "токен недействителен: выполните «musikalice login»".into()
        } else {
            format!("{what}: {error}")
        };
    }

    fn event_for(&self, kind: &'static str, reason: Option<&'static str>) -> Option<Event> {
        let now = self.now.as_ref()?;
        let session = self.session.as_ref()?;
        let position = self.player.shared.position_ms.load(Ordering::Relaxed) as f64 / 1000.0;
        let duration = match self.player.shared.duration_ms.load(Ordering::Relaxed) {
            0 => now.track.duration,
            ms => ms as f64 / 1000.0,
        };
        Some(Event {
            kind,
            event_id: new_event_id(),
            session_id: session.id.clone(),
            track_id: now.track.id,
            impression_id: now.track.impression_id.clone(),
            position_sec: position,
            duration_sec: duration,
            listened_sec: position,
            reason,
        })
    }

    pub fn status(&self) -> Status {
        let (artist, title, album) = self
            .now
            .as_ref()
            .map(|n| (n.track.artist.clone(), n.track.title.clone(), n.track.album.clone()))
            .unwrap_or_default();
        Status {
            state: self.state,
            artist,
            title,
            album,
            position_ms: self.player.shared.position_ms.load(Ordering::Relaxed),
            duration_ms: match self.player.shared.duration_ms.load(Ordering::Relaxed) {
                0 => self.now.as_ref().map_or(0, |n| (n.track.duration * 1000.0) as u64),
                ms => ms,
            },
            session: self
                .session
                .as_ref()
                .map(|s| if s.name.is_empty() { s.mode.clone() } else { format!("{}: {}", s.mode, s.name) })
                .unwrap_or_default(),
            volume: self.mixer.get().ok(),
            voice: self.cfg.voice,
            busy: self.busy,
            library: self.catalog.len(),
            message: self.message.clone(),
            server: self.cfg.server.clone(),
            display: self.cfg.display,
            buttons: self.cfg.buttons,
        }
    }
}

/// «Микс дня» / `daily` / «избранное» → `kind` полки сервера.
fn resolve_mix(api: &dyn MusikApi, wanted: &str) -> Result<String, ApiError> {
    let wanted_norm = intent::normalize(wanted);
    let mixes = api.mixes()?;
    let norm = |s: &str| intent::normalize(s);
    if let Some(m) = mixes.iter().find(|m| m.ready && m.kind == wanted) {
        return Ok(m.kind.clone());
    }
    if let Some(m) = mixes.iter().filter(|m| m.ready).find(|m| {
        let (title, name) = (norm(&m.title), norm(&m.name));
        title == wanted_norm
            || name == wanted_norm
            || title.contains(&wanted_norm)
            || wanted_norm.contains(&title) && !title.is_empty()
    }) {
        return Ok(m.kind.clone());
    }
    // Сервер сам знает свои полки: если в списке нет, всё равно пробуем как есть.
    Ok(wanted.to_owned())
}

/// Запуск движка в отдельном потоке. Возвращает отправитель сообщений и дескриптор потока.
fn spawn_engine(deps: Deps, tx: mpsc::Sender<Msg>, rx: mpsc::Receiver<Msg>) -> thread::JoinHandle<()> {
    let engine = Engine::new(deps, tx);
    thread::Builder::new().name("engine".into()).spawn(move || engine.run(rx)).expect("поток движка")
}

/// Дескриптор запущенного движка.
pub struct Handle {
    pub tx: mpsc::Sender<Msg>,
    join: Option<thread::JoinHandle<()>>,
}

impl Handle {
    /// Отправляет команду и ждёт короткий текстовый ответ.
    pub fn request(&self, intent: Intent, from_voice: bool) -> Option<String> {
        let (reply, answer) = mpsc::channel();
        self.tx.send(Msg::Command { intent, from_voice, reply: Some(reply) }).ok()?;
        answer.recv_timeout(Duration::from_secs(5)).ok()
    }

    pub fn status(&self) -> Option<Status> {
        let (reply, answer) = mpsc::channel();
        self.tx.send(Msg::Status(reply)).ok()?;
        answer.recv_timeout(Duration::from_secs(5)).ok()
    }

    pub fn shutdown(mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Собирает плеер и движок, подключает события плеера к движку.
pub fn launch<A>(
    config: Config,
    state_dir: PathBuf,
    api: Arc<A>,
    output: Box<dyn crate::audio::Output>,
    native: Arc<dyn Native>,
    mixer: Arc<dyn Mixer>,
) -> Handle
where
    A: MusikApi + crate::api::Fetcher + 'static,
{
    let (tx, rx) = mpsc::channel();
    let ducking = Arc::new(Ducking::new());
    let events = std::sync::Mutex::new(tx.clone());
    let player = Player::start(output, api.clone(), ducking.clone(), config.duck_level, move |event| {
        let _ = events.lock().unwrap().send(Msg::Player(event));
    });
    let deps = Deps { config, state_dir, api, player, native, mixer, ducking };
    let join = spawn_engine(deps, tx.clone(), rx);
    Handle { tx, join: Some(join) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::HttpApi,
        audio::{NullMixer, NullOutput},
        testutil::{FakeServer, Request, Response},
    };
    use std::sync::Mutex;

    const MP3: &[u8] = include_bytes!("../tests/fixtures/tone.mp3");

    fn library_json() -> &'static str {
        r#"[
        {"id":1,"artist":"Radiohead","title":"Creep","album":"Pablo Honey","duration":1,"ready":true},
        {"id":2,"artist":"Radiohead","title":"Karma Police","album":"OK Computer","duration":1,"ready":true},
        {"id":3,"artist":"Queen","title":"Bohemian Rhapsody","album":"A Night at the Opera","duration":1,"ready":true}
        ]"#
    }

    fn track_json(id: i64) -> String {
        format!(
            r#"{{"id":{id},"artist":"Radiohead","title":"T{id}","album":"A","duration":1,"stream":"/api/stream/{id}","impression_id":"imp{id}"}}"#
        )
    }

    fn handler(req: &Request) -> Response {
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null);
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/api/library") => Response::json(200, library_json()),
            ("POST", "/api/radio/start") => {
                let id = body["seed_track_id"].as_i64().unwrap_or(1);
                Response::json(200, &format!(r#"{{"session_id":"s1","mode":"radio","current":{}}}"#, track_json(id)))
            }
            ("POST", "/api/play") => {
                let id = body["track_ids"][0].as_i64().unwrap_or(1);
                Response::json(
                    200,
                    &format!(r#"{{"session_id":"s2","mode":"playlist","name":"x","current":{}}}"#, track_json(id)),
                )
            }
            ("POST", "/api/session/back") => {
                Response::json(200, &format!(r#"{{"session_id":"s1","current":{}}}"#, track_json(1)))
            }
            ("POST", "/api/events") => match body["type"].as_str() {
                Some("track_end") | Some("skip") if body["track_id"] == 1 => {
                    Response::json(200, &format!(r#"{{"ok":true,"next":{},"session_id":"s1"}}"#, track_json(2)))
                }
                Some("track_end") | Some("skip") => Response::json(200, r#"{"ok":true,"ended":true}"#),
                _ => Response::json(200, r#"{"ok":true}"#),
            },
            ("POST", "/api/favorites") => Response::json(200, r#"{"ok":true}"#),
            ("GET", p) if p.starts_with("/api/stream/") => Response::bytes(200, "audio/mpeg", MP3.to_vec()),
            ("GET", "/api/mixes") => {
                Response::json(200, r#"{"mixes":[{"kind":"daily","title":"Микс дня","ready":true}]}"#)
            }
            ("POST", "/api/mixes/daily/play") => Response::json(
                200,
                &format!(r#"{{"session_id":"s3","mode":"mix","name":"Микс дня","current":{}}}"#, track_json(3)),
            ),
            _ => Response::json(404, r#"{"error":"нет","code":"not_found"}"#),
        }
    }

    #[derive(Default)]
    struct FakeNative {
        calls: Mutex<Vec<String>>,
    }
    impl Native for FakeNative {
        fn prepare_playback(&self, from_voice: bool, pause: bool) {
            self.calls.lock().unwrap().push(format!("prepare:{from_voice}:{pause}"));
        }
        fn pause_native(&self) {
            self.calls.lock().unwrap().push("pause".into());
        }
        fn cancel(&self) {
            self.calls.lock().unwrap().push("cancel".into());
        }
    }

    struct Rig {
        server: FakeServer,
        handle: Handle,
        native: Arc<FakeNative>,
        mixer: Arc<NullMixer>,
        dir: PathBuf,
    }

    impl Rig {
        fn new() -> Self {
            Self::build(false)
        }

        /// Вывод идёт в реальном времени: трек длится столько, сколько звучит.
        fn paced() -> Self {
            Self::build(true)
        }

        fn build(paced: bool) -> Self {
            let server = FakeServer::start(handler);
            let dir = std::env::temp_dir().join(format!("musikalice-engine-{}-{}", std::process::id(), new_event_id()));
            let config = Config { server: server.url(), token: "tok".into(), earcons: false, ..Config::default() };
            let api = Arc::new(HttpApi::new(&config.server, &config.token, "Кухня"));
            let native = Arc::new(FakeNative::default());
            let mixer = Arc::new(NullMixer::new(50));
            let sink = Arc::new(Mutex::new(Vec::new()));
            let output = Box::new(if paced { NullOutput::paced(44100, sink) } else { NullOutput::new(44100, sink) });
            let handle = launch(config, dir.clone(), api, output, native.clone(), mixer.clone());
            Self { server, handle, native, mixer, dir }
        }

        fn wait(&self, what: &str, ok: impl Fn(&Status) -> bool) -> Status {
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                let status = self.handle.status().expect("статус");
                if ok(&status) {
                    return status;
                }
                assert!(Instant::now() < deadline, "не дождались: {what}; статус {status:?}");
                thread::sleep(Duration::from_millis(30));
            }
        }

        fn ready(&self) {
            self.wait("каталог", |s| s.library == 3);
        }

        fn event_types(&self) -> Vec<String> {
            self.server
                .requests()
                .iter()
                .filter(|r| r.path == "/api/events")
                .map(|r| {
                    let b: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
                    format!("{}:{}", b["type"].as_str().unwrap(), b["track_id"])
                })
                .collect()
        }

        fn say(&self, text: &str) {
            self.handle.tx.send(Msg::Alice(AliceEvent::Recognized { text: text.into() })).unwrap();
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn login_after_start_takes_effect_without_restart() {
        let server = FakeServer::start(handler);
        let dir = std::env::temp_dir().join(format!("musikalice-late-login-{}", new_event_id()));
        // Служба стартует до `login`: пустые адрес и токен.
        let api = Arc::new(HttpApi::new("", "", ""));
        let output = Box::new(NullOutput::new(44100, Arc::new(Mutex::new(Vec::new()))));
        let handle = launch(
            Config { earcons: false, ..Config::default() },
            dir.clone(),
            api,
            output,
            Arc::new(FakeNative::default()),
            Arc::new(NullMixer::new(10)),
        );
        assert_eq!(handle.status().unwrap().library, 0);
        let configured = Config { server: server.url(), token: "tok".into(), earcons: false, ..Config::default() };
        handle.tx.send(Msg::Reload(Box::new(configured))).unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        while handle.status().unwrap().library != 3 {
            assert!(Instant::now() < deadline, "каталог не загрузился: {:?}", handle.status().unwrap().message);
            thread::sleep(Duration::from_millis(50));
        }
        handle.request(Intent::Radio, false).unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        while handle.status().unwrap().state != State::Playing {
            assert!(Instant::now() < deadline, "музыка не пошла: {:?}", handle.status().unwrap().message);
            thread::sleep(Duration::from_millis(50));
        }
        let auth = server.requests().into_iter().find(|r| r.path == "/api/library").unwrap().header("authorization");
        assert_eq!(auth.as_deref(), Some("Bearer tok"));
        handle.shutdown();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unconfigured_engine_explains_what_to_do() {
        let dir = std::env::temp_dir().join("musikalice-unconfigured");
        let api = Arc::new(HttpApi::new("", "", ""));
        let output = Box::new(NullOutput::new(44100, Arc::new(Mutex::new(Vec::new()))));
        let handle =
            launch(Config::default(), dir, api, output, Arc::new(FakeNative::default()), Arc::new(NullMixer::new(10)));
        assert!(handle.request(Intent::Radio, false).unwrap().contains("login"));
        handle.shutdown();
    }

    #[test]
    fn radio_plays_reports_start_and_advances_through_the_server_queue() {
        let rig = Rig::new();
        rig.ready();
        rig.handle.request(Intent::Radio, true).unwrap();
        let playing = rig.wait("играет первый трек", |s| s.state == State::Playing && s.title == "T1");
        assert_eq!(playing.session, "radio");
        assert_eq!(rig.native.calls.lock().unwrap().first().map(String::as_str), Some("prepare:true:true"));
        // Трек доиграл → track_end → сервер отдаёт второй → потом очередь заканчивается.
        rig.wait("играет второй трек", |s| s.title == "T2");
        rig.wait("очередь закончилась", |s| s.state == State::Idle);
        let events = rig.event_types();
        assert_eq!(events[0], "track_start:1");
        assert!(events.contains(&"track_end:1".to_string()), "{events:?}");
        assert!(events.contains(&"track_start:2".to_string()), "{events:?}");
        assert!(events.contains(&"track_end:2".to_string()), "{events:?}");
        let start = rig.server.requests().into_iter().find(|r| r.path == "/api/events").unwrap();
        let body: serde_json::Value = serde_json::from_slice(&start.body).unwrap();
        assert_eq!(body["impression_id"], "imp1");
        assert_eq!(body["session_id"], "s1");
    }

    #[test]
    fn spoken_artist_in_russian_finds_the_latin_library_entry() {
        let rig = Rig::new();
        rig.ready();
        rig.say("Алиса, включи Радиохед");
        let status = rig.wait("играет исполнитель", |s| s.state == State::Playing);
        assert!(status.session.starts_with("playlist"), "{status:?}");
        let play = rig.server.requests().into_iter().find(|r| r.path == "/api/play").expect("запрос /api/play");
        let body: serde_json::Value = serde_json::from_slice(&play.body).unwrap();
        let mut ids: Vec<i64> = body["track_ids"].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2]);
        assert_eq!(body["name"], "Radiohead");
    }

    #[test]
    fn transport_commands_need_an_active_session_and_work_with_one() {
        let rig = Rig::new();
        rig.ready();
        rig.say("следующий трек");
        thread::sleep(Duration::from_millis(200));
        assert_eq!(rig.handle.status().unwrap().state, State::Idle, "без сессии фраза принадлежит Алисе");
        assert!(rig.native.calls.lock().unwrap().is_empty());

        rig.handle.request(Intent::Radio, false).unwrap();
        rig.wait("играет", |s| s.state == State::Playing);
        rig.handle.request(Intent::Pause, false).unwrap();
        assert_eq!(rig.handle.status().unwrap().state, State::Paused);
        rig.handle.request(Intent::Toggle, false).unwrap();
        rig.wait("продолжил", |s| s.state == State::Playing);
        rig.handle.request(Intent::Like, false).unwrap();
        rig.handle.request(Intent::Next, false).unwrap();
        rig.wait("второй трек после next", |s| s.title == "T2");
        rig.handle.request(Intent::Previous, false).unwrap();
        rig.wait("назад к первому", |s| s.title == "T1");
        let events = rig.event_types();
        assert!(events.contains(&"like:1".to_string()), "{events:?}");
        assert!(events.contains(&"skip:1".to_string()), "{events:?}");
        assert!(rig.server.requests().iter().any(|r| r.path == "/api/favorites"));
    }

    #[test]
    fn plain_play_music_phrase_starts_the_radio_unless_disabled() {
        let rig = Rig::new();
        rig.say("Алиса, включи музыку");
        rig.wait("радио по фразе", |s| s.state == State::Playing && s.session == "radio");
        rig.handle.request(Intent::Release, false).unwrap();
        rig.say("продолжи");
        thread::sleep(Duration::from_millis(200));
        assert_eq!(rig.handle.status().unwrap().state, State::Idle, "голое «продолжи» принадлежит Алисе");
        let mut cfg = Config::load(&rig.dir).unwrap_or_default();
        cfg.server = rig.server.url();
        cfg.token = "tok".into();
        cfg.earcons = false;
        cfg.voice_radio_fallback = false;
        rig.handle.tx.send(Msg::Reload(Box::new(cfg))).unwrap();
        rig.say("включи музыку");
        thread::sleep(Duration::from_millis(300));
        assert_eq!(rig.handle.status().unwrap().state, State::Idle);
    }

    #[test]
    fn native_player_is_kept_paused_only_while_musik_plays() {
        let rig = Rig::paced();
        let pauses = |rig: &Rig| rig.native.calls.lock().unwrap().iter().filter(|c| *c == "pause").count();
        rig.handle.request(Intent::Radio, true).unwrap();
        rig.wait("играет", |s| s.state == State::Playing);
        thread::sleep(Duration::from_millis(1300));
        assert!(pauses(&rig) >= 2, "паузу штатного плеера надо повторять, было {}", pauses(&rig));
        // На паузе musik Алиса свободна: повторять больше не нужно.
        rig.handle.request(Intent::Pause, false).unwrap();
        thread::sleep(Duration::from_millis(300));
        let settled = pauses(&rig);
        thread::sleep(Duration::from_millis(1100));
        assert_eq!(pauses(&rig), settled);
    }

    #[test]
    fn dislike_reports_the_rating_then_skips() {
        let rig = Rig::new();
        rig.handle.request(Intent::Radio, false).unwrap();
        rig.wait("играет", |s| s.state == State::Playing && s.title == "T1");
        rig.handle.request(Intent::Dislike, false).unwrap();
        rig.wait("следующий", |s| s.title == "T2");
        let events = rig.event_types();
        let dislike = events.iter().position(|e| e == "dislike:1").expect("dislike");
        let skip = events.iter().position(|e| e == "skip:1").expect("skip");
        assert!(dislike < skip, "{events:?}");
    }

    #[test]
    fn named_mix_resolves_through_the_shelf_list() {
        let rig = Rig::new();
        rig.handle.request(Intent::Mix("микс дня".into()), true).unwrap();
        let status = rig.wait("микс играет", |s| s.state == State::Playing);
        assert_eq!(status.title, "T3");
        assert!(status.session.starts_with("mix"));
    }

    #[test]
    fn unknown_music_is_refused_and_leaves_alice_alone() {
        let rig = Rig::new();
        rig.ready();
        let answer = rig.handle.request(Intent::Play(Query { text: "zzzz qqqq".into(), kind: None }), true).unwrap();
        assert!(answer.contains("Не нашла"), "{answer}");
        assert!(rig.native.calls.lock().unwrap().is_empty(), "штатный плеер Алисы не трогаем");
        assert_eq!(rig.handle.status().unwrap().state, State::Idle);
    }

    #[test]
    fn server_failure_stops_cleanly_with_a_message() {
        let rig = Rig::new();
        // Трек 99 найдётся в радио-запросе, но стрим вернёт ошибку: имитируем неработающим сервером.
        let dead = FakeServer::start(|_| Response::json(500, r#"{"error":"сломался","code":"internal"}"#));
        let config = Config { server: dead.url(), token: "t".into(), earcons: false, ..Config::default() };
        let api = Arc::new(HttpApi::new(&config.server, &config.token, "d"));
        let output = Box::new(NullOutput::new(44100, Arc::new(Mutex::new(Vec::new()))));
        let handle = launch(
            config,
            rig.dir.join("dead"),
            api,
            output,
            Arc::new(FakeNative::default()),
            Arc::new(NullMixer::new(10)),
        );
        handle.request(Intent::Radio, false).unwrap();
        thread::sleep(Duration::from_millis(500));
        let status = handle.status().unwrap();
        assert_eq!(status.state, State::Idle);
        assert!(status.message.contains("сломался"), "{}", status.message);
        handle.shutdown();
    }

    #[test]
    fn volume_is_remembered_and_restored_after_a_dialog() {
        let rig = Rig::new();
        rig.handle.request(Intent::Volume(40), false).unwrap();
        assert_eq!(rig.mixer.get().unwrap(), 40);
        // Алиса во время диалога меняет Master.
        rig.handle.tx.send(Msg::Alice(AliceEvent::Busy(true))).unwrap();
        rig.mixer.set(90).unwrap();
        rig.handle.tx.send(Msg::Alice(AliceEvent::Busy(false))).unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        while rig.mixer.get().unwrap() != 40 {
            assert!(Instant::now() < deadline, "громкость не вернулась");
            thread::sleep(Duration::from_millis(50));
        }
        let saved = Config::load(&rig.dir).unwrap();
        assert_eq!(saved.volume, Some(40));
    }

    #[test]
    fn release_frees_the_speaker_and_voice_switch_persists() {
        let rig = Rig::new();
        rig.handle.request(Intent::Radio, false).unwrap();
        rig.wait("играет", |s| s.state == State::Playing);
        rig.handle.request(Intent::Release, false).unwrap();
        let status = rig.handle.status().unwrap();
        assert_eq!((status.state, status.title.as_str()), (State::Idle, ""));
        rig.handle.request(Intent::Voice(false), false).unwrap();
        rig.say("включи радиохед");
        thread::sleep(Duration::from_millis(300));
        assert_eq!(rig.handle.status().unwrap().state, State::Idle, "при выключенном голосе фразы игнорируются");
        assert!(!Config::load(&rig.dir).unwrap().voice);
    }

    #[test]
    fn whats_playing_is_answered_in_alices_voice() {
        let dir = std::env::temp_dir().join(format!("musikalice-maind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("stdin.txt");
        let fake = dir.join("maind");
        std::fs::write(&fake, format!("#!/bin/sh\ncat >> '{}'\n", log.display())).unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        // Переменная действует на весь процесс; в остальных тестах объявления выключены.
        std::env::set_var("MUSIKALICE_MAIND", &fake);
        let read = || std::fs::read_to_string(&log).unwrap_or_default();

        let rig = Rig::paced();
        // Без вопроса Алиса молчит, даже при голосовом запуске (announce = off).
        rig.handle.request(Intent::Radio, true).unwrap();
        rig.wait("играет", |s| s.state == State::Playing);
        rig.say("Алиса, что сейчас играет?");
        let deadline = Instant::now() + Duration::from_secs(6);
        while !read().contains("x\n") {
            assert!(Instant::now() < deadline, "ответ не ушёл: {:?}", read());
            thread::sleep(Duration::from_millis(50));
        }
        let sent = read();
        assert_eq!(sent.matches("повтори за мной").count(), 1, "{sent:?}");
        assert!(sent.contains("q\nповтори за мной Radiohead, T"), "{sent:?}");
        assert!(rig.native.calls.lock().unwrap().contains(&"cancel".to_string()), "свой ответ Алисы надо оборвать");
        // Эхо нашего запроса не должно превратиться в команду.
        let prepares = || rig.native.calls.lock().unwrap().iter().filter(|c| c.starts_with("prepare")).count();
        let before = prepares();
        rig.say("повтори за мной Radiohead, T1");
        thread::sleep(Duration::from_millis(200));
        assert_eq!(prepares(), before, "эхо запустило команду");
    }

    #[test]
    fn quick_next_without_alice_works_once_even_if_alice_repeats_it() {
        let rig = Rig::paced();
        rig.handle.request(Intent::Radio, false).unwrap();
        rig.wait("играет", |s| s.state == State::Playing && s.title == "T1");
        rig.handle.tx.send(Msg::Alice(AliceEvent::Spotted { text: "дальше".into() })).unwrap();
        rig.wait("следующий", |s| s.title == "T2");
        // Тот же «дальше» пришёл ещё и запросом Алисы — второй раз не переключаем.
        rig.say("дальше");
        thread::sleep(Duration::from_millis(300));
        let skips = rig
            .server
            .requests()
            .iter()
            .filter(|r| r.path == "/api/events" && String::from_utf8_lossy(&r.body).contains("\"skip\""))
            .count();
        assert_eq!(skips, 1);
        // Посторонние фразы спотера и фразы без musik ничего не делают.
        rig.handle.tx.send(Msg::Alice(AliceEvent::Spotted { text: "громче".into() })).unwrap();
        rig.handle.request(Intent::Release, false).unwrap();
        rig.handle.tx.send(Msg::Alice(AliceEvent::Spotted { text: "назад".into() })).unwrap();
        thread::sleep(Duration::from_millis(300));
        assert_eq!(rig.handle.status().unwrap().state, State::Idle);
    }
}
