//! Клиент HTTP API сервера musik (см. docs/API.md в репозитории musik).
//!
//! Все вызовы блокирующие и выполняются в рабочих потоках движка. Трейт `MusikApi` позволяет
//! подменять сервер в тестах.

use crate::{search::Row, util::nullable};
use serde::Deserialize;
use std::{io::Read, time::Duration};

#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    /// HTTP-статус; 0 — сервер недоступен (сеть, DNS, таймаут).
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl ApiError {
    pub fn network(message: impl Into<String>) -> Self {
        Self { status: 0, code: "network".into(), message: message.into() }
    }
    pub fn local(code: &str, message: impl Into<String>) -> Self {
        Self { status: 0, code: code.into(), message: message.into() }
    }
    #[allow(dead_code)]
    pub fn is_network(&self) -> bool {
        self.status == 0 && self.code == "network"
    }
    pub fn is_unauthorized(&self) -> bool {
        self.status == 401
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.status == 0 {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{} (HTTP {}, {})", self.message, self.status, self.code)
        }
    }
}

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Track {
    pub id: i64,
    #[serde(default, deserialize_with = "nullable")]
    pub artist: String,
    #[serde(default, deserialize_with = "nullable")]
    pub title: String,
    #[serde(default, deserialize_with = "nullable")]
    pub album: String,
    #[serde(default, deserialize_with = "nullable")]
    pub duration: f64,
    /// Относительный путь потока, обычно `/api/stream/{id}`.
    #[serde(default)]
    pub stream: Option<String>,
    /// Показ рекомендации: возвращается в событиях, чтобы обратная связь попала в нужную запись.
    #[serde(default)]
    pub impression_id: Option<String>,
}

impl Track {
    #[allow(dead_code)]
    pub fn display(&self) -> String {
        match (self.artist.is_empty(), self.title.is_empty()) {
            (false, false) => format!("{} — {}", self.artist, self.title),
            (true, false) => self.title.clone(),
            (false, true) => self.artist.clone(),
            _ => format!("трек {}", self.id),
        }
    }
}

/// Ответ `radio/start`, `play`, `mixes/{kind}/play`, `session/back`.
#[derive(Deserialize, Clone, Debug, Default)]
pub struct SessionPayload {
    #[serde(default, deserialize_with = "nullable")]
    pub session_id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub mode: String,
    #[serde(default)]
    pub current: Option<Track>,
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
}

/// Ответ `events` для track_end и skip.
#[derive(Deserialize, Clone, Debug, Default)]
pub struct EventResult {
    #[serde(default)]
    pub next: Option<Track>,
    #[serde(default, deserialize_with = "nullable")]
    pub session_id: String,
}

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Mix {
    #[serde(default, deserialize_with = "nullable")]
    pub kind: String,
    #[serde(default, deserialize_with = "nullable")]
    pub title: String,
    #[serde(default, deserialize_with = "nullable")]
    pub subtitle: String,
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    #[serde(default = "yes")]
    pub ready: bool,
}
fn yes() -> bool {
    true
}

/// Тело `POST /api/play`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayRequest {
    pub track_ids: Vec<i64>,
    pub name: String,
    pub start_track_id: Option<i64>,
}

/// Событие воспроизведения для `POST /api/events`.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub kind: &'static str,
    pub event_id: String,
    pub session_id: String,
    pub track_id: i64,
    pub impression_id: Option<String>,
    pub position_sec: f64,
    pub duration_sec: f64,
    pub listened_sec: f64,
    pub reason: Option<&'static str>,
}

pub trait MusikApi: Send + Sync {
    fn library(&self) -> Result<Vec<Row>, ApiError>;
    fn radio_start(&self, seed_track_id: Option<i64>) -> Result<SessionPayload, ApiError>;
    fn play(&self, request: &PlayRequest) -> Result<SessionPayload, ApiError>;
    fn mixes(&self) -> Result<Vec<Mix>, ApiError>;
    fn mix_play(&self, kind: &str) -> Result<SessionPayload, ApiError>;
    fn session_back(&self, session_id: &str) -> Result<SessionPayload, ApiError>;
    fn event(&self, event: &Event) -> Result<EventResult, ApiError>;
    fn favorite_add(&self, track_id: i64) -> Result<(), ApiError>;
    /// Проверка токена: `GET /api/auth/me`.
    fn auth_ok(&self) -> Result<bool, ApiError>;
    /// Применяет новые адрес, токен и имя устройства (после `login` или `config set`).
    fn configure(&self, server: &str, token: &str, device_name: &str);
}

