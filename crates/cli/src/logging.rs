//! Где консольный клиент держит журнал обмена с провайдером.
//!
//! Путь больше не привязан к каталогу исходников: по умолчанию журнал
//! пишется в пользовательскую директорию данных, а `AGENTCLI_LOG_DIR`
//! переопределяет её.

use agentcore::logging::ExchangeLog;
use std::path::PathBuf;
use std::sync::Arc;

/// Переменная окружения, задающая свою директорию журнала.
pub const LOG_DIR_ENV: &str = "AGENTCLI_LOG_DIR";

/// Подсказка консольного клиента в сообщении об отказе аутентификации.
pub const UNAUTHORIZED_HINT: &str = "Задайте токен в настройках чата \
    (Ctrl+P → «Подключение») или выполните: agentcli config set-token <TOKEN>";

pub fn log_dir() -> Option<PathBuf> {
    if let Ok(value) = std::env::var(LOG_DIR_ENV) {
        if !value.trim().is_empty() {
            return Some(PathBuf::from(value));
        }
    }
    dirs::data_dir().map(|dir| dir.join("agentcli").join("logs"))
}

/// Журнал обмена для консольного клиента. Если директорию данных определить
/// не удалось, журнал выключается — но диалог продолжает работать.
pub fn exchange_log() -> Arc<ExchangeLog> {
    Arc::new(match log_dir() {
        Some(dir) => ExchangeLog::to_dir(dir),
        None => ExchangeLog::disabled(),
    })
}
