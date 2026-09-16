//! Чаты сервиса `agentd` со стороны клиента.
//!
//! Хранилища чатов у консольного клиента нет: список, история и настройки
//! живут в сервисе, а этот тип инкапсулирует его контракт `/v1/chats`
//! (specs/client-chat-storage). Разбор конверта ошибок и трактовка кодов —
//! те же, что у [`ServerAgent`](crate::ServerAgent): вызывающая сторона
//! различает причины через `downcast_ref::<AgentError>`, а не по тексту.

use crate::{header_request_id, parse_service_error, ResponseFormatPayload};
use agentcore::agent::{transport_error, AgentError, Message, MessageMeta, Role};
use agentcore::config::{ChatSettings, ContextStrategy, Provider, ReasoningMode, ThinkingMode};
use agentcore::logging::{request_id, unix_timestamp, ExchangeLog, RequestLogEntry, ResponseLogEntry};
use anyhow::Result;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;

/// Размер страницы списка чатов. Совпадает с максимумом сервиса: страницы
/// дочитываются по курсору, и крупная страница экономит запросы.
const CHATS_PAGE_LIMIT: u32 = 200;
/// Размер страницы сообщений одного чата, тоже максимум сервиса.
const MESSAGES_PAGE_LIMIT: u32 = 500;

/// Чат без истории: то, что отдаёт список.
#[derive(Debug, Clone)]
pub struct ChatSummary {
    pub id: String,
    pub title: String,
    pub settings: ChatSettings,
    pub created_at: i64,
    pub updated_at: i64,
    pub message_count: i64,
}

/// Сообщение чата вместе с назначенным сервисом номером.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub seq: i64,
    pub created_at: i64,
    pub message: Message,
}

/// Чат вместе с полной историей.
#[derive(Debug, Clone)]
pub struct ChatHistory {
    pub chat: ChatSummary,
    pub messages: Vec<StoredMessage>,
    /// Ветка, чья история возвращена. `None` — сервис без веток
    /// (совместимость со старым контрактом).
    pub branch_id: Option<String>,
}

/// Факт чата — пара «ключ-значение» стратегии `facts`.
#[derive(Debug, Clone)]
pub struct Fact {
    pub key: String,
    pub value: String,
    pub updated_at: i64,
    pub through_seq: i64,
}

/// Запись рабочей памяти активной задачи чата — стратегия `memory_layers`.
#[derive(Debug, Clone)]
pub struct WorkingMemoryEntry {
    pub key: String,
    pub value: String,
    pub source: String,
    pub updated_at: i64,
}

/// Запись долговременной памяти владельца — стратегия `memory_layers`.
#[derive(Debug, Clone)]
pub struct LongTermMemoryEntry {
    pub id: String,
    pub entry_type: String,
    pub key: Option<String>,
    pub value: String,
    pub source: String,
    pub updated_at: i64,
}

/// Профиль владельца: встроенный (`teacher`/`psychologist`/`reviewer`) или
/// собственный (specs/user-profiles).
#[derive(Debug, Clone)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub persona: String,
    pub style: String,
    pub format: String,
    pub constraints: Vec<String>,
    pub built_in: bool,
}

/// Переход в журнале состояния задачи (specs/task-state, «Журнал переходов
/// задачи»).
#[derive(Debug, Clone)]
pub struct TaskTransition {
    pub from_stage: String,
    pub to_stage: String,
    pub source: String,
    pub reason: String,
    pub created_at: i64,
}

/// Состояние активной задачи чата: этап, шаг, ожидаемое действие, пауза и
/// журнал переходов (specs/task-state).
#[derive(Debug, Clone)]
pub struct TaskState {
    pub id: String,
    pub stage: String,
    pub step: String,
    pub expected_action: String,
    pub paused: bool,
    pub resume_brief: String,
    pub transitions: Vec<TaskTransition>,
}

/// Допустимые рёбра автомата состояния задачи (design.md, решение 3), в
/// одном месте с сервером: клиент не провоцирует заведомо отклоняемый
/// запрос, но окончательным арбитром остаётся сервер (design.md, решение 9).
const TASK_STAGE_EDGES: [(&str, &str); 5] = [
    ("planning", "execution"),
    ("execution", "validation"),
    ("validation", "done"),
    ("validation", "execution"),
    ("execution", "planning"),
];

