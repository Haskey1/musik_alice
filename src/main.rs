//! musik-alice: клиент сервера musik для Яндекс Станций.
//!
//! Один бинарник: служба (`run`), супервизор (`supervise`), консольные команды и TUI.

mod alice;
mod announce;
mod api;
mod audio;
mod config;
mod control;
mod controls;
mod decode;
mod ducking;
mod earcon;
mod engine;
mod intent;
mod player;
mod profile;
mod search;
mod spotter;
mod supervisor;
#[cfg(test)]
mod testutil;
mod tui;
mod util;
mod volume_guard;

use config::Config;
use std::{
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn install_signals() {
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

fn log(message: &str) {
    let seconds = util::unix_millis() / 1000 % 86_400;
    eprintln!("[{:02}:{:02}:{:02}] {message}", seconds / 3600, seconds / 60 % 60, seconds % 60);
}

/// Pid-файл процесса; удаляется при нормальном выходе. Скрипты start/stop находят службу по нему.
struct PidFile(PathBuf);

impl PidFile {
    fn create(path: PathBuf) -> Self {
        let _ = util::write_atomic(&path, format!("{}\n", std::process::id()).as_bytes(), 0o644);
        Self(path)
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Расположение файлов. Переменные окружения нужны для разработки и тестов вне станции.
struct Paths {
    base: PathBuf,
    state: PathBuf,
    control: PathBuf,
    run_dir: PathBuf,
}

impl Paths {
    fn detect() -> Self {
        let base =
            std::env::var_os("MUSIKALICE_BASE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(config::BASE));
        let control =
            std::env::var_os("MUSIKALICE_CONTROL").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(config::CONTROL));
        let run_dir = std::env::var_os("MUSIKALICE_RUN").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/run"));
        Self { state: base.join("state"), base, control, run_dir }
    }
}

const HELP: &str = "\
musikalice — клиент сервера musik для Яндекс Станции

Служба:
  run                       запустить службу в этом терминале
  supervise                 запустить службу и перезапускать при падении
  quit                      остановить работающую службу
Настройка:
  login [--server URL] [--user ИМЯ] [--password ПАРОЛЬ] [--name УСТРОЙСТВО]
                            получить токен устройства и сохранить его
  config [show]             показать настройки (токен скрыт)
  config set КЛЮЧ ЗНАЧЕНИЕ  изменить настройку (ключи: musikalice config keys)
  config keys               список ключей
  profile [show|list]       показать выбранный или все профили станции
  profile set auto|ID|ПУТЬ  выбрать профиль
Управление (работающая служба):
  play ЗАПРОС               например: play Radiohead, play альбом OK Computer
  radio | mix НАЗВАНИЕ | pause | resume | toggle | next | previous
  volume 0-100 | like | dislike | voice on|off | release | retry
  status [--json]           что играет сейчас
  tui                       интерактивный экран
  speak ТЕКСТ               Алиса произносит текст (проверка объявлений, служба не нужна)
  alice-dump [СЕКУНДЫ]      показать все события Алисы (по умолчанию 20 с; диагностика)
Прочее:
  version | help
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let paths = Paths::detect();
    let result = dispatch(&args, &paths);
    match result {
        Ok(code) => code,
        Err(message) => {
            eprintln!("ошибка: {message}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(args: &[String], paths: &Paths) -> Result<ExitCode, String> {
    let rest = |n: usize| args.get(n..).unwrap_or(&[]);
    match args.first().map(String::as_str) {
        None | Some("help") | Some("--help") | Some("-h") => {
            print!("{HELP}");
        }
        Some("version") | Some("--version") | Some("-V") => println!("musikalice {VERSION}"),
        Some("run") => return run_service(paths),
        Some("supervise") | Some("--supervise") => return supervise(paths),
        Some("quit") | Some("stop-service") => say(paths, "QUIT")?,
        Some("login") => login(paths, rest(1))?,
        Some("config") => config_command(paths, rest(1))?,
        Some("profile") => profile_command(paths, rest(1))?,
        Some("profiles") => profile_command(paths, &["list".to_owned()])?,
        Some("tui") => tui::run(&paths.control).map_err(|e| e.to_string())?,
        Some("status") => {
            let reply = control::request(&paths.control, "STATUS").map_err(not_running)?;
            if rest(1).first().map(String::as_str) == Some("--json") {
                println!("{reply}");
            } else {
                println!("{}", tui::render(&serde_json::from_str(&reply).unwrap_or_default(), 80, "").join("\n"));
            }
        }
        Some("play") => {
            if rest(1).is_empty() {
                return Err("что включить? Например: musikalice play Radiohead".into());
            }
            say(paths, &format!("PLAY {}", rest(1).join(" ")))?;
        }
        Some("alice-dump") => {
            let seconds = rest(1).first().and_then(|v| v.parse().ok()).unwrap_or(20u64).clamp(1, 600);
            let profile = profile::resolve(&profile::selection(&paths.state), Path::new("/"), &paths.base)
                .map_err(|e| format!("профиль станции: {e}"))?;
            let address = profile.voice_address().ok_or("в профиле нет голоса Алисы")?;
            eprintln!("Слушаю {address} {seconds} с. Говорите…");
            let count = alice::dump_stream(address, seconds, |text| print!("{text}"))
                .map_err(|e| format!("Алиса на {address}: {e}"))?;
            eprintln!("кадров: {count}");
        }
        Some("speak") => {
            if rest(1).is_empty() {
                return Err("что сказать? Например: musikalice speak Radiohead, Creep".into());
            }
            if !announce::available() {
                return Err("на этом устройстве нет консоли Алисы (/system/vendor/quasar/maind)".into());
            }
            announce::speak(&rest(1).join(" "))?;
            println!("отправлено Алисе: «{} {}»", announce::PREFIX, rest(1).join(" "));
        }
        Some("mix") => {
            if rest(1).is_empty() {
                return Err("укажите название микса".into());
            }
            say(paths, &format!("MIX {}", rest(1).join(" ")))?;
        }
        Some("voice") => match rest(1).first().map(String::as_str) {
            Some("on") => say(paths, "VOICE ON")?,
            Some("off") => say(paths, "VOICE OFF")?,
            _ => return Err("voice on|off".into()),
        },
        Some("volume") => match rest(1).first().and_then(|v| v.parse::<u8>().ok()).filter(|v| *v <= 100) {
            Some(v) => say(paths, &format!("VOLUME {v}"))?,
            None => return Err("volume 0-100".into()),
        },
        Some(
            word @ ("radio" | "pause" | "resume" | "toggle" | "next" | "previous" | "like" | "dislike" | "release"
            | "retry"),
        ) => say(paths, &word.to_uppercase())?,
        Some(other) => {
            eprintln!("неизвестная команда «{other}»\n");
            eprint!("{HELP}");
            return Ok(ExitCode::from(2));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn not_running(error: io::Error) -> String {
    format!("служба не отвечает ({error}); запустите её: musikalice start")
}

fn say(paths: &Paths, line: &str) -> Result<(), String> {
    println!("{}", control::request(&paths.control, line).map_err(not_running)?);
    Ok(())
}

// ---------------------------------------------------------------- служба

fn run_service(paths: &Paths) -> Result<ExitCode, String> {
    if control::request(&paths.control, "STATUS").is_ok() {
        return Err("служба уже запущена".into());
    }
    install_signals();
    let _pid = PidFile::create(paths.run_dir.join("musikalice.pid"));
    let config = Config::load(&paths.state).map_err(|e| e.to_string())?;
    let profile = profile::resolve(&profile::selection(&paths.state), Path::new("/"), &paths.base)
        .map_err(|e| format!("профиль станции: {e}"))?;
    profile.validate().map_err(|e| format!("профиль станции: {e}"))?;
    log(&format!("musik-alice {VERSION}, профиль «{}»", profile.id));
    if !config.configured() {
        log("сервер не настроен: выполните «musikalice login»");
    }

    let output = audio::open_output(&profile.audio)?;
    log(&format!("звук: {} @ {} Гц", profile.audio.device, output.rate()));
    let mixer = audio::open_mixer(&profile.audio);
    let native: Arc<dyn alice::Native> = match profile.voice_address() {
        Some(address) => Arc::new(alice::NativeClient::new(address)),
        None => Arc::new(alice::NoNative),
    };
    let api = Arc::new(api::HttpApi::new(&config.server, &config.token, &config.device_name));
    let handle = Arc::new(engine::launch(config, paths.state.clone(), api, output, native, mixer));

    let alice_stop = Arc::new(AtomicBool::new(false));
    if let Some(address) = profile.voice_address() {
        let tx = handle.tx.clone();
        let stop = alice_stop.clone();
        std::thread::Builder::new()
            .name("alice".into())
            .spawn(move || {
                alice::watch(address, stop, move |event| {
                    let _ = tx.send(engine::Msg::Alice(event));
                })
            })
            .map_err(|e| e.to_string())?;
        log(&format!("слушаю Алису на {address}"));
        let tx = handle.tx.clone();
        let stop = alice_stop.clone();
        let path = spotter::log_path();
        log(&format!("короткие команды без «Алиса»: журнал {}", path.display()));
        std::thread::Builder::new()
            .name("spotter".into())
            .spawn(move || {
                spotter::watch(path, stop, move |text| {
                    let _ = tx.send(engine::Msg::Alice(alice::AliceEvent::Spotted { text }));
                })
            })
            .map_err(|e| e.to_string())?;
    } else {
        log("профиль без голоса: управление только через консоль");
    }

    let controls_stop = Arc::new(AtomicBool::new(false));
    let controls_thread = match controls::Settings::from_profile(profile.controls.as_ref()) {
        Some(settings) => {
            let remote: Arc<dyn controls::Remote> = handle.clone();
            controls::spawn(settings, remote, controls_stop.clone())
        }
        None => {
            log("кнопки и светодиоды: профиль их не описывает");
            None
        }
    };

    let (quit_tx, quit_rx) = mpsc::channel();
    let _server = control::Server::start(&paths.control, handle.clone(), paths.state.clone(), quit_tx)
        .map_err(|e| format!("управляющий сокет {}: {e}", paths.control.display()))?;
    log("готово");
    while !STOP.load(Ordering::SeqCst) {
        if quit_rx.recv_timeout(Duration::from_millis(200)).is_ok() {
            break;
        }
    }
    log("останавливаюсь");
    alice_stop.store(true, Ordering::Relaxed);
    controls_stop.store(true, Ordering::Relaxed);
    // Поток кнопок отпускает устройство ввода и возвращает часы; он держит ссылку на движок.
    if let Some(thread) = controls_thread {
        let _ = thread.join();
    }
    if let Ok(handle) = Arc::try_unwrap(handle) {
        handle.shutdown();
    }
    Ok(ExitCode::SUCCESS)
}

fn supervise(paths: &Paths) -> Result<ExitCode, String> {
    install_signals();
    let _pid = PidFile::create(paths.run_dir.join("musikalice-supervisor.pid"));
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut delay = Duration::from_secs(2);
    while !STOP.load(Ordering::SeqCst) {
        let started = Instant::now();
        let mut child = std::process::Command::new(&exe)
            .arg("run")
            .spawn()
            .map_err(|e| format!("не удалось запустить {}: {e}", exe.display()))?;
        let status = loop {
            if STOP.load(Ordering::SeqCst) {
                unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline && child.try_wait().ok().flatten().is_none() {
                    std::thread::sleep(Duration::from_millis(100));
                }
                let _ = child.kill();
                let _ = child.wait();
                return Ok(ExitCode::SUCCESS);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                Err(e) => return Err(e.to_string()),
            }
        };
        if status.success() {
            // Остановка по команде `quit`: перезапускать не нужно.
            return Ok(ExitCode::SUCCESS);
        }
        delay = supervisor::next_delay(started.elapsed(), delay);
        log(&format!("служба завершилась ({status}), перезапуск через {} с", delay.as_secs()));
        let until = Instant::now() + delay;
        while Instant::now() < until && !STOP.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- login / config / profile

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn prompt(question: &str) -> Result<String, String> {
    eprint!("{question}");
    io::stderr().flush().ok();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).map_err(|e| e.to_string())?;
    Ok(line.trim().to_owned())
}

fn login(paths: &Paths, args: &[String]) -> Result<(), String> {
    let mut cfg = Config::load(&paths.state).map_err(|e| e.to_string())?;
    let server = match flag(args, "--server") {
        Some(s) => s,
        None if !cfg.server.is_empty() => cfg.server.clone(),
        None => prompt("Адрес сервера musik (например 192.168.3.141:8787): ")?,
    };
    let server = config::normalize_server(&server);
    if server.is_empty() {
        return Err("адрес сервера не указан".into());
    }
    api::health(&server).map_err(|e| format!("{server} недоступен: {e}"))?;
    let user = match flag(args, "--user") {
        Some(u) => Some(u).filter(|u| !u.is_empty()),
        None if args.iter().any(|a| a == "--password") => None,
        None => Some(prompt("Имя пользователя (пусто — режим одного владельца): ")?).filter(|u| !u.is_empty()),
    };
    let password = match flag(args, "--password").or_else(|| std::env::var("MUSIKALICE_PASSWORD").ok()) {
        Some(p) => p,
        None => tui::read_password("Пароль: ").map_err(|e| e.to_string())?,
    };
    let name = flag(args, "--name").unwrap_or_else(|| {
        if cfg.device_name.is_empty() {
            "Яндекс Станция".into()
        } else {
            cfg.device_name.clone()
        }
    });
    let token = api::issue_token(&server, user.as_deref(), &password, &name).map_err(|e| e.to_string())?;
    cfg.server = server;
    cfg.token = token;
    cfg.device_name = name;
    let check = api::HttpApi::new(&cfg.server, &cfg.token, &cfg.device_name);
    use api::MusikApi;
    if !check.auth_ok().map_err(|e| format!("токен выдан, но проверка не прошла: {e}"))? {
        return Err("сервер не принял выданный токен".into());
    }
    cfg.normalize();
    cfg.save(&paths.state).map_err(|e| e.to_string())?;
    println!("Готово: токен сохранён, сервер {}", cfg.server);
    if control::request(&paths.control, "RELOAD").is_ok() {
        println!("Работающая служба подхватила настройки.");
    }
    Ok(())
}

fn config_command(paths: &Paths, args: &[String]) -> Result<(), String> {
    let mut cfg = Config::load(&paths.state).map_err(|e| e.to_string())?;
    match args.first().map(String::as_str) {
        None | Some("show") => println!("{}", config::redacted_json(&cfg)),
        Some("keys") => println!("server, token, device_name, quality (mp3|aac|mobile|original), voice, voice_radio_fallback,\ntrack_mode (radio|single), shuffle_artist, duck_level (0.05-1), volume, native_pause,\nsearch_threshold (0.5-1), earcons, announce (off|voice|always),\ndisplay (clock|position), buttons, quick_commands"),
        Some("set") => {
            let (key, value) = match (args.get(1), args.get(2)) {
                (Some(k), Some(_)) => (k.as_str(), args[2..].join(" ")),
                _ => return Err("config set КЛЮЧ ЗНАЧЕНИЕ".into()),
            };
            config::set_key(&mut cfg, key, &value)?;
            cfg.normalize();
            cfg.save(&paths.state).map_err(|e| e.to_string())?;
            println!("{key} обновлён");
            if control::request(&paths.control, "RELOAD").is_ok() {
                println!("Работающая служба подхватила настройки (duck_level — после перезапуска).");
            }
        }
        Some(other) => return Err(format!("неизвестная подкоманда «{other}»")),
    }
    Ok(())
}

fn profile_command(paths: &Paths, args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        None | Some("show") => {
            let choice = profile::selection(&paths.state);
            let p = profile::resolve(&choice, Path::new("/"), &paths.base).map_err(|e| e.to_string())?;
            println!(
                "выбор: {choice}\nпрофиль: {} ({})\nзвук: {} @ {} Гц, регулятор {}",
                p.id, p.name, p.audio.device, p.audio.rate, p.audio.mixer_control
            );
            match p.voice_address() {
                Some(a) => println!("голос: Алиса на {a}"),
                None => println!("голос: нет"),
            }
        }
        Some("list") => {
            println!("auto — определить по станции (сейчас: {})", profile::detect(Path::new("/")));
            for id in profile::Profile::builtin_ids() {
                println!("{id}");
            }
        }
        Some("set") => {
            let choice = args.get(1).ok_or("profile set auto|ID|ПУТЬ")?;
            profile::resolve(choice, Path::new("/"), &paths.base)
                .and_then(|p| p.validate().map(|_| p))
                .map_err(|e| format!("профиль не подходит: {e}"))?;
            profile::save_selection(&paths.state, choice).map_err(|e| e.to_string())?;
            println!("Профиль «{choice}» выбран. Перезапустите службу: musikalice restart");
        }
        Some(other) => return Err(format!("неизвестная подкоманда «{other}»")),
    }
    Ok(())
}
