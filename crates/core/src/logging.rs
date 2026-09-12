//! Журнал обмена с провайдером в формате JSON Lines.
//!
//! Назначение журнала задаёт вызывающая сторона: ядро не выводит путь из
//! расположения исходников и умеет работать с полностью выключенной записью.
//! Строки уходят в фоновый поток через канал, поэтому запись на диск не
//! задерживает асинхронный вызов модели, а сбой записи не превращается
//! в ошибку ответа.

use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

pub const REQUESTS_FILE: &str = "requests.jsonl";
pub const RESPONSES_FILE: &str = "responses.jsonl";

/// Запись об отправленном запросе к провайдеру.
#[derive(Serialize)]
pub struct RequestLogEntry<'a> {
    pub id: &'a str,
    pub timestamp: u64,
    pub url: &'a str,
    pub model: &'a str,
    pub request: serde_json::Value,
}

/// Запись о полученном ответе провайдера.
#[derive(Serialize)]
pub struct ResponseLogEntry<'a> {
    pub id: &'a str,
    pub timestamp: u64,
    pub status: u16,
    pub duration_ms: u128,
    pub response: serde_json::Value,
}

struct LogLine {
    file: &'static str,
    line: String,
}

/// Какая половина обмена записывается. Приёмнику этого достаточно, чтобы
/// разложить записи по своим каналам, не разбирая содержимое.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeKind {
    Request,
    Response,
}

/// Приёмник записей обмена: позволяет вызывающей стороне отправить их не в
/// файл, а в свой журнал. Нужен сервису, у которого внутри контейнера нет
/// места под файлы JSONL, но есть structured logging.
pub trait ExchangeSink: Send + Sync + 'static {
    fn record(&self, kind: ExchangeKind, entry: &serde_json::Value);
}

/// Куда уходят записи: никуда, в файлы директории или в приёмник вызывающей
/// стороны.
enum Destination {
    Disabled,
    Dir {
        sender: Option<Sender<LogLine>>,
        worker: Option<JoinHandle<()>>,
    },
    Sink(Arc<dyn ExchangeSink>),
}

/// Журнал обмена: выключен, пишет в директорию или отдаёт записи приёмнику.
pub struct ExchangeLog {
    destination: Destination,
}

impl ExchangeLog {
    /// Выключенный журнал: ничего не пишет и не создаёт директорий.
    pub fn disabled() -> Self {
        Self {
            destination: Destination::Disabled,
        }
    }

    /// Журнал, пишущий `requests.jsonl` и `responses.jsonl` в директорию `dir`.
    /// Директория создаётся фоновым потоком при первой записи.
    pub fn to_dir(dir: PathBuf) -> Self {
        let (sender, receiver) = mpsc::channel();
        match std::thread::Builder::new()
            .name("agentcore-exchange-log".to_string())
            .spawn(move || writer_loop(dir, receiver))
        {
            Ok(worker) => Self {
                destination: Destination::Dir {
                    sender: Some(sender),
                    worker: Some(worker),
                },
            },
            // Поток не запустился — журнал молча выключается: диагностика
            // обмена не должна ломать сам обмен.
            Err(_) => Self::disabled(),
        }
    }

    /// Журнал, отдающий записи приёмнику. Фонового потока здесь нет: приёмник
    /// вызывается по месту, поэтому он обязан быть дешёвым и не блокировать —
    /// запись в structured logging этому условию отвечает.
    pub fn to_sink(sink: Arc<dyn ExchangeSink>) -> Self {
        Self {
            destination: Destination::Sink(sink),
        }
    }

    pub fn is_enabled(&self) -> bool {
        match &self.destination {
            Destination::Disabled => false,
            Destination::Dir { sender, .. } => sender.is_some(),
            Destination::Sink(_) => true,
        }
    }

    pub fn log_request(&self, entry: &RequestLogEntry<'_>) {
        self.send(ExchangeKind::Request, REQUESTS_FILE, entry);
    }

    pub fn log_response(&self, entry: &ResponseLogEntry<'_>) {
        self.send(ExchangeKind::Response, RESPONSES_FILE, entry);
    }