/// Этапы, в которые можно перейти из данного, в порядке объявления рёбер —
/// используется экраном состояния задачи, чтобы не предлагать заведомо
/// недопустимые переходы (specs/task-state, design.md решение 9).
pub fn allowed_next_stages(from: &str) -> Vec<&'static str> {
    TASK_STAGE_EDGES.iter().filter(|(a, _)| *a == from).map(|(_, b)| *b).collect()
}

/// Ветка чата стратегии `branching`.
#[derive(Debug, Clone)]
pub struct Branch {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    /// Точка отделения — порядковый номер сообщения родителя. `None` — ветка
    /// корневая.
    pub fork_seq: Option<i64>,
    pub message_count: i64,
    pub active: bool,
}

pub struct ChatsClient {
    client: reqwest::Client,
    server_url: String,
    /// Пустая строка означает «токен не задан»: сервис с пустым списком
    /// клиентских токенов аутентификацию не проверяет.
    token: String,
    log: Arc<ExchangeLog>,
    /// Подсказка вызывающей стороны в сообщении об отказе аутентификации.
    unauthorized_hint: Option<String>,
}

impl ChatsClient {
    pub fn new(server_url: impl Into<String>, token: impl Into<String>, log: Arc<ExchangeLog>) -> Self {
        Self {
            client: reqwest::Client::new(),
            server_url: server_url.into(),
            token: token.into(),
            log,
            unauthorized_hint: None,
        }
    }

    pub fn with_unauthorized_hint(mut self, hint: impl Into<String>) -> Self {
        self.unauthorized_hint = Some(hint.into());
        self
    }

