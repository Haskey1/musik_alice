//! Локальный управляющий сокет: одна строка запроса — одна строка ответа.
//!
//! `STATUS` отдаёт JSON, остальные команды (`PLAY …`, `NEXT`, `VOLUME 30`) разбираются так же, как
//! в `intent::from_control`. Сокет доступен только владельцу (режим 0600) и пользуется отдельным
//! потоком на соединение, поэтому зависший клиент не мешает остальным.

use crate::{
    config::Config,
    engine::{Handle, Msg},
    intent,
};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
        Arc,
    },
    thread,
    time::Duration,
};

pub struct Server {
    path: PathBuf,
    stop: Arc<AtomicBool>,
}

impl Server {
    /// `quit` вызывается по команде `QUIT` (останов сервиса из CLI).
    pub fn start(path: &Path, handle: Arc<Handle>, state_dir: PathBuf, quit: Sender<()>) -> io::Result<Server> {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        thread::Builder::new().name("control".into()).spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (handle, state_dir, quit) = (handle.clone(), state_dir.clone(), quit.clone());
                        let _ = thread::Builder::new().name("control-conn".into()).spawn(move || {
                            let _ = serve(stream, &handle, &state_dir, &quit);
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(50)),
                    Err(_) => thread::sleep(Duration::from_millis(200)),
                }
            }
        })?;
        Ok(Server { path: path.to_owned(), stop })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.path);
    }
}

fn serve(mut stream: UnixStream, handle: &Handle, state_dir: &Path, quit: &Sender<()>) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut line = String::new();
    BufReader::new((&stream).take(1024)).read_line(&mut line)?;
    let line = line.trim();
    let reply = match line {
        "STATUS" => handle.status().and_then(|s| serde_json::to_string(&s).ok()).unwrap_or_else(|| "{}".into()),
        "RELOAD" => match Config::load(state_dir) {
            Ok(cfg) => {
                let _ = handle.tx.send(Msg::Reload(Box::new(cfg)));
                "настройки перечитаны".into()
            }
            Err(e) => format!("ошибка: {e}"),
        },
        "QUIT" => {
            let _ = quit.send(());
            "останавливаюсь".into()
        }
        _ => match intent::from_control(line) {
            Some(parsed) => handle.request(parsed, false).unwrap_or_else(|| "движок не отвечает".into()),
            None => "ошибка: неизвестная команда".into(),
        },
    };
    stream.write_all(reply.replace('\n', " ").as_bytes())?;
    stream.write_all(b"\n")
}

/// Клиентская сторона: отправляет строку и возвращает ответ.
pub fn request(path: &Path, line: &str) -> io::Result<String> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(8)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(reply.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        alice::NoNative,
        api::HttpApi,
        audio::{NullMixer, NullOutput},
        engine::launch,
    };
    use std::sync::{mpsc, Mutex};

    #[test]
    fn serves_status_commands_and_rejects_garbage() {
        let dir = std::env::temp_dir().join(format!("musikalice-control-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let api = Arc::new(HttpApi::new("", "", ""));
        let output = Box::new(NullOutput::new(44100, Arc::new(Mutex::new(Vec::new()))));
        let handle = Arc::new(launch(
            Config::default(),
            dir.clone(),
            api,
            output,
            Arc::new(NoNative),
            Arc::new(NullMixer::new(33)),
        ));
        let (quit_tx, quit_rx) = mpsc::channel();
        let socket = dir.join("c.sock");
        let _server = Server::start(&socket, handle.clone(), dir.clone(), quit_tx).unwrap();
        assert_eq!(std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777, 0o600);

        let status: serde_json::Value = serde_json::from_str(&request(&socket, "STATUS").unwrap()).unwrap();
        assert_eq!(status["state"], "idle");
        assert_eq!(status["volume"], 33);
        assert!(request(&socket, "RADIO").unwrap().contains("login"), "не настроенный сервис подсказывает login");
        assert!(request(&socket, "DROP TABLE").unwrap().starts_with("ошибка"));
        assert!(request(&socket, "VOLUME 500").unwrap().starts_with("ошибка"));
        assert_eq!(request(&socket, "RELOAD").unwrap(), "настройки перечитаны");
        request(&socket, "QUIT").unwrap();
        assert!(quit_rx.recv_timeout(Duration::from_secs(2)).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }
}