/// Открытый поток аудио.
pub struct Opened {
    pub reader: Box<dyn Read + Send>,
    pub content_type: Option<String>,
}

pub trait Fetcher: Send + Sync {
    fn open(&self, url: &str) -> Result<Opened, ApiError>;
}

pub struct HttpApi {
    agent: ureq::Agent,
    stream_agent: ureq::Agent,
    /// Меняется на лету: после `login` служба получает новые адрес и токен без перезапуска.
    settings: std::sync::RwLock<Settings>,
}

#[derive(Clone)]
struct Settings {
    base: String,
    token: String,
    device_id: String,
    client_id: String,
}

impl Settings {
    fn new(server: &str, token: &str, device_name: &str) -> Self {
        let device_id =
            if device_name.is_empty() { "Яндекс Станция".to_owned() } else { device_name.to_owned() };
        Self {
            base: server.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            client_id: format!("musik-alice:{device_id}"),
            device_id,
        }
    }
}

fn agent(read: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(read)
        .timeout_write(Duration::from_secs(10))
        .user_agent(concat!("musik-alice/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn map_error(error: ureq::Error) -> ApiError {
    match error {
        ureq::Error::Status(status, response) => {
            let mut body = String::new();
            let _ = response.into_reader().take(16 * 1024).read_to_string(&mut body);
            let parsed = serde_json::from_str::<serde_json::Value>(&body).ok();
            let field =
                |name: &str| parsed.as_ref().and_then(|v| v.get(name)).and_then(|v| v.as_str()).map(str::to_owned);
            ApiError {
                status,
                code: field("code").unwrap_or_else(|| "http".into()),
                message: field("error").unwrap_or_else(|| format!("HTTP {status}")),
            }
        }
        ureq::Error::Transport(t) => ApiError::network(format!("сервер недоступен: {t}")),
    }
}

fn parse<T: for<'de> Deserialize<'de>>(response: ureq::Response) -> Result<T, ApiError> {
    serde_json::from_reader(response.into_reader().take(64 * 1024 * 1024))
        .map_err(|e| ApiError::local("bad_response", format!("неожиданный ответ сервера: {e}")))
}

impl HttpApi {
    pub fn new(server: &str, token: &str, device_name: &str) -> Self {
        Self {
            agent: agent(Duration::from_secs(20)),
            stream_agent: agent(Duration::from_secs(20)),
            settings: std::sync::RwLock::new(Settings::new(server, token, device_name)),
        }
    }

    fn settings(&self) -> Settings {
        self.settings.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn get(&self, path: &str) -> ureq::Request {
        let s = self.settings();
        self.agent.get(&format!("{}{}", s.base, path)).set("Authorization", &format!("Bearer {}", s.token))
    }

    fn post(&self, path: &str) -> ureq::Request {
        let s = self.settings();
        self.agent.post(&format!("{}{}", s.base, path)).set("Authorization", &format!("Bearer {}", s.token))
    }

    fn post_json(&self, path: &str, body: serde_json::Value) -> Result<ureq::Response, ApiError> {
        self.post(path).send_json(body).map_err(map_error)
    }
}

impl MusikApi for HttpApi {
    fn library(&self) -> Result<Vec<Row>, ApiError> {
        let response = self.get("/api/library").timeout(Duration::from_secs(60)).call().map_err(map_error)?;
        parse(response)
    }

    fn radio_start(&self, seed_track_id: Option<i64>) -> Result<SessionPayload, ApiError> {
        let body = match seed_track_id {
            Some(id) => serde_json::json!({ "seed_track_id": id }),
            None => serde_json::json!({}),
        };
        parse(self.post_json("/api/radio/start", body)?)
    }

    fn play(&self, request: &PlayRequest) -> Result<SessionPayload, ApiError> {
        let mut body = serde_json::json!({ "track_ids": request.track_ids });
        if !request.name.is_empty() {
            body["name"] = request.name.clone().into();
        }
        if let Some(id) = request.start_track_id {
            body["start_track_id"] = id.into();
        }
        parse(self.post_json("/api/play", body)?)
    }

    fn mixes(&self) -> Result<Vec<Mix>, ApiError> {
        #[derive(Deserialize)]
        struct Shelf {
            #[serde(default)]
            mixes: Vec<Mix>,
        }
        let shelf: Shelf = parse(self.get("/api/mixes").call().map_err(map_error)?)?;
        Ok(shelf.mixes)
    }

    fn mix_play(&self, kind: &str) -> Result<SessionPayload, ApiError> {
        let path = format!("/api/mixes/{}/play", crate::util::percent_encode(kind));
        parse(self.post_json(&path, serde_json::json!({}))?)
    }

    fn session_back(&self, session_id: &str) -> Result<SessionPayload, ApiError> {
        parse(self.post_json("/api/session/back", serde_json::json!({ "session_id": session_id }))?)
    }

    #[allow(clippy::result_large_err)]
    fn event(&self, event: &Event) -> Result<EventResult, ApiError> {
        let Settings { client_id, device_id, .. } = self.settings();
        let mut body = serde_json::json!({
            "type": event.kind,
            "event_id": event.event_id,
            "track_id": event.track_id,
            "session_id": event.session_id,
            "client_id": client_id,
            "device_id": device_id,
            "position_sec": event.position_sec,
            "duration_sec": event.duration_sec,
            "listened_sec": event.listened_sec,
        });
        if let Some(id) = &event.impression_id {
            body["impression_id"] = id.clone().into();
        }
        if let Some(reason) = event.reason {
            body["reason"] = reason.into();
        }
        let send = || self.post("/api/events").timeout(Duration::from_secs(7)).send_json(body.clone());
        let started = std::time::Instant::now();
        let response = match send() {
            Ok(r) => r,
            // Быстрый обрыв стоит повторить: сервер дедуплицирует по тому же event_id.
            Err(ureq::Error::Transport(_)) if started.elapsed() < Duration::from_secs(3) => {
                std::thread::sleep(Duration::from_millis(500));
                send().map_err(map_error)?
            }
            Err(e) => return Err(map_error(e)),
        };
        parse(response)
    }

    fn favorite_add(&self, track_id: i64) -> Result<(), ApiError> {
        self.post_json("/api/favorites", serde_json::json!({ "type": "track", "track_id": track_id })).map(|_| ())
    }

    fn configure(&self, server: &str, token: &str, device_name: &str) {
        *self.settings.write().unwrap_or_else(|e| e.into_inner()) = Settings::new(server, token, device_name);
    }

    fn auth_ok(&self) -> Result<bool, ApiError> {
        #[derive(Deserialize)]
        struct Me {
            #[serde(default)]
            ok: bool,
        }
        let me: Me = parse(self.get("/api/auth/me").call().map_err(map_error)?)?;
        Ok(me.ok)
    }
}

impl Fetcher for HttpApi {
    fn open(&self, url: &str) -> Result<Opened, ApiError> {
        let mut request = self.stream_agent.get(url);
        let s = self.settings();
        if !s.base.is_empty() && url.starts_with(&s.base) {
            request = request.set("Authorization", &format!("Bearer {}", s.token));
        }
        let response = request.call().map_err(map_error)?;
        let content_type =
            response.header("Content-Type").map(|v| v.split(';').next().unwrap_or(v).trim().to_lowercase());
        Ok(Opened { reader: Box::new(response.into_reader()), content_type })
    }
}

/// Обмен учётных данных на токен устройства (`POST /api/auth/token`), без авторизации.
/// С `username` — многопользовательский режим; без — режим одного владельца (только пароль).
pub fn issue_token(
    server: &str,
    username: Option<&str>,
    password: &str,
    device_name: &str,
) -> Result<String, ApiError> {
    #[derive(Deserialize)]
    struct Issued {
        #[serde(default)]
        token: Option<String>,
    }
    let body = match username {
        Some(user) => serde_json::json!({ "username": user, "password": password, "device_name": device_name }),
        None => serde_json::json!({ "password": password }),
    };
    let response = agent(Duration::from_secs(20))
        .post(&format!("{}/api/auth/token", server.trim_end_matches('/')))
        .send_json(body)
        .map_err(map_error)?;
    let issued: Issued = parse(response)?;
    issued.token.filter(|t| !t.is_empty()).ok_or_else(|| ApiError::local("no_token", "сервер не выдал токен"))
}

/// Быстрая проверка доступности сервера без авторизации (`GET /api/health`).
pub fn health(server: &str) -> Result<(), ApiError> {
    agent(Duration::from_secs(8))
        .get(&format!("{}/api/health", server.trim_end_matches('/')))
        .call()
        .map(|_| ())
        .map_err(map_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakeServer, Response};

    fn server() -> FakeServer {
        FakeServer::start(|req| match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/api/auth/me") => Response::json(200, r#"{"ok":true,"auth_enabled":true}"#),
            ("GET", "/api/library") => Response::json(
                200,
                r#"[{"id":1,"artist":"Radiohead","title":"Creep","album":"Pablo Honey","duration":238.5,"ready":true},
                    {"id":2,"artist":null,"title":"X","album":null}]"#,
            ),
            ("POST", "/api/radio/start") => Response::json(
                200,
                r#"{"session_id":"s1","mode":"radio","maturity":"ready","queue":[],
                    "current":{"id":5,"artist":"A","title":"T","album":"B","duration":100,"stream":"/api/stream/5","impression_id":"abc","cluster_id":3}}"#,
            ),
            ("POST", "/api/play") => {
                Response::json(200, r#"{"session_id":"s2","mode":"playlist","fixed":true,"name":"Mix","current":null}"#)
            }
            ("GET", "/api/mixes") => Response::json(
                200,
                r#"{"mixes":[{"kind":"daily","title":"Микс дня","subtitle":"","ready":true},{"kind":"later","title":"Потом","ready":false}]}"#,
            ),
            ("POST", "/api/mixes/for%20you/play") => Response::json(200, r#"{"session_id":"s3","current":{"id":9}}"#),
            ("POST", "/api/events") => {
                Response::json(200, r#"{"ok":true,"next":{"id":6,"title":"N"},"ended":false,"session_id":"s1"}"#)
            }
            ("POST", "/api/favorites") => Response::json(200, r#"{"ok":true}"#),
            ("POST", "/api/auth/token") => Response::json(200, r#"{"token":"tok123","profiles":[]}"#),
            ("GET", "/api/health") => Response::json(200, r#"{"ok":true}"#),
            ("GET", "/api/stream/5") => Response::bytes(200, "audio/mpeg; charset=x", b"ID3data".to_vec()),
            ("POST", "/api/session/back") => {
                Response::json(404, r#"{"error":"нет предыдущего трека","code":"not_found"}"#)
            }
            _ => Response::json(404, r#"{"error":"nope","code":"not_found"}"#),
        })
    }

    #[test]
    fn library_parses_nulls_and_sends_the_bearer_token() {
        let s = server();
        let api = HttpApi::new(&s.url(), "secret", "Кухня");
        let rows = api.library().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].artist, "Radiohead");
        assert_eq!(rows[1].album, "");
        let req = s.requests().into_iter().find(|r| r.path == "/api/library").unwrap();
        assert_eq!(req.header("authorization").as_deref(), Some("Bearer secret"));
    }

    #[test]
    fn radio_and_play_payloads() {
        let s = server();
        let api = HttpApi::new(&s.url(), "t", "d");
        let radio = api.radio_start(Some(5)).unwrap();
        assert_eq!(radio.session_id, "s1");
        let current = radio.current.unwrap();
        assert_eq!((current.id, current.impression_id.as_deref()), (5, Some("abc")));
        assert_eq!(current.stream.as_deref(), Some("/api/stream/5"));
        let body: serde_json::Value = serde_json::from_slice(&s.requests().last().unwrap().body).unwrap();
        assert_eq!(body["seed_track_id"], 5);

        let played =
            api.play(&PlayRequest { track_ids: vec![1, 2, 3], name: "Mix".into(), start_track_id: Some(2) }).unwrap();
        assert!(played.current.is_none());
        let body: serde_json::Value = serde_json::from_slice(&s.requests().last().unwrap().body).unwrap();
        assert_eq!(body["track_ids"], serde_json::json!([1, 2, 3]));
        assert_eq!(body["start_track_id"], 2);
        assert_eq!(body["name"], "Mix");
    }

    #[test]
    fn mixes_and_mix_play_encode_the_kind() {
        let s = server();
        let api = HttpApi::new(&s.url(), "t", "d");
        let mixes = api.mixes().unwrap();
        assert_eq!(mixes.len(), 2);
        assert_eq!(mixes[0].title, "Микс дня");
        assert!(!mixes[1].ready);
        assert_eq!(api.mix_play("for you").unwrap().session_id, "s3");
    }

    #[test]
    fn event_body_carries_identity_and_feedback_fields() {
        let s = server();
        let api = HttpApi::new(&s.url(), "t", "Кухня");
        let result = api
            .event(&Event {
                kind: "skip",
                event_id: "e1".into(),
                session_id: "s1".into(),
                track_id: 5,
                impression_id: Some("abc".into()),
                position_sec: 12.5,
                duration_sec: 200.0,
                listened_sec: 12.0,
                reason: Some("skipped"),
            })
            .unwrap();
        assert_eq!(result.next.unwrap().id, 6);
        let req = s.requests().into_iter().find(|r| r.path == "/api/events").unwrap();
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["type"], "skip");
        assert_eq!(body["event_id"], "e1");
        assert_eq!(body["impression_id"], "abc");
        assert_eq!(body["reason"], "skipped");
        assert_eq!(body["device_id"], "Кухня");
        assert_eq!(body["client_id"], "musik-alice:Кухня");
        assert_eq!(body["listened_sec"], 12.0);
    }

    #[test]
    fn errors_use_the_server_envelope() {
        let s = server();
        let api = HttpApi::new(&s.url(), "t", "d");
        let err = api.session_back("s1").unwrap_err();
        assert_eq!((err.status, err.code.as_str()), (404, "not_found"));
        assert_eq!(err.message, "нет предыдущего трека");
        assert!(!err.is_network());
    }

    #[test]
    fn unreachable_server_is_a_network_error() {
        let api = HttpApi::new("http://127.0.0.1:1", "t", "d");
        let err = api.auth_ok().unwrap_err();
        assert!(err.is_network(), "{err}");
    }

    #[test]
    fn token_exchange_and_health_need_no_authorization() {
        let s = server();
        assert_eq!(issue_token(&s.url(), Some("anna"), "pw", "Станция").unwrap(), "tok123");
        let body: serde_json::Value = serde_json::from_slice(&s.requests().last().unwrap().body).unwrap();
        assert_eq!(body["username"], "anna");
        assert_eq!(body["device_name"], "Станция");
        assert_eq!(issue_token(&s.url(), None, "pw", "x").unwrap(), "tok123");
        let body: serde_json::Value = serde_json::from_slice(&s.requests().last().unwrap().body).unwrap();
        assert!(body.get("username").is_none());
        health(&s.url()).unwrap();
        assert!(s.requests().iter().all(|r| r.header("authorization").is_none()));
    }

    #[test]
    fn auth_check_and_favorites() {
        let s = server();
        let api = HttpApi::new(&s.url(), "t", "d");
        assert!(api.auth_ok().unwrap());
        api.favorite_add(7).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&s.requests().last().unwrap().body).unwrap();
        assert_eq!((body["type"].as_str(), body["track_id"].as_i64()), (Some("track"), Some(7)));
    }

    #[test]
    fn stream_open_returns_content_type_and_only_authorizes_the_own_server() {
        let s = server();
        let api = HttpApi::new(&s.url(), "secret", "d");
        let mut opened = api.open(&format!("{}/api/stream/5", s.url())).unwrap();
        assert_eq!(opened.content_type.as_deref(), Some("audio/mpeg"));
        let mut data = Vec::new();
        opened.reader.read_to_end(&mut data).unwrap();
        assert_eq!(data, b"ID3data");
        let req = s.requests().into_iter().find(|r| r.path == "/api/stream/5").unwrap();
        assert_eq!(req.header("authorization").as_deref(), Some("Bearer secret"));
    }

    #[test]
    fn track_display_handles_missing_parts() {
        let t = Track { id: 1, artist: "A".into(), title: "T".into(), ..Track::default() };
        assert_eq!(t.display(), "A — T");
        assert_eq!(Track { id: 2, title: "T".into(), ..Track::default() }.display(), "T");
        assert_eq!(Track { id: 3, ..Track::default() }.display(), "трек 3");
    }
}