    fn url(&self, suffix: &str) -> String {
        format!("{}/v1{suffix}", self.server_url.trim_end_matches('/'))
    }

    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.token.trim().is_empty() {
            request
        } else {
            request.bearer_auth(self.token.trim())
        }
    }

    /// Один запрос к сервису: журнал обмена, разбор конверта ошибок и
    /// разбор успешного тела. `None` в `body` — запрос без тела.
    async fn send<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        url: String,
        body: Option<serde_json::Value>,
    ) -> Result<T> {
        let raw = self.send_raw(method, url, body).await?;
        // Ответ без тела (`204 No Content`) разбирается как `null`: так
        // `delete` возвращает `()` тем же путём, что остальные операции.
        let text = if raw.trim().is_empty() { "null" } else { raw.as_str() };
        serde_json::from_str(text)
            .map_err(|err| AgentError::Decode(format!("не удалось разобрать ответ сервиса: {err}")).into())
    }

    async fn send_raw(
        &self,
        method: reqwest::Method,
        url: String,
        body: Option<serde_json::Value>,
    ) -> Result<String> {
        let id = request_id();
        self.log.log_request(&RequestLogEntry {
            id: &id,
            timestamp: unix_timestamp(),
            url: &url,
            // Модели у операций над чатами нет: запись описывает не обмен с
            // моделью, а обращение к хранилищу сервиса.
            model: "",
            request: body.clone().unwrap_or(serde_json::Value::Null),
        });

        let started_at = Instant::now();
        let mut request = self.authorize(self.client.request(method, &url));
        if let Some(body) = &body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|err| {
            transport_error(
                &format!("сервис недоступен по адресу {}", self.server_url),
                err,
            )
        })?;

        let status = response.status();
        let header_id = header_request_id(&response);
        let text = response
            .text()
            .await
            .map_err(|err| transport_error("не удалось прочитать ответ сервиса", err))?;

        self.log.log_response(&ResponseLogEntry {
            id: &id,
            timestamp: unix_timestamp(),
            status: status.as_u16(),
            duration_ms: started_at.elapsed().as_millis(),
            response: serde_json::from_str::<serde_json::Value>(&text)
                .unwrap_or(serde_json::Value::String(text.clone())),
        });

        if !status.is_success() {
            return Err(parse_service_error(
                status,
                &text,
                header_id,
                self.unauthorized_hint.clone(),
            )
            .into());
        }
        Ok(text)
    }

    /// Все чаты клиента, от недавно изменённого к более старому. Страницы
    /// дочитываются по курсору: список чатов целиком нужен боковой панели.
    pub async fn list(&self) -> Result<Vec<ChatSummary>> {
        let mut chats = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let url = match &cursor {
                Some(cursor) => self.url(&format!("/chats?limit={CHATS_PAGE_LIMIT}&cursor={cursor}")),
                None => self.url(&format!("/chats?limit={CHATS_PAGE_LIMIT}")),
            };
            let page: ChatListPayload = self.send(reqwest::Method::GET, url, None).await?;
            chats.extend(page.chats.into_iter().map(ChatSummary::from));
            match page.next_cursor.filter(|cursor| !cursor.trim().is_empty()) {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(chats)
    }

    /// Чат вместе с историей целиком: сообщения дочитываются по `next_after`,
    /// потому что отправка реплики с неполной историей испортила бы контекст.
    pub async fn load(&self, id: &str) -> Result<ChatHistory> {
        self.load_branch(id, None).await
    }

    /// История чата, необязательно с явным указанием ветки. `None` —
    /// возвращается история активной ветки (specs/chat-branching, «Чтение
    /// истории учитывает ветку»).
    pub async fn load_branch(&self, id: &str, branch_id: Option<&str>) -> Result<ChatHistory> {
        let mut messages = Vec::new();
        let mut after: i64 = 0;
        let mut chat: Option<ChatSummary> = None;
        let mut branch: Option<String>;
        loop {
            let branch_query = branch_id
                .map(|b| format!("&branch={b}"))
                .unwrap_or_default();
            let url = self.url(&format!(
                "/chats/{id}?limit={MESSAGES_PAGE_LIMIT}&after={after}{branch_query}"
            ));
            let page: ChatHistoryPayload = self.send(reqwest::Method::GET, url, None).await?;
            // Поля чата одинаковы на всех страницах его сообщений, поэтому
            // берутся с первой.
            chat.get_or_insert_with(|| ChatSummary::from(page.chat.clone()));
            branch = page.branch_id.clone();
            messages.extend(page.messages.into_iter().map(StoredMessage::from));
            match page.next_after {
                Some(next) => after = next,
                None => break,
            }
        }
        Ok(ChatHistory {
            chat: chat.expect("страница чата разобрана хотя бы один раз"),
            messages,
            branch_id: branch,
        })
    }

    /// Факты чата стратегии `facts` (specs/context-facts, «Факты читаются и
    /// правятся вручную»).
    pub async fn facts(&self, chat_id: &str) -> Result<Vec<Fact>> {
        let payload: FactsPayload = self
            .send(
                reqwest::Method::GET,
                self.url(&format!("/chats/{chat_id}/facts")),
                None,
            )
            .await?;
        Ok(payload.facts.into_iter().map(Fact::from).collect())
    }

    /// Установка значения факта по ключу: существующий ключ перезаписывается.
    pub async fn set_fact(&self, chat_id: &str, key: &str, value: &str) -> Result<Fact> {
        let payload: FactPayload = self
            .send(
                reqwest::Method::PUT,
                self.url(&format!("/chats/{chat_id}/facts/{key}")),
                Some(serde_json::json!({ "value": value })),
            )
            .await?;
        Ok(Fact::from(payload))
    }

    pub async fn delete_fact(&self, chat_id: &str, key: &str) -> Result<()> {
        self.send_raw(
            reqwest::Method::DELETE,
            self.url(&format!("/chats/{chat_id}/facts/{key}")),
            None,
        )
        .await?;
        Ok(())
    }

    /// Ветки чата стратегии `branching` (specs/chat-branching, «Ветки
    /// перечисляются и переключаются»).
    pub async fn branches(&self, chat_id: &str) -> Result<Vec<Branch>> {
        let payload: BranchesPayload = self
            .send(
                reqwest::Method::GET,
                self.url(&format!("/chats/{chat_id}/branches")),
                None,
            )
            .await?;
        Ok(payload.branches.into_iter().map(Branch::from).collect())
    }

    /// Ветка от указанного сообщения — точки ветвления.
    pub async fn create_branch(&self, chat_id: &str, from_seq: i64, name: &str) -> Result<Branch> {
        let payload: BranchPayload = self
            .send(
                reqwest::Method::POST,
                self.url(&format!("/chats/{chat_id}/branches")),
                Some(serde_json::json!({ "from_seq": from_seq, "name": name })),
            )
            .await?;
        Ok(Branch::from(payload))
    }

    pub async fn activate_branch(&self, chat_id: &str, branch_id: &str) -> Result<()> {
        self.send_raw(
            reqwest::Method::POST,
            self.url(&format!("/chats/{chat_id}/branches/{branch_id}/activate")),
            None,
        )
        .await?;
        Ok(())
    }

    /// Рабочая память активной задачи чата (specs/memory-layers, «Ручное
    /// управление памятью через HTTP»).
    pub async fn working_memory(&self, chat_id: &str) -> Result<Vec<WorkingMemoryEntry>> {
        let payload: WorkingMemoryPayload = self
            .send(
                reqwest::Method::GET,
                self.url(&format!("/chats/{chat_id}/memory/working")),
                None,
            )
            .await?;
        Ok(payload.entries.into_iter().map(WorkingMemoryEntry::from).collect())
    }

    pub async fn set_working_memory(&self, chat_id: &str, key: &str, value: &str) -> Result<WorkingMemoryEntry> {
        let payload: WorkingMemoryEntryPayload = self
            .send(
                reqwest::Method::POST,
                self.url(&format!("/chats/{chat_id}/memory/working")),
                Some(serde_json::json!({ "key": key, "value": value })),
            )
            .await?;
        Ok(WorkingMemoryEntry::from(payload))
    }

    pub async fn delete_working_memory(&self, chat_id: &str, key: &str) -> Result<()> {
        self.send_raw(
            reqwest::Method::DELETE,
            self.url(&format!("/chats/{chat_id}/memory/working?key={key}")),
            None,
        )
        .await?;
        Ok(())
    }

    /// Явное завершение текущей задачи: перенос отмеченных ключей в
    /// долговременную память, затем очистка рабочей памяти прежней задачи
    /// (specs/memory-layers, «Рабочая память привязана к задаче»).
    pub async fn finish_task(&self, chat_id: &str, carry_forward_keys: &[String]) -> Result<Vec<LongTermMemoryEntry>> {
        let payload: LongTermMemoryPayload = self
            .send(
                reqwest::Method::POST,
                self.url(&format!("/chats/{chat_id}/memory/working/finish-task")),
                Some(serde_json::json!({ "carry_forward_keys": carry_forward_keys })),
            )
            .await?;
        Ok(payload.entries.into_iter().map(LongTermMemoryEntry::from).collect())
    }

    /// Состояние активной задачи чата вместе с журналом переходов
    /// (specs/task-state).
    pub async fn task(&self, chat_id: &str) -> Result<TaskState> {
        let payload: TaskStatePayload = self
            .send(reqwest::Method::GET, self.url(&format!("/chats/{chat_id}/task")), None)
            .await?;
        Ok(TaskState::from(payload))
    }

    /// Переход этапа и/или правка шага и ожидаемого действия, без смены
    /// этапа при `stage: None` (specs/task-state, «Обновление шага без
    /// смены этапа»). `stage: Some("done")` завершает задачу и заводит
    /// новую в `planning` — сервер несёт её идентификатор, но экран задачи
    /// перечитывает состояние отдельным вызовом `task`.
    pub async fn task_transition(
        &self,
        chat_id: &str,
        stage: Option<&str>,
        step: Option<&str>,
        expected_action: Option<&str>,
        carry_forward_keys: &[String],
    ) -> Result<TaskState> {
        let payload: TaskStatePayload = self
            .send(
                reqwest::Method::POST,
                self.url(&format!("/chats/{chat_id}/task/transition")),
                Some(serde_json::json!({
                    "stage": stage,
                    "step": step,
                    "expected_action": expected_action,
                    "carry_forward_keys": carry_forward_keys,
                })),
            )
            .await?;
        Ok(TaskState::from(payload))
    }

    /// Ставит задачу на паузу с брифом возобновления, собранным сервером
    /// (specs/task-state, «Пауза на любом этапе»).
    pub async fn task_pause(&self, chat_id: &str) -> Result<TaskState> {
        let payload: TaskStatePayload = self
            .send(
                reqwest::Method::POST,
                self.url(&format!("/chats/{chat_id}/task/pause")),
                Some(serde_json::json!({})),
            )
            .await?;
        Ok(TaskState::from(payload))
    }

    /// Снимает задачу с паузы, сохраняя прежний этап, шаг и ожидаемое
    /// действие.
    pub async fn task_resume(&self, chat_id: &str) -> Result<TaskState> {
        let payload: TaskStatePayload = self
            .send(
                reqwest::Method::POST,
                self.url(&format!("/chats/{chat_id}/task/resume")),
                Some(serde_json::json!({})),
            )
            .await?;
        Ok(TaskState::from(payload))
    }

    /// Долговременная память владельца (specs/memory-layers).
    pub async fn long_term_memory(&self) -> Result<Vec<LongTermMemoryEntry>> {
        let payload: LongTermMemoryPayload =
            self.send(reqwest::Method::GET, self.url("/memory/long-term"), None).await?;
        Ok(payload.entries.into_iter().map(LongTermMemoryEntry::from).collect())
    }

    pub async fn set_long_term_memory(
        &self,
        entry_type: &str,
        key: Option<&str>,
        value: &str,
    ) -> Result<LongTermMemoryEntry> {
        let payload: LongTermMemoryEntryPayload = self
            .send(
                reqwest::Method::POST,
                self.url("/memory/long-term"),
                Some(serde_json::json!({ "entry_type": entry_type, "key": key, "value": value })),
            )
            .await?;
        Ok(LongTermMemoryEntry::from(payload))
    }

    pub async fn delete_long_term_memory(&self, id: &str) -> Result<()> {
        self.send_raw(reqwest::Method::DELETE, self.url(&format!("/memory/long-term?id={id}")), None).await?;
        Ok(())
    }

    /// Профили, доступные владельцу: встроенные плюс собственные
    /// (specs/user-profiles, «Встроенные профили видны без создания»).
    pub async fn profiles(&self) -> Result<Vec<Profile>> {
        let payload: ProfilesPayload = self.send(reqwest::Method::GET, self.url("/profiles"), None).await?;
        Ok(payload.profiles.into_iter().map(Profile::from).collect())
    }

    pub async fn profile(&self, id: &str) -> Result<Profile> {
        let payload: ProfilePayload =
            self.send(reqwest::Method::GET, self.url(&format!("/profiles/{id}")), None).await?;
        Ok(Profile::from(payload))
    }

    pub async fn create_profile(
        &self,
        name: &str,
        persona: &str,
        style: &str,
        format: &str,
        constraints: &[String],
    ) -> Result<Profile> {
        let payload: ProfilePayload = self
            .send(
                reqwest::Method::POST,
                self.url("/profiles"),
                Some(serde_json::json!({
                    "name": name,
                    "persona": persona,
                    "style": style,
                    "format": format,
                    "constraints": constraints,
                })),
            )
            .await?;
        Ok(Profile::from(payload))
    }

    pub async fn create(&self, title: Option<&str>, settings: &ChatSettings) -> Result<ChatSummary> {
        let mut body = serde_json::json!({ "settings": settings_payload(settings) });
        if let Some(title) = title.map(str::trim).filter(|title| !title.is_empty()) {
            body["title"] = serde_json::Value::String(title.to_string());
        }
        let chat: ChatPayload = self
            .send(reqwest::Method::POST, self.url("/chats"), Some(body))
            .await?;
        Ok(ChatSummary::from(chat))
    }

    /// Переименование и изменение настроек. Пустое изменение сервис
    /// отклоняет, поэтому вызывающая сторона обязана задать хотя бы одно.
    pub async fn update(
        &self,
        id: &str,
        title: Option<&str>,
        settings: Option<&ChatSettings>,
    ) -> Result<ChatSummary> {
        let mut body = serde_json::Map::new();
        if let Some(title) = title.map(str::trim).filter(|title| !title.is_empty()) {
            body.insert("title".to_string(), serde_json::Value::String(title.to_string()));
        }
        if let Some(settings) = settings {
            body.insert("settings".to_string(), settings_payload(settings));
        }
        let chat: ChatPayload = self
            .send(
                reqwest::Method::PATCH,
                self.url(&format!("/chats/{id}")),
                Some(serde_json::Value::Object(body)),
            )
            .await?;
        Ok(ChatSummary::from(chat))
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.send_raw(reqwest::Method::DELETE, self.url(&format!("/chats/{id}")), None)
            .await?;
        Ok(())
    }

    /// Дозапись готовых сообщений: обмен с локальной моделью получил сам
    /// клиент, и в сервис он уходит одним запросом — иначе в чате мог бы
    /// остаться вопрос без ответа.
    pub async fn append(&self, id: &str, messages: &[Message]) -> Result<Vec<i64>> {
        let payload: Vec<NewMessagePayload> = messages.iter().map(NewMessagePayload::from).collect();
        let response: AppendPayload = self
            .send(
                reqwest::Method::POST,
                self.url(&format!("/chats/{id}/messages")),
                Some(serde_json::json!({ "messages": payload })),
            )
            .await?;
        Ok(response.seqs)
    }
}

