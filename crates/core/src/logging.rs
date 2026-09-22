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

    /// Каждая запись проходит `redact_secrets`: результаты инструментов
    /// (диффы, логи) попадают и в журнал вызовов инструментов, и в тела
    /// запросов к модели, и одно место маскирования закрывает все пути.
    fn send<T: Serialize>(&self, kind: ExchangeKind, file: &'static str, entry: &T) {
        if matches!(self.destination, Destination::Disabled) {
            return;
        }
        let Ok(value) = serde_json::to_value(entry) else {
            return;
        };
        let value = redact_secrets(&value);
        match &self.destination {
            Destination::Disabled => {}
            Destination::Dir { sender, .. } => {
                let Some(sender) = sender else { return };
                let Ok(line) = serde_json::to_string(&value) else {
                    return;
                };
                let _ = sender.send(LogLine { file, line });
            }
            Destination::Sink(sink) => sink.record(kind, &value),
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

/// Префиксы, с которых начинаются ключи распространённых сервисов.
const SECRET_PREFIXES: [&str; 5] = ["sk-", "ghp_", "github_pat_", "xox", "AKIA"];

/// Ключи, значение после которых (`ключ=значение`) считается секретом.
const SECRET_ASSIGNMENTS: [&str; 4] = ["password=", "token=", "secret=", "api_key="];

/// Минимальная длина ключа с префиксом: короче — скорее обычное слово
/// (`sk-learn`), чем ключ.
const MIN_PREFIXED_SECRET_LEN: usize = 12;

/// Копия JSON, в строках которой замаскированы значения, похожие на
/// секреты: ключи с известными префиксами, `Bearer <…>` и значения после
/// `password=`, `token=`, `secret=`, `api_key=`.
///
/// Регулярных выражений в ядре нет, а правила простые, поэтому разбор
/// ручной. Маска — как `mask` у сервиса (`secr***alue`): по краям видно,
/// какой ключ был, но не он сам.
pub fn redact_secrets(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(text) => Value::String(redact_text(text)),
        Value::Array(items) => Value::Array(items.iter().map(redact_secrets).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), redact_secrets(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Символы, которыми заканчивается значение после `ключ=` или `Bearer `.
fn ends_value(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '&' | ',' | ';' | ')' | ']' | '}')
}

fn mask_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    match chars.len() {
        n if n <= 8 => "*".repeat(n),
        n => {
            let head: String = chars[..4].iter().collect();
            let tail: String = chars[n - 4..].iter().collect();
            format!("{head}***{tail}")
        }
    }
}

fn redact_text(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = chars.iter().map(|c| c.to_ascii_lowercase()).collect();
    let starts_with = |haystack: &[char], at: usize, needle: &str| {
        let needle: Vec<char> = needle.chars().collect();
        haystack.len() >= at + needle.len() && haystack[at..at + needle.len()] == needle[..]
    };
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    'scan: while i < chars.len() {
        let at_boundary = i == 0 || !is_token_char(chars[i - 1]);
        if at_boundary {
            for prefix in SECRET_PREFIXES {
                if starts_with(&chars, i, prefix) {
                    let end = (i..chars.len())
                        .find(|&j| !is_token_char(chars[j]))
                        .unwrap_or(chars.len());
                    if end - i >= MIN_PREFIXED_SECRET_LEN {
                        let token: String = chars[i..end].iter().collect();
                        out.push_str(&mask_secret(&token));
                        i = end;
                        continue 'scan;
                    }
                }
            }
            let markers = ["bearer "].into_iter().chain(SECRET_ASSIGNMENTS);
            for marker in markers {
                if starts_with(&lower, i, marker) {
                    let start = i + marker.chars().count();
                    let end = (start..chars.len())
                        .find(|&j| ends_value(chars[j]))
                        .unwrap_or(chars.len());
                    if end > start {
                        out.extend(&chars[i..start]);
                        let secret: String = chars[start..end].iter().collect();
                        out.push_str(&mask_secret(&secret));
                        i = end;
                        continue 'scan;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
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

    #[test]
    fn secrets_are_masked_in_written_journal() {
        let dir = temp_dir("redact");
        let log = ExchangeLog::to_dir(dir.clone());
        log.log_request(&RequestLogEntry {
            id: "id-4",
            timestamp: 42,
            url: "mcp+stdio://mcp-server-git/tools/call",
            model: "git_diff",
            request: serde_json::json!({
                "diff": "+OPENAI_KEY=sk-abcdefghijklmnop1234\n+db password=hunter2secret",
                "nested": ["Authorization: Bearer abcdef0123456789"],
            }),
        });
        log.shutdown();

        let content = std::fs::read_to_string(dir.join(REQUESTS_FILE)).expect("файл журнала");
        assert!(!content.contains("sk-abcdefghijklmnop1234"), "{content}");
        assert!(!content.contains("hunter2secret"), "{content}");
        assert!(!content.contains("abcdef0123456789"), "{content}");
        assert!(content.contains("sk-a***1234"), "{content}");
        assert!(content.contains("password=hunt***cret"), "{content}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ordinary_text_is_not_redacted() {
        let value = serde_json::json!({
            "text": "task-list sk-learn tokens=3 xoxo disk-usage",
            "n": 5,
        });
        assert_eq!(redact_secrets(&value), value);
    }

    #[test]
    fn prefixed_keys_are_masked() {
        let value = serde_json::json!("ключ ghp_0123456789abcdefABCD и AKIAABCDEFGHIJKLMNOP");
        let redacted = redact_secrets(&value);
        let text = redacted.as_str().unwrap();
        assert!(!text.contains("ghp_0123456789abcdefABCD"));
        assert!(!text.contains("AKIAABCDEFGHIJKLMNOP"));
        assert!(text.starts_with("ключ ghp_***ABCD"));
    }
}