    fn send<T: Serialize>(&self, kind: ExchangeKind, file: &'static str, entry: &T) {
        match &self.destination {
            Destination::Disabled => {}
            Destination::Dir { sender, .. } => {
                let Some(sender) = sender else { return };
                let Ok(line) = serde_json::to_string(entry) else {
                    return;
                };
                let _ = sender.send(LogLine { file, line });
            }
            Destination::Sink(sink) => {
                let Ok(value) = serde_json::to_value(entry) else {
                    return;
                };
                sink.record(kind, &value);
            }
        }
    }

    /// Дожидается записи всех отправленных строк. Нужно там, где важно
    /// увидеть журнал сразу после вызова, — например в тестах. Для приёмника
    /// ждать нечего: записи уже отданы.
    pub fn shutdown(mut self) {
        if let Destination::Dir { sender, worker } = &mut self.destination {
            *sender = None;
            if let Some(worker) = worker.take() {
                let _ = worker.join();
            }
        }
    }
}

fn writer_loop(dir: PathBuf, receiver: Receiver<LogLine>) {
    let mut created = false;
    for entry in receiver {
        if !created {
            if std::fs::create_dir_all(&dir).is_err() {
                // Директория недоступна: дочитываем канал, чтобы отправитель
                // не копил строки, но на диск ничего не пишем.
                continue;
            }
            created = true;
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(entry.file))
        {
            let _ = writeln!(file, "{}", entry.line);
        }
    }
}

pub fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Идентификатор записи журнала: секунды плюс наносекунды текущего момента.
pub fn request_id() -> String {
    format!(
        "{}-{:x}",
        unix_timestamp(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("agentcore-log-{name}-{}", request_id()))
    }

    #[test]
    fn to_dir_writes_request_line() {
        let dir = temp_dir("write");
        let log = ExchangeLog::to_dir(dir.clone());
        log.log_request(&RequestLogEntry {
            id: "id-1",
            timestamp: 42,
            url: "https://example.test/chat",
            model: "model-1",
            request: serde_json::json!({ "hello": "world" }),
        });
        log.shutdown();

        let content = std::fs::read_to_string(dir.join(REQUESTS_FILE)).expect("файл журнала");
        let parsed: serde_json::Value =
            serde_json::from_str(content.trim()).expect("строка журнала — JSON");
        assert_eq!(parsed["id"], "id-1");
        assert_eq!(parsed["model"], "model-1");
        assert_eq!(parsed["request"]["hello"], "world");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn to_sink_receives_both_halves() {
        #[derive(Default)]
        struct Collector(std::sync::Mutex<Vec<(ExchangeKind, serde_json::Value)>>);

        impl ExchangeSink for Collector {
            fn record(&self, kind: ExchangeKind, entry: &serde_json::Value) {
                self.0.lock().expect("буфер").push((kind, entry.clone()));
            }
        }

        let sink = Arc::new(Collector::default());
        let log = ExchangeLog::to_sink(sink.clone());
        assert!(log.is_enabled());
        log.log_request(&RequestLogEntry {
            id: "id-3",
            timestamp: 42,
            url: "https://example.test/chat",
            model: "model-1",
            request: serde_json::json!({ "hello": "world" }),
        });
        log.log_response(&ResponseLogEntry {
            id: "id-3",
            timestamp: 43,
            status: 200,
            duration_ms: 5,
            response: serde_json::json!({ "ok": true }),
        });
        log.shutdown();

        let entries = sink.0.lock().expect("буфер");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, ExchangeKind::Request);
        assert_eq!(entries[0].1["id"], "id-3");
        assert_eq!(entries[0].1["model"], "model-1");
        assert_eq!(entries[0].1["request"]["hello"], "world");
        assert_eq!(entries[1].0, ExchangeKind::Response);
        assert_eq!(entries[1].1["status"], 200);
        assert_eq!(entries[1].1["response"]["ok"], true);
    }

    #[test]
    fn disabled_creates_nothing() {
        let dir = temp_dir("disabled");
        let log = ExchangeLog::disabled();
        assert!(!log.is_enabled());
        log.log_response(&ResponseLogEntry {
            id: "id-2",
            timestamp: 42,
            status: 200,
            duration_ms: 1,
            response: serde_json::Value::Null,
        });
        log.shutdown();
        assert!(!dir.exists());
    }
}