/// Настройки чата в формате `settings` контракта `/v1`. Поля сэмплирования
/// сериализуются всегда, включая `null`: клиент — источник правды по
/// настройкам чата, а явный `null` снимает значение на стороне сервиса.
fn settings_payload(settings: &ChatSettings) -> serde_json::Value {
    let sampling = &settings.sampling;
    let payload = ChatSettingsUpdate {
        provider: match settings.provider {
            Provider::Cloud => "cloud",
            Provider::Ollama => "ollama",
        },
        model: settings.model.clone().filter(|model| !model.trim().is_empty()),
        reasoning: settings.reasoning,
        thinking: settings.thinking,
        experts: settings.experts.clone(),
        response_format: Some(ResponseFormatPayload {
            description: settings.response_format.description.clone(),
            max_length: settings.response_format.max_length,
            stop: settings.response_format.stop.clone(),
            stop_instruction: settings.response_format.stop_instruction.clone(),
        }),
        // Поле идёт после `response_format`: сам формат включает кастомный
        // режим, а это поле задаёт его окончательно.
        custom_response_mode: settings.custom_response_mode,
        temperature: sampling.temperature,
        top_p: sampling.top_p,
        top_k: sampling.top_k,
        frequency_penalty: sampling.frequency_penalty,
        presence_penalty: sampling.presence_penalty,
        max_context_tokens: settings.max_context_tokens,
        summary_enabled: settings.summary_enabled,
        summary_keep_messages: settings.summary_keep_messages,
        summary_step_messages: settings.summary_step_messages,
        context_strategy: settings.context_strategy,
        context_window_messages: settings.context_window_messages,
        profile_id: settings.profile_id.clone(),
        task_state_enabled: settings.task_state_enabled,
        task_state_auto_enabled: settings.task_state_auto_enabled,
    };
    serde_json::to_value(payload).unwrap_or(serde_json::Value::Null)
}

/// Тело `POST /v1/chats` и `PATCH /v1/chats/{id}`: экран настроек чата
/// присылает своё состояние целиком, поэтому поля без `skip_serializing_if`
/// (кроме `model` и `response_format`, у которых есть собственное правило
/// «не задано») отправляются всегда, включая `null` — в отличие от разового
/// вызова `POST /v1/chat` (`ChatPayload` выше), где отсутствие поля обязано
/// оставлять сохранённый лимит и настройки компактизации чата нетронутыми
/// (specs/chat-context-limit, «Отправка лимита клиентом»).
#[derive(Serialize)]
struct ChatSettingsUpdate {
    provider: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    reasoning: ReasoningMode,
    thinking: ThinkingMode,
    experts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormatPayload>,
    custom_response_mode: bool,
    temperature: Option<f32>,
    top_p: Option<f32>,
    top_k: Option<u32>,
    frequency_penalty: Option<f32>,
    presence_penalty: Option<f32>,
    max_context_tokens: Option<u32>,
    summary_enabled: Option<bool>,
    summary_keep_messages: Option<u32>,
    summary_step_messages: Option<u32>,
    context_strategy: Option<ContextStrategy>,
    context_window_messages: Option<u32>,
    profile_id: Option<String>,
    task_state_enabled: Option<bool>,
    task_state_auto_enabled: Option<bool>,
}

#[derive(Serialize)]
struct NewMessagePayload {
    role: &'static str,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<UsagePayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timing: Option<TimingPayload>,
}

impl From<&Message> for NewMessagePayload {
    fn from(message: &Message) -> Self {
        // Пустые объекты не отправляются: у реплики пользователя счётчиков
        // токенов нет, и `usage: {}` в теле был бы шумом.
        let (usage, timing, model) = match &message.meta {
            Some(meta) => {
                let usage = UsagePayload {
                    prompt_tokens: meta.prompt_tokens,
                    completion_tokens: meta.completion_tokens,
                    total_tokens: meta.total_tokens,
                    reasoning_tokens: meta.reasoning_tokens,
                };
                let timing = TimingPayload {
                    duration_ms: meta.duration_ms,
                    sent_at: meta.sent_at,
                    received_at: meta.received_at,
                };
                (
                    usage.has_values().then_some(usage),
                    timing.has_values().then_some(timing),
                    meta.model.clone(),
                )
            }
            None => (None, None, None),
        };
        Self {
            role: match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
            },
            content: message.content.clone(),
            reasoning: message.reasoning.clone(),
            model,
            usage,
            timing,
        }
    }
}

impl UsagePayload {
    fn has_values(&self) -> bool {
        self.prompt_tokens.is_some()
            || self.completion_tokens.is_some()
            || self.total_tokens.is_some()
            || self.reasoning_tokens.is_some()
    }
}

impl TimingPayload {
    fn has_values(&self) -> bool {
        self.duration_ms.is_some() || self.sent_at.is_some() || self.received_at.is_some()
    }
}

#[derive(Serialize, Deserialize, Default)]
struct UsagePayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompt_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_tokens: Option<u32>,
}

#[derive(Serialize, Deserialize, Default)]
struct TimingPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sent_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    received_at: Option<i64>,
}

#[derive(Deserialize, Clone)]
struct ChatPayload {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    settings: ChatSettings,
    #[serde(default)]
    created_at: i64,
    #[serde(default)]
    updated_at: i64,
    #[serde(default)]
    message_count: i64,
}

impl From<ChatPayload> for ChatSummary {
    fn from(chat: ChatPayload) -> Self {
        Self {
            id: chat.id,
            title: chat.title,
            settings: chat.settings,
            created_at: chat.created_at,
            updated_at: chat.updated_at,
            message_count: chat.message_count,
        }
    }
}

#[derive(Deserialize)]
struct ChatListPayload {
    #[serde(default)]
    chats: Vec<ChatPayload>,
    #[serde(default)]
    next_cursor: Option<String>,
}

/// Поля чата лежат на верхнем уровне ответа `GET /v1/chats/{id}` рядом с
/// сообщениями, поэтому чат собирается из того же объекта.
#[derive(Deserialize)]
struct ChatHistoryPayload {
    #[serde(flatten)]
    chat: ChatPayload,
    #[serde(default)]
    messages: Vec<MessagePayload>,
    #[serde(default)]
    next_after: Option<i64>,
    /// Ветка, чья история отдана. `None` — сервис без веток.
    #[serde(default)]
    branch_id: Option<String>,
}

#[derive(Deserialize)]
struct FactsPayload {
    #[serde(default)]
    facts: Vec<FactPayload>,
}

#[derive(Deserialize)]
struct FactPayload {
    key: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    updated_at: i64,
    #[serde(default)]
    through_seq: i64,
}

impl From<FactPayload> for Fact {
    fn from(payload: FactPayload) -> Self {
        Self {
            key: payload.key,
            value: payload.value,
            updated_at: payload.updated_at,
            through_seq: payload.through_seq,
        }
    }
}

#[derive(Deserialize)]
struct WorkingMemoryPayload {
    #[serde(default)]
    entries: Vec<WorkingMemoryEntryPayload>,
}

#[derive(Deserialize)]
struct WorkingMemoryEntryPayload {
    key: String,
    value: String,
    source: String,
    updated_at: i64,
}

impl From<WorkingMemoryEntryPayload> for WorkingMemoryEntry {
    fn from(payload: WorkingMemoryEntryPayload) -> Self {
        Self { key: payload.key, value: payload.value, source: payload.source, updated_at: payload.updated_at }
    }
}

#[derive(Deserialize)]
struct TaskStatePayload {
    id: String,
    stage: String,
    step: String,
    expected_action: String,
    paused: bool,
    resume_brief: String,
    #[serde(default)]
    transitions: Vec<TaskTransitionPayload>,
}

#[derive(Deserialize)]
struct TaskTransitionPayload {
    from_stage: String,
    to_stage: String,
    source: String,
    reason: String,
    created_at: i64,
}

impl From<TaskTransitionPayload> for TaskTransition {
    fn from(payload: TaskTransitionPayload) -> Self {
        Self {
            from_stage: payload.from_stage,
            to_stage: payload.to_stage,
            source: payload.source,
            reason: payload.reason,
            created_at: payload.created_at,
        }
    }
}

impl From<TaskStatePayload> for TaskState {
    fn from(payload: TaskStatePayload) -> Self {
        Self {
            id: payload.id,
            stage: payload.stage,
            step: payload.step,
            expected_action: payload.expected_action,
            paused: payload.paused,
            resume_brief: payload.resume_brief,
            transitions: payload.transitions.into_iter().map(TaskTransition::from).collect(),
        }
    }
}

#[derive(Deserialize)]
struct ProfilesPayload {
    #[serde(default)]
    profiles: Vec<ProfilePayload>,
}

#[derive(Deserialize)]
struct ProfilePayload {
    id: String,
    name: String,
    persona: String,
    style: String,
    format: String,
    constraints: Vec<String>,
    built_in: bool,
}

impl From<ProfilePayload> for Profile {
    fn from(payload: ProfilePayload) -> Self {
        Self {
            id: payload.id,
            name: payload.name,
            persona: payload.persona,
            style: payload.style,
            format: payload.format,
            constraints: payload.constraints,
            built_in: payload.built_in,
        }
    }
}

#[derive(Deserialize)]
struct LongTermMemoryPayload {
    #[serde(default)]
    entries: Vec<LongTermMemoryEntryPayload>,
}

#[derive(Deserialize)]
struct LongTermMemoryEntryPayload {
    id: String,
    entry_type: String,
    #[serde(default)]
    key: Option<String>,
    value: String,
    source: String,
    updated_at: i64,
}

impl From<LongTermMemoryEntryPayload> for LongTermMemoryEntry {
    fn from(payload: LongTermMemoryEntryPayload) -> Self {
        Self {
            id: payload.id,
            entry_type: payload.entry_type,
            key: payload.key,
            value: payload.value,
            source: payload.source,
            updated_at: payload.updated_at,
        }
    }
}

#[derive(Deserialize)]
struct BranchesPayload {
    #[serde(default)]
    branches: Vec<BranchPayload>,
}

#[derive(Deserialize)]
struct BranchPayload {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    fork_seq: Option<i64>,
    #[serde(default)]
    message_count: i64,
    #[serde(default)]
    active: bool,
}

impl From<BranchPayload> for Branch {
    fn from(payload: BranchPayload) -> Self {
        Self {
            id: payload.id,
            name: payload.name,
            parent_id: payload.parent_id,
            fork_seq: payload.fork_seq,
            message_count: payload.message_count,
            active: payload.active,
        }
    }
}

#[derive(Deserialize)]
struct MessagePayload {
    seq: i64,
    role: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    created_at: i64,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<UsagePayload>,
    #[serde(default)]
    timing: Option<TimingPayload>,
}

impl From<MessagePayload> for StoredMessage {
    fn from(message: MessagePayload) -> Self {
        // Роль сервиса, отличная от известных, трактуется как ответ модели:
        // терять сообщение из-за незнакомого значения хуже, чем показать его
        // не с той стороны.
        let role = if message.role == "user" {
            Role::User
        } else {
            Role::Assistant
        };
        let usage = message.usage.unwrap_or_default();
        let timing = message.timing.unwrap_or_default();
        let meta = MessageMeta {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            duration_ms: timing.duration_ms,
            sent_at: timing.sent_at,
            received_at: timing.received_at,
            model: message.model,
        };
        let has_telemetry = meta.prompt_tokens.is_some()
            || meta.completion_tokens.is_some()
            || meta.total_tokens.is_some()
            || meta.reasoning_tokens.is_some()
            || meta.duration_ms.is_some()
            || meta.sent_at.is_some()
            || meta.received_at.is_some()
            || meta.model.is_some();
        Self {
            seq: message.seq,
            created_at: message.created_at,
            message: Message {
                role,
                content: message.content,
                reasoning: message.reasoning,
                meta: has_telemetry.then_some(meta),
            },
        }
    }
}

#[derive(Deserialize)]
struct AppendPayload {
    #[serde(default)]
    seqs: Vec<i64>,
}

#[cfg(test)]
mod tests;
