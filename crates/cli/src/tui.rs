use crate::agent::CliAgent;
use agentcore::agent::{AgentReply, Message, MessageMeta, Role};
use crate::chats::{self, ChatSession};
use agentclient::{
    Branch, ChatHistory, ChatSummary, ChatsClient, Fact, LongTermMemoryEntry, ProfileChoice, StoredMessage,
    WorkingMemoryEntry,
};
use agentcore::config::{
    ChatSettings, Config, ContextStrategy, Provider, ReasoningMode, ResponseFormat, SamplingParams,
    ThinkingMode,
};
use crate::markdown::agent_skin;
use ansi_to_tui::IntoText;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
        EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{FutureExt, StreamExt};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const BILLY_ART: &str = include_str!("assets/billy.ans");
/// Максимум одновременно открытых на экране чатов (панелей).
const MAX_PANES: usize = 3;

/// Исход загрузки списка чатов. Список принадлежит сервису, поэтому
/// «список неизвестен» и «список пуст» — разные состояния: во втором
/// случае писать некуда, но причина не в отказе (specs/client-chat-storage,
/// «Клиент работает при недоступном сервисе»).
enum ChatsLoad {
    Loading,
    Loaded,
    Failed(String),
}

enum ChatEvent {
    Response(String, anyhow::Result<AgentReply>),
    /// Список локальных моделей Ollama: пришёл фоновой задачей.
    OllamaModels(Result<Vec<String>, String>),
    /// Список облачных моделей сервиса: пришёл фоновой задачей.
    CloudModels(Result<Vec<String>, String>),
    /// Список профилей владельца (`GET /v1/profiles`): пришёл фоновой
    /// задачей (specs/user-profiles).
    Profiles(Result<Vec<ProfileChoice>, String>),
    /// Список чатов сервиса (`GET /v1/chats`).
    ChatsLoaded(Result<Vec<ChatSummary>, String>),
    /// История одного чата (`GET /v1/chats/{id}`).
    HistoryLoaded(String, Result<ChatHistory, String>),
    /// Созданный сервисом чат (`POST /v1/chats`).
    ChatCreated(Result<ChatSummary, String>),
    /// Подтверждённое сервисом изменение чата (`PATCH /v1/chats/{id}`).
    ChatUpdated(String, Result<ChatSummary, String>),
    /// Подтверждённое сервисом удаление чата (`DELETE /v1/chats/{id}`).
    ChatDeleted(String, Result<(), String>),
    /// Дозапись обмена локального чата (`POST /v1/chats/{id}/messages`).
    ExchangeSaved(String, Result<(), String>),
    /// Факты чата (`GET /v1/chats/{id}/facts`).
    FactsLoaded(String, Result<Vec<Fact>, String>),
    /// Установка значения факта (`PUT /v1/chats/{id}/facts/{key}`).
    FactSet(String, Result<Fact, String>),
    /// Удаление факта (`DELETE /v1/chats/{id}/facts/{key}`).
    FactDeleted(String, Result<String, String>),
    /// Ветки чата (`GET /v1/chats/{id}/branches`).
    BranchesLoaded(String, Result<Vec<Branch>, String>),
    /// Сообщения чата для выбора точки ветвления (`GET /v1/chats/{id}`).
    BranchSourceMessagesLoaded(String, Result<Vec<StoredMessage>, String>),
    /// Созданная ветка (`POST /v1/chats/{id}/branches`).
    BranchCreated(String, Result<Branch, String>),
    /// Подтверждённое переключение активной ветки
    /// (`POST /v1/chats/{id}/branches/{branch_id}/activate`).
    BranchActivated(String, Result<String, String>),
    /// Рабочая память чата (`GET /v1/chats/{id}/memory/working`).
    WorkingMemoryLoaded(String, Result<Vec<WorkingMemoryEntry>, String>),
    /// Установка записи рабочей памяти (`POST /v1/chats/{id}/memory/working`).
    WorkingMemorySet(String, Result<WorkingMemoryEntry, String>),
    /// Удаление записи рабочей памяти (`DELETE /v1/chats/{id}/memory/working`).
    WorkingMemoryDeleted(String, Result<String, String>),
    /// Завершение задачи (`POST /v1/chats/{id}/memory/working/finish-task`).
    TaskFinished(String, Result<Vec<LongTermMemoryEntry>, String>),
    /// Долговременная память владельца (`GET /v1/memory/long-term`).
    LongTermMemoryLoaded(String, Result<Vec<LongTermMemoryEntry>, String>),
    /// Установка записи долговременной памяти (`POST /v1/memory/long-term`).
    LongTermMemorySet(String, Result<LongTermMemoryEntry, String>),
    /// Удаление записи долговременной памяти (`DELETE /v1/memory/long-term`).
    LongTermMemoryDeleted(String, Result<String, String>),
    /// Состояние задачи чата (`GET /v1/chats/{id}/task`).
    TaskLoaded(String, Result<agentclient::TaskState, String>),
    /// Переход состояния задачи (`POST /v1/chats/{id}/task/transition`).
    TaskTransitioned(String, Result<agentclient::TaskState, String>),
    /// Пауза задачи (`POST /v1/chats/{id}/task/pause`).
    TaskPaused(String, Result<agentclient::TaskState, String>),
    /// Снятие задачи с паузы (`POST /v1/chats/{id}/task/resume`).
    TaskResumed(String, Result<agentclient::TaskState, String>),
}

#[derive(PartialEq)]
enum Focus {
    Input,
    Sidebar,
    /// Режим выбора сообщения в истории: перебор сообщений с клавиатуры ради
    /// копирования текста одного из них.
    MessageSelect,
    Settings,
    Import,
    Confirm,
    Facts,
    Branches,
    Memory,
    Task,
}

/// Запрос подтверждения на удаление чата.
struct DeleteConfirm {
    chat_id: String,
    chat_title: String,
}

/// Экран фактов чата: список, правка значения, удаление ключа, добавление
/// нового ключа (specs/context-facts, «Факты читаются и правятся вручную»).
struct FactsPicker {
    chat_id: String,
    chat_title: String,
    facts: Vec<Fact>,
    cursor: usize,
    loading: bool,
    error: Option<String>,
    /// Открытый редактор факта: `Some` — идёт правка значения или создание
    /// новой пары «ключ-значение».
    editor: Option<FactEditor>,
}

struct FactEditor {
    /// Пусто и редактируемо — создание новой пары; иначе — правка значения
    /// существующего ключа, и поле ключа недоступно вводу.
    key: String,
    value: String,
    /// Поле, принимающее ввод: ключ — только у новой записи.
    editing_key: bool,
}

impl FactsPicker {
    fn new(chat_id: &str, chat_title: &str) -> Self {
        Self {
            chat_id: chat_id.to_string(),
            chat_title: chat_title.to_string(),
            facts: Vec::new(),
            cursor: 0,
            loading: true,
            error: None,
            editor: None,
        }
    }

    fn move_cursor(&mut self, delta: i32) {
        let len = self.facts.len() as i32;
        if len == 0 {
            return;
        }
        self.cursor = (self.cursor as i32 + delta).rem_euclid(len) as usize;
    }

    fn selected_key(&self) -> Option<&str> {
        self.facts.get(self.cursor).map(|f| f.key.as_str())
    }
}

/// Экран веток чата: список с активной веткой, создание ветки от выбранного
/// сообщения, переключение (specs/chat-branching, «Управление ветками из
/// клиента»).
struct BranchesPicker {
    chat_id: String,
    chat_title: String,
    branches: Vec<Branch>,
    branch_cursor: usize,
    loading: bool,
    error: Option<String>,
    /// Выбор точки ветвления и имени новой ветки, если он открыт.
    creating: Option<BranchCreation>,
}

struct BranchCreation {
    /// Сообщения чата — источник точек ветвления, с их `seq`.
    messages: Vec<StoredMessage>,
    message_cursor: usize,
    loading: bool,
    /// `Some` — сообщение выбрано, идёт ввод имени новой ветки.
    name: Option<String>,
}

impl BranchesPicker {
    fn new(chat_id: &str, chat_title: &str) -> Self {
        Self {
            chat_id: chat_id.to_string(),
            chat_title: chat_title.to_string(),
            branches: Vec::new(),
            branch_cursor: 0,
            loading: true,
            error: None,
            creating: None,
        }
    }

    fn move_cursor(&mut self, delta: i32) {
        let len = self.branches.len() as i32;
        if len == 0 {
            return;
        }
        self.branch_cursor = (self.branch_cursor as i32 + delta).rem_euclid(len) as usize;
    }

    fn selected_branch_id(&self) -> Option<&str> {
        self.branches.get(self.branch_cursor).map(|b| b.id.as_str())
    }
}

/// Разделы экрана слоистой памяти (specs/memory-layers).
#[derive(Clone, Copy, PartialEq)]
enum MemorySection {
    /// Только просмотр хвоста сообщений — редактирования нет: краткосрочная
    /// память это сама история чата, а не отдельное хранилище.
    ShortTerm,
    Working,
    LongTerm,
}

impl MemorySection {
    const ALL: [MemorySection; 3] = [MemorySection::ShortTerm, MemorySection::Working, MemorySection::LongTerm];

    fn label(self) -> &'static str {
        match self {
            MemorySection::ShortTerm => "Краткосрочная",
            MemorySection::Working => "Рабочая",
            MemorySection::LongTerm => "Долговременная",
        }
    }
}

/// Экран слоистой памяти чата: три раздела — краткосрочная (только просмотр
/// хвоста сообщений), рабочая и долговременная (просмотр, добавление,
/// правка, удаление записей) (specs/memory-layers, «Ручное управление
/// памятью через HTTP»).
struct MemoryPicker {
    chat_id: String,
    chat_title: String,
    section: usize,
    working: Vec<WorkingMemoryEntry>,
    working_cursor: usize,
    working_loading: bool,
    working_error: Option<String>,
    long_term: Vec<LongTermMemoryEntry>,
    long_term_cursor: usize,
    long_term_loading: bool,
    long_term_error: Option<String>,
    /// Открытый редактор записи рабочей или долговременной памяти.
    editor: Option<MemoryEditor>,
}

struct MemoryEditor {
    for_long_term: bool,
    key: String,
    value: String,
    /// Только для долговременной памяти: `profile`/`decision`/`knowledge`.
    entry_type: String,
    /// Поле, принимающее ввод: 0 — ключ, 1 — значение, 2 — тип записи
    /// (только долговременная).
    field: usize,
}

impl MemoryPicker {
    fn new(chat_id: &str, chat_title: &str) -> Self {
        Self {
            chat_id: chat_id.to_string(),
            chat_title: chat_title.to_string(),
            section: 0,
            working: Vec::new(),
            working_cursor: 0,
            working_loading: true,
            working_error: None,
            long_term: Vec::new(),
            long_term_cursor: 0,
            long_term_loading: true,
            long_term_error: None,
            editor: None,
        }
    }

    fn current_section(&self) -> MemorySection {
        MemorySection::ALL[self.section.min(MemorySection::ALL.len() - 1)]
    }

    fn cycle_section(&mut self, delta: i32) {
        let len = MemorySection::ALL.len() as i32;
        self.section = (self.section as i32 + delta).rem_euclid(len) as usize;
    }

    fn move_cursor(&mut self, delta: i32) {
        match self.current_section() {
            MemorySection::ShortTerm => {}
            MemorySection::Working => {
                let len = self.working.len() as i32;
                if len > 0 {
                    self.working_cursor = (self.working_cursor as i32 + delta).rem_euclid(len) as usize;
                }
            }
            MemorySection::LongTerm => {
                let len = self.long_term.len() as i32;
                if len > 0 {
                    self.long_term_cursor = (self.long_term_cursor as i32 + delta).rem_euclid(len) as usize;
                }
            }
        }
    }

    fn selected_working_key(&self) -> Option<&str> {
        self.working.get(self.working_cursor).map(|e| e.key.as_str())
    }

    fn selected_long_term_id(&self) -> Option<&str> {
        self.long_term.get(self.long_term_cursor).map(|e| e.id.as_str())
    }
}

/// Экран состояния задачи чата по `Ctrl+T`: этап, шаг, ожидаемое действие,
/// пауза и последние переходы; переход по допустимым рёбрам, правка
/// текстов, пауза и возобновление (specs/task-state, design.md решение 9).
struct TaskPicker {
    chat_id: String,
    chat_title: String,
    state: Option<agentclient::TaskState>,
    loading: bool,
    error: Option<String>,
    /// Индекс предложенного следующего этапа в
    /// `allowed_next_stages(текущий этап)`, сдвинутый на 1: 0 — «без смены
    /// этапа». Недопустимые рёбра в список не попадают — автомат на сервере
    /// остаётся арбитром, но клиент не предлагает заведомо отклоняемый
    /// переход (design.md, решение 9).
    next_stage_cursor: usize,
    /// Открытая правка шага или ожидаемого действия.
    editor: Option<TaskFieldEditor>,
}

struct TaskFieldEditor {
    editing_expected_action: bool,
    value: String,
}

impl TaskPicker {
    fn new(chat_id: &str, chat_title: &str) -> Self {
        Self {
            chat_id: chat_id.to_string(),
            chat_title: chat_title.to_string(),
            state: None,
            loading: true,
            error: None,
            next_stage_cursor: 0,
            editor: None,
        }
    }

    /// Допустимые следующие этапы для текущего состояния, пустой список у
    /// незагруженного состояния или у задачи в `done`.
    fn allowed_next_stages(&self) -> Vec<&'static str> {
        self.state
            .as_ref()
            .map(|task| agentclient::allowed_next_stages(&task.stage))
            .unwrap_or_default()
    }

    fn cycle_next_stage(&mut self, delta: i32) {
        let len = self.allowed_next_stages().len() as i32 + 1;
        self.next_stage_cursor = (self.next_stage_cursor as i32 + delta).rem_euclid(len) as usize;
    }

    /// `None` — «без смены этапа» (курсор на позиции 0).
    fn selected_next_stage(&self) -> Option<&'static str> {
        let options = self.allowed_next_stages();
        if self.next_stage_cursor == 0 {
            None
        } else {
            options.get(self.next_stage_cursor - 1).copied()
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum FormatField {
    Provider,
    Model,
    ServerUrl,
    ClientToken,
    OllamaUrl,
    ContextLimit,
    SummaryEnabled,
    SummaryKeepMessages,
    SummaryStepMessages,
    ContextStrategy,
    ContextWindowMessages,
    Profile,
    MemoryLayersEnabled,
    MemoryRouterEnabled,
    MemoryWorkingMaxEntries,
    MemoryLongTermMaxEntries,
    TaskStateEnabled,
    TaskStateAutoEnabled,
    Mode,
    Reasoning,
    Thinking,
    Experts,
    Description,
    MaxLength,
    Stop,
    StopInstruction,
    Temperature,
    TopP,
    TopK,
    FrequencyPenalty,
    PresencePenalty,
}

/// Раздел настроек: группирует поля по смыслу.
#[derive(Clone, Copy, PartialEq)]
enum SettingsSection {
    Connection,
    Context,
    Memory,
    Profile,
    Format,
    Reasoning,
    Sampling,
}

impl SettingsSection {
    const ALL: [SettingsSection; 7] = [
        SettingsSection::Connection,
        SettingsSection::Context,
        SettingsSection::Memory,
        SettingsSection::Profile,
        SettingsSection::Format,
        SettingsSection::Reasoning,
        SettingsSection::Sampling,
    ];

    fn label(self) -> &'static str {
        match self {
            SettingsSection::Connection => "Подключение",
            SettingsSection::Context => "Контекст",
            SettingsSection::Memory => "Память",
            SettingsSection::Profile => "Профиль",
            SettingsSection::Format => "Формат ответа",
            SettingsSection::Reasoning => "Рассуждение",
            SettingsSection::Sampling => "Сэмплинг",
        }
    }

    /// Пояснение раздела — показывается в нижней панели, пока фокус
    /// стоит на списке разделов (курсор ещё не зашёл в поля).
    fn description(self) -> &'static str {
        match self {
            SettingsSection::Connection => {
                "Провайдер, модель и адрес/токен сервиса — общие параметры доступа для этого чата."
            }
            SettingsSection::Context => {
                "Лимит контекстного окна и компактизация истории — настройки конкретно этого чата."
            }
            SettingsSection::Memory => {
                "Слоистая память: рабочая и долговременная память, независимо от стратегии контекста."
            }
            SettingsSection::Profile => {
                "Профиль владельца: роль, стиль, формат и ограничения ответа, заданные сервисом."
            }
            SettingsSection::Format => "Формат ответа: кастомный режим, длина, стоп-условия.",
            SettingsSection::Reasoning => {
                "Как агент подходит к задаче: стратегия рассуждения и режим thinking модели."
            }
            SettingsSection::Sampling => {
                "Параметры сэмплирования, передаются модели при генерации ответа."
            }
        }
    }

    fn fields(self) -> &'static [FormatField] {
        match self {
            SettingsSection::Connection => &[
                FormatField::Provider,
                FormatField::Model,
                FormatField::ServerUrl,
                FormatField::ClientToken,
                FormatField::OllamaUrl,
            ],
            SettingsSection::Context => &[
                FormatField::ContextLimit,
                FormatField::ContextStrategy,
                FormatField::ContextWindowMessages,
                FormatField::SummaryEnabled,
                FormatField::SummaryKeepMessages,
                FormatField::SummaryStepMessages,
            ],
            SettingsSection::Memory => &[
                FormatField::MemoryLayersEnabled,
                FormatField::MemoryRouterEnabled,
                FormatField::MemoryWorkingMaxEntries,
                FormatField::MemoryLongTermMaxEntries,
                FormatField::TaskStateEnabled,
                FormatField::TaskStateAutoEnabled,
            ],
            SettingsSection::Profile => &[FormatField::Profile],
            SettingsSection::Format => &[
                FormatField::Mode,
                FormatField::Description,
                FormatField::MaxLength,
                FormatField::Stop,
                FormatField::StopInstruction,
            ],
            SettingsSection::Reasoning => &[
                FormatField::Thinking,
                FormatField::Reasoning,
                FormatField::Experts,
            ],
            SettingsSection::Sampling => &[
                FormatField::Temperature,
                FormatField::TopP,
                FormatField::TopK,
                FormatField::FrequencyPenalty,
                FormatField::PresencePenalty,
            ],
        }
    }
}

/// Активная панель попапа настроек: список разделов либо поля раздела.
#[derive(Clone, Copy, PartialEq)]
enum SettingsPane {
    Sections,
    Fields,
}

/// Один чат-кандидат в окне импорта контекста.
struct ImportCandidate {
    id: String,
    title: String,
    messages: usize,
    selected: bool,
}

/// Состояние окна «импортировать контекст других чатов в текущий».
struct ImportPicker {
    /// Чат, в который переносится контекст.
    target_id: String,
    target_title: String,
    candidates: Vec<ImportCandidate>,
    cursor: usize,
}

impl ImportPicker {
    /// Кандидаты — все чаты с историей, кроме самого целевого.
    fn new(target: &ChatSession, chats: &[ChatSession]) -> Self {
        let candidates = chats
            .iter()
            .filter(|chat| chat.id != target.id && !chat.messages.is_empty())
            .map(|chat| ImportCandidate {
                id: chat.id.clone(),
                title: chat.title.clone(),
                messages: chat.messages.len(),
                selected: false,
            })
            .collect();
        Self {
            target_id: target.id.clone(),
            target_title: target.title.clone(),
            candidates,
            cursor: 0,
        }
    }

    fn move_cursor(&mut self, delta: i32) {
        let len = self.candidates.len() as i32;
        if len == 0 {
            return;
        }
        self.cursor = (self.cursor as i32 + delta).rem_euclid(len) as usize;
    }

    fn toggle_current(&mut self) {
        if let Some(candidate) = self.candidates.get_mut(self.cursor) {
            candidate.selected = !candidate.selected;
        }
    }

    fn selected_ids(&self) -> Vec<String> {
        self.candidates
            .iter()
            .filter(|c| c.selected)
            .map(|c| c.id.clone())
            .collect()
    }
}

impl FormatField {
    fn label(self) -> &'static str {
        match self {
            FormatField::Provider => "Провайдер",
            FormatField::Model => "Модель",
            FormatField::ServerUrl => "Адрес сервиса agentd",
            FormatField::ClientToken => "Токен сервиса",
            FormatField::OllamaUrl => "Адрес Ollama",
            FormatField::ContextLimit => "Лимит контекста (токены)",
            FormatField::SummaryEnabled => "Компактизация истории",
            FormatField::SummaryKeepMessages => "Дословный хвост (сообщений)",
            FormatField::SummaryStepMessages => "Шаг пересказа (сообщений)",
            FormatField::ContextStrategy => "Стратегия контекста",
            FormatField::ContextWindowMessages => "Окно последних сообщений",
            FormatField::Profile => "Профиль",
            FormatField::MemoryLayersEnabled => "Слоистая память",
            FormatField::MemoryRouterEnabled => "Автомаршрутизатор памяти",
            FormatField::MemoryWorkingMaxEntries => "Лимит записей рабочей памяти",
            FormatField::MemoryLongTermMaxEntries => "Лимит записей долговременной памяти",
            FormatField::TaskStateEnabled => "Состояние задачи",
            FormatField::TaskStateAutoEnabled => "Автотрекер состояния задачи",
            FormatField::Mode => "Режим",
            FormatField::Reasoning => "Стратегия рассуждения",
            FormatField::Thinking => "Режим thinking у модели",
            FormatField::Experts => "Эксперты",
            FormatField::Description => "Описание формата",
            FormatField::MaxLength => "Макс. длина ответа (токены)",
            FormatField::Stop => "Stop-последовательности",
            FormatField::StopInstruction => "Инструкция завершения ответа",
            FormatField::Temperature => "Temperature",
            FormatField::TopP => "Top-p",
            FormatField::TopK => "Top-k",
            FormatField::FrequencyPenalty => "Frequency penalty",
            FormatField::PresencePenalty => "Presence penalty",
        }
    }

    /// Развёрнутое пояснение поля — показывается в нижней панели попапа,
    /// когда поле выделено.
    fn description(self) -> &'static str {
        match self {
            FormatField::Provider => {
                "◀/▶ или Space — переключить между облачным API и локальной Ollama. Меняет остальные поля этого раздела."
            }
            FormatField::Model => {
                "Модель именно этого чата. ◀/▶ — выбрать из списка сервиса/Ollama, ввод — задать любое имя, Ctrl+D — сбросить на умолчание."
            }
            FormatField::ServerUrl => "Адрес сервиса agentd — общий для всех чатов.",
            FormatField::ClientToken => {
                "Клиентский токен сервиса, общий для всех чатов. На экране видны только последние 4 символа."
            }
            FormatField::OllamaUrl => "Адрес локального сервера Ollama — общий для всех чатов.",
            FormatField::ContextLimit => {
                "Максимальный размер контекстного окна в токенах, который сервис использует для этого конкретного чата. \
Значение задаётся клиентом и может только сужать операторский лимит сервиса, но не превышать его — если здесь указано \
больше операторского лимита, действует всё равно операторский. Когда история чата превышает этот лимит, старые \
сообщения вытесняются (или сжимаются пересказом, если включена суммаризация ниже). Пусто — лимит клиентом не задан, \
действует только операторский лимит сервиса."
            }
            FormatField::SummaryEnabled => {
                "◀/▶ или Space — переключить. Когда включено, часть истории, вытесненная за пределы контекстного окна, \
не отбрасывается, а заменяется коротким пересказом (суммари), который сервис генерирует автоматически и передаёт \
модели вместо исходных сообщений. Это позволяет модели помнить о более ранней части разговора при ограниченном \
контексте, ценой точности деталей вытесненных сообщений. «Умолчание сервиса» — поведение определяет сервис, если \
клиент явно не включил и не выключил суммаризацию для этого чата."
            }
            FormatField::SummaryKeepMessages => {
                "Сколько последних сообщений чата всегда отправляются провайдеру дословно, без пересказа, независимо \
от того, насколько заполнено контекстное окно. Защищает самую свежую часть диалога от огрубления пересказом — чем \
больше значение, тем точнее модель видит недавний контекст, но тем меньше места остаётся под саму суммаризацию \
старой истории. Пусто — действует операторское умолчание сервиса."
            }
            FormatField::SummaryStepMessages => {
                "Шаг в сообщениях, с которым уже составленный пересказ вытесненной истории перестраивается заново \
(а не пересобирается на каждом новом сообщении). Например, шаг 10 значит: пересказ обновляется раз в 10 новых \
сообщений, а между обновлениями используется прежняя версия. Меньший шаг — пересказ точнее и актуальнее, но чаще \
пересчитывается; больший шаг — реже пересчитывается, экономя токены и время на генерацию суммари. Пусто — действует \
операторское умолчание сервиса."
            }
            FormatField::ContextStrategy => {
                "◀/▶ или Space — переключить. Стратегия управления контекстом чата: «Умолчание сервиса», «Пересказ» \
(summary — текущее поведение), «Окно последних сообщений» (sliding_window — старые сообщения отбрасываются без \
замены), «Устойчивые факты» (facts — ключевые данные диалога хранятся отдельно и уходят вместе с хвостом истории) \
или «Ветвление диалога» (branching — история собирается по цепочке активной ветки). «Умолчание сервиса» — \
стратегию определяет переменная AGENTD_CONTEXT_STRATEGY. Слоистая память включается отдельным переключателем и \
работает поверх любой из этих стратегий."
            }
            FormatField::ContextWindowMessages => {
                "Сколько последних сообщений чата уходят провайдеру при стратегиях «Окно последних сообщений» и \
«Устойчивые факты». Пусто — действует операторское умолчание сервиса."
            }
            FormatField::Profile => {
                "◀/▶ — выбрать из профилей, доступных владельцу (встроенные и свои, список приходит с сервиса), \
ввод — задать идентификатор вручную, Ctrl+D — снять профиль. Профиль подставляет роль, стиль, формат и ограничения \
в системное сообщение каждого запроса этого чата, поверх стратегии контекста и слоистой памяти. «Умолчание сервиса» \
— решает AGENTD_DEFAULT_PROFILE (по умолчанию без профиля). Список профилей: agentcli profiles list."
            }
            FormatField::MemoryLayersEnabled => {
                "◀/▶ или Space — переключить. Включает или выключает для этого чата слоистую память (рабочий и \
долговременный слои) поверх действующей стратегии контекста. «Умолчание сервиса» — решает \
AGENTD_MEMORY_LAYERS_ENABLED (по умолчанию выключено)."
            }
            FormatField::MemoryRouterEnabled => {
                "◀/▶ или Space — переключить. Включает или выключает для этого чата автоматический маршрутизатор, \
который после каждого сообщения решает, что записать в рабочую и долговременную память. «Умолчание сервиса» — \
решает AGENTD_MEMORY_ROUTER_ENABLED. Имеет смысл только при включённой слоистой памяти."
            }
            FormatField::MemoryWorkingMaxEntries => {
                "Сколько записей рабочей памяти текущей задачи подставляется в контекст при включённой слоистой \
памяти. Пусто — действует операторское умолчание сервиса."
            }
            FormatField::MemoryLongTermMaxEntries => {
                "Сколько записей долговременной памяти владельца подставляется в контекст при включённой слоистой \
памяти. Пусто — действует операторское умолчание сервиса."
            }
            FormatField::TaskStateEnabled => {
                "◀/▶ или Space — переключить. Включает или выключает для этого чата явное состояние активной задачи \
(этап, шаг, ожидаемое действие) — экран Ctrl+T и раздел в системном сообщении. «Умолчание сервиса» — решает \
AGENTD_TASK_STATE_ENABLED (по умолчанию выключено)."
            }
            FormatField::TaskStateAutoEnabled => {
                "◀/▶ или Space — переключить. Включает или выключает автоматический трекер, который после каждого \
ответа предлагает переход состояния задачи. «Умолчание сервиса» — решает AGENTD_TASK_STATE_AUTO_ENABLED. Имеет \
смысл только при включённом состоянии задачи."
            }
            FormatField::Mode => {
                "◀/▶ или Space — переключить. Кастомный режим задаёт свой формат ответа вместо формата по умолчанию."
            }
            FormatField::Reasoning => {
                "◀/▶ или Space — переключить. Стратегия, которой агент следует при обдумывании ответа."
            }
            FormatField::Thinking => {
                "◀/▶ или Space — переключить. Режим встроенного размышления модели, если провайдер его поддерживает."
            }
            FormatField::Experts => {
                "Состав экспертной группы для стратегии «Группа экспертов», через запятую. Пусто — состав по умолчанию."
            }
            FormatField::Description => "Свободное описание формата ответа для кастомного режима.",
            FormatField::MaxLength => "Ограничение длины ответа в токенах.",
            FormatField::Stop => {
                "Стоп-последовательности, при которых генерация останавливается, через запятую."
            }
            FormatField::StopInstruction => "Инструкция модели о том, как завершать ответ.",
            FormatField::Temperature => "Температура сэмплирования: выше — разнообразнее и менее предсказуемо.",
            FormatField::TopP => "Nucleus sampling: доля вероятностной массы токенов-кандидатов.",
            FormatField::TopK => "Ограничивает выбор модели K самыми вероятными токенами.",
            FormatField::FrequencyPenalty => "Штраф за повтор уже встречавшихся токенов.",
            FormatField::PresencePenalty => "Штраф за повтор уже упомянутых тем/токенов независимо от частоты.",
        }
    }

    /// Поля стратегии рассуждения: не зависят от режима формата ответа.
    fn is_reasoning_detail(self) -> bool {
        matches!(self, FormatField::Experts)
    }

    /// Поля-переключатели: редактируются стрелками/пробелом, а не вводом текста.
    fn is_toggle(self) -> bool {
        matches!(
            self,
            FormatField::Mode
                | FormatField::Reasoning
                | FormatField::Thinking
                | FormatField::Provider
                | FormatField::SummaryEnabled
                | FormatField::ContextStrategy
                | FormatField::MemoryLayersEnabled
                | FormatField::MemoryRouterEnabled
                | FormatField::TaskStateEnabled
                | FormatField::TaskStateAutoEnabled
        )
    }

    /// Поля подключения: модель, адрес сервиса и токен — доступны всегда.
    fn is_connection(self) -> bool {
        matches!(
            self,
            FormatField::Provider
                | FormatField::Model
                | FormatField::ServerUrl
                | FormatField::ClientToken
                | FormatField::OllamaUrl
                | FormatField::ContextLimit
                | FormatField::SummaryEnabled
                | FormatField::SummaryKeepMessages
                | FormatField::SummaryStepMessages
                | FormatField::ContextStrategy
                | FormatField::ContextWindowMessages
                | FormatField::Profile
                | FormatField::MemoryLayersEnabled
                | FormatField::MemoryRouterEnabled
                | FormatField::MemoryWorkingMaxEntries
                | FormatField::MemoryLongTermMaxEntries
                | FormatField::TaskStateEnabled
                | FormatField::TaskStateAutoEnabled
        )
    }

    /// Поля, не зависящие от режима формата (доступны всегда).
    fn is_sampling(self) -> bool {
        matches!(
            self,
            FormatField::Temperature
                | FormatField::TopP
                | FormatField::TopK
                | FormatField::FrequencyPenalty
                | FormatField::PresencePenalty
        )
    }
}

/// Состояние редактора настроек формата ответа для конкретного чата.
struct SettingsEditor {
    /// Чат, чьи параметры редактируются.
    chat_id: String,
    chat_title: String,
    /// Провайдер этого чата: облачный API или локальный Ollama.
    provider: Provider,
    /// Модель этого чата: пусто — модель по умолчанию из конфига.
    model: String,
    /// Адрес сервиса и токен — общие для всех чатов, живут в глобальном
    /// конфиге. Ключа провайдера у клиента нет: он принадлежит сервису.
    server_url: String,
    client_token: String,
    /// Адрес локального Ollama — тоже общий для всех чатов.
    ollama_url: String,
    /// Клиентский лимит контекста в токенах — настройка этого чата. Пусто —
    /// лимит не задан.
    context_limit: String,
    /// Компактизация истории этого чата: "" — операторское умолчание
    /// сервиса, "on"/"off" — явное включение/выключение
    /// (specs/context-summary, «Настройки компактизации на уровне чата»).
    summary_enabled: String,
    /// Дословный хвост компактизации — настройка этого чата. Пусто —
    /// операторское умолчание сервиса.
    summary_keep_messages: String,
    /// Шаг пересказа — настройка этого чата. Пусто — операторское умолчание
    /// сервиса.
    summary_step_messages: String,
    /// Стратегия управления контекстом этого чата: "" — умолчание сервиса,
    /// иначе имя стратегии в snake_case (`ContextStrategy::as_str`).
    context_strategy: String,
    /// Размер окна последних сообщений для стратегий `sliding_window` и
    /// `facts` — настройка этого чата. Пусто — операторское умолчание сервиса.
    context_window_messages: String,
    /// Профиль этого чата (id): "" — операторское умолчание сервиса
    /// (`AGENTD_DEFAULT_PROFILE`). Подставляется поверх стратегии контекста
    /// и слоистой памяти (specs/user-profiles).
    profile_id: String,
    /// Профили, доступные владельцу — встроенные и свои, для перебора
    /// стрелками и подписи текущего значения по имени, а не только id.
    profile_choices: Vec<ProfileChoice>,
    /// Слоистая память этого чата: "" — операторское умолчание сервиса,
    /// "on"/"off" — явное включение/выключение. Независима от
    /// `context_strategy` — применяется поверх любой стратегии.
    memory_layers_enabled: String,
    /// Автомаршрутизатор памяти этого чата: "" — операторское умолчание
    /// сервиса, "on"/"off" — явное включение/выключение. Имеет смысл только
    /// при включённой слоистой памяти.
    memory_router_enabled: String,
    /// Лимит записей рабочей памяти — настройка этого чата. Пусто —
    /// операторское умолчание сервиса.
    memory_working_max_entries: String,
    /// Лимит записей долговременной памяти — настройка этого чата. Пусто —
    /// операторское умолчание сервиса.
    memory_long_term_max_entries: String,
    /// Состояние задачи этого чата: "" — операторское умолчание сервиса,
    /// "on"/"off" — явное включение/выключение (specs/task-state).
    task_state_enabled: String,
    /// Автоматический трекер состояния задачи этого чата: "" — операторское
    /// умолчание сервиса, "on"/"off" — явное включение/выключение. Имеет
    /// смысл только при включённом состоянии задачи.
    task_state_auto_enabled: String,
    /// Облачные модели для переключения стрелками в поле «Модель».
    model_choices: Vec<String>,
    /// Локально скачанные модели Ollama, полученные с `/api/tags`.
    ollama_models: Vec<String>,
    /// Модель, введённая для другого провайдера: при переключении провайдера
    /// имя модели не теряется, а меняется местами с текущим.
    stashed_model: String,
    /// Модель по умолчанию из конфига — показывается, когда поле пустое.
    default_model: String,
    /// Модель Ollama по умолчанию из конфига.
    default_ollama_model: String,
    custom_mode: bool,
    reasoning: ReasoningMode,
    thinking: ThinkingMode,
    experts: String,
    description: String,
    max_length: String,
    stop: String,
    stop_instruction: String,
    temperature: String,
    top_p: String,
    top_k: String,
    frequency_penalty: String,
    presence_penalty: String,
    /// Индекс активного раздела в SettingsSection::ALL.
    section: usize,
    /// Индекс поля внутри visible_fields() активного раздела.
    field: usize,
    /// Панель, которая принимает ввод.
    pane: SettingsPane,
    error: Option<String>,
}

impl SettingsEditor {
    fn from_chat(
        chat: &ChatSession,
        config: &Config,
        model_choices: &[String],
        ollama_models: &[String],
        profile_choices: &[ProfileChoice],
    ) -> Self {
        let settings = &chat.settings;
        let custom_mode = settings.custom_response_mode;
        let format = settings.response_format.clone();
        let sampling = &settings.sampling;
        Self {
            chat_id: chat.id.clone(),
            chat_title: chat.title.clone(),
            provider: settings.provider,
            model: settings.model.clone().unwrap_or_default(),
            server_url: config.server_url.clone().unwrap_or_default(),
            client_token: config.client_token.clone().unwrap_or_default(),
            ollama_url: config.ollama_url.clone().unwrap_or_default(),
            context_limit: settings
                .max_context_tokens
                .map(|v| v.to_string())
                .unwrap_or_default(),
            summary_enabled: match settings.summary_enabled {
                None => String::new(),
                Some(true) => "on".to_string(),
                Some(false) => "off".to_string(),
            },
            summary_keep_messages: settings
                .summary_keep_messages
                .map(|v| v.to_string())
                .unwrap_or_default(),
            summary_step_messages: settings
                .summary_step_messages
                .map(|v| v.to_string())
                .unwrap_or_default(),
            context_strategy: settings
                .context_strategy
                .map(|s| s.as_str().to_string())
                .unwrap_or_default(),
            context_window_messages: settings
                .context_window_messages
                .map(|v| v.to_string())
                .unwrap_or_default(),
            profile_id: settings.profile_id.clone().unwrap_or_default(),
            profile_choices: profile_choices.to_vec(),
            memory_layers_enabled: match settings.memory_layers_enabled {
                None => String::new(),
                Some(true) => "on".to_string(),
                Some(false) => "off".to_string(),
            },
            memory_router_enabled: match settings.memory_router_enabled {
                None => String::new(),
                Some(true) => "on".to_string(),
                Some(false) => "off".to_string(),
            },
            memory_working_max_entries: settings
                .memory_working_max_entries
                .map(|v| v.to_string())
                .unwrap_or_default(),
            memory_long_term_max_entries: settings
                .memory_long_term_max_entries
                .map(|v| v.to_string())
                .unwrap_or_default(),
            task_state_enabled: match settings.task_state_enabled {
                None => String::new(),
                Some(true) => "on".to_string(),
                Some(false) => "off".to_string(),
            },
            task_state_auto_enabled: match settings.task_state_auto_enabled {
                None => String::new(),
                Some(true) => "on".to_string(),
                Some(false) => "off".to_string(),
            },
            // приходит из AppState.model_choices: список сервиса, если фоновый
            // запрос уже ответил, иначе — встроенный/конфигурный список
            model_choices: model_choices.to_vec(),
            ollama_models: ollama_models.to_vec(),
            stashed_model: String::new(),
            default_model: config.effective_model(),
            default_ollama_model: config.ollama_model.clone().unwrap_or_default(),
            custom_mode,
            reasoning: settings.reasoning,
            thinking: settings.thinking,
            experts: settings.experts.join(", "),
            description: format.description.unwrap_or_default(),
            max_length: format.max_length.map(|v| v.to_string()).unwrap_or_default(),
            stop: format.stop.map(|v| v.join(", ")).unwrap_or_default(),
            stop_instruction: format.stop_instruction.unwrap_or_default(),
            temperature: sampling.temperature.map(|v| v.to_string()).unwrap_or_default(),
            top_p: sampling.top_p.map(|v| v.to_string()).unwrap_or_default(),
            top_k: sampling.top_k.map(|v| v.to_string()).unwrap_or_default(),
            frequency_penalty: sampling
                .frequency_penalty
                .map(|v| v.to_string())
                .unwrap_or_default(),
            presence_penalty: sampling
                .presence_penalty
                .map(|v| v.to_string())
                .unwrap_or_default(),
            section: 0,
            field: 0,
            pane: SettingsPane::Sections,
            error: None,
        }
    }

    fn current_section(&self) -> SettingsSection {
        SettingsSection::ALL[self.section.min(SettingsSection::ALL.len() - 1)]
    }

    /// Поля активного раздела: «Эксперты» имеют смысл только для
    /// стратегии «Группа экспертов».
    fn visible_fields(&self) -> Vec<FormatField> {
        self.current_section()
            .fields()
            .iter()
            .copied()
            .filter(|field| match field {
                // состав экспертов имеет смысл только для своей стратегии
                FormatField::Experts => self.reasoning == ReasoningMode::ExpertPanel,
                // адрес и ключ облака не нужны локальным моделям, и наоборот
                FormatField::ServerUrl
                | FormatField::ClientToken
                | FormatField::ContextLimit
                | FormatField::SummaryEnabled
                | FormatField::SummaryKeepMessages
                | FormatField::SummaryStepMessages => self.provider == Provider::Cloud,
                FormatField::OllamaUrl => self.provider == Provider::Ollama,
                // параметры слоистой памяти видны при включённом переключателе,
                // независимо от действующей стратегии контекста
                FormatField::MemoryRouterEnabled
                | FormatField::MemoryWorkingMaxEntries
                | FormatField::MemoryLongTermMaxEntries => self.memory_layers_enabled == "on",
                // автотрекер имеет смысл только при включённом состоянии задачи
                FormatField::TaskStateAutoEnabled => self.task_state_enabled == "on",
                _ => true,
            })
            .collect()
    }

    fn current_field(&self) -> Option<FormatField> {
        let fields = self.visible_fields();
        fields.get(self.field.min(fields.len().saturating_sub(1))).copied()
    }

    /// Переместить выделение внутри активной панели.
    fn move_focus(&mut self, delta: i32) {
        match self.pane {
            SettingsPane::Sections => {
                let len = SettingsSection::ALL.len() as i32;
                self.section = (self.section as i32 + delta).rem_euclid(len) as usize;
                self.field = 0;
            }
            SettingsPane::Fields => {
                let len = self.visible_fields().len() as i32;
                if len == 0 {
                    return;
                }
                let current = self.field.min(len as usize - 1) as i32;
                self.field = (current + delta).rem_euclid(len) as usize;
            }
        }
    }

    /// Tab: список разделов → поля раздела → обратно к списку с последнего поля.
    fn focus_next(&mut self) {
        match self.pane {
            SettingsPane::Sections => {
                if self.visible_fields().is_empty() {
                    self.move_focus(1);
                } else {
                    self.pane = SettingsPane::Fields;
                    self.field = 0;
                }
            }
            SettingsPane::Fields => {
                let len = self.visible_fields().len();
                if len == 0 || self.field + 1 >= len {
                    self.pane = SettingsPane::Sections;
                    self.field = 0;
                } else {
                    self.field += 1;
                }
            }
        }
    }

    fn cycle_reasoning(&mut self, delta: i32) {
        let modes = ReasoningMode::ALL;
        let len = modes.len() as i32;
        let current = modes
            .iter()
            .position(|m| *m == self.reasoning)
            .unwrap_or(0) as i32;
        self.reasoning = modes[(current + delta).rem_euclid(len) as usize];
        // «Эксперты» появляются и исчезают вместе со стратегией — не даём
        // курсору уехать за пределы списка полей
        let len = self.visible_fields().len();
        self.field = self.field.min(len.saturating_sub(1));
    }

    /// Переключение провайдера: набор видимых полей и список моделей
    /// для стрелок зависят от него.
    fn cycle_provider(&mut self, delta: i32) {
        let providers = Provider::ALL;
        let len = providers.len() as i32;
        let current = providers.iter().position(|p| *p == self.provider).unwrap_or(0) as i32;
        let next = providers[(current + delta).rem_euclid(len) as usize];
        if next != self.provider {
            // имя облачной модели локальной не подходит и наоборот
            std::mem::swap(&mut self.model, &mut self.stashed_model);
            self.provider = next;
        }
        let len = self.visible_fields().len();
        self.field = self.field.min(len.saturating_sub(1));
    }

    /// Модели, между которыми переключает поле «Модель» у текущего провайдера.
    fn current_model_choices(&self) -> &[String] {
        match self.provider {
            Provider::Cloud => &self.model_choices,
            Provider::Ollama => &self.ollama_models,
        }
    }

    fn cycle_thinking(&mut self, delta: i32) {
        let modes = ThinkingMode::ALL;
        let len = modes.len() as i32;
        let current = modes.iter().position(|m| *m == self.thinking).unwrap_or(0) as i32;
        self.thinking = modes[(current + delta).rem_euclid(len) as usize];
    }

    /// Перебор трёх состояний компактизации: не задано → включена →
    /// выключена → снова не задано.
    fn cycle_summary_enabled(&mut self, delta: i32) {
        const STATES: [&str; 3] = ["", "on", "off"];
        let current = STATES
            .iter()
            .position(|s| *s == self.summary_enabled)
            .unwrap_or(0) as i32;
        let len = STATES.len() as i32;
        self.summary_enabled = STATES[(current + delta).rem_euclid(len) as usize].to_string();
    }

    /// Перебор стратегий контекста: не задано → summary → sliding_window →
    /// facts → branching → снова не задано.
    fn cycle_context_strategy(&mut self, delta: i32) {
        const STATES: [&str; 5] = ["", "summary", "sliding_window", "facts", "branching"];
        let current = STATES
            .iter()
            .position(|s| *s == self.context_strategy)
            .unwrap_or(0) as i32;
        let len = STATES.len() as i32;
        self.context_strategy = STATES[(current + delta).rem_euclid(len) as usize].to_string();
    }

    /// Перебор трёх состояний слоистой памяти: не задано → включена →
    /// выключена → снова не задано. Независим от стратегии контекста.
    fn cycle_memory_layers_enabled(&mut self, delta: i32) {
        const STATES: [&str; 3] = ["", "on", "off"];
        let current = STATES
            .iter()
            .position(|s| *s == self.memory_layers_enabled)
            .unwrap_or(0) as i32;
        let len = STATES.len() as i32;
        self.memory_layers_enabled = STATES[(current + delta).rem_euclid(len) as usize].to_string();
        // поля памяти появляются и исчезают вместе с переключателем — не
        // даём курсору уехать за пределы списка полей
        let len = self.visible_fields().len();
        self.field = self.field.min(len.saturating_sub(1));
    }

    /// Перебор трёх состояний автомаршрутизатора памяти: не задано →
    /// включён → выключен → снова не задано.
    fn cycle_memory_router_enabled(&mut self, delta: i32) {
        const STATES: [&str; 3] = ["", "on", "off"];
        let current = STATES
            .iter()
            .position(|s| *s == self.memory_router_enabled)
            .unwrap_or(0) as i32;
        let len = STATES.len() as i32;
        self.memory_router_enabled = STATES[(current + delta).rem_euclid(len) as usize].to_string();
    }

    /// Перебор трёх состояний состояния задачи: не задано → включено →
    /// выключено → снова не задано.
    fn cycle_task_state_enabled(&mut self, delta: i32) {
        const STATES: [&str; 3] = ["", "on", "off"];
        let current = STATES
            .iter()
            .position(|s| *s == self.task_state_enabled)
            .unwrap_or(0) as i32;
        let len = STATES.len() as i32;
        self.task_state_enabled = STATES[(current + delta).rem_euclid(len) as usize].to_string();
        // поле автотрекера появляется и исчезает вместе с переключателем
        let len = self.visible_fields().len();
        self.field = self.field.min(len.saturating_sub(1));
    }

    /// Перебор трёх состояний автотрекера состояния задачи: не задано →
    /// включён → выключен → снова не задано.
    fn cycle_task_state_auto_enabled(&mut self, delta: i32) {
        const STATES: [&str; 3] = ["", "on", "off"];
        let current = STATES
            .iter()
            .position(|s| *s == self.task_state_auto_enabled)
            .unwrap_or(0) as i32;
        let len = STATES.len() as i32;
        self.task_state_auto_enabled = STATES[(current + delta).rem_euclid(len) as usize].to_string();
    }

    /// Перебор известных моделей стрелками. Если в поле введено что-то своё,
    /// перебор начинается с первой модели списка.
    fn cycle_model(&mut self, delta: i32) {
        let choices = self.current_model_choices();
        if choices.is_empty() {
            return;
        }
        let len = choices.len() as i32;
        let next = match choices.iter().position(|m| *m == self.model) {
            Some(current) => (current as i32 + delta).rem_euclid(len),
            None if delta >= 0 => 0,
            None => len - 1,
        };
        self.model = choices[next as usize].clone();
    }

    /// Перебор профилей, доступных владельцу, стрелками: пусто → первый →
    /// … → последний → снова пусто. Если в поле введён id, которого нет в
    /// списке, перебор начинается с первого профиля.
    fn cycle_profile(&mut self, delta: i32) {
        if self.profile_choices.is_empty() {
            return;
        }
        // Состояния — "" (нет профиля) плюс id каждого профиля списка.
        let len = self.profile_choices.len() as i32 + 1;
        let current = match self.profile_choices.iter().position(|p| p.id == self.profile_id) {
            Some(index) if !self.profile_id.is_empty() => index as i32 + 1,
            _ if self.profile_id.is_empty() => 0,
            _ => 0,
        };
        let next = (current + delta).rem_euclid(len);
        self.profile_id = if next == 0 {
            String::new()
        } else {
            self.profile_choices[(next - 1) as usize].id.clone()
        };
    }

    /// Сбросить текущее поле к значению по умолчанию.
    fn reset_field(&mut self) {
        match self.current_field() {
            Some(FormatField::Mode) => self.custom_mode = false,
            Some(FormatField::Provider) => self.provider = Provider::default(),
            Some(FormatField::Reasoning) => self.reasoning = ReasoningMode::default(),
            Some(FormatField::Thinking) => self.thinking = ThinkingMode::default(),
            Some(FormatField::SummaryEnabled) => self.summary_enabled.clear(),
            Some(FormatField::ContextStrategy) => self.context_strategy.clear(),
            Some(FormatField::MemoryLayersEnabled) => self.memory_layers_enabled.clear(),
            Some(FormatField::MemoryRouterEnabled) => self.memory_router_enabled.clear(),
            Some(FormatField::TaskStateEnabled) => self.task_state_enabled.clear(),
            Some(FormatField::TaskStateAutoEnabled) => self.task_state_auto_enabled.clear(),
            _ => {
                if let Some(value) = self.field_value_mut() {
                    value.clear();
                }
            }
        }
    }

    fn field_value_mut(&mut self) -> Option<&mut String> {
        match self.current_field()? {
            FormatField::Mode
            | FormatField::Reasoning
            | FormatField::Thinking
            | FormatField::Provider
            | FormatField::SummaryEnabled
            | FormatField::ContextStrategy
            | FormatField::MemoryLayersEnabled
            | FormatField::MemoryRouterEnabled
            | FormatField::TaskStateEnabled
            | FormatField::TaskStateAutoEnabled => None,
            FormatField::Model => Some(&mut self.model),
            FormatField::ServerUrl => Some(&mut self.server_url),
            FormatField::ClientToken => Some(&mut self.client_token),
            FormatField::OllamaUrl => Some(&mut self.ollama_url),
            FormatField::ContextLimit => Some(&mut self.context_limit),
            FormatField::SummaryKeepMessages => Some(&mut self.summary_keep_messages),
            FormatField::SummaryStepMessages => Some(&mut self.summary_step_messages),
            FormatField::ContextWindowMessages => Some(&mut self.context_window_messages),
            FormatField::Profile => Some(&mut self.profile_id),
            FormatField::MemoryWorkingMaxEntries => Some(&mut self.memory_working_max_entries),
            FormatField::MemoryLongTermMaxEntries => Some(&mut self.memory_long_term_max_entries),
            FormatField::Experts => Some(&mut self.experts),
            FormatField::Description => Some(&mut self.description),
            FormatField::MaxLength => Some(&mut self.max_length),
            FormatField::Stop => Some(&mut self.stop),
            FormatField::StopInstruction => Some(&mut self.stop_instruction),
            FormatField::Temperature => Some(&mut self.temperature),
            FormatField::TopP => Some(&mut self.top_p),
            FormatField::TopK => Some(&mut self.top_k),
            FormatField::FrequencyPenalty => Some(&mut self.frequency_penalty),
            FormatField::PresencePenalty => Some(&mut self.presence_penalty),
        }
    }

    /// Собрать ResponseFormat из введённых значений. Возвращает ошибку текстом,
    /// если "макс. длина" не парсится в число.
    fn build(&mut self) -> Result<Option<ResponseFormat>, String> {
        if !self.custom_mode {
            return Ok(None);
        }
        let max_length = if self.max_length.trim().is_empty() {
            None
        } else {
            let parsed = self
                .max_length
                .trim()
                .parse::<u32>()
                .map_err(|_| "Макс. длина должна быть целым числом".to_string())?;
            if parsed == 0 {
                return Err("Макс. длина должна быть больше нуля".to_string());
            }
            Some(parsed)
        };
        let stop = split_list(&self.stop);
        let description = non_empty(&self.description);
        let stop_instruction = non_empty(&self.stop_instruction);

        Ok(Some(ResponseFormat {
            description,
            max_length,
            stop: if stop.is_empty() { None } else { Some(stop) },
            stop_instruction,
        }))
    }

    /// Собрать SamplingParams из введённых значений. Возвращает ошибку текстом,
    /// если одно из числовых полей не парсится.
    fn build_sampling(&mut self) -> Result<SamplingParams, String> {
        fn parse_range(
            value: &str,
            label: &str,
            min: f32,
            max: f32,
        ) -> Result<Option<f32>, String> {
            if value.trim().is_empty() {
                return Ok(None);
            }
            let parsed = value
                .trim()
                .parse::<f32>()
                .map_err(|_| format!("{label} должно быть числом"))?;
            if !(min..=max).contains(&parsed) {
                return Err(format!("{label}: допустим диапазон {min}..{max}"));
            }
            Ok(Some(parsed))
        }

        let temperature = parse_range(&self.temperature, "Temperature", 0.0, 2.0)?;
        let top_p = parse_range(&self.top_p, "Top-p", 0.0, 1.0)?;
        let top_k = if self.top_k.trim().is_empty() {
            None
        } else {
            let parsed = self
                .top_k
                .trim()
                .parse::<u32>()
                .map_err(|_| "Top-k должно быть целым числом".to_string())?;
            if parsed == 0 {
                return Err("Top-k должен быть больше нуля".to_string());
            }
            Some(parsed)
        };
        let frequency_penalty = parse_range(&self.frequency_penalty, "Frequency penalty", -2.0, 2.0)?;
        let presence_penalty = parse_range(&self.presence_penalty, "Presence penalty", -2.0, 2.0)?;

        Ok(SamplingParams {
            temperature,
            top_p,
            top_k,
            frequency_penalty,
            presence_penalty,
        })
    }

    /// Разобрать лимит контекста: пусто — `None`, иначе положительное целое.
    fn build_context_limit(&self) -> Result<Option<u32>, String> {
        if self.context_limit.trim().is_empty() {
            return Ok(None);
        }
        let parsed = self
            .context_limit
            .trim()
            .parse::<u32>()
            .map_err(|_| "Лимит контекста должен быть целым числом".to_string())?;
        if parsed == 0 {
            return Err("Лимит контекста должен быть больше нуля".to_string());
        }
        Ok(Some(parsed))
    }

    fn build_summary_enabled(&self) -> Option<bool> {
        match self.summary_enabled.as_str() {
            "on" => Some(true),
            "off" => Some(false),
            _ => None,
        }
    }

    /// Разобрать число сообщений компактизации: пусто — `None`, иначе
    /// положительное целое (общее правило для хвоста и шага пересказа).
    fn build_summary_count(value: &str, label: &str) -> Result<Option<u32>, String> {
        if value.trim().is_empty() {
            return Ok(None);
        }
        let parsed = value
            .trim()
            .parse::<u32>()
            .map_err(|_| format!("{label} должен быть целым числом"))?;
        if parsed == 0 {
            return Err(format!("{label} должен быть больше нуля"));
        }
        Ok(Some(parsed))
    }

    fn build_summary_keep_messages(&self) -> Result<Option<u32>, String> {
        Self::build_summary_count(&self.summary_keep_messages, "Дословный хвост компактизации")
    }

    fn build_summary_step_messages(&self) -> Result<Option<u32>, String> {
        Self::build_summary_count(&self.summary_step_messages, "Шаг пересказа")
    }

    /// Разобрать стратегию контекста: пусто — `None` (умолчание сервиса).
    /// Значение всегда взято из `cycle_context_strategy`, поэтому неизвестных
    /// имён здесь не бывает.
    fn build_context_strategy(&self) -> Option<ContextStrategy> {
        ContextStrategy::parse(&self.context_strategy)
    }

    fn build_context_window_messages(&self) -> Result<Option<u32>, String> {
        Self::build_summary_count(&self.context_window_messages, "Окно последних сообщений")
    }

    /// Разобрать профиль: пусто — `None` (умолчание сервиса).
    fn build_profile_id(&self) -> Option<String> {
        non_empty(&self.profile_id)
    }

    fn build_memory_layers_enabled(&self) -> Option<bool> {
        match self.memory_layers_enabled.as_str() {
            "on" => Some(true),
            "off" => Some(false),
            _ => None,
        }
    }

    fn build_memory_router_enabled(&self) -> Option<bool> {
        match self.memory_router_enabled.as_str() {
            "on" => Some(true),
            "off" => Some(false),
            _ => None,
        }
    }

    fn build_task_state_enabled(&self) -> Option<bool> {
        match self.task_state_enabled.as_str() {
            "on" => Some(true),
            "off" => Some(false),
            _ => None,
        }
    }

    fn build_task_state_auto_enabled(&self) -> Option<bool> {
        match self.task_state_auto_enabled.as_str() {
            "on" => Some(true),
            "off" => Some(false),
            _ => None,
        }
    }

    fn build_memory_working_max_entries(&self) -> Result<Option<u32>, String> {
        Self::build_summary_count(
            &self.memory_working_max_entries,
            "Лимит записей рабочей памяти",
        )
    }

    fn build_memory_long_term_max_entries(&self) -> Result<Option<u32>, String> {
        Self::build_summary_count(
            &self.memory_long_term_max_entries,
            "Лимит записей долговременной памяти",
        )
    }
}

/// Разбор списка через запятую с отбрасыванием пустых элементов.
fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn non_empty(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub async fn run(agent: CliAgent, config: Config) -> anyhow::Result<()> {
    let agent = Arc::new(agent);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Захват мыши нужен ради колеса: история длинная, и листать её клавишами
    // неудобно. Нативное выделение текста терминалом при этом не теряется —
    // оно доступно с зажатым Shift, а сообщение целиком копируется из самого
    // TUI (Ctrl+G, затем Enter или Ctrl+Y).
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, agent, config).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    result
}

/// Итог обработки одного события клавиатуры/канала на текущей итерации цикла.
enum LoopControl {
    Continue,
    Break,
}

/// Состояние ввода/прокрутки, привязанное к конкретному чату (по его id),
/// а не к панели — так каждый открытый чат ведёт себя независимо.
struct ChatUi {
    input: String,
    pending: bool,
    /// Момент отправки запроса: из него считается таймер ожидания ответа.
    pending_since: Option<Instant>,
    scroll: u16,
    max_scroll: u16,
    auto_scroll: bool,
    scroll_to_message: Option<usize>,
    /// Сообщение, выбранное в режиме `Focus::MessageSelect`. Выбор привязан к
    /// чату, как и прокрутка, поэтому переживает переключение панелей.
    selected_message: Option<usize>,
    /// Запрошена история чата у сервиса: ввод заблокирован, пока она не
    /// пришла, иначе реплика ушла бы с неполным контекстом.
    history_loading: bool,
    /// Причина, по которой история чата не загрузилась.
    history_error: Option<String>,
    /// Обмен, который не удалось дозаписать в сервис: причина и сами
    /// реплики для повторной попытки.
    unsaved: Option<UnsavedExchange>,
    /// Блок наблюдаемости стратегии контекста последнего ответа
    /// (specs/context-strategies, «Переключение стратегии из клиента»).
    last_context: Option<agentcore::config::ContextObservability>,
    /// Кеш отрисованных сообщений истории, по одному элементу на сообщение
    /// в том же порядке.
    rendered: Vec<RenderedMessage>,
}

/// Одно сообщение истории, уже разобранное в строки терминала.
///
/// Markdown прогоняется через termimad и парсер ANSI — это самая дорогая
/// часть кадра, поэтому результат живёт между кадрами и пересобирается
/// только при изменении самого сообщения или того, как его надо показать.
struct RenderedMessage {
    /// Отпечаток исходных данных: текст, рассуждение, показ рассуждения и
    /// подсветка выбора. Несовпадение — повод отрисовать заново.
    fingerprint: u64,
    lines: Vec<Line<'static>>,
    /// Высота блока после переноса строк: ширина, на которой она посчитана,
    /// и число строк. Перенос считается заново при смене ширины окна.
    wrapped: Option<(u16, usize)>,
}

/// Обмен локального чата, оставшийся только в памяти клиента.
struct UnsavedExchange {
    reason: String,
    messages: Vec<Message>,
}

impl Default for ChatUi {
    fn default() -> Self {
        Self {
            input: String::new(),
            pending: false,
            pending_since: None,
            scroll: 0,
            max_scroll: 0,
            auto_scroll: true,
            scroll_to_message: None,
            selected_message: None,
            history_loading: false,
            history_error: None,
            unsaved: None,
            last_context: None,
            rendered: Vec::new(),
        }
    }
}

struct AppState {
    /// Глобальный конфиг: ключ API, адрес и модель по умолчанию.
    config: Config,
    /// Клиент чатов сервиса: хранилища у самого клиента нет.
    chats_client: Arc<ChatsClient>,
    /// Взведён, пока идёт запрос на создание чата: второй запрос не нужен.
    creating_chat: bool,
    /// Взведён, когда изменились ключ или адрес API: агента надо пересобрать.
    agent_dirty: bool,
    chats: Vec<ChatSession>,
    /// Исход загрузки списка чатов у сервиса.
    chats_load: ChatsLoad,
    chat_ui: HashMap<String, ChatUi>,
    /// Id чатов, открытых сейчас на экране, по одной панели на элемент.
    panes: Vec<String>,
    active_pane: usize,
    sidebar_selected: usize,
    spinner_frame: usize,
    focus: Focus,
    settings: Option<SettingsEditor>,
    /// Окно импорта контекста, если оно открыто.
    import: Option<ImportPicker>,
    /// Запрос подтверждения удаления чата, если он открыт.
    delete_confirm: Option<DeleteConfirm>,
    /// Экран фактов чата, если он открыт.
    facts: Option<FactsPicker>,
    /// Экран веток чата, если он открыт.
    branches: Option<BranchesPicker>,
    /// Экран слоистой памяти чата, если он открыт.
    memory: Option<MemoryPicker>,
    /// Экран состояния задачи чата, если он открыт.
    task: Option<TaskPicker>,
    /// Короткое уведомление внизу экрана (например, «скопировано»).
    notice: Option<(String, Instant)>,
    /// Показывать ли цепочку рассуждений модели в истории.
    show_reasoning: bool,
    /// Локально скачанные модели Ollama: подгружаются фоном при старте
    /// и обновляются по Ctrl+L в настройках.
    ollama_models: Vec<String>,
    /// Облачные модели, разрешённые сервисом (`GET /v1/models`): подгружаются
    /// фоном при старте и обновляются по Ctrl+L. Начинаются со встроенного/
    /// конфигурного списка, чтобы выбор модели не был пустым, пока сервис не
    /// ответил или если он недоступен.
    model_choices: Vec<String>,
    /// Профили, доступные владельцу (`GET /v1/profiles`): подгружаются фоном
    /// при старте и обновляются по Ctrl+L, как модели (specs/user-profiles).
    profile_choices: Vec<ProfileChoice>,
    /// Области истории каждой открытой панели с прошлого кадра: по ним
    /// колесо мыши находит чат под курсором.
    history_areas: Vec<(String, Rect)>,
}

impl AppState {
    /// Чат активной панели. `None` — открытых панелей нет: список чатов
    /// ещё не загружен, не загрузился или пуст.
    fn active_chat_id(&self) -> Option<String> {
        self.panes.get(self.active_pane).cloned()
    }

    fn chat_index(&self, id: &str) -> Option<usize> {
        self.chats.iter().position(|c| c.id == id)
    }

    /// Чат активной панели вместе с его позицией в списке.
    fn active_chat_index(&self) -> Option<usize> {
        self.active_chat_id().and_then(|id| self.chat_index(&id))
    }

    fn active_pending(&self) -> bool {
        self.active_chat_id()
            .map(|id| self.is_pending(&id))
            .unwrap_or(false)
    }

    /// Открыть чат в активной панели, создав её, если панелей ещё нет.
    fn show_in_active_pane(&mut self, id: String) {
        if self.panes.is_empty() {
            self.panes.push(id);
            self.active_pane = 0;
        } else {
            self.panes[self.active_pane] = id;
        }
    }

    /// Причина, по которой сейчас нельзя ни создать чат, ни отправить
    /// сообщение: список чатов не получен от сервиса.
    fn blocked_reason(&self) -> Option<String> {
        match &self.chats_load {
            ChatsLoad::Loading => Some("Список чатов ещё загружается с сервиса".to_string()),
            ChatsLoad::Failed(reason) => Some(reason.clone()),
            ChatsLoad::Loaded => None,
        }
    }

    fn is_pending(&self, id: &str) -> bool {
        self.chat_ui.get(id).map(|u| u.pending).unwrap_or(false)
    }

    fn any_pane_pending(&self) -> bool {
        self.panes.iter().any(|id| self.is_pending(id))
    }

    fn notify(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    /// Уведомление живёт пару секунд, дальше снова показываем подсказки.
    fn active_notice(&self) -> Option<&str> {
        self.notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(2))
            .map(|(text, _)| text.as_str())
    }

    /// Текст, введённый пользователем в активной панели.
    fn active_input(&self) -> &str {
        self.chat_ui.get(&self.panes[self.active_pane]).map(|u| u.input.as_str()).unwrap_or("")
    }

    /// Вставка из буфера обмена: в настройках она идёт в активное поле
    /// (ключ API удобнее вставлять, чем набирать), иначе — в поле ввода чата.
    fn insert_into_input(&mut self, text: &str) {
        if self.focus == Focus::Settings {
            let flat = text.replace(['\n', '\r'], "");
            if let Some(value) = self
                .settings
                .as_mut()
                .filter(|editor| editor.pane == SettingsPane::Fields)
                .and_then(|editor| editor.field_value_mut())
            {
                value.push_str(&flat);
            }
            return;
        }
        if matches!(self.focus, Focus::Import | Focus::Confirm | Focus::Facts | Focus::Branches | Focus::Memory) {
            return;
        }
        let Some(chat_id) = self.active_chat_id() else {
            return;
        };
        if self.is_pending(&chat_id) {
            return;
        }
        // многострочная вставка схлопывается в пробелы: поле ввода однострочное
        let flat = text.replace(['\n', '\r'], " ");
        self.chat_ui.entry(chat_id).or_default().input.push_str(&flat);
    }
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut agent: Arc<CliAgent>,
    config: Config,
) -> anyhow::Result<()> {
    // встроенный/конфигурный список — стартовое значение, пока сервис не
    // ответил на фоновый запрос (или если он недоступен)
    let model_choices = config.model_choices();
    let chats = Arc::new(chats_client(&config));

    let mut state = AppState {
        config,
        chats_client: chats,
        creating_chat: false,
        agent_dirty: false,
        // Список чатов принадлежит сервису и приходит фоновой задачей:
        // пустой список до ответа — состояние загрузки, а не результат.
        chats: Vec::new(),
        chats_load: ChatsLoad::Loading,
        chat_ui: HashMap::new(),
        panes: Vec::new(),
        active_pane: 0,
        sidebar_selected: 0,
        spinner_frame: 0,
        focus: Focus::Input,
        settings: None,
        import: None,
        delete_confirm: None,
        facts: None,
        branches: None,
        memory: None,
        task: None,
        notice: None,
        show_reasoning: true,
        ollama_models: Vec::new(),
        model_choices,
        profile_choices: Vec::new(),
        history_areas: Vec::new(),
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<ChatEvent>();
    // список локальных моделей тянем фоном: Ollama может быть не запущен,
    // и ждать его на старте незачем
    fetch_ollama_models(&state.config, &tx);
    // список облачных моделей тоже тянем фоном: сервис может быть недоступен,
    // а падать в этом случае незачем — остаёмся на встроенном списке
    fetch_cloud_models(&state.config, &tx);
    // Список профилей владельца тоже тянем фоном (specs/user-profiles):
    // сервис может быть недоступен, поле профиля тогда просто пустует.
    fetch_profiles(&state.config, &tx);
    // Список чатов тоже тянем фоном: сервис может быть недоступен, и тогда
    // TUI открывается с баннером причины, а не падает.
    fetch_chats(&state, &tx);
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    // Кадр рисуется только после изменения состояния: на длинной истории
    // холостая перерисовка по каждому тику заметна как лаг.
    let mut dirty = true;

    loop {
        if dirty {
            terminal.draw(|f| render_ui(f, &mut state))?;
            dirty = false;
        }

        tokio::select! {
            _ = tick.tick() => {
                if state.any_pane_pending() {
                    state.spinner_frame = (state.spinner_frame + 1) % SPINNER_FRAMES.len();
                    dirty = true;
                }
            }
            maybe_event = events.next() => {
                let Some(Ok(event)) = maybe_event else { continue };
                // События разбираются пачкой: при быстром наборе или удержании
                // клавиши они приходят чаще, чем успевает кадр, и рисовать надо
                // итог пачки, а не каждое промежуточное состояние.
                let mut next = Some(event);
                let mut stop = false;
                while let Some(event) = next.take() {
                    dirty = true;
                    if matches!(
                        handle_terminal_event(event, &mut state, &mut agent, &tx),
                        LoopControl::Break
                    ) {
                        stop = true;
                        break;
                    }
                    next = events
                        .next()
                        .now_or_never()
                        .flatten()
                        .and_then(|event| event.ok());
                }
                if stop {
                    break;
                }
            }
            Some(chat_event) = rx.recv() => {
                dirty = true;
                handle_chat_event(chat_event, &mut state, &tx);
                // Ответы и фоновые загрузки тоже приходят пачками — добираем
                // всё, что уже в очереди, до следующей отрисовки.
                while let Ok(chat_event) = rx.try_recv() {
                    handle_chat_event(chat_event, &mut state, &tx);
                }
            }
        }
    }

    Ok(())
}

/// Обработка одного события терминала. Вынесена из цикла, чтобы события
/// можно было разбирать пачкой между кадрами.
fn handle_terminal_event(
    event: Event,
    state: &mut AppState,
    agent: &mut Arc<CliAgent>,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> LoopControl {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            if matches!(handle_key(key, state, agent, tx), LoopControl::Break) {
                return LoopControl::Break;
            }
            // токен или адрес сервиса поменяли — дальше шлём запросы
            // уже новым агентом (запущенные ждут на старом)
            if state.agent_dirty {
                state.agent_dirty = false;
                match CliAgent::from_config(&state.config, crate::logging::exchange_log())
                    .map(|agent| agent.with_unauthorized_hint(crate::logging::UNAUTHORIZED_HINT))
                {
                    Ok(updated) => *agent = Arc::new(updated),
                    Err(err) => state.notify(format!(
                        "Не удалось применить настройки подключения: {err}"
                    )),
                }
                // Клиент чатов ходит по тому же адресу с тем же токеном,
                // поэтому пересобирается вместе с агентом.
                state.chats_client = Arc::new(chats_client(&state.config));
                fetch_chats(state, tx);
            }
        }
        Event::Mouse(mouse) => {
            let delta = match mouse.kind {
                MouseEventKind::ScrollUp => -(MOUSE_SCROLL_STEP as i32),
                MouseEventKind::ScrollDown => MOUSE_SCROLL_STEP as i32,
                // Клики и перетаскивания TUI не использует: выделение текста
                // остаётся за терминалом (Shift + перетаскивание).
                _ => return LoopControl::Continue,
            };
            scroll_history_at(state, mouse.column, mouse.row, delta);
        }
        // вставка из буфера обмена приходит одним событием (bracketed paste)
        Event::Paste(text) => state.insert_into_input(&text),
        _ => {}
    }
    LoopControl::Continue
}

/// Глобальные сочетания клавиш, работающие вне зависимости от фокуса/состояния "pending".
fn handle_global_key(
    key: crossterm::event::KeyEvent,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> Option<LoopControl> {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Some(LoopControl::Break);
    }
    if key.code == KeyCode::Char('n') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if matches!(state.focus, Focus::Settings | Focus::Import | Focus::Confirm | Focus::Facts | Focus::Branches | Focus::Memory)
            || state.active_pending()
        {
            return Some(LoopControl::Continue);
        }
        // Чат создаёт сервис: пока список чатов не получен, писать некуда.
        if let Some(reason) = state.blocked_reason() {
            state.notify(format!("Чат не создать: {reason}"));
            return Some(LoopControl::Continue);
        }
        request_create_chat(state, tx);
        state.notify("Создаю чат в сервисе…");
        return Some(LoopControl::Continue);
    }
    // В режиме выбора у Ctrl+Y другой смысл — копировать выбранное сообщение,
    // поэтому глобальная ветка его туда пропускает.
    if key.code == KeyCode::Char('y')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && state.focus != Focus::MessageSelect
    {
        let text = state.active_input().to_string();
        if text.is_empty() {
            state.notify("Поле ввода пустое — копировать нечего");
        } else {
            match crate::clipboard::copy(&text) {
                Ok(()) => state.notify("Текст ввода скопирован в буфер обмена"),
                Err(err) => state.notify(format!("Не удалось скопировать: {err}")),
            }
        }
        return Some(LoopControl::Continue);
    }
    // Ctrl+G — режим выбора сообщения. `Char('п')` — та же физическая клавиша
    // на русской раскладке: crossterm отдаёт символ раскладки, а не позицию.
    if matches!(key.code, KeyCode::Char('g') | KeyCode::Char('п'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(state.focus, Focus::Input | Focus::Sidebar)
    {
        let count = state
            .active_chat_index()
            .map(|index| state.chats[index].messages.len())
            .unwrap_or(0);
        if count == 0 {
            state.notify("В чате нет сообщений — копировать нечего");
            return Some(LoopControl::Continue);
        }
        // выбор начинается с последнего сообщения: чаще всего копируют
        // свежий ответ модели
        let last = count - 1;
        if let Some(chat_id) = state.active_chat_id() {
            let ui = state.chat_ui.entry(chat_id).or_default();
            ui.selected_message = Some(last);
            ui.auto_scroll = false;
            ui.scroll_to_message = Some(last);
        }
        state.focus = Focus::MessageSelect;
        return Some(LoopControl::Continue);
    }
    if key.code == KeyCode::Char('e') && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.chats_load = ChatsLoad::Loading;
        fetch_chats(state, tx);
        state.notify("Обновляю список чатов…");
        return Some(LoopControl::Continue);
    }
    if key.code == KeyCode::Char('u') && key.modifiers.contains(KeyModifiers::CONTROL) {
        let Some(chat_id) = state.active_chat_id() else {
            return Some(LoopControl::Continue);
        };
        let messages = state
            .chat_ui
            .get(&chat_id)
            .and_then(|ui| ui.unsaved.as_ref())
            .map(|unsaved| unsaved.messages.clone());
        match messages {
            Some(messages) => {
                request_append_exchange(state, &chat_id, messages, tx);
                state.notify("Повторяю запись обмена в сервис…");
            }
            None => state.notify("Несохранённого обмена в этом чате нет"),
        }
        return Some(LoopControl::Continue);
    }
    if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.show_reasoning = !state.show_reasoning;
        state.notify(if state.show_reasoning {
            "Рассуждение модели показано"
        } else {
            "Рассуждение модели скрыто"
        });
        return Some(LoopControl::Continue);
    }
    if matches!(state.focus, Focus::Settings | Focus::Import | Focus::Confirm | Focus::Facts | Focus::Branches | Focus::Memory) {
        return None;
    }
    if key.code == KeyCode::Char('w') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if state.panes.len() > 1 {
            state.panes.remove(state.active_pane);
            if state.active_pane >= state.panes.len() {
                state.active_pane = state.panes.len() - 1;
            }
        }
        return Some(LoopControl::Continue);
    }
    if key.modifiers.contains(KeyModifiers::ALT) || key.modifiers.contains(KeyModifiers::SUPER) {
        match key.code {
            KeyCode::Right => {
                if state.panes.len() > 1 {
                    state.active_pane = (state.active_pane + 1) % state.panes.len();
                }
                return Some(LoopControl::Continue);
            }
            KeyCode::Left => {
                if state.panes.len() > 1 {
                    state.active_pane = (state.active_pane + state.panes.len() - 1) % state.panes.len();
                }
                return Some(LoopControl::Continue);
            }
            _ => {}
        }
    }
    None
}

fn handle_key(
    key: crossterm::event::KeyEvent,
    state: &mut AppState,
    agent: &Arc<CliAgent>,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> LoopControl {
    if let Some(control) = handle_global_key(key, state, tx) {
        return control;
    }
    if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if state.focus == Focus::Import {
            state.import = None;
            state.focus = Focus::Input;
        } else if !matches!(state.focus, Focus::Settings | Focus::Confirm | Focus::Facts | Focus::Branches | Focus::Memory)
            && let Some(chat_index) = state.active_chat_index() {
                state.import = Some(ImportPicker::new(&state.chats[chat_index], &state.chats));
                state.focus = Focus::Import;
            }
        return LoopControl::Continue;
    }
    if key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if matches!(state.focus, Focus::Import | Focus::Confirm | Focus::Facts | Focus::Branches | Focus::Memory) {
            return LoopControl::Continue;
        }
        if state.focus == Focus::Settings {
            state.settings = None;
            state.focus = Focus::Input;
        } else if let Some(chat_index) = state.active_chat_index() {
            state.settings = Some(SettingsEditor::from_chat(
                &state.chats[chat_index],
                &state.config,
                &state.model_choices,
                &state.ollama_models,
                &state.profile_choices,
            ));
            state.focus = Focus::Settings;
        }
        return LoopControl::Continue;
    }
    if key.code == KeyCode::Char('f') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if state.focus == Focus::Facts {
            state.facts = None;
            state.focus = Focus::Input;
        } else if matches!(state.focus, Focus::Input | Focus::Sidebar)
            && let Some(chat_index) = state.active_chat_index()
        {
            let chat_id = state.chats[chat_index].id.clone();
            let chat_title = state.chats[chat_index].title.clone();
            state.facts = Some(FactsPicker::new(&chat_id, &chat_title));
            state.focus = Focus::Facts;
            request_facts(state, &chat_id, tx);
        }
        return LoopControl::Continue;
    }
    if key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if state.focus == Focus::Branches {
            state.branches = None;
            state.focus = Focus::Input;
        } else if matches!(state.focus, Focus::Input | Focus::Sidebar)
            && let Some(chat_index) = state.active_chat_index()
        {
            let chat_id = state.chats[chat_index].id.clone();
            let chat_title = state.chats[chat_index].title.clone();
            state.branches = Some(BranchesPicker::new(&chat_id, &chat_title));
            state.focus = Focus::Branches;
            request_branches(state, &chat_id, tx);
        }
        return LoopControl::Continue;
    }
    // Ctrl+K, не Ctrl+M: в raw-режиме терминала Ctrl+M неотличим от Enter
    // (оба шлют \r), поэтому такая привязка никогда бы не сработала.
    if key.code == KeyCode::Char('k') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if state.focus == Focus::Memory {
            state.memory = None;
            state.focus = Focus::Input;
        } else if matches!(state.focus, Focus::Input | Focus::Sidebar)
            && let Some(chat_index) = state.active_chat_index()
        {
            let chat_id = state.chats[chat_index].id.clone();
            let chat_title = state.chats[chat_index].title.clone();
            state.memory = Some(MemoryPicker::new(&chat_id, &chat_title));
            state.focus = Focus::Memory;
            request_working_memory(state, &chat_id, tx);
            request_long_term_memory(state, &chat_id, tx);
        }
        return LoopControl::Continue;
    }
    if key.code == KeyCode::Char('t') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if state.focus == Focus::Task {
            state.task = None;
            state.focus = Focus::Input;
        } else if matches!(state.focus, Focus::Input | Focus::Sidebar)
            && let Some(chat_index) = state.active_chat_index()
        {
            let chat_id = state.chats[chat_index].id.clone();
            let chat_title = state.chats[chat_index].title.clone();
            state.task = Some(TaskPicker::new(&chat_id, &chat_title));
            state.focus = Focus::Task;
            request_task(state, &chat_id, tx);
        }
        return LoopControl::Continue;
    }
    if key.code == KeyCode::Tab
        && !matches!(
            state.focus,
            Focus::Settings | Focus::Import | Focus::Confirm | Focus::Facts | Focus::Branches | Focus::Memory | Focus::Task | Focus::MessageSelect
        )
    {
        state.focus = match state.focus {
            Focus::Input => Focus::Sidebar,
            Focus::Sidebar => Focus::Input,
            Focus::Settings
            | Focus::Import
            | Focus::Confirm
            | Focus::Facts
            | Focus::Branches
            | Focus::Memory
            | Focus::Task
            | Focus::MessageSelect => unreachable!(),
        };
        return LoopControl::Continue;
    }

    match state.focus {
        Focus::Settings => handle_settings_key(key, state, tx),
        Focus::Import => handle_import_key(key, state, tx),
        Focus::Confirm => handle_confirm_key(key, state, tx),
        Focus::Facts => handle_facts_key(key, state, tx),
        Focus::Branches => handle_branches_key(key, state, tx),
        Focus::Memory => handle_memory_key(key, state, tx),
        Focus::Task => handle_task_key(key, state, tx),
        Focus::MessageSelect => handle_message_select_key(key, state),
        Focus::Sidebar => handle_sidebar_key(key, state, tx),
        Focus::Input => {
            if state.active_pending() || state.active_chat_id().is_none() {
                LoopControl::Continue
            } else {
                handle_input_key(key, state, agent, tx)
            }
        }
    }
}

fn handle_settings_key(
    key: crossterm::event::KeyEvent,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> LoopControl {
    let editor = state.settings.as_mut().expect("settings focus implies editor");
    match key.code {
        KeyCode::Esc => {
            state.settings = None;
            state.focus = Focus::Input;
        }
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            match editor
                .build()
                .and_then(|format| editor.build_sampling().map(|sampling| (format, sampling)))
                .and_then(|(format, sampling)| {
                    editor
                        .build_context_limit()
                        .map(|context_limit| (format, sampling, context_limit))
                })
                .and_then(|(format, sampling, context_limit)| {
                    editor
                        .build_summary_keep_messages()
                        .map(|summary_keep_messages| {
                            (format, sampling, context_limit, summary_keep_messages)
                        })
                })
                .and_then(|(format, sampling, context_limit, summary_keep_messages)| {
                    editor.build_summary_step_messages().map(|summary_step_messages| {
                        (
                            format,
                            sampling,
                            context_limit,
                            summary_keep_messages,
                            summary_step_messages,
                        )
                    })
                })
                .and_then(
                    |(format, sampling, context_limit, summary_keep_messages, summary_step_messages)| {
                        editor.build_context_window_messages().map(|context_window_messages| {
                            (
                                format,
                                sampling,
                                context_limit,
                                summary_keep_messages,
                                summary_step_messages,
                                context_window_messages,
                            )
                        })
                    },
                )
                .and_then(
                    |(
                        format,
                        sampling,
                        context_limit,
                        summary_keep_messages,
                        summary_step_messages,
                        context_window_messages,
                    )| {
                        editor.build_memory_working_max_entries().map(|memory_working_max_entries| {
                            (
                                format,
                                sampling,
                                context_limit,
                                summary_keep_messages,
                                summary_step_messages,
                                context_window_messages,
                                memory_working_max_entries,
                            )
                        })
                    },
                )
                .and_then(
                    |(
                        format,
                        sampling,
                        context_limit,
                        summary_keep_messages,
                        summary_step_messages,
                        context_window_messages,
                        memory_working_max_entries,
                    )| {
                        editor.build_memory_long_term_max_entries().map(
                            |memory_long_term_max_entries| {
                                (
                                    format,
                                    sampling,
                                    context_limit,
                                    summary_keep_messages,
                                    summary_step_messages,
                                    context_window_messages,
                                    memory_working_max_entries,
                                    memory_long_term_max_entries,
                                )
                            },
                        )
                    },
                )
            {
                Ok((
                    format,
                    sampling,
                    context_limit,
                    summary_keep_messages,
                    summary_step_messages,
                    context_window_messages,
                    memory_working_max_entries,
                    memory_long_term_max_entries,
                )) => {
                    let summary_enabled = editor.build_summary_enabled();
                    let context_strategy = editor.build_context_strategy();
                    let profile_id = editor.build_profile_id();
                    let memory_layers_enabled = editor.build_memory_layers_enabled();
                    let memory_router_enabled = editor.build_memory_router_enabled();
                    let task_state_enabled = editor.build_task_state_enabled();
                    let task_state_auto_enabled = editor.build_task_state_auto_enabled();
                    let reasoning = editor.reasoning;
                    let thinking = editor.thinking;
                    // состав сохраняем всегда: при возврате к «Группе экспертов»
                    // ранее введённый список не теряется
                    let experts = split_list(&editor.experts);
                    let model = non_empty(&editor.model);
                    let provider = editor.provider;
                    let chat_id = editor.chat_id.clone();
                    let server_url = non_empty(&editor.server_url);
                    let client_token = non_empty(&editor.client_token);
                    let ollama_url = non_empty(&editor.ollama_url);
                    state.settings = None;
                    state.focus = Focus::Input;
                    if let Some(index) = state.chat_index(&chat_id) {
                        let current = &state.chats[index].settings;
                        let settings = ChatSettings {
                            provider,
                            model,
                            custom_response_mode: format.is_some(),
                            response_format: format.unwrap_or_default(),
                            sampling,
                            reasoning,
                            thinking,
                            experts,
                            max_context_tokens: context_limit,
                            summary_enabled,
                            summary_keep_messages,
                            summary_step_messages,
                            context_strategy,
                            context_window_messages,
                            memory_layers_enabled,
                            memory_router_enabled,
                            memory_working_max_entries,
                            memory_long_term_max_entries,
                            profile_id,
                            task_state_enabled,
                            task_state_auto_enabled,
                            git_tools_enabled: current.git_tools_enabled,
                            git_repository: current.git_repository.clone(),
                            git_allowed_tools: current.git_allowed_tools.clone(),
                            tool_max_iterations: current.tool_max_iterations,
                        };
                        // Настройки чата хранит сервис: локально они
                        // применяются ответом на PATCH, а не сразу.
                        request_update_chat(state, &chat_id, None, Some(settings), tx);
                    }
                    save_connection(state, server_url, client_token, ollama_url);
                }
                Err(err) => editor.error = Some(err),
            }
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if editor.pane == SettingsPane::Fields {
                editor.reset_field();
            }
        }
        KeyCode::Tab => editor.focus_next(),
        KeyCode::Down => editor.move_focus(1),
        KeyCode::Up => editor.move_focus(-1),
        KeyCode::Right | KeyCode::Enter if editor.pane == SettingsPane::Sections => {
            if !editor.visible_fields().is_empty() {
                editor.pane = SettingsPane::Fields;
                editor.field = 0;
            }
        }
        KeyCode::Left if editor.pane == SettingsPane::Sections => {}
        KeyCode::Left
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::Reasoning) =>
        {
            editor.cycle_reasoning(-1);
        }
        KeyCode::Right | KeyCode::Char(' ')
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::Reasoning) =>
        {
            editor.cycle_reasoning(1);
        }
        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // одна клавиша на обоих провайдеров: обновляем список активного,
            // чтобы не плодить отдельные горячие клавиши под каждый
            let provider = editor.provider;
            editor.error = None;
            match provider {
                Provider::Ollama => {
                    state.notify("Обновляю список моделей Ollama…");
                    fetch_ollama_models(&state.config, tx);
                }
                Provider::Cloud => {
                    state.notify("Обновляю список моделей сервиса…");
                    fetch_cloud_models(&state.config, tx);
                }
            }
        }
        KeyCode::Left
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::Provider) =>
        {
            editor.cycle_provider(-1);
        }
        KeyCode::Right | KeyCode::Char(' ')
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::Provider) =>
        {
            editor.cycle_provider(1);
        }
        KeyCode::Left
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::Model) =>
        {
            editor.cycle_model(-1);
        }
        KeyCode::Right
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::Model) =>
        {
            editor.cycle_model(1);
        }
        KeyCode::Left
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::Thinking) =>
        {
            editor.cycle_thinking(-1);
        }
        KeyCode::Right | KeyCode::Char(' ')
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::Thinking) =>
        {
            editor.cycle_thinking(1);
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::Mode) =>
        {
            editor.custom_mode = !editor.custom_mode;
        }
        KeyCode::Left
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::SummaryEnabled) =>
        {
            editor.cycle_summary_enabled(-1);
        }
        KeyCode::Right | KeyCode::Char(' ')
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::SummaryEnabled) =>
        {
            editor.cycle_summary_enabled(1);
        }
        KeyCode::Left
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::ContextStrategy) =>
        {
            editor.cycle_context_strategy(-1);
        }
        KeyCode::Right | KeyCode::Char(' ')
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::ContextStrategy) =>
        {
            editor.cycle_context_strategy(1);
        }
        KeyCode::Left
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::Profile) =>
        {
            editor.cycle_profile(-1);
        }
        KeyCode::Right
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::Profile) =>
        {
            editor.cycle_profile(1);
        }
        KeyCode::Left
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::MemoryLayersEnabled) =>
        {
            editor.cycle_memory_layers_enabled(-1);
        }
        KeyCode::Right | KeyCode::Char(' ')
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::MemoryLayersEnabled) =>
        {
            editor.cycle_memory_layers_enabled(1);
        }
        KeyCode::Left
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::MemoryRouterEnabled) =>
        {
            editor.cycle_memory_router_enabled(-1);
        }
        KeyCode::Right | KeyCode::Char(' ')
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::MemoryRouterEnabled) =>
        {
            editor.cycle_memory_router_enabled(1);
        }
        KeyCode::Left
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::TaskStateEnabled) =>
        {
            editor.cycle_task_state_enabled(-1);
        }
        KeyCode::Right | KeyCode::Char(' ')
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::TaskStateEnabled) =>
        {
            editor.cycle_task_state_enabled(1);
        }
        KeyCode::Left
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::TaskStateAutoEnabled) =>
        {
            editor.cycle_task_state_auto_enabled(-1);
        }
        KeyCode::Right | KeyCode::Char(' ')
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::TaskStateAutoEnabled) =>
        {
            editor.cycle_task_state_auto_enabled(1);
        }
        KeyCode::Left => {
            // из полей — обратно к списку разделов
            editor.pane = SettingsPane::Sections;
        }
        KeyCode::Char(c) if editor.pane == SettingsPane::Fields => {
            if let Some(value) = editor.field_value_mut() {
                value.push(c);
            }
        }
        KeyCode::Backspace if editor.pane == SettingsPane::Fields => {
            if let Some(value) = editor.field_value_mut() {
                value.pop();
            }
        }
        _ => {}
    }
    LoopControl::Continue
}

/// Сохранить общие параметры подключения в конфиг. Адрес сервиса и токен
/// общие для всех чатов, поэтому после изменения агента нужно пересобрать.
fn save_connection(
    state: &mut AppState,
    server_url: Option<String>,
    client_token: Option<String>,
    ollama_url: Option<String>,
) {
    if state.config.server_url == server_url
        && state.config.client_token == client_token
        && state.config.ollama_url == ollama_url
    {
        return;
    }
    state.config.server_url = server_url;
    state.config.client_token = client_token;
    state.config.ollama_url = ollama_url;
    match state.config.save() {
        Ok(()) => {
            state.agent_dirty = true;
            state.notify("Настройки подключения сохранены");
        }
        Err(err) => state.notify(format!("Не удалось сохранить конфиг: {err}")),
    }
}

/// Клавиши окна импорта: ↑/↓ — выбор чата, Space — отметить, Enter — перенести.
fn handle_import_key(
    key: crossterm::event::KeyEvent,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> LoopControl {
    let picker = state.import.as_mut().expect("import focus implies picker");
    match key.code {
        KeyCode::Esc => {
            state.import = None;
            state.focus = Focus::Input;
        }
        KeyCode::Up => picker.move_cursor(-1),
        KeyCode::Down => picker.move_cursor(1),
        KeyCode::Char(' ') => picker.toggle_current(),
        KeyCode::Enter => {
            let ids = picker.selected_ids();
            let target_id = picker.target_id.clone();
            state.import = None;
            state.focus = Focus::Input;
            if !ids.is_empty() {
                import_context(state, &target_id, &ids, tx);
            }
        }
        _ => {}
    }
    LoopControl::Continue
}

/// Перенести историю выбранных чатов в целевой одним сообщением-контекстом.
/// История берётся у сервиса, поэтому чат-источник без загруженной истории
/// переносить нельзя (specs/client-chat-storage).
fn import_context(
    state: &mut AppState,
    target_id: &str,
    source_ids: &[String],
    tx: &mpsc::UnboundedSender<ChatEvent>,
) {
    let mut blocks: Vec<String> = Vec::new();
    let mut waiting: Vec<String> = Vec::new();
    for id in source_ids {
        let Some(index) = state.chat_index(id) else {
            continue;
        };
        if state.chats[index].history_loaded {
            blocks.push(chats::context_block(&state.chats[index]));
        } else {
            waiting.push(id.clone());
        }
    }
    if !waiting.is_empty() {
        for id in &waiting {
            ensure_history(state, id, tx);
        }
        state.notify("История чата-источника ещё не загружена: перенос не выполнен");
        return;
    }
    if blocks.is_empty() {
        return;
    }
    let content = format!(
        "Ниже — контекст из других чатов. Используй его как справочную информацию \
и подтверди, что он учтён.\n\n{}",
        blocks.join("\n")
    );
    let Some(chat_index) = state.chats.iter().position(|c| c.id == target_id) else {
        return;
    };
    // Перенос контекста — реплика пользователя в целевом чате: она уйдёт в
    // сервис вместе с обменом, когда пользователь отправит сообщение, и
    // заголовок чата от переноса не меняется.
    state.chats[chat_index].messages.push(Message::user(content));
    state.chats[chat_index].touch_quietly();
    let ui = state.chat_ui.entry(target_id.to_string()).or_default();
    ui.auto_scroll = true;
    ui.scroll_to_message = None;
}

fn handle_sidebar_key(
    key: crossterm::event::KeyEvent,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> LoopControl {
    match key.code {
        KeyCode::Up => {
            if state.sidebar_selected > 0 {
                state.sidebar_selected -= 1;
            }
        }
        KeyCode::Down => {
            if state.sidebar_selected + 1 < state.chats.len() {
                state.sidebar_selected += 1;
            }
        }
        KeyCode::Left => {
            if state.panes.len() > 1 {
                state.active_pane = (state.active_pane + state.panes.len() - 1) % state.panes.len();
            }
        }
        KeyCode::Right => {
            if state.panes.len() > 1 {
                state.active_pane = (state.active_pane + 1) % state.panes.len();
            }
        }
        KeyCode::Enter => {
            let Some(chat) = state.chats.get(state.sidebar_selected) else {
                return LoopControl::Continue;
            };
            let id = chat.id.clone();
            state.show_in_active_pane(id.clone());
            ensure_history(state, &id, tx);
            state.focus = Focus::Input;
        }
        KeyCode::Char('s') => {
            let Some(chat) = state.chats.get(state.sidebar_selected) else {
                return LoopControl::Continue;
            };
            let id = chat.id.clone();
            if let Some(existing) = state.panes.iter().position(|p| p == &id) {
                state.active_pane = existing;
            } else if state.panes.len() < MAX_PANES {
                state.panes.push(id.clone());
                state.active_pane = state.panes.len() - 1;
                ensure_history(state, &id, tx);
            }
            state.focus = Focus::Input;
        }
        KeyCode::Char('d') => {
            let Some(chat) = state.chats.get(state.sidebar_selected) else {
                return LoopControl::Continue;
            };
            if !state.is_pending(&chat.id) {
                state.delete_confirm = Some(DeleteConfirm {
                    chat_id: chat.id.clone(),
                    chat_title: chat.title.clone(),
                });
                state.focus = Focus::Confirm;
            }
        }
        KeyCode::Esc => return LoopControl::Break,
        _ => {}
    }
    LoopControl::Continue
}

/// Клавиши окна подтверждения удаления: y/Enter — удалить, n/Esc — отмена.
fn handle_facts_key(
    key: crossterm::event::KeyEvent,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> LoopControl {
    let picker = state.facts.as_mut().expect("facts focus implies picker");

    if let Some(editor) = picker.editor.as_mut() {
        match key.code {
            KeyCode::Esc => picker.editor = None,
            KeyCode::Tab if editor.key.is_empty() || editor.editing_key => {
                editor.editing_key = !editor.editing_key;
            }
            KeyCode::Enter => {
                let key_text = editor.key.trim().to_string();
                let value_text = editor.value.trim().to_string();
                if key_text.is_empty() {
                    state.notify("Ключ факта не может быть пустым");
                    return LoopControl::Continue;
                }
                let chat_id = picker.chat_id.clone();
                request_set_fact(state, &chat_id, &key_text, &value_text, tx);
            }
            KeyCode::Backspace => {
                if editor.editing_key {
                    editor.key.pop();
                } else {
                    editor.value.pop();
                }
            }
            KeyCode::Char(c) => {
                if editor.editing_key {
                    editor.key.push(c);
                } else {
                    editor.value.push(c);
                }
            }
            _ => {}
        }
        return LoopControl::Continue;
    }

    match key.code {
        KeyCode::Esc => {
            state.facts = None;
            state.focus = Focus::Input;
        }
        KeyCode::Up => picker.move_cursor(-1),
        KeyCode::Down => picker.move_cursor(1),
        KeyCode::Char('n') => {
            picker.editor = Some(FactEditor {
                key: String::new(),
                value: String::new(),
                editing_key: true,
            });
        }
        KeyCode::Enter => {
            if let Some(fact) = picker.facts.get(picker.cursor) {
                picker.editor = Some(FactEditor {
                    key: fact.key.clone(),
                    value: fact.value.clone(),
                    editing_key: false,
                });
            }
        }
        KeyCode::Char('d') => {
            if let Some(key) = picker.selected_key().map(str::to_string) {
                let chat_id = picker.chat_id.clone();
                request_delete_fact(state, &chat_id, &key, tx);
            }
        }
        _ => {}
    }
    LoopControl::Continue
}

fn handle_branches_key(
    key: crossterm::event::KeyEvent,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> LoopControl {
    let picker = state.branches.as_mut().expect("branches focus implies picker");

    if let Some(creation) = picker.creating.as_mut() {
        if let Some(name) = creation.name.as_mut() {
            match key.code {
                KeyCode::Esc => picker.creating = None,
                KeyCode::Backspace => {
                    name.pop();
                }
                KeyCode::Char(c) => name.push(c),
                KeyCode::Enter => {
                    let Some(message) = creation.messages.get(creation.message_cursor) else {
                        return LoopControl::Continue;
                    };
                    let from_seq = message.seq;
                    let branch_name = if name.trim().is_empty() {
                        format!("ветка от {from_seq}")
                    } else {
                        name.trim().to_string()
                    };
                    let chat_id = picker.chat_id.clone();
                    request_create_branch(state, &chat_id, from_seq, &branch_name, tx);
                }
                _ => {}
            }
            return LoopControl::Continue;
        }
        match key.code {
            KeyCode::Esc => picker.creating = None,
            KeyCode::Up => {
                creation.message_cursor = creation.message_cursor.saturating_sub(1);
            }
            KeyCode::Down => {
                creation.message_cursor =
                    (creation.message_cursor + 1).min(creation.messages.len().saturating_sub(1));
            }
            KeyCode::Enter if !creation.messages.is_empty() => {
                creation.name = Some(String::new());
            }
            _ => {}
        }
        return LoopControl::Continue;
    }

    match key.code {
        KeyCode::Esc => {
            state.branches = None;
            state.focus = Focus::Input;
        }
        KeyCode::Up => picker.move_cursor(-1),
        KeyCode::Down => picker.move_cursor(1),
        KeyCode::Char('n') => {
            let chat_id = picker.chat_id.clone();
            picker.creating = Some(BranchCreation {
                messages: Vec::new(),
                message_cursor: 0,
                loading: true,
                name: None,
            });
            request_branch_source_messages(state, &chat_id, tx);
        }
        KeyCode::Enter => {
            if let Some(branch_id) = picker.selected_branch_id().map(str::to_string) {
                let chat_id = picker.chat_id.clone();
                request_activate_branch(state, &chat_id, &branch_id, tx);
            }
        }
        _ => {}
    }
    LoopControl::Continue
}

fn handle_memory_key(
    key: crossterm::event::KeyEvent,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> LoopControl {
    let picker = state.memory.as_mut().expect("memory focus implies picker");

    if let Some(editor) = picker.editor.as_mut() {
        match key.code {
            KeyCode::Esc => picker.editor = None,
            KeyCode::Tab => {
                let fields = if editor.for_long_term { 3 } else { 2 };
                editor.field = (editor.field + 1) % fields;
            }
            KeyCode::Enter => {
                let key_text = editor.key.trim().to_string();
                let value_text = editor.value.trim().to_string();
                if key_text.is_empty() {
                    state.notify("Ключ записи памяти не может быть пустым");
                    return LoopControl::Continue;
                }
                let chat_id = picker.chat_id.clone();
                if editor.for_long_term {
                    let entry_type = editor.entry_type.clone();
                    request_set_long_term_memory(state, &chat_id, &entry_type, &key_text, &value_text, tx);
                } else {
                    request_set_working_memory(state, &chat_id, &key_text, &value_text, tx);
                }
            }
            KeyCode::Left | KeyCode::Right if editor.for_long_term && editor.field == 2 => {
                const TYPES: [&str; 3] = ["profile", "decision", "knowledge"];
                let current = TYPES.iter().position(|t| *t == editor.entry_type).unwrap_or(0) as i32;
                let delta = if key.code == KeyCode::Right { 1 } else { -1 };
                let len = TYPES.len() as i32;
                editor.entry_type = TYPES[(current + delta).rem_euclid(len) as usize].to_string();
            }
            KeyCode::Backspace => match editor.field {
                0 => {
                    editor.key.pop();
                }
                1 => {
                    editor.value.pop();
                }
                _ => {}
            },
            KeyCode::Char(c) => match editor.field {
                0 => editor.key.push(c),
                1 => editor.value.push(c),
                _ => {}
            },
            _ => {}
        }
        return LoopControl::Continue;
    }

    match key.code {
        KeyCode::Esc => {
            state.memory = None;
            state.focus = Focus::Input;
        }
        KeyCode::Left => picker.cycle_section(-1),
        KeyCode::Right => picker.cycle_section(1),
        KeyCode::Up => picker.move_cursor(-1),
        KeyCode::Down => picker.move_cursor(1),
        KeyCode::Char('n') => match picker.current_section() {
            MemorySection::ShortTerm => {}
            MemorySection::Working => {
                picker.editor = Some(MemoryEditor {
                    for_long_term: false,
                    key: String::new(),
                    value: String::new(),
                    entry_type: String::new(),
                    field: 0,
                });
            }
            MemorySection::LongTerm => {
                picker.editor = Some(MemoryEditor {
                    for_long_term: true,
                    key: String::new(),
                    value: String::new(),
                    entry_type: "knowledge".to_string(),
                    field: 0,
                });
            }
        },
        KeyCode::Enter => match picker.current_section() {
            MemorySection::ShortTerm => {}
            MemorySection::Working => {
                if let Some(entry) = picker.working.get(picker.working_cursor) {
                    picker.editor = Some(MemoryEditor {
                        for_long_term: false,
                        key: entry.key.clone(),
                        value: entry.value.clone(),
                        entry_type: String::new(),
                        field: 1,
                    });
                }
            }
            MemorySection::LongTerm => {
                if let Some(entry) = picker.long_term.get(picker.long_term_cursor) {
                    picker.editor = Some(MemoryEditor {
                        for_long_term: true,
                        key: entry.key.clone().unwrap_or_default(),
                        value: entry.value.clone(),
                        entry_type: entry.entry_type.clone(),
                        field: 1,
                    });
                }
            }
        },
        KeyCode::Char('d') => match picker.current_section() {
            MemorySection::ShortTerm => {}
            MemorySection::Working => {
                if let Some(key) = picker.selected_working_key().map(str::to_string) {
                    let chat_id = picker.chat_id.clone();
                    request_delete_working_memory(state, &chat_id, &key, tx);
                }
            }
            MemorySection::LongTerm => {
                if let Some(id) = picker.selected_long_term_id().map(str::to_string) {
                    let chat_id = picker.chat_id.clone();
                    request_delete_long_term_memory(state, &chat_id, &id, tx);
                }
            }
        },
        KeyCode::Char('t') if picker.current_section() == MemorySection::Working => {
            let chat_id = picker.chat_id.clone();
            request_finish_task(state, &chat_id, tx);
        }
        _ => {}
    }
    LoopControl::Continue
}

/// Экран состояния задачи по `Ctrl+T`: перебор допустимых следующих этапов
/// стрелками, Enter — применить; `s`/`a` — правка шага/ожидаемого действия;
/// `p`/`r` — пауза/возобновление (specs/task-state, design.md решение 9).
fn handle_task_key(
    key: crossterm::event::KeyEvent,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> LoopControl {
    let picker = state.task.as_mut().expect("task focus implies picker");

    if let Some(editor) = picker.editor.as_mut() {
        match key.code {
            KeyCode::Esc => picker.editor = None,
            KeyCode::Enter => {
                let chat_id = picker.chat_id.clone();
                let value = editor.value.trim().to_string();
                if editor.editing_expected_action {
                    request_task_transition(state, &chat_id, None, None, Some(&value), tx);
                } else {
                    request_task_transition(state, &chat_id, None, Some(&value), None, tx);
                }
            }
            KeyCode::Backspace => {
                editor.value.pop();
            }
            KeyCode::Char(c) => editor.value.push(c),
            _ => {}
        }
        return LoopControl::Continue;
    }

    match key.code {
        KeyCode::Esc => {
            state.task = None;
            state.focus = Focus::Input;
        }
        KeyCode::Left => picker.cycle_next_stage(-1),
        KeyCode::Right => picker.cycle_next_stage(1),
        KeyCode::Enter => {
            if let Some(stage) = picker.selected_next_stage() {
                let chat_id = picker.chat_id.clone();
                request_task_transition(state, &chat_id, Some(stage), None, None, tx);
            }
        }
        KeyCode::Char('s') => {
            if let Some(task) = &picker.state {
                picker.editor = Some(TaskFieldEditor {
                    editing_expected_action: false,
                    value: task.step.clone(),
                });
            }
        }
        KeyCode::Char('a') => {
            if let Some(task) = &picker.state {
                picker.editor = Some(TaskFieldEditor {
                    editing_expected_action: true,
                    value: task.expected_action.clone(),
                });
            }
        }
        KeyCode::Char('p') => {
            let chat_id = picker.chat_id.clone();
            request_task_pause(state, &chat_id, tx);
        }
        KeyCode::Char('r') => {
            let chat_id = picker.chat_id.clone();
            request_task_resume(state, &chat_id, tx);
        }
        _ => {}
    }
    LoopControl::Continue
}

fn handle_confirm_key(
    key: crossterm::event::KeyEvent,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> LoopControl {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Char('д') | KeyCode::Enter => {
            let confirm = state.delete_confirm.take().expect("confirm focus implies request");
            // Чат удаляет сервис: из списка клиента он исчезает по
            // подтверждению, а не до него.
            request_delete_chat(state, &confirm.chat_id, tx);
            state.focus = Focus::Sidebar;
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('н') | KeyCode::Esc => {
            state.delete_confirm = None;
            state.focus = Focus::Sidebar;
        }
        _ => {}
    }
    LoopControl::Continue
}

/// Убрать удалённый сервисом чат из состояния, освободив панели, где он был
/// открыт.
fn forget_chat(state: &mut AppState, chat_id: &str) {
    let Some(index) = state.chats.iter().position(|c| c.id == chat_id) else {
        return;
    };
    state.chats.remove(index);
    state.chat_ui.remove(chat_id);

    // Пустой список чатов законен: черновой чат вместо удалённого больше не
    // подставляется, потому что чат создаёт сервис по действию пользователя.
    if state.panes.iter().any(|id| id == chat_id) {
        match state.chats.first().map(|chat| chat.id.clone()) {
            Some(fallback_id) if state.panes.len() == 1 => state.panes[0] = fallback_id,
            _ => state.panes.retain(|id| id != chat_id),
        }
    }
    if state.active_pane >= state.panes.len() {
        state.active_pane = state.panes.len().saturating_sub(1);
    }
    if state.sidebar_selected >= state.chats.len() {
        state.sidebar_selected = state.chats.len().saturating_sub(1);
    }
}

fn handle_input_key(
    key: crossterm::event::KeyEvent,
    state: &mut AppState,
    agent: &Arc<CliAgent>,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> LoopControl {
    let Some(chat_id) = state.active_chat_id() else {
        return LoopControl::Continue;
    };
    match key.code {
        KeyCode::Enter => {
            let line = {
                let ui = state.chat_ui.entry(chat_id.clone()).or_default();
                let line = ui.input.trim().to_string();
                ui.input.clear();
                line
            };
            if line.is_empty() {
                return LoopControl::Continue;
            }
            if line == "exit" || line == "quit" {
                return LoopControl::Break;
            }
            let Some(chat_index) = state.chat_index(&chat_id) else {
                return LoopControl::Continue;
            };
            // Отправлять некуда, пока список чатов не получен от сервиса:
            // обмену негде записаться.
            if let Some(reason) = state.blocked_reason() {
                state.notify(format!("Сообщение не отправлено: {reason}"));
                return LoopControl::Continue;
            }
            // Неполная история испортила бы контекст запроса к модели.
            if !state.chats[chat_index].history_loaded {
                ensure_history(state, &chat_id, tx);
                state.notify("История чата ещё загружается: сообщение не отправлено");
                return LoopControl::Continue;
            }
            state.chats[chat_index].messages.push(Message::user(line.clone()));
            state.chats[chat_index].touch_quietly();
            // Заголовок нового чата придумывает сервис после первого обмена
            // (AGENTD_AUTO_TITLE): клиент больше не подставляет свой,
            // иначе он гарантированно перебивал бы серверную генерацию,
            // отправляясь раньше, чем сервис успевал ответить.
            {
                let ui = state.chat_ui.entry(chat_id.clone()).or_default();
                ui.pending = true;
                ui.pending_since = Some(Instant::now());
                ui.auto_scroll = true;
                ui.scroll_to_message = None;
            }

            let agent = agent.clone();
            let hist = state.chats[chat_index].messages.clone();
            let settings = state.chats[chat_index].settings.clone();
            let tx_response = tx.clone();
            let event_chat_id = chat_id.clone();
            let request_chat_id = chat_id.clone();
            tokio::spawn(async move {
                let result = agent
                    .ask_in_chat(&request_chat_id, &line, &hist, &settings)
                    .await;
                let _ = tx_response.send(ChatEvent::Response(event_chat_id, result));
            });
        }
        KeyCode::Esc => return LoopControl::Break,
        KeyCode::Char(c) => {
            state.chat_ui.entry(chat_id).or_default().input.push(c);
        }
        KeyCode::Backspace => {
            state.chat_ui.entry(chat_id).or_default().input.pop();
        }
        // Стрелки историю не листают: построчная прокрутка ушла на колесо
        // мыши, а ↑/↓ в поле ввода нужны под перемещение по сообщениям.
        KeyCode::PageUp => {
            let ui = state.chat_ui.entry(chat_id).or_default();
            ui.scroll = ui.scroll.saturating_sub(10);
            ui.auto_scroll = false;
        }
        KeyCode::PageDown => {
            let ui = state.chat_ui.entry(chat_id).or_default();
            ui.scroll = ui.scroll.saturating_add(10).min(ui.max_scroll);
            ui.auto_scroll = ui.scroll >= ui.max_scroll;
        }
        _ => {}
    }
    LoopControl::Continue
}

/// Сколько строк истории проматывает один щелчок колеса.
const MOUSE_SCROLL_STEP: u16 = 3;

/// Чат, чья история накрывает точку под курсором.
fn chat_at_position(areas: &[(String, Rect)], column: u16, row: u16) -> Option<&str> {
    areas
        .iter()
        .find(|(_, area)| {
            column >= area.x
                && column < area.x.saturating_add(area.width)
                && row >= area.y
                && row < area.y.saturating_add(area.height)
        })
        .map(|(chat_id, _)| chat_id.as_str())
}

/// Прокрутка истории под курсором на `delta` строк.
///
/// Колесо листает ту панель, на которую пользователь смотрит, а не активную:
/// при двух открытых чатах это разные вещи. Пока открыто модальное окно,
/// колесо историю не трогает — она в этот момент перекрыта.
fn scroll_history_at(state: &mut AppState, column: u16, row: u16, delta: i32) {
    if !matches!(
        state.focus,
        Focus::Input | Focus::Sidebar | Focus::MessageSelect
    ) {
        return;
    }
    let Some(chat_id) = chat_at_position(&state.history_areas, column, row).map(str::to_string)
    else {
        return;
    };
    let ui = state.chat_ui.entry(chat_id).or_default();
    if delta < 0 {
        ui.scroll = ui.scroll.saturating_sub(delta.unsigned_abs() as u16);
        // Отмотали вверх — новые ответы больше не утаскивают историю вниз,
        // пока пользователь сам не вернётся к последней строке.
        ui.auto_scroll = false;
    } else {
        ui.scroll = ui.scroll.saturating_add(delta as u16).min(ui.max_scroll);
        ui.auto_scroll = ui.scroll >= ui.max_scroll;
    }
}

/// Смещение выбора по истории с упором в границы: на краях выбор остаётся на
/// первом или последнем сообщении, а не перескакивает по кругу.
fn shift_selection(current: usize, len: usize, down: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if down {
        (current + 1).min(len - 1)
    } else {
        current.saturating_sub(1)
    }
}

/// Выбор, приведённый к текущей длине истории: после перезагрузки чата
/// (например, при переключении ветки) прежний индекс может указывать за конец
/// списка, а у пустого чата выбирать нечего.
fn normalize_selection(selected: Option<usize>, len: usize) -> Option<usize> {
    let selected = selected?;
    if len == 0 {
        None
    } else {
        Some(selected.min(len - 1))
    }
}

/// Копирование текста выбранного сообщения. В буфер уходит `Message.content`
/// из модели: без заголовка роли, метрик токенов, рамок и переносов по ширине
/// панели, которые существуют только в отрисовке. `Message.reasoning` не
/// добавляется независимо от Ctrl+R — он управляет только показом.
fn copy_selected_message(state: &mut AppState, chat_id: &str, selected: usize) {
    let text = state
        .chat_index(chat_id)
        .and_then(|index| state.chats[index].messages.get(selected))
        .map(|message| message.content.clone());
    let Some(text) = text else {
        state.notify("Сообщение не найдено — копировать нечего");
        return;
    };
    match crate::clipboard::copy(&text) {
        Ok(()) => state.notify("Текст сообщения скопирован в буфер обмена"),
        Err(err) => state.notify(format!("Не удалось скопировать: {err}")),
    }
}

/// Клавиши режима выбора сообщения. `Esc` здесь перехватывается раньше ветки
/// `Focus::Input`, где он завершает TUI, поэтому выход из режима работу не
/// прекращает.
fn handle_message_select_key(key: crossterm::event::KeyEvent, state: &mut AppState) -> LoopControl {
    let Some(chat_id) = state.active_chat_id() else {
        state.focus = Focus::Input;
        return LoopControl::Continue;
    };
    let len = state
        .chat_index(&chat_id)
        .map(|index| state.chats[index].messages.len())
        .unwrap_or(0);
    let selected = normalize_selection(
        state.chat_ui.get(&chat_id).and_then(|ui| ui.selected_message),
        len,
    );
    // История опустела, пока режим был открыт: выбирать нечего.
    let Some(selected) = selected else {
        state.chat_ui.entry(chat_id).or_default().selected_message = None;
        state.focus = Focus::Input;
        return LoopControl::Continue;
    };
    match key.code {
        KeyCode::Up | KeyCode::Down => {
            let next = shift_selection(selected, len, key.code == KeyCode::Down);
            let ui = state.chat_ui.entry(chat_id).or_default();
            ui.selected_message = Some(next);
            // выбранное доводится в видимую область тем же механизмом, что и
            // новый ответ модели
            ui.auto_scroll = false;
            ui.scroll_to_message = Some(next);
        }
        KeyCode::Enter => {
            copy_selected_message(state, &chat_id, selected);
            state.chat_ui.entry(chat_id).or_default().selected_message = None;
            state.focus = Focus::Input;
        }
        KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            copy_selected_message(state, &chat_id, selected);
        }
        KeyCode::Esc => {
            state.chat_ui.entry(chat_id).or_default().selected_message = None;
            state.focus = Focus::Input;
        }
        _ => {}
    }
    LoopControl::Continue
}

/// Клиент чатов по текущему конфигу: адрес сервиса и клиентский токен те же,
/// что у облачных запросов.
pub(crate) fn chats_client(config: &Config) -> ChatsClient {
    ChatsClient::new(
        config.effective_server_url(),
        config.client_token(),
        crate::logging::exchange_log(),
    )
    .with_unauthorized_hint(crate::logging::UNAUTHORIZED_HINT)
}

/// Текст причины отказа сервиса для баннера и уведомлений: с адресом
/// сервиса и идентификатором запроса, если сервис его вернул.
fn failure_text(err: &anyhow::Error) -> String {
    format!("{err:#}")
}

/// Фоновый запрос списка чатов.
fn fetch_chats(state: &AppState, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = client.list().await.map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::ChatsLoaded(result));
    });
}

/// Заголовок, который сервис ставит новому чату и меняет один раз после
/// первого обмена (`AGENTD_AUTO_TITLE`) — клиент это название сам не
/// подбирает, только подтягивает его у сервиса.
const DEFAULT_CHAT_TITLE: &str = "Новый чат";

/// Отложенный запрос списка чатов: подтягивает заголовок, который сервис
/// придумывает в фоне после первого обмена. Задержка даёт серверному
/// вызову модели время завершиться — обновлённого списка сразу после
/// ответа ещё не будет.
fn schedule_title_refresh(state: &AppState, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let result = client.list().await.map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::ChatsLoaded(result));
    });
}

/// Фоновый запрос истории чата. Вызывается при первом открытии чата: список
/// отдаёт только заголовки.
fn fetch_history(state: &mut AppState, chat_id: &str, tx: &mpsc::UnboundedSender<ChatEvent>) {
    {
        let ui = state.chat_ui.entry(chat_id.to_string()).or_default();
        if ui.history_loading {
            return;
        }
        ui.history_loading = true;
        ui.history_error = None;
    }
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    tokio::spawn(async move {
        let result = client.load(&id).await.map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::HistoryLoaded(id, result));
    });
}

/// История нужна перед отправкой реплики и перед переносом контекста.
fn ensure_history(state: &mut AppState, chat_id: &str, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let loaded = state
        .chat_index(chat_id)
        .map(|index| state.chats[index].history_loaded)
        .unwrap_or(false);
    if !loaded {
        fetch_history(state, chat_id, tx);
    }
}

/// Создание чата запросом к сервису: идентификатор назначает он.
fn request_create_chat(state: &mut AppState, tx: &mpsc::UnboundedSender<ChatEvent>) {
    if state.creating_chat {
        return;
    }
    state.creating_chat = true;
    let client = state.chats_client.clone();
    let settings = state.config.default_chat_settings();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = client
            .create(None, &settings)
            .await
            .map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::ChatCreated(result));
    });
}

/// Переименование или изменение параметров чата. Локально изменение не
/// применяется до подтверждения: список должен оставаться в том состоянии,
/// которое подтвердил сервис (specs/client-chat-storage).
fn request_update_chat(
    state: &AppState,
    chat_id: &str,
    title: Option<String>,
    settings: Option<ChatSettings>,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    tokio::spawn(async move {
        let result = client
            .update(&id, title.as_deref(), settings.as_ref())
            .await
            .map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::ChatUpdated(id, result));
    });
}

fn request_delete_chat(state: &AppState, chat_id: &str, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    tokio::spawn(async move {
        let result = client.delete(&id).await.map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::ChatDeleted(id, result));
    });
}

/// Дозапись обмена локального чата: вопрос и ответ уходят одним запросом,
/// иначе в чате мог бы остаться вопрос без ответа.
fn request_append_exchange(
    state: &AppState,
    chat_id: &str,
    messages: Vec<Message>,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    tokio::spawn(async move {
        let result = client
            .append(&id, &messages)
            .await
            .map(|_| ())
            .map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::ExchangeSaved(id, result));
    });
}

/// Факты чата (`GET /v1/chats/{id}/facts`).
fn request_facts(state: &AppState, chat_id: &str, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    tokio::spawn(async move {
        let result = client.facts(&id).await.map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::FactsLoaded(id, result));
    });
}

fn request_set_fact(state: &AppState, chat_id: &str, key: &str, value: &str, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    let key = key.to_string();
    let value = value.to_string();
    tokio::spawn(async move {
        let result = client.set_fact(&id, &key, &value).await.map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::FactSet(id, result));
    });
}

fn request_delete_fact(state: &AppState, chat_id: &str, key: &str, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    let key = key.to_string();
    tokio::spawn(async move {
        let result = client
            .delete_fact(&id, &key)
            .await
            .map(|()| key.clone())
            .map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::FactDeleted(id, result));
    });
}

/// Ветки чата (`GET /v1/chats/{id}/branches`).
fn request_branches(state: &AppState, chat_id: &str, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    tokio::spawn(async move {
        let result = client.branches(&id).await.map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::BranchesLoaded(id, result));
    });
}

/// Сообщения чата как источник точек ветвления — отдельным запросом, а не
/// из уже загрученной `ChatSession`: там нет `seq`, нужного для выбора
/// точки ветвления (specs/chat-branching, «Ветка создаётся от выбранного
/// сообщения»).
fn request_branch_source_messages(state: &AppState, chat_id: &str, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    tokio::spawn(async move {
        let result = client
            .load(&id)
            .await
            .map(|history| history.messages)
            .map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::BranchSourceMessagesLoaded(id, result));
    });
}

fn request_create_branch(state: &AppState, chat_id: &str, from_seq: i64, name: &str, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    let name = name.to_string();
    tokio::spawn(async move {
        let result = client.create_branch(&id, from_seq, &name).await.map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::BranchCreated(id, result));
    });
}

fn request_activate_branch(state: &AppState, chat_id: &str, branch_id: &str, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    let branch_id = branch_id.to_string();
    tokio::spawn(async move {
        let result = client
            .activate_branch(&id, &branch_id)
            .await
            .map(|()| branch_id.clone())
            .map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::BranchActivated(id, result));
    });
}

// --- Память (specs/memory-layers) ---

fn request_working_memory(state: &AppState, chat_id: &str, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    tokio::spawn(async move {
        let result = client.working_memory(&id).await.map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::WorkingMemoryLoaded(id, result));
    });
}

fn request_set_working_memory(
    state: &AppState,
    chat_id: &str,
    key: &str,
    value: &str,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    let key = key.to_string();
    let value = value.to_string();
    tokio::spawn(async move {
        let result = client.set_working_memory(&id, &key, &value).await.map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::WorkingMemorySet(id, result));
    });
}

fn request_delete_working_memory(state: &AppState, chat_id: &str, key: &str, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    let key = key.to_string();
    tokio::spawn(async move {
        let result = client
            .delete_working_memory(&id, &key)
            .await
            .map(|()| key.clone())
            .map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::WorkingMemoryDeleted(id, result));
    });
}

fn request_finish_task(state: &AppState, chat_id: &str, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    tokio::spawn(async move {
        // Ручное завершение задачи из TUI переносит все записи текущей
        // рабочей памяти без выборочного отбора: точечный выбор ключей для
        // переноса остаётся полем маршрутизатора и HTTP-клиентов сервиса.
        let result = client.finish_task(&id, &[]).await.map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::TaskFinished(id, result));
    });
}

fn request_long_term_memory(state: &AppState, chat_id: &str, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    tokio::spawn(async move {
        let result = client.long_term_memory().await.map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::LongTermMemoryLoaded(id, result));
    });
}

fn request_set_long_term_memory(
    state: &AppState,
    chat_id: &str,
    entry_type: &str,
    key: &str,
    value: &str,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    let entry_type = entry_type.to_string();
    let key = key.to_string();
    let value = value.to_string();
    tokio::spawn(async move {
        let key_arg = if key.is_empty() { None } else { Some(key.as_str()) };
        let result = client.set_long_term_memory(&entry_type, key_arg, &value).await.map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::LongTermMemorySet(id, result));
    });
}

fn request_delete_long_term_memory(
    state: &AppState,
    chat_id: &str,
    entry_id: &str,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    let entry_id = entry_id.to_string();
    tokio::spawn(async move {
        let result = client
            .delete_long_term_memory(&entry_id)
            .await
            .map(|()| entry_id.clone())
            .map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::LongTermMemoryDeleted(id, result));
    });
}

/// Состояние задачи чата вместе с журналом переходов (specs/task-state).
fn request_task(state: &AppState, chat_id: &str, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    tokio::spawn(async move {
        let result = client.task(&id).await.map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::TaskLoaded(id, result));
    });
}

#[allow(clippy::too_many_arguments)]
fn request_task_transition(
    state: &AppState,
    chat_id: &str,
    stage: Option<&str>,
    step: Option<&str>,
    expected_action: Option<&str>,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    let stage = stage.map(str::to_string);
    let step = step.map(str::to_string);
    let expected_action = expected_action.map(str::to_string);
    tokio::spawn(async move {
        let result = client
            .task_transition(&id, stage.as_deref(), step.as_deref(), expected_action.as_deref(), &[])
            .await
            .map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::TaskTransitioned(id, result));
    });
}

fn request_task_pause(state: &AppState, chat_id: &str, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    tokio::spawn(async move {
        let result = client.task_pause(&id).await.map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::TaskPaused(id, result));
    });
}

fn request_task_resume(state: &AppState, chat_id: &str, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let client = state.chats_client.clone();
    let tx = tx.clone();
    let id = chat_id.to_string();
    tokio::spawn(async move {
        let result = client.task_resume(&id).await.map_err(|err| failure_text(&err));
        let _ = tx.send(ChatEvent::TaskResumed(id, result));
    });
}

/// Фоновый запрос списка локальных моделей Ollama.
fn fetch_ollama_models(config: &Config, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let url = config.effective_ollama_url();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = agentcore::agent::list_ollama_models(&url)
            .await
            .map_err(|err| err.to_string());
        let _ = tx.send(ChatEvent::OllamaModels(result));
    });
}

/// Фоновый запрос списка облачных моделей у сервиса (`GET /v1/models`).
/// Список принадлежит сервису: клиенту нельзя было слать ключ провайдера
/// сам, поэтому единственный способ узнать разрешённые модели — спросить
/// сервис через `agentclient`, тот же клиент, что использует чат.
fn fetch_cloud_models(config: &Config, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let server_url = config.effective_server_url();
    let token = config.client_token();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = agentclient::list_models(&server_url, &token)
            .await
            .map_err(|err| err.to_string());
        let _ = tx.send(ChatEvent::CloudModels(result));
    });
}

/// Фоновый запрос списка профилей владельца у сервиса (`GET /v1/profiles`,
/// specs/user-profiles) — для перебора стрелками в поле «Профиль».
fn fetch_profiles(config: &Config, tx: &mpsc::UnboundedSender<ChatEvent>) {
    let server_url = config.effective_server_url();
    let token = config.client_token();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = agentclient::list_profiles(&server_url, &token)
            .await
            .map_err(|err| err.to_string());
        let _ = tx.send(ChatEvent::Profiles(result));
    });
}

fn handle_chat_event(
    chat_event: ChatEvent,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) {
    let chat_event = match chat_event {
        ChatEvent::OllamaModels(result) => {
            handle_ollama_models(result, state);
            return;
        }
        ChatEvent::CloudModels(result) => {
            handle_cloud_models(result, state);
            return;
        }
        ChatEvent::Profiles(result) => {
            handle_profiles(result, state);
            return;
        }
        ChatEvent::ChatsLoaded(result) => {
            handle_chats_loaded(result, state, tx);
            return;
        }
        ChatEvent::HistoryLoaded(chat_id, result) => {
            handle_history_loaded(chat_id, result, state);
            return;
        }
        ChatEvent::ChatCreated(result) => {
            handle_chat_created(result, state);
            return;
        }
        ChatEvent::ChatUpdated(chat_id, result) => {
            handle_chat_updated(chat_id, result, state);
            return;
        }
        ChatEvent::ChatDeleted(chat_id, result) => {
            handle_chat_deleted(chat_id, result, state);
            return;
        }
        ChatEvent::ExchangeSaved(chat_id, result) => {
            handle_exchange_saved(chat_id, result, state);
            return;
        }
        ChatEvent::FactsLoaded(chat_id, result) => {
            handle_facts_loaded(chat_id, result, state);
            return;
        }
        ChatEvent::FactSet(chat_id, result) => {
            handle_fact_set(chat_id, result, state);
            return;
        }
        ChatEvent::FactDeleted(chat_id, result) => {
            handle_fact_deleted(chat_id, result, state);
            return;
        }
        ChatEvent::BranchesLoaded(chat_id, result) => {
            handle_branches_loaded(chat_id, result, state);
            return;
        }
        ChatEvent::BranchSourceMessagesLoaded(chat_id, result) => {
            handle_branch_source_messages_loaded(chat_id, result, state);
            return;
        }
        ChatEvent::BranchCreated(chat_id, result) => {
            handle_branch_created(chat_id, result, state, tx);
            return;
        }
        ChatEvent::BranchActivated(chat_id, result) => {
            handle_branch_activated(chat_id, result, state, tx);
            return;
        }
        ChatEvent::WorkingMemoryLoaded(chat_id, result) => {
            handle_working_memory_loaded(chat_id, result, state);
            return;
        }
        ChatEvent::WorkingMemorySet(chat_id, result) => {
            handle_working_memory_set(chat_id, result, state);
            return;
        }
        ChatEvent::WorkingMemoryDeleted(chat_id, result) => {
            handle_working_memory_deleted(chat_id, result, state);
            return;
        }
        ChatEvent::TaskFinished(chat_id, result) => {
            handle_task_finished(chat_id, result, state, tx);
            return;
        }
        ChatEvent::LongTermMemoryLoaded(chat_id, result) => {
            handle_long_term_memory_loaded(chat_id, result, state);
            return;
        }
        ChatEvent::LongTermMemorySet(chat_id, result) => {
            handle_long_term_memory_set(chat_id, result, state);
            return;
        }
        ChatEvent::LongTermMemoryDeleted(chat_id, result) => {
            handle_long_term_memory_deleted(chat_id, result, state);
            return;
        }
        ChatEvent::TaskLoaded(chat_id, result) => {
            handle_task_loaded(chat_id, result, state);
            return;
        }
        ChatEvent::TaskTransitioned(chat_id, result) => {
            handle_task_transitioned(chat_id, result, state);
            return;
        }
        ChatEvent::TaskPaused(chat_id, result) => {
            handle_task_transitioned(chat_id, result, state);
            return;
        }
        ChatEvent::TaskResumed(chat_id, result) => {
            handle_task_transitioned(chat_id, result, state);
            return;
        }
        other => other,
    };
    let ChatEvent::Response(chat_id, result) = chat_event else {
        return;
    };
    let failed = result.is_err();
    let mut reply_context = None;
    let mut message = match result {
        Ok(reply) => {
            reply_context = reply.context;
            let mut message = Message::assistant(reply.content);
            message.reasoning = reply.reasoning;
            message.meta = Some(reply.meta);
            message
        }
        Err(err) => Message::assistant(format!("Ошибка: {err}")),
    };
    let Some(chat_index) = state.chats.iter().position(|c| c.id == chat_id) else {
        return;
    };
    if let Some(context) = reply_context {
        state.chat_ui.entry(chat_id.clone()).or_default().last_context = Some(context);
    }
    // у ошибки телеметрии нет — оставляем хотя бы время получения
    if message.meta.is_none() {
        message.meta = Some(MessageMeta {
            received_at: Some(agentcore::agent::now_secs()),
            ..MessageMeta::default()
        });
    }
    state.chats[chat_index].messages.push(message.clone());
    state.chats[chat_index].touch_quietly();
    // Первый обмен чата с заголовком по умолчанию: сервис после него сам
    // придумывает название (title.rs), но фоном — список чатов, полученный
    // прямо сейчас, его ещё не знает. Подтягиваем список ещё раз спустя
    // паузу, чтобы название появилось без ручного обновления.
    if !failed
        && state.chats[chat_index].title == DEFAULT_CHAT_TITLE
        && state.chats[chat_index].messages.len() == 2
    {
        schedule_title_refresh(&state, tx);
    }
    // Обмен облачного чата записал сам сервис (запрос шёл с `chat_id`), а
    // обмен локального записывает клиент: ответ дала модель на машине
    // пользователя. Неудачный запрос не записывается: текст ошибки — не
    // реплика модели.
    if !failed && state.chats[chat_index].settings.provider == Provider::Ollama {
        let history = &state.chats[chat_index].messages;
        let exchange: Vec<Message> = history
            .iter()
            .rev()
            .take(2)
            .rev()
            .cloned()
            .collect();
        state.chat_ui.entry(chat_id.clone()).or_default().unsaved = Some(UnsavedExchange {
            reason: "запись обмена в сервис ещё не подтверждена".to_string(),
            messages: exchange.clone(),
        });
        request_append_exchange(state, &chat_id, exchange, tx);
    }
    let last_index = state.chats[chat_index].messages.len() - 1;
    let ui = state.chat_ui.entry(chat_id).or_default();
    ui.pending = false;
    ui.pending_since = None;
    ui.auto_scroll = false;
    ui.scroll_to_message = Some(last_index);
}

/// Список чатов от сервиса заменяет прежний. Панели, ссылающиеся на
/// исчезнувшие чаты, закрываются, а первый чат открывается, если панелей
/// не осталось.
fn handle_chats_loaded(
    result: Result<Vec<ChatSummary>, String>,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) {
    match result {
        Ok(summaries) => {
            // Состояние ввода и прокрутки переживает перезагрузку списка:
            // оно привязано к чату, а не к его месту в списке.
            let mut chats: Vec<ChatSession> = Vec::with_capacity(summaries.len());
            for summary in summaries {
                let id = summary.id.clone();
                let mut chat = ChatSession::from_summary(summary);
                // Уже загруженную историю не выбрасываем: список её не
                // содержит, а повторный запрос ни к чему.
                if let Some(existing) = state.chats.iter().find(|c| c.id == id)
                    && existing.history_loaded {
                        chat.messages = existing.messages.clone();
                        chat.history_loaded = true;
                    }
                chats.push(chat);
            }
            let known: Vec<String> = chats.iter().map(|chat| chat.id.clone()).collect();
            state.chats = chats;
            state.chats_load = ChatsLoad::Loaded;
            state.chat_ui.retain(|id, _| known.contains(id));
            for id in &known {
                state.chat_ui.entry(id.clone()).or_default();
            }
            state.panes.retain(|id| known.contains(id));
            if state.panes.is_empty()
                && let Some(first) = known.first() {
                    state.panes.push(first.clone());
                }
            if state.active_pane >= state.panes.len() {
                state.active_pane = state.panes.len().saturating_sub(1);
            }
            if state.sidebar_selected >= state.chats.len() {
                state.sidebar_selected = state.chats.len().saturating_sub(1);
            }
            let open: Vec<String> = state.panes.clone();
            for id in open {
                ensure_history(state, &id, tx);
            }
        }
        Err(reason) => {
            let address = state.config.effective_server_url();
            state.chats_load = ChatsLoad::Failed(format!("{reason} (сервис: {address})"));
            state.chats.clear();
            state.panes.clear();
            state.active_pane = 0;
            state.sidebar_selected = 0;
        }
    }
}

fn handle_history_loaded(chat_id: String, result: Result<ChatHistory, String>, state: &mut AppState) {
    match result {
        Ok(history) => {
            if let Some(index) = state.chat_index(&chat_id) {
                state.chats[index].apply_history(history);
            }
            let len = state
                .chat_index(&chat_id)
                .map(|index| state.chats[index].messages.len())
                .unwrap_or(0);
            let ui = state.chat_ui.entry(chat_id).or_default();
            ui.history_loading = false;
            ui.history_error = None;
            ui.auto_scroll = true;
            // Перезагруженная история могла стать короче: прежний индекс
            // выбора указывал бы мимо сообщения.
            ui.selected_message = normalize_selection(ui.selected_message, len);
        }
        Err(reason) => {
            let ui = state.chat_ui.entry(chat_id).or_default();
            ui.history_loading = false;
            ui.history_error = Some(reason.clone());
            state.notify(format!("История чата не загружена: {reason}"));
        }
    }
}

fn handle_chat_created(result: Result<ChatSummary, String>, state: &mut AppState) {
    state.creating_chat = false;
    match result {
        Ok(summary) => {
            let id = summary.id.clone();
            state.chats.insert(0, ChatSession::from_summary(summary));
            state.chat_ui.insert(id.clone(), ChatUi::default());
            state.show_in_active_pane(id);
            state.sidebar_selected = 0;
            state.focus = Focus::Input;
        }
        Err(reason) => state.notify(format!("Чат не создан: {reason}")),
    }
}

fn handle_chat_updated(chat_id: String, result: Result<ChatSummary, String>, state: &mut AppState) {
    match result {
        Ok(summary) => {
            if let Some(index) = state.chat_index(&chat_id) {
                let chat = &mut state.chats[index];
                chat.title = summary.title;
                chat.settings = summary.settings;
                chat.updated_at = summary.updated_at.max(0) as u64;
            }
        }
        Err(reason) => state.notify(format!("Изменение чата не сохранено: {reason}")),
    }
}

fn handle_chat_deleted(chat_id: String, result: Result<(), String>, state: &mut AppState) {
    match result {
        Ok(()) => forget_chat(state, &chat_id),
        Err(reason) => state.notify(format!("Чат не удалён: {reason}")),
    }
}

fn handle_exchange_saved(chat_id: String, result: Result<(), String>, state: &mut AppState) {
    match result {
        Ok(()) => {
            let ui = state.chat_ui.entry(chat_id).or_default();
            ui.unsaved = None;
        }
        Err(reason) => {
            state.notify(format!(
                "Обмен не сохранён в сервисе: {reason}. Ctrl+U — повторить"
            ));
            if let Some(ui) = state.chat_ui.get_mut(&chat_id)
                && let Some(unsaved) = ui.unsaved.as_mut() {
                    unsaved.reason = reason;
                }
        }
    }
}

fn handle_facts_loaded(chat_id: String, result: Result<Vec<Fact>, String>, state: &mut AppState) {
    let Some(picker) = state.facts.as_mut() else { return };
    if picker.chat_id != chat_id {
        return;
    }
    picker.loading = false;
    match result {
        Ok(facts) => {
            picker.facts = facts;
            picker.cursor = picker.cursor.min(picker.facts.len().saturating_sub(1));
            picker.error = None;
        }
        Err(reason) => picker.error = Some(reason),
    }
}

fn handle_fact_set(chat_id: String, result: Result<Fact, String>, state: &mut AppState) {
    let Some(picker) = state.facts.as_mut() else { return };
    if picker.chat_id != chat_id {
        return;
    }
    match result {
        Ok(fact) => {
            picker.editor = None;
            if let Some(existing) = picker.facts.iter_mut().find(|f| f.key == fact.key) {
                *existing = fact;
            } else {
                picker.facts.push(fact);
                picker.facts.sort_by(|a, b| a.key.cmp(&b.key));
            }
        }
        Err(reason) => state.notify(format!("Не удалось сохранить факт: {reason}")),
    }
}

fn handle_fact_deleted(chat_id: String, result: Result<String, String>, state: &mut AppState) {
    let Some(picker) = state.facts.as_mut() else { return };
    if picker.chat_id != chat_id {
        return;
    }
    match result {
        Ok(key) => {
            picker.facts.retain(|f| f.key != key);
            picker.cursor = picker.cursor.min(picker.facts.len().saturating_sub(1));
        }
        Err(reason) => state.notify(format!("Не удалось удалить факт: {reason}")),
    }
}

fn handle_branches_loaded(chat_id: String, result: Result<Vec<Branch>, String>, state: &mut AppState) {
    let Some(picker) = state.branches.as_mut() else { return };
    if picker.chat_id != chat_id {
        return;
    }
    picker.loading = false;
    match result {
        Ok(branches) => {
            picker.branch_cursor = picker.branch_cursor.min(branches.len().saturating_sub(1));
            picker.branches = branches;
            picker.error = None;
        }
        Err(reason) => picker.error = Some(reason),
    }
}

fn handle_branch_source_messages_loaded(
    chat_id: String,
    result: Result<Vec<StoredMessage>, String>,
    state: &mut AppState,
) {
    let Some(picker) = state.branches.as_mut() else { return };
    if picker.chat_id != chat_id {
        return;
    }
    let Some(creation) = picker.creating.as_mut() else { return };
    creation.loading = false;
    match result {
        Ok(messages) => {
            creation.message_cursor = messages.len().saturating_sub(1);
            creation.messages = messages;
        }
        Err(reason) => {
            picker.creating = None;
            picker.error = Some(reason);
        }
    }
}

fn handle_branch_created(
    chat_id: String,
    result: Result<Branch, String>,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) {
    let Some(picker) = state.branches.as_mut() else { return };
    if picker.chat_id != chat_id {
        return;
    }
    picker.creating = None;
    match result {
        Ok(_) => request_branches(state, &chat_id, tx),
        Err(reason) => state.notify(format!("Не удалось создать ветку: {reason}")),
    }
}

fn handle_branch_activated(
    chat_id: String,
    result: Result<String, String>,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) {
    match result {
        Ok(_) => {
            if let Some(picker) = state.branches.as_ref()
                && picker.chat_id == chat_id
            {
                request_branches(state, &chat_id, tx);
            }
            // Активная ветка сменилась: перечитываем историю чата.
            if let Some(ui) = state.chat_ui.get_mut(&chat_id) {
                ui.history_loading = false;
            }
            if let Some(index) = state.chat_index(&chat_id) {
                state.chats[index].history_loaded = false;
            }
            fetch_history(state, &chat_id, tx);
        }
        Err(reason) => state.notify(format!("Не удалось переключить ветку: {reason}")),
    }
}

// --- Память (specs/memory-layers) ---

fn handle_working_memory_loaded(chat_id: String, result: Result<Vec<WorkingMemoryEntry>, String>, state: &mut AppState) {
    let Some(picker) = state.memory.as_mut() else { return };
    if picker.chat_id != chat_id {
        return;
    }
    picker.working_loading = false;
    match result {
        Ok(entries) => {
            picker.working_cursor = picker.working_cursor.min(entries.len().saturating_sub(1));
            picker.working = entries;
            picker.working_error = None;
        }
        Err(reason) => picker.working_error = Some(reason),
    }
}

fn handle_working_memory_set(chat_id: String, result: Result<WorkingMemoryEntry, String>, state: &mut AppState) {
    let Some(picker) = state.memory.as_mut() else { return };
    if picker.chat_id != chat_id {
        return;
    }
    match result {
        Ok(entry) => {
            picker.editor = None;
            if let Some(existing) = picker.working.iter_mut().find(|e| e.key == entry.key) {
                *existing = entry;
            } else {
                picker.working.push(entry);
                picker.working.sort_by(|a, b| a.key.cmp(&b.key));
            }
        }
        Err(reason) => state.notify(format!("Не удалось сохранить запись рабочей памяти: {reason}")),
    }
}

fn handle_working_memory_deleted(chat_id: String, result: Result<String, String>, state: &mut AppState) {
    let Some(picker) = state.memory.as_mut() else { return };
    if picker.chat_id != chat_id {
        return;
    }
    match result {
        Ok(key) => {
            picker.working.retain(|e| e.key != key);
            picker.working_cursor = picker.working_cursor.min(picker.working.len().saturating_sub(1));
        }
        Err(reason) => state.notify(format!("Не удалось удалить запись рабочей памяти: {reason}")),
    }
}

fn handle_task_finished(
    chat_id: String,
    result: Result<Vec<LongTermMemoryEntry>, String>,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) {
    match result {
        Ok(transferred) => {
            state.notify(format!("Задача завершена, перенесено записей: {}", transferred.len()));
            if let Some(picker) = state.memory.as_ref()
                && picker.chat_id == chat_id
            {
                request_working_memory(state, &chat_id, tx);
                request_long_term_memory(state, &chat_id, tx);
            }
        }
        Err(reason) => state.notify(format!("Не удалось завершить задачу: {reason}")),
    }
}

fn handle_long_term_memory_loaded(chat_id: String, result: Result<Vec<LongTermMemoryEntry>, String>, state: &mut AppState) {
    let Some(picker) = state.memory.as_mut() else { return };
    if picker.chat_id != chat_id {
        return;
    }
    picker.long_term_loading = false;
    match result {
        Ok(entries) => {
            picker.long_term_cursor = picker.long_term_cursor.min(entries.len().saturating_sub(1));
            picker.long_term = entries;
            picker.long_term_error = None;
        }
        Err(reason) => picker.long_term_error = Some(reason),
    }
}

fn handle_long_term_memory_set(chat_id: String, result: Result<LongTermMemoryEntry, String>, state: &mut AppState) {
    let Some(picker) = state.memory.as_mut() else { return };
    if picker.chat_id != chat_id {
        return;
    }
    match result {
        Ok(entry) => {
            picker.editor = None;
            if let Some(existing) = picker.long_term.iter_mut().find(|e| e.id == entry.id) {
                *existing = entry;
            } else {
                picker.long_term.push(entry);
            }
        }
        Err(reason) => state.notify(format!("Не удалось сохранить запись долговременной памяти: {reason}")),
    }
}

fn handle_long_term_memory_deleted(chat_id: String, result: Result<String, String>, state: &mut AppState) {
    let Some(picker) = state.memory.as_mut() else { return };
    if picker.chat_id != chat_id {
        return;
    }
    match result {
        Ok(id) => {
            picker.long_term.retain(|e| e.id != id);
            picker.long_term_cursor = picker.long_term_cursor.min(picker.long_term.len().saturating_sub(1));
        }
        Err(reason) => state.notify(format!("Не удалось удалить запись долговременной памяти: {reason}")),
    }
}

fn handle_task_loaded(
    chat_id: String,
    result: Result<agentclient::TaskState, String>,
    state: &mut AppState,
) {
    let Some(picker) = state.task.as_mut() else { return };
    if picker.chat_id != chat_id {
        return;
    }
    picker.loading = false;
    match result {
        Ok(task) => {
            picker.state = Some(task);
            picker.next_stage_cursor = 0;
            picker.error = None;
        }
        Err(reason) => picker.error = Some(reason),
    }
}

/// Общий обработчик для перехода, паузы и возобновления: во всех трёх
/// случаях сервис возвращает свежее состояние задачи, а экран просто его
/// подставляет и закрывает открытую правку текста.
fn handle_task_transitioned(
    chat_id: String,
    result: Result<agentclient::TaskState, String>,
    state: &mut AppState,
) {
    let Some(picker) = state.task.as_mut() else { return };
    if picker.chat_id != chat_id {
        return;
    }
    match result {
        Ok(task) => {
            picker.state = Some(task);
            picker.next_stage_cursor = 0;
            picker.editor = None;
            picker.error = None;
        }
        Err(reason) => state.notify(format!("Не удалось изменить состояние задачи: {reason}")),
    }
}

/// Обновить список локальных моделей в состоянии и в открытом редакторе
/// настроек. Ошибка не мешает работе: облачные чаты от неё не зависят.
fn handle_ollama_models(result: Result<Vec<String>, String>, state: &mut AppState) {
    match result {
        Ok(models) => {
            let count = models.len();
            state.ollama_models = models.clone();
            if let Some(editor) = state.settings.as_mut() {
                editor.ollama_models = models;
            }
            if state.focus == Focus::Settings {
                state.notify(format!("Ollama: найдено моделей — {count}"));
            }
        }
        Err(err) => {
            state.ollama_models.clear();
            if let Some(editor) = state.settings.as_mut() {
                editor.ollama_models.clear();
                editor.error = Some(err.clone());
            }
            if state.focus == Focus::Settings {
                state.notify(format!("Ollama недоступен: {err}"));
            }
        }
    }
}

/// Обновить список облачных моделей в состоянии и в открытом редакторе
/// настроек. В отличие от Ollama, при ошибке список НЕ очищаем: у облака
/// есть встроенный/конфигурный список-запасной вариант (у Ollama такого нет,
/// там пустой список — единственно честное состояние), и терять выбор модели
/// только из-за недоступности сервиса не нужно.
fn handle_cloud_models(result: Result<Vec<String>, String>, state: &mut AppState) {
    match result {
        Ok(models) if !models.is_empty() => {
            let count = models.len();
            state.model_choices = models.clone();
            if let Some(editor) = state.settings.as_mut() {
                editor.model_choices = models;
            }
            if state.focus == Focus::Settings {
                state.notify(format!("Сервис: найдено моделей — {count}"));
            }
        }
        Ok(_) => {
            if state.focus == Focus::Settings {
                state.notify("Сервис не сообщил ни одной модели, использую список из конфига");
            }
        }
        Err(err) => {
            if state.focus == Focus::Settings {
                state.notify(format!("Список моделей сервиса недоступен: {err}"));
            }
            if let Some(editor) = state.settings.as_mut() {
                editor.error = Some(err);
            }
        }
    }
}

/// Обновить список профилей в состоянии и в открытом редакторе настроек.
/// Как у облачных моделей: при ошибке список не очищаем — недоступность
/// сервиса не должна сбрасывать уже выбранный профиль (specs/user-profiles).
fn handle_profiles(result: Result<Vec<ProfileChoice>, String>, state: &mut AppState) {
    match result {
        Ok(profiles) => {
            let count = profiles.len();
            state.profile_choices = profiles.clone();
            if let Some(editor) = state.settings.as_mut() {
                editor.profile_choices = profiles;
            }
            if state.focus == Focus::Settings {
                state.notify(format!("Сервис: найдено профилей — {count}"));
            }
        }
        Err(err) => {
            if state.focus == Focus::Settings {
                state.notify(format!("Список профилей сервиса недоступен: {err}"));
            }
        }
    }
}

fn render_ui(f: &mut Frame, state: &mut AppState) {
    // Области истории пересобираются каждый кадр: панели открываются,
    // закрываются и меняют размер вместе с окном терминала.
    state.history_areas.clear();
    let outer = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(28), Constraint::Min(20)])
        .split(f.area());

    render_sidebar(f, state, outer[0]);

    let main = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(outer[1]);

    if state.panes.is_empty() {
        render_no_chat(f, state, main[0]);
    } else {
        let pane_count = state.panes.len() as u32;
        let pane_constraints: Vec<Constraint> =
            (0..pane_count).map(|_| Constraint::Ratio(1, pane_count)).collect();
        let pane_areas = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(pane_constraints)
            .split(main[0]);

        for (i, area) in pane_areas.iter().enumerate() {
            render_pane(f, state, i, *area);
        }
    }

    render_help(f, state, main[1]);

    if let Some(editor) = &state.settings {
        render_settings_popup(f, editor);
    }
    if let Some(picker) = &state.import {
        render_import_popup(f, picker);
    }
    if let Some(confirm) = &state.delete_confirm {
        render_delete_popup(f, confirm);
    }
    if let Some(picker) = &state.facts {
        render_facts_popup(f, picker);
    }
    if let Some(picker) = &state.branches {
        render_branches_popup(f, picker);
    }
    if let Some(picker) = &state.memory {
        let tail: Vec<Message> = state
            .chat_index(&picker.chat_id)
            .map(|index| {
                let messages = &state.chats[index].messages;
                messages.iter().rev().take(10).rev().cloned().collect()
            })
            .unwrap_or_default();
        render_memory_popup(f, picker, &tail);
    }
    if let Some(picker) = &state.task {
        render_task_popup(f, picker);
    }
}

/// Подтверждение удаления чата: удаление необратимо, поэтому спрашиваем явно.
fn render_delete_popup(f: &mut Frame, confirm: &DeleteConfirm) {
    let area = centered_rect(60, 7, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
        .title(" Удалить чат ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines = vec![
        Line::from(Span::styled(
            format!(" Удалить чат «{}»?", confirm.chat_title),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            " Чат и его история будут удалены в сервисе без возможности восстановления.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            " y / Enter — удалить · n / Esc — отмена",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), inner);
}

fn render_pane(f: &mut Frame, state: &mut AppState, pane_idx: usize, area: Rect) {
    let Some(chat_id) = state.panes.get(pane_idx).cloned() else {
        return;
    };
    // Чат мог исчезнуть из списка (удалён или список перезагружен): панель
    // тогда просто ничего не рисует, вместо паники по индексу.
    let Some(chat_index) = state.chat_index(&chat_id) else {
        return;
    };
    let is_active_pane = pane_idx == state.active_pane;

    // Строку телеметрии токенов резервируем только когда есть что показать:
    // иначе высота панели ввода будет дёргаться между кадрами без ответа.
    let mut footer = token_footer_line(&state.chats[chat_index].messages);
    if let Some(context) = state.chat_ui.get(&chat_id).and_then(|ui| ui.last_context.as_ref()) {
        let strategy_line = context_status_line(context);
        footer = Some(match footer {
            Some(existing) => format!("{existing} · {strategy_line}"),
            None => strategy_line,
        });
    }
    let mut constraints = vec![Constraint::Length(1), Constraint::Min(3)];
    if footer.is_some() {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(3));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    render_pane_title(f, state, chat_index, is_active_pane, chunks[0]);
    state.history_areas.push((chat_id.clone(), chunks[1]));
    render_history(f, state, &chat_id, chat_index, chunks[1]);
    let input_area = if let Some(footer) = &footer {
        render_token_footer(f, footer, chunks[2]);
        chunks[3]
    } else {
        chunks[2]
    };
    render_input(f, state, &chat_id, is_active_pane, input_area);
}

/// Строка «что сделала стратегия контекста» для последнего ответа
/// (specs/context-strategies, «Переключение стратегии из клиента»): поля,
/// не имеющие смысла для действующей стратегии, сервис не присылает —
/// здесь это выражено пропуском, а не нулём.
fn context_status_line(context: &agentcore::config::ContextObservability) -> String {
    let strategy_label = context
        .strategy
        .map(|s| s.label())
        .unwrap_or("умолчание сервиса");
    let mut parts = vec![format!("Стратегия: {strategy_label}")];
    if let Some(sent) = context.sent_messages {
        parts.push(format!("отправлено {sent}"));
    }
    if let Some(dropped) = context.dropped_messages {
        parts.push(format!("отброшено {dropped}"));
    }
    if let Some(replaced) = context.replaced_messages {
        parts.push(format!("заменено пересказом {replaced}"));
    }
    if let Some(true) = context.summary_built {
        parts.push("пересказ перестроен".to_string());
    }
    if let Some(applied) = context.facts_applied {
        parts.push(format!("фактов {applied}"));
    }
    match context.facts_updated {
        Some(true) => parts.push("факты обновлены".to_string()),
        Some(false) => parts.push("факты не обновлены".to_string()),
        None => {}
    }
    if let Some(branch_id) = &context.branch_id {
        let short: String = branch_id.chars().take(8).collect();
        parts.push(format!("ветка {short}"));
    }
    if let Some(entries) = context.memory_long_term_entries {
        let chars = context.memory_long_term_chars.unwrap_or(0);
        parts.push(format!("долговременная {entries} ({chars} симв.)"));
    }
    if let Some(entries) = context.memory_working_entries {
        let chars = context.memory_working_chars.unwrap_or(0);
        parts.push(format!("рабочая {entries} ({chars} симв.)"));
    }
    if let Some(messages) = context.memory_short_term_messages {
        let chars = context.memory_short_term_chars.unwrap_or(0);
        parts.push(format!("краткосрочная {messages} сообщ. ({chars} симв.)"));
    }
    let router_applied = context.memory_router_applied_set.unwrap_or(0)
        + context.memory_router_applied_update.unwrap_or(0)
        + context.memory_router_applied_delete.unwrap_or(0);
    if router_applied > 0 || context.memory_router_rejected.unwrap_or(0) > 0 {
        parts.push(format!(
            "маршрутизатор: применено {router_applied}, отброшено {}",
            context.memory_router_rejected.unwrap_or(0)
        ));
    }
    // Состояние задачи в шапке чата видно без открытия экрана Ctrl+T
    // (specs/task-state, design.md решение 9).
    if let Some(stage) = &context.task_stage {
        let paused = if context.task_paused == Some(true) { " (на паузе)" } else { "" };
        parts.push(format!("задача: {stage}{paused}"));
    }
    parts.join(" · ")
}

/// Строка телеметрии токенов под историей чата: слева — последний обмен,
/// справа — накопленный итог по всему чату. `None`, если телеметрии нет ни
/// у одного сообщения — тогда строка не резервирует место в layout.
fn token_footer_line(messages: &[Message]) -> Option<String> {
    let exchange = messages
        .iter()
        .rev()
        .find(|message| matches!(message.role, Role::Assistant))
        .and_then(|message| message.meta.as_ref())
        .map(meta_token_summary)
        .filter(|summary| !summary.is_empty());
    let chat = token_counters(&chat_token_totals(messages));

    let mut parts = Vec::new();
    if let Some(exchange) = exchange {
        parts.push(format!("Обмен: {exchange}"));
    }
    if let Some(chat) = chat {
        parts.push(format!("Чат: {chat}"));
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("   ·   "))
}

fn render_token_footer(f: &mut Frame, text: &str, area: Rect) {
    let footer = Paragraph::new(Line::from(Span::styled(
        format!(" {text}"),
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(footer, area);
}

/// Экран без открытых чатов: список ещё грузится, не загрузился или пуст.
/// Причина отказа видна здесь же, вместе с адресом сервиса.
fn render_no_chat(f: &mut Frame, state: &AppState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    match &state.chats_load {
        ChatsLoad::Loading => lines.push(Line::from(Span::styled(
            " Загружаю список чатов с сервиса…",
            Style::default().fg(Color::DarkGray),
        ))),
        ChatsLoad::Failed(reason) => {
            lines.push(Line::from(Span::styled(
                " Список чатов не загружен",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                format!(" {reason}"),
                Style::default().fg(Color::White),
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                " Ctrl+E — повторить загрузку",
                Style::default().fg(Color::DarkGray),
            )));
        }
        ChatsLoad::Loaded => {
            lines.push(Line::from(Span::styled(
                " Чатов пока нет",
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                " Ctrl+N — создать чат",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), inner);
}

fn render_sidebar(f: &mut Frame, state: &AppState, area: Rect) {
    let sidebar_border_style = if state.focus == Focus::Sidebar {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(sidebar_border_style)
        .title(" Чаты ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    // подсказки живут отдельной строкой снизу и переносятся по ширине панели,
    // поэтому больше не обрезаются заголовком рамки
    let hint_lines = 2u16;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(hint_lines)])
        .split(inner);

    let width = rows[0].width as usize;
    let sidebar_items: Vec<ListItem> = state
        .chats
        .iter()
        .enumerate()
        .map(|(i, chat)| {
            let pane_pos = state.panes.iter().position(|id| id == &chat.id);
            let is_cursor = i == state.sidebar_selected;
            let style = if is_cursor && state.focus == Focus::Sidebar {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if pane_pos.is_some() {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::White)
            };
            let prefix = match pane_pos {
                Some(p) if p == state.active_pane => "● ",
                Some(_) => "○ ",
                None => "  ",
            };
            let title = truncate(&chat.title, width.saturating_sub(prefix.chars().count()));
            let stamp_style = if is_cursor && state.focus == Focus::Sidebar {
                style
            } else {
                Style::default().fg(Color::DarkGray)
            };
            ListItem::new(vec![
                Line::from(Span::styled(format!("{prefix}{title}"), style)),
                Line::from(Span::styled(
                    format!("    {}", chats::last_activity_label(chat.updated_at)),
                    stamp_style,
                )),
            ])
        })
        .collect();

    f.render_widget(List::new(sidebar_items), rows[0]);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Enter — открыть · s — сплит · d — удалить",
            Style::default().fg(Color::DarkGray),
        )))
        .wrap(Wrap { trim: true }),
        rows[1],
    );
}

/// Обрезать строку по видимой ширине, добавив многоточие.
fn truncate(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut result: String = text.chars().take(width.saturating_sub(1)).collect();
    result.push('…');
    result
}

fn render_pane_title(
    f: &mut Frame,
    state: &AppState,
    chat_index: usize,
    is_active_pane: bool,
    area: Rect,
) {
    let badge_style = if is_active_pane {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Black)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    };
    let settings = &state.chats[chat_index].settings;
    let model = settings
        .model
        .clone()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| match settings.provider {
            Provider::Cloud => state.config.effective_model(),
            Provider::Ollama => state
                .config
                .ollama_model
                .clone()
                .filter(|m| !m.trim().is_empty())
                .unwrap_or_else(|| "модель не выбрана".to_string()),
        });
    // локальные чаты помечаем: по имени модели провайдер не всегда очевиден
    let model = match settings.provider {
        Provider::Cloud => model,
        Provider::Ollama => format!("ollama · {model}"),
    };
    let title = Paragraph::new(Line::from(vec![
        Span::styled(" agentcli ", badge_style),
        Span::raw(format!("  {}", state.chats[chat_index].title)),
        Span::styled(
            format!("  [{model}]"),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    f.render_widget(title, area);
}

/// Заголовок сообщения: чем и когда отвечено. Числа токенов сюда не идут —
/// они выводятся строкой под самим ответом.
fn meta_summary(meta: &MessageMeta) -> String {
    let mut parts = Vec::new();
    // Модель из ответа: фактическая, а не запрошенная чатом.
    if let Some(model) = &meta.model {
        parts.push(model.clone());
    }
    if let Some(received) = meta.received_at.or(meta.sent_at) {
        parts.push(format_clock(received));
    }
    parts.join(" · ")
}

/// Счётчики одного обмена: они читаются после ответа, поэтому строка
/// показывается под сообщением, а не в его заголовке.
fn meta_token_summary(meta: &MessageMeta) -> String {
    let mut parts = Vec::new();
    if let Some(tokens) = token_counters(&TokenTotals::from_meta(meta)) {
        parts.push(tokens);
    }
    if let Some(ms) = meta.duration_ms {
        parts.push(format!("{:.1} с", ms as f64 / 1000.0));
    }
    if let Some(speed) = meta.tokens_per_second() {
        parts.push(format!("{speed:.0} ток/с"));
    }
    parts.join(" · ")
}

/// Сумма токенов: по одному обмену или по всему чату.
#[derive(Default, Clone, Copy)]
struct TokenTotals {
    prompt: Option<u32>,
    completion: Option<u32>,
    reasoning: Option<u32>,
    total: Option<u32>,
}

impl TokenTotals {
    fn from_meta(meta: &MessageMeta) -> Self {
        Self {
            prompt: meta.prompt_tokens,
            completion: meta.completion_tokens,
            reasoning: meta.reasoning_tokens,
            // Не каждый провайдер отдаёт total_tokens, поэтому при его
            // отсутствии складываем запрос и ответ сами.
            total: meta.total_tokens.or(match (meta.prompt_tokens, meta.completion_tokens) {
                (Some(prompt), Some(completion)) => Some(prompt + completion),
                _ => None,
            }),
        }
    }

    fn add(&mut self, other: &Self) {
        fn merge(acc: &mut Option<u32>, value: Option<u32>) {
            if let Some(value) = value {
                *acc = Some(acc.unwrap_or(0) + value);
            }
        }
        merge(&mut self.prompt, other.prompt);
        merge(&mut self.completion, other.completion);
        merge(&mut self.reasoning, other.reasoning);
        merge(&mut self.total, other.total);
    }
}

/// Читаемая строка счётчиков; `None`, если провайдер не прислал ни одного.
fn token_counters(totals: &TokenTotals) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(prompt) = totals.prompt {
        parts.push(format!("↑ запрос {prompt}"));
    }
    if let Some(completion) = totals.completion {
        parts.push(format!("↓ ответ {completion}"));
    }
    if let Some(reasoning) = totals.reasoning {
        parts.push(format!("рассужд. {reasoning}"));
    }
    if let Some(total) = totals.total {
        parts.push(format!("всего {total}"));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("{} ток.", parts.join(" · ")))
}

/// Счётчики за весь чат: складываем телеметрию всех сообщений.
fn chat_token_totals(messages: &[Message]) -> TokenTotals {
    let mut totals = TokenTotals::default();
    for meta in messages.iter().filter_map(|message| message.meta.as_ref()) {
        totals.add(&TokenTotals::from_meta(meta));
    }
    totals
}

fn format_clock(timestamp: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_default()
}

fn render_history(
    f: &mut Frame,
    state: &mut AppState,
    chat_id: &str,
    chat_index: usize,
    area: Rect,
) {
    let pending = state.is_pending(chat_id);
    let pending_since = state.chat_ui.get(chat_id).and_then(|u| u.pending_since);
    let show_reasoning = state.show_reasoning;
    let scroll_to_message = state.chat_ui.get(chat_id).and_then(|u| u.scroll_to_message);
    // Подсветка выбора живёт только пока открыт режим выбора: вне его индекс
    // сохраняется, но на экране ничем не выделен.
    let selected_message = state
        .chat_ui
        .get(chat_id)
        .and_then(|u| u.selected_message)
        .filter(|_| state.focus == Focus::MessageSelect);

    let history_loading = state
        .chat_ui
        .get(chat_id)
        .map(|ui| ui.history_loading)
        .unwrap_or(false);
    let history_error = state
        .chat_ui
        .get(chat_id)
        .and_then(|ui| ui.history_error.clone());
    let unsaved = state
        .chat_ui
        .get(chat_id)
        .and_then(|ui| ui.unsaved.as_ref().map(|unsaved| unsaved.reason.clone()));

    // Ширина/высота содержимого внутри рамки.
    let inner_width = area.width.saturating_sub(2);
    let visible = area.height.saturating_sub(2);

    // Шапка истории: баннеры загрузки и заставка пустого чата. Она короткая
    // и зависит от состояния загрузки, поэтому собирается каждый кадр.
    let mut head: Vec<Line<'static>> = Vec::new();
    if history_loading {
        head.push(Line::from(Span::styled(
            " Загружаю историю чата с сервиса…",
            Style::default().fg(Color::DarkGray),
        )));
        head.push(Line::raw(""));
    }
    if let Some(reason) = &history_error {
        head.push(Line::from(Span::styled(
            format!(" История чата не загружена: {reason}"),
            Style::default().fg(Color::Red),
        )));
        head.push(Line::raw(""));
    }
    if !state.chats[chat_index].history_loaded
        && !history_loading
        && history_error.is_none()
        && state.chats[chat_index].messages.is_empty()
    {
        head.push(Line::from(Span::styled(
            " История чата ещё не запрошена у сервиса",
            Style::default().fg(Color::DarkGray),
        )));
        head.push(Line::raw(""));
    }
    if state.chats[chat_index].messages.is_empty()
        && state.chats[chat_index].history_loaded
        && state.panes.len() == 1
    {
        head.extend(billy_art().iter().cloned());
        head.push(Line::raw(""));
        head.push(Line::from(Span::styled(
            "        agent-cli — консольный AI-агент",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        head.push(Line::raw(""));
    }

    // Подвал истории: предупреждение о несохранённом обмене и индикатор
    // ожидания ответа. Таймер обновляется каждый тик, кешировать нечего.
    let mut tail: Vec<Line<'static>> = Vec::new();
    if let Some(reason) = &unsaved {
        tail.push(Line::from(Span::styled(
            format!(" ⚠ Обмен не сохранён в сервисе: {reason}. Ctrl+U — повторить"),
            Style::default().fg(Color::Yellow),
        )));
        tail.push(Line::raw(""));
    }
    if pending {
        let elapsed = pending_since
            .map(|since| format!(" {:.1} с", since.elapsed().as_secs_f64()))
            .unwrap_or_default();
        tail.push(Line::from(Span::styled(
            format!(
                "{} Агент думает...{elapsed}",
                SPINNER_FRAMES[state.spinner_frame]
            ),
            Style::default().fg(Color::Magenta),
        )));
    }

    // Обновляем кеш отрисованных сообщений: заново собираются только те,
    // у которых изменился отпечаток, — обычно это последняя реплика.
    {
        let messages = &state.chats[chat_index].messages;
        let ui = state.chat_ui.entry(chat_id.to_string()).or_default();
        ui.rendered.truncate(messages.len());
        for (i, entry) in messages.iter().enumerate() {
            let selected = selected_message == Some(i);
            let fingerprint = message_fingerprint(entry, show_reasoning, selected);
            if ui.rendered.get(i).map(|cached| cached.fingerprint) == Some(fingerprint) {
                continue;
            }
            let item = RenderedMessage {
                fingerprint,
                lines: render_message_lines(entry, show_reasoning, selected),
                wrapped: None,
            };
            match ui.rendered.get_mut(i) {
                Some(slot) => *slot = item,
                None => ui.rendered.push(item),
            }
        }
        // Высота блока после переноса строк тоже кешируется: считать её по
        // всей истории на каждый кадр — второй проход той же стоимости.
        for item in ui.rendered.iter_mut() {
            if item.wrapped.map(|(width, _)| width) != Some(inner_width) {
                item.wrapped = Some((inner_width, wrapped_line_count(&item.lines, inner_width)));
            }
        }
    }

    let ui = state.chat_ui.entry(chat_id.to_string()).or_default();

    // Смещение прокрутки у Paragraph считается по строкам ПОСЛЕ переноса,
    // поэтому длину истории тоже надо мерить с учётом Wrap, иначе низ
    // длинных сообщений становится недостижим. Складываем высоты блоков:
    // они уже посчитаны для текущей ширины.
    let head_height = wrapped_line_count(&head, inner_width);
    let tail_height = wrapped_line_count(&tail, inner_width);
    let mut total_lines = head_height + tail_height;
    let mut target_offset: Option<usize> = None;
    let mut offset = head_height;
    for (i, item) in ui.rendered.iter().enumerate() {
        if scroll_to_message == Some(i) {
            target_offset = Some(offset);
        }
        let height = item.wrapped.map(|(_, height)| height).unwrap_or(0);
        offset += height;
        total_lines += height;
    }

    let max_scroll = clamp_u16(total_lines).saturating_sub(visible);
    ui.max_scroll = max_scroll;
    if ui.auto_scroll {
        ui.scroll = max_scroll;
    } else if let Some(target) = target_offset {
        ui.scroll = clamp_u16(target).min(max_scroll);
        ui.scroll_to_message = None;
    } else {
        ui.scroll = ui.scroll.min(max_scroll);
    }
    let scroll = ui.scroll as usize;

    // Виджету отдаём только те блоки, что попадают в окно просмотра:
    // Paragraph переносит строки от начала текста до смещения, и на длинной
    // истории этот проход и есть основной тормоз кадра.
    let blocks = std::iter::once((head.as_slice(), head_height))
        .chain(ui.rendered.iter().map(|item| {
            (
                item.lines.as_slice(),
                item.wrapped.map(|(_, height)| height).unwrap_or(0),
            )
        }))
        .chain(std::iter::once((tail.as_slice(), tail_height)));
    let (lines, inner_offset) = visible_window(blocks, scroll, visible as usize);

    let history_widget = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::ALL).title(" История "))
        .wrap(Wrap { trim: false })
        .scroll((inner_offset, 0));

    f.render_widget(history_widget, area);
}

/// Отбирает блоки истории, попадающие в окно просмотра, и остаточное
/// смещение внутри первого из них.
///
/// Блоки идут подряд, их высоты уже посчитаны с учётом переноса строк, так
/// что смещение внутри первого видимого блока совпадает с тем, что отсчитал
/// бы Paragraph по всей истории.
fn visible_window<'a, I>(blocks: I, scroll: usize, visible: usize) -> (Vec<Line<'a>>, u16)
where
    I: IntoIterator<Item = (&'a [Line<'a>], usize)>,
{
    let window_end = scroll + visible;
    let mut lines: Vec<Line<'a>> = Vec::new();
    let mut inner_offset = 0u16;
    let mut first_visible = true;
    let mut cursor = 0usize;
    for (block, height) in blocks {
        let block_end = cursor + height;
        if block_end <= scroll {
            cursor = block_end;
            continue;
        }
        if cursor >= window_end {
            break;
        }
        if first_visible {
            first_visible = false;
            inner_offset = clamp_u16(scroll - cursor);
        }
        lines.extend(block.iter().cloned());
        cursor = block_end;
    }
    (lines, inner_offset)
}

/// Заставка пустого чата: ANSI-картинка разбирается один раз.
fn billy_art() -> &'static [Line<'static>] {
    static ART: std::sync::OnceLock<Vec<Line<'static>>> = std::sync::OnceLock::new();
    ART.get_or_init(|| {
        BILLY_ART
            .into_text()
            .map(|text| text.lines)
            .unwrap_or_default()
    })
}

/// Отпечаток сообщения для кеша отрисовки: всё, от чего зависят его строки.
fn message_fingerprint(entry: &Message, show_reasoning: bool, selected: bool) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    entry.content.hash(&mut hasher);
    entry.reasoning.hash(&mut hasher);
    show_reasoning.hash(&mut hasher);
    selected.hash(&mut hasher);
    // Из телеметрии в строках видны только модель и время в заголовке.
    if let Some(meta) = entry.meta.as_ref() {
        meta.model.hash(&mut hasher);
        meta.received_at.hash(&mut hasher);
        meta.sent_at.hash(&mut hasher);
    }
    hasher.finish()
}

/// Строки одного сообщения истории: заголовок, рассуждение и markdown тела.
fn render_message_lines(
    entry: &Message,
    show_reasoning: bool,
    selected: bool,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let (label, color) = match entry.role {
        Role::User => ("Вы", Color::Green),
        Role::Assistant => ("Агент", Color::Cyan),
        Role::System => ("Система", Color::Yellow),
        Role::Tool => ("Инструмент", Color::Magenta),
    };
    // Подсвечиваем строку заголовка, а не весь блок: перекрашивать
    // многострочный отрисованный markdown значило бы потерять его разметку.
    let mut header_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    if selected {
        header_style = header_style.add_modifier(Modifier::REVERSED);
    }
    let mut header = vec![Span::styled(format!("● {label}"), header_style)];
    if let Some(stats) = entry.meta.as_ref().map(meta_summary).filter(|s| !s.is_empty()) {
        header.push(Span::styled(
            format!("  {stats}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    lines.push(Line::from(header));
    if let Some(reasoning) = entry.reasoning.as_ref() {
        if show_reasoning {
            lines.push(Line::from(Span::styled(
                "  ┌ Рассуждение",
                Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            )));
            for reasoning_line in reasoning.lines() {
                lines.push(Line::from(Span::styled(
                    format!("  │ {reasoning_line}"),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::DIM | Modifier::ITALIC),
                )));
            }
            lines.push(Line::from(Span::styled(
                "  └",
                Style::default().fg(Color::Magenta),
            )));
        } else {
            let count = reasoning.lines().count();
            lines.push(Line::from(Span::styled(
                format!("  ▸ Рассуждение скрыто ({count} стр.) — Ctrl+R"),
                Style::default().fg(Color::Magenta).add_modifier(Modifier::DIM),
            )));
        }
    }
    let rendered = agent_skin().term_text(&entry.content).to_string();
    match rendered.into_text() {
        Ok(text) => lines.extend(text.lines),
        Err(_) => lines.push(Line::raw(entry.content.clone())),
    }
    lines.push(Line::raw(""));
    lines
}

fn clamp_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// Высота текста после переноса на заданной ширине.
fn wrapped_text_line_count(text: Text<'_>, width: u16) -> usize {
    if width == 0 {
        return 0;
    }
    Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .line_count(width)
}

/// Высота набора строк после переноса на заданной ширине.
fn wrapped_line_count(lines: &[Line<'static>], width: u16) -> usize {
    if width == 0 || lines.is_empty() {
        return 0;
    }
    Paragraph::new(Text::from(lines.to_vec()))
        .wrap(Wrap { trim: false })
        .line_count(width)
}

fn render_input(f: &mut Frame, state: &AppState, chat_id: &str, is_active_pane: bool, area: Rect) {
    let typing = is_active_pane && state.focus == Focus::Input;
    let input_border_style = if is_active_pane {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let ui = state.chat_ui.get(chat_id);
    let input_text = ui.map(|u| u.input.as_str()).unwrap_or("");
    let pending = ui.map(|u| u.pending).unwrap_or(false);
    let title = if pending { " Сообщение (ожидание ответа...) " } else { " Сообщение " };
    let input_widget = Paragraph::new(input_text)
        .style(Style::default().fg(Color::White))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(input_border_style)
                .title(title),
        );
    f.render_widget(input_widget, area);
    if !pending && typing {
        f.set_cursor_position((area.x + 1 + input_text.chars().count() as u16, area.y + 1));
    }
}

fn render_help(f: &mut Frame, state: &AppState, area: Rect) {
    let (text, color) = match state.active_notice() {
        Some(notice) => (notice, Color::Green),
        // В режиме выбора клавиши другие, и у Ctrl+Y другой смысл: подсказка
        // показывает именно их, пока режим открыт.
        None if state.focus == Focus::MessageSelect => (
            "↑/↓ — сообщение · Ctrl+Y — копировать текст · Enter — копировать и выйти · Esc — выйти из режима выбора",
            Color::DarkGray,
        ),
        None => (
            "Tab — панель · ←/→ — окно · колесо/PageUp/PageDown — история · Ctrl+W — закрыть · Ctrl+P — настройки · Ctrl+O — импорт контекста · Ctrl+N — новый чат · Ctrl+E — обновить список · Ctrl+U — повторить запись · Ctrl+Y — копировать ввод · Ctrl+G — выбрать сообщение · Ctrl+R — рассуждение · Esc/Ctrl+C — выход",
            Color::DarkGray,
        ),
    };
    let help = Paragraph::new(Line::from(Span::styled(text, Style::default().fg(color))))
    .wrap(Wrap { trim: true });
    f.render_widget(help, area);
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

fn render_settings_popup(f: &mut Frame, editor: &SettingsEditor) {
    let area = centered_rect(104, 34, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(
            " Настройки чата «{}» (Ctrl+S — сохранить, Esc — отмена) ",
            editor.chat_title
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Высота панели пояснения зависит от длины текста: короткие описания разделов
    // не должны отъедать место у списка полей, а длинные — не должны обрезаться.
    let description_text = settings_description_text(editor);
    let description_width = inner.width.saturating_sub(2).max(1);
    let description_lines = wrapped_text_line_count(description_text.into(), description_width);
    let description_height =
        (clamp_u16(description_lines) + 2).clamp(4, inner.height.saturating_sub(6).max(4));

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(description_height),
            Constraint::Length(1),
        ])
        .split(inner);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(26), Constraint::Min(0)])
        .split(rows[0]);

    render_settings_sections(f, editor, columns[0]);
    render_settings_fields(f, editor, columns[1]);
    render_settings_description(f, rows[1], description_text);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " Tab — панель/поле · ↑/↓ — выбор · Ctrl+D — сброс · Ctrl+L — модели Ollama · \
Ctrl+S — сохранить · Esc — отмена",
            Style::default().fg(Color::DarkGray),
        ))),
        rows[2],
    );
}

/// Текст нижней панели пояснения: для выделенного поля либо для раздела,
/// пока курсор ещё в списке слева.
fn settings_description_text(editor: &SettingsEditor) -> &'static str {
    match editor.pane {
        SettingsPane::Fields => editor
            .current_field()
            .map(|field| field.description())
            .unwrap_or("В этом разделе нет полей для текущего провайдера."),
        SettingsPane::Sections => editor.current_section().description(),
    }
}

/// Нижняя панель попапа: развёрнутое пояснение к выделенному полю
/// (или к разделу, пока курсор ещё в списке слева).
fn render_settings_description(f: &mut Frame, area: Rect, text: &str) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" Пояснение ");
    f.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(Color::Gray))
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

/// Левая панель попапа: список разделов настроек.
fn render_settings_sections(f: &mut Frame, editor: &SettingsEditor, area: Rect) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let active = editor.pane == SettingsPane::Sections;
    let lines: Vec<Line> = SettingsSection::ALL
        .iter()
        .enumerate()
        .map(|(index, section)| {
            let selected = index == editor.section;
            let style = match (selected, active) {
                (true, true) => Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                (true, false) => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                _ => Style::default().fg(Color::White),
            };
            let marker = if selected { "▸" } else { " " };
            Line::from(Span::styled(
                format!(" {marker} {:<width$}", section.label(), width = 18),
                style,
            ))
        })
        .collect();

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Подсказка вместо пустого значения поля.
fn empty_field_hint(field: FormatField, editor: &SettingsEditor) -> String {
    match field {
        FormatField::Model => match editor.provider {
            Provider::Cloud => format!("не задана — {} из конфига", editor.default_model),
            Provider::Ollama if !editor.default_ollama_model.is_empty() => format!(
                "не задана — {} из конфига",
                editor.default_ollama_model
            ),
            Provider::Ollama if editor.ollama_models.is_empty() => {
                "локальных моделей не видно — Ctrl+L обновить список".to_string()
            }
            Provider::Ollama => format!(
                "не задана — ◀/▶ выбрать из {} локальных",
                editor.ollama_models.len()
            ),
        },
        FormatField::ServerUrl => {
            format!("не задан — {}", agentcore::config::DEFAULT_SERVER_URL)
        }
        FormatField::ClientToken => {
            "не задан — сервис без аутентификации ответит и так".to_string()
        }
        FormatField::OllamaUrl => {
            format!("не задан — {}", agentcore::config::DEFAULT_OLLAMA_URL)
        }
        FormatField::ContextLimit => {
            "не задан — действует операторский лимит сервиса".to_string()
        }
        FormatField::SummaryKeepMessages
        | FormatField::SummaryStepMessages
        | FormatField::ContextWindowMessages
        | FormatField::MemoryWorkingMaxEntries
        | FormatField::MemoryLongTermMaxEntries => {
            "не задано — действует операторское умолчание сервиса".to_string()
        }
        _ => "не задано — используется значение модели".to_string(),
    }
}

/// Ключ на экране не показываем целиком: видны только последние 4 символа.
fn mask_secret(value: &str) -> String {
    let count = value.chars().count();
    if count == 0 {
        return String::new();
    }
    if count <= 4 {
        return "•".repeat(count);
    }
    let tail: String = value.chars().skip(count - 4).collect();
    format!("{}{tail}", "•".repeat(count - 4))
}

/// Правая панель попапа: поля активного раздела.
fn render_settings_fields(f: &mut Frame, editor: &SettingsEditor, area: Rect) {
    let active = editor.pane == SettingsPane::Fields;
    let current = editor.current_field();
    let mut lines: Vec<Line> = Vec::new();

    for field in editor.visible_fields() {
        let selected = active && Some(field) == current;
        let label_style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        };

        let raw = match field {
            FormatField::Provider => editor.provider.label().to_string(),
            FormatField::Model => editor.model.clone(),
            FormatField::ServerUrl => editor.server_url.clone(),
            FormatField::ClientToken => mask_secret(&editor.client_token),
            FormatField::OllamaUrl => editor.ollama_url.clone(),
            FormatField::ContextLimit => editor.context_limit.clone(),
            FormatField::SummaryEnabled => match editor.summary_enabled.as_str() {
                "on" => "Включена".to_string(),
                "off" => "Выключена".to_string(),
                _ => "Умолчание сервиса".to_string(),
            },
            FormatField::SummaryKeepMessages => editor.summary_keep_messages.clone(),
            FormatField::SummaryStepMessages => editor.summary_step_messages.clone(),
            FormatField::ContextStrategy => match ContextStrategy::parse(&editor.context_strategy) {
                Some(strategy) => strategy.label().to_string(),
                None => "Умолчание сервиса".to_string(),
            },
            FormatField::ContextWindowMessages => editor.context_window_messages.clone(),
            FormatField::Profile => match editor.profile_choices.iter().find(|p| p.id == editor.profile_id) {
                Some(profile) => format!("{} ({})", profile.name, profile.id),
                None if editor.profile_id.is_empty() => "Умолчание сервиса".to_string(),
                None => editor.profile_id.clone(),
            },
            FormatField::MemoryLayersEnabled => match editor.memory_layers_enabled.as_str() {
                "on" => "Включена".to_string(),
                "off" => "Выключена".to_string(),
                _ => "Умолчание сервиса".to_string(),
            },
            FormatField::MemoryRouterEnabled => match editor.memory_router_enabled.as_str() {
                "on" => "Включён".to_string(),
                "off" => "Выключен".to_string(),
                _ => "Умолчание сервиса".to_string(),
            },
            FormatField::MemoryWorkingMaxEntries => editor.memory_working_max_entries.clone(),
            FormatField::MemoryLongTermMaxEntries => editor.memory_long_term_max_entries.clone(),
            FormatField::TaskStateEnabled => match editor.task_state_enabled.as_str() {
                "on" => "Включено".to_string(),
                "off" => "Выключено".to_string(),
                _ => "Умолчание сервиса".to_string(),
            },
            FormatField::TaskStateAutoEnabled => match editor.task_state_auto_enabled.as_str() {
                "on" => "Включён".to_string(),
                "off" => "Выключен".to_string(),
                _ => "Умолчание сервиса".to_string(),
            },
            FormatField::Mode => {
                if editor.custom_mode {
                    "Кастомный".to_string()
                } else {
                    "Дефолтный".to_string()
                }
            }
            FormatField::Reasoning => editor.reasoning.label().to_string(),
            FormatField::Thinking => editor.thinking.label().to_string(),
            FormatField::Experts => editor.experts.clone(),
            FormatField::Description => editor.description.clone(),
            FormatField::MaxLength => editor.max_length.clone(),
            FormatField::Stop => editor.stop.clone(),
            FormatField::StopInstruction => editor.stop_instruction.clone(),
            FormatField::Temperature => editor.temperature.clone(),
            FormatField::TopP => editor.top_p.clone(),
            FormatField::TopK => editor.top_k.clone(),
            FormatField::FrequencyPenalty => editor.frequency_penalty.clone(),
            FormatField::PresencePenalty => editor.presence_penalty.clone(),
        };
        let cursor = if selected && !field.is_toggle() { "▏" } else { "" };
        let enabled = field.is_connection()
            || field.is_sampling()
            || field.is_toggle()
            || field.is_reasoning_detail()
            || editor.custom_mode;
        let placeholder = raw.is_empty() && !field.is_toggle();
        let value = if placeholder {
            empty_field_hint(field, editor)
        } else {
            raw
        };
        let value_color = if !enabled || placeholder {
            Color::DarkGray
        } else {
            Color::White
        };

        lines.push(Line::from(Span::styled(format!(" {} ", field.label()), label_style)));
        lines.push(Line::from(Span::styled(
            format!("   {value}{cursor}"),
            Style::default().fg(value_color),
        )));
        lines.push(Line::from(Span::styled(
            "─".repeat(area.width as usize),
            Style::default().fg(Color::DarkGray),
        )));
    }

    if let Some(err) = &editor.error {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!(" Ошибка: {err}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }

    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        area,
    );
}

/// Окно выбора чатов, чей контекст переносится в текущий.
fn render_import_popup(f: &mut Frame, picker: &ImportPicker) {
    let area = centered_rect(72, 20, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(
            " Импорт контекста в «{}» (Space — выбрать, Enter — перенести) ",
            picker.target_title
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    let lines: Vec<Line> = if picker.candidates.is_empty() {
        vec![Line::from(Span::styled(
            " Нет других чатов с историей",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        picker
            .candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let style = if index == picker.cursor {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else if candidate.selected {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default().fg(Color::White)
                };
                let mark = if candidate.selected { "[x]" } else { "[ ]" };
                Line::from(Span::styled(
                    format!(" {mark} {} ({} сообщ.)", candidate.title, candidate.messages),
                    style,
                ))
            })
            .collect()
    };

    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        rows[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ↑/↓ — выбор · Space — отметить · Enter — перенести · Esc — отмена",
            Style::default().fg(Color::DarkGray),
        ))),
        rows[1],
    );
}

/// Экран фактов чата: список пар «ключ-значение», либо редактор одной
/// записи, если он открыт (specs/context-facts, «Факты читаются и
/// правятся вручную»).
fn render_facts_popup(f: &mut Frame, picker: &FactsPicker) {
    let area = centered_rect(72, 20, f.area());
    f.render_widget(Clear, area);

    let title = format!(" Факты чата «{}» ", picker.chat_title);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Some(editor) = &picker.editor {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
            .split(inner);
        let key_style = if editor.editing_key {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };
        let value_style = if editor.editing_key {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        };
        f.render_widget(Paragraph::new(Line::from(vec![
            Span::raw(" Ключ: "),
            Span::styled(editor.key.clone(), key_style),
        ])), rows[0]);
        f.render_widget(Paragraph::new(Line::from(vec![
            Span::raw(" Значение: "),
            Span::styled(editor.value.clone(), value_style),
        ])), rows[1]);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " Tab — переключить поле · Enter — сохранить · Esc — отмена",
                Style::default().fg(Color::DarkGray),
            ))),
            rows[3],
        );
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    let lines: Vec<Line> = if picker.loading {
        vec![Line::from(Span::styled(" Загрузка...", Style::default().fg(Color::DarkGray)))]
    } else if let Some(error) = &picker.error {
        vec![Line::from(Span::styled(format!(" Ошибка: {error}"), Style::default().fg(Color::Red)))]
    } else if picker.facts.is_empty() {
        vec![Line::from(Span::styled(
            " Фактов пока нет — n добавит первый",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        picker
            .facts
            .iter()
            .enumerate()
            .map(|(index, fact)| {
                let style = if index == picker.cursor {
                    Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                Line::from(Span::styled(format!(" {}: {}", fact.key, fact.value), style))
            })
            .collect()
    };
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), rows[0]);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ↑/↓ — выбор · Enter — править · n — новый факт · d — удалить · Esc — закрыть",
            Style::default().fg(Color::DarkGray),
        ))),
        rows[1],
    );
}

/// Экран веток чата: список с активной веткой, либо выбор точки ветвления
/// и имени новой ветки, если он открыт (specs/chat-branching, «Управление
/// ветками из клиента»).
fn render_branches_popup(f: &mut Frame, picker: &BranchesPicker) {
    let area = centered_rect(72, 20, f.area());
    f.render_widget(Clear, area);

    let title = format!(" Ветки чата «{}» ", picker.chat_title);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Some(creation) = &picker.creating {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(inner);

        if let Some(name) = &creation.name {
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(" Имя новой ветки: "),
                    Span::styled(name.clone(), Style::default().fg(Color::Black).bg(Color::Cyan)),
                ])),
                rows[0],
            );
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    " Enter — создать · Esc — отмена",
                    Style::default().fg(Color::DarkGray),
                ))),
                rows[1],
            );
            return;
        }

        let lines: Vec<Line> = if creation.loading {
            vec![Line::from(Span::styled(" Загрузка сообщений...", Style::default().fg(Color::DarkGray)))]
        } else if creation.messages.is_empty() {
            vec![Line::from(Span::styled(" В чате нет сообщений для точки ветвления", Style::default().fg(Color::DarkGray)))]
        } else {
            creation
                .messages
                .iter()
                .enumerate()
                .map(|(index, message)| {
                    let style = if index == creation.message_cursor {
                        Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    let preview: String = message.message.content.chars().take(60).collect();
                    Line::from(Span::styled(format!(" #{} {}", message.seq, preview), style))
                })
                .collect()
        };
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), rows[0]);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " ↑/↓ — выбор сообщения · Enter — ветвить отсюда · Esc — отмена",
                Style::default().fg(Color::DarkGray),
            ))),
            rows[1],
        );
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    let lines: Vec<Line> = if picker.loading {
        vec![Line::from(Span::styled(" Загрузка...", Style::default().fg(Color::DarkGray)))]
    } else if let Some(error) = &picker.error {
        vec![Line::from(Span::styled(format!(" Ошибка: {error}"), Style::default().fg(Color::Red)))]
    } else {
        picker
            .branches
            .iter()
            .enumerate()
            .map(|(index, branch)| {
                let style = if index == picker.branch_cursor {
                    Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let mark = if branch.active { "●" } else { " " };
                Line::from(Span::styled(
                    format!(" {mark} {} ({} сообщ.)", branch.name, branch.message_count),
                    style,
                ))
            })
            .collect()
    };
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), rows[0]);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ↑/↓ — выбор · Enter — переключить · n — новая ветка · Esc — закрыть",
            Style::default().fg(Color::DarkGray),
        ))),
        rows[1],
    );
}

/// Экран слоистой памяти чата: три раздела — краткосрочная (только просмотр
/// хвоста сообщений), рабочая и долговременная (просмотр, добавление,
/// правка, удаление записей) (specs/memory-layers, «Ручное управление
/// памятью через HTTP»).
fn render_memory_popup(f: &mut Frame, picker: &MemoryPicker, short_term_tail: &[Message]) {
    let area = centered_rect(76, 24, f.area());
    f.render_widget(Clear, area);

    let title = format!(" Память чата «{}» ", picker.chat_title);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    let tabs: Vec<Span> = MemorySection::ALL
        .iter()
        .enumerate()
        .flat_map(|(index, section)| {
            let style = if index == picker.section {
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            vec![Span::styled(format!(" {} ", section.label()), style), Span::raw(" ")]
        })
        .collect();
    f.render_widget(Paragraph::new(Line::from(tabs)), rows[0]);

    if let Some(editor) = &picker.editor {
        render_memory_editor(f, editor, rows[2].union(rows[1]));
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " Tab — переключить поле · ←/→ — тип записи · Enter — сохранить · Esc — отмена",
                Style::default().fg(Color::DarkGray),
            ))),
            rows[3],
        );
        return;
    }

    let lines: Vec<Line> = match picker.current_section() {
        MemorySection::ShortTerm => {
            if short_term_tail.is_empty() {
                vec![Line::from(Span::styled(" Сообщений пока нет", Style::default().fg(Color::DarkGray)))]
            } else {
                short_term_tail
                    .iter()
                    .map(|m| {
                        let who = match m.role {
                            Role::User => "Вы",
                            Role::Assistant => "Модель",
                            Role::System => "Система",
                            Role::Tool => "Инструмент",
                        };
                        Line::from(Span::raw(format!(" {who}: {}", m.content)))
                    })
                    .collect()
            }
        }
        MemorySection::Working => {
            if picker.working_loading {
                vec![Line::from(Span::styled(" Загрузка...", Style::default().fg(Color::DarkGray)))]
            } else if let Some(error) = &picker.working_error {
                vec![Line::from(Span::styled(format!(" Ошибка: {error}"), Style::default().fg(Color::Red)))]
            } else if picker.working.is_empty() {
                vec![Line::from(Span::styled(
                    " Рабочей памяти пока нет — n добавит первую запись",
                    Style::default().fg(Color::DarkGray),
                ))]
            } else {
                picker
                    .working
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        let style = if index == picker.working_cursor {
                            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::White)
                        };
                        Line::from(Span::styled(
                            format!(" [{}] {}: {}", entry.source, entry.key, entry.value),
                            style,
                        ))
                    })
                    .collect()
            }
        }
        MemorySection::LongTerm => {
            if picker.long_term_loading {
                vec![Line::from(Span::styled(" Загрузка...", Style::default().fg(Color::DarkGray)))]
            } else if let Some(error) = &picker.long_term_error {
                vec![Line::from(Span::styled(format!(" Ошибка: {error}"), Style::default().fg(Color::Red)))]
            } else if picker.long_term.is_empty() {
                vec![Line::from(Span::styled(
                    " Долговременной памяти пока нет — n добавит первую запись",
                    Style::default().fg(Color::DarkGray),
                ))]
            } else {
                picker
                    .long_term
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        let style = if index == picker.long_term_cursor {
                            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::White)
                        };
                        Line::from(Span::styled(
                            format!(
                                " [{}] {}: {}",
                                entry.entry_type,
                                entry.key.as_deref().unwrap_or("(без ключа)"),
                                entry.value
                            ),
                            style,
                        ))
                    })
                    .collect()
            }
        }
    };
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), rows[2]);

    let hint = match picker.current_section() {
        MemorySection::ShortTerm => " ←/→ — раздел · Esc — закрыть",
        MemorySection::Working => " ←/→ — раздел · ↑/↓ — выбор · Enter — править · n — новая · d — удалить · t — завершить задачу · Esc — закрыть",
        MemorySection::LongTerm => " ←/→ — раздел · ↑/↓ — выбор · Enter — править · n — новая · d — удалить · Esc — закрыть",
    };
    f.render_widget(Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray)))), rows[3]);
}

/// Экран состояния задачи чата: этап, шаг, ожидаемое действие, пауза,
/// предложенный следующий этап и последние переходы (specs/task-state,
/// design.md решение 9). Правка шага/ожидаемого действия — отдельный режим
/// с одним полем ввода.
fn render_task_popup(f: &mut Frame, picker: &TaskPicker) {
    let area = centered_rect(76, 22, f.area());
    f.render_widget(Clear, area);

    let title = format!(" Задача чата «{}» ", picker.chat_title);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Some(editor) = &picker.editor {
        let field_label = if editor.editing_expected_action { "Ожидаемое действие" } else { "Шаг" };
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
            .split(inner);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(format!(" {field_label}: ")),
                Span::styled(editor.value.clone(), Style::default().fg(Color::Black).bg(Color::Cyan)),
            ]))
            .wrap(Wrap { trim: false }),
            rows[0],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " Enter — сохранить · Esc — отмена",
                Style::default().fg(Color::DarkGray),
            ))),
            rows[2],
        );
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    let mut lines: Vec<Line> = Vec::new();
    if picker.loading {
        lines.push(Line::from(Span::styled(" Загрузка...", Style::default().fg(Color::DarkGray))));
    } else if let Some(error) = &picker.error {
        lines.push(Line::from(Span::styled(format!(" Ошибка: {error}"), Style::default().fg(Color::Red))));
    } else if let Some(task) = &picker.state {
        lines.push(Line::from(Span::styled(
            format!(" Этап: {}{}", task.stage, if task.paused { " (на паузе)" } else { "" }),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(format!(
            " Шаг: {}",
            if task.step.is_empty() { "(не задан)" } else { &task.step }
        )));
        lines.push(Line::from(format!(
            " Ожидаемое действие: {}",
            if task.expected_action.is_empty() { "(не задано)" } else { &task.expected_action }
        )));
        if task.paused && !task.resume_brief.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(" Бриф возобновления:", Style::default().fg(Color::DarkGray))));
            for line in task.resume_brief.lines() {
                lines.push(Line::from(format!(" {line}")));
            }
        }
        let next_label = match picker.selected_next_stage() {
            Some(stage) => stage.to_string(),
            None => "без смены этапа".to_string(),
        };
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!(" ←/→ выбирает переход: {next_label}"),
            Style::default().fg(Color::Cyan),
        )));
        if !task.transitions.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(" Последние переходы:", Style::default().fg(Color::DarkGray))));
            for transition in task.transitions.iter().rev().take(5) {
                lines.push(Line::from(Span::styled(
                    format!(" {} → {} ({})", transition.from_stage, transition.to_stage, transition.source),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    } else {
        lines.push(Line::from(Span::styled(" Состояние задачи недоступно", Style::default().fg(Color::DarkGray))));
    }

    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), rows[0]);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ←/→ — переход · Enter — применить · s — шаг · a — ожидаемое действие · p — пауза · r — возобновить · Esc — закрыть",
            Style::default().fg(Color::DarkGray),
        ))),
        rows[1],
    );
}

fn render_memory_editor(f: &mut Frame, editor: &MemoryEditor, area: Rect) {
    let field_count = if editor.for_long_term { 3 } else { 2 };
    let mut constraints = vec![Constraint::Length(1); field_count];
    constraints.push(Constraint::Min(0));
    let rows = Layout::default().direction(Direction::Vertical).constraints(constraints).split(area);

    let field_style = |index: usize| {
        if editor.field == index {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        }
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" Ключ: "),
            Span::styled(editor.key.clone(), field_style(0)),
        ])),
        rows[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" Значение: "),
            Span::styled(editor.value.clone(), field_style(1)),
        ])),
        rows[1],
    );
    if editor.for_long_term {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(" Тип: "),
                Span::styled(editor.entry_type.clone(), field_style(2)),
            ])),
            rows[2],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentcore::config::ChatSettings;

    fn summary(id: &str, title: &str, message_count: i64) -> ChatSummary {
        ChatSummary {
            id: id.to_string(),
            title: title.to_string(),
            settings: ChatSettings::default(),
            created_at: 1000,
            updated_at: 2000,
            message_count,
        }
    }

    fn test_state() -> AppState {
        let config = Config::default();
        AppState {
            chats_client: Arc::new(chats_client(&config)),
            creating_chat: false,
            config,
            agent_dirty: false,
            chats: Vec::new(),
            chats_load: ChatsLoad::Loading,
            chat_ui: HashMap::new(),
            panes: Vec::new(),
            active_pane: 0,
            sidebar_selected: 0,
            spinner_frame: 0,
            focus: Focus::Input,
            settings: None,
            import: None,
            delete_confirm: None,
            facts: None,
            branches: None,
            memory: None,
            task: None,
            notice: None,
            show_reasoning: true,
            ollama_models: Vec::new(),
            model_choices: Vec::new(),
            profile_choices: Vec::new(),
            history_areas: Vec::new(),
        }
    }

    fn channel() -> mpsc::UnboundedSender<ChatEvent> {
        mpsc::unbounded_channel().0
    }

    // --- 4.1 Список чатов приходит от сервиса ---

    #[test]
    fn loaded_list_opens_first_chat_in_service_order() {
        let mut state = test_state();
        handle_chats_loaded(
            Ok(vec![summary("chat-1", "Первый", 0), summary("chat-2", "Второй", 4)]),
            &mut state,
            &channel(),
        );

        assert!(matches!(state.chats_load, ChatsLoad::Loaded));
        assert_eq!(
            state.chats.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["chat-1", "chat-2"]
        );
        assert_eq!(state.panes, vec!["chat-1".to_string()]);
        assert!(state.blocked_reason().is_none());
        // Пустой чат считается загруженным, чат с сообщениями — нет.
        assert!(state.chats[0].history_loaded);
        assert!(!state.chats[1].history_loaded);
    }

    #[test]
    fn empty_list_is_loaded_state_without_panes() {
        let mut state = test_state();
        handle_chats_loaded(Ok(Vec::new()), &mut state, &channel());

        assert!(matches!(state.chats_load, ChatsLoad::Loaded));
        assert!(state.chats.is_empty());
        assert!(state.panes.is_empty());
        assert!(state.active_chat_id().is_none());
        assert!(state.blocked_reason().is_none());
    }

    // --- 3.3 Обратный разбор лимита контекста в экран настроек ---

    #[test]
    fn settings_editor_shows_context_limit_from_chat_settings() {
        // Ответ о чате с settings.max_context_tokens разбирается ChatSession
        // напрямую (agentcore::config::ChatSettings::deserialize), а экран
        // настроек чата показывает это значение в поле лимита
        // (specs/chat-context-limit, «Лимит показывается в настройках
        // чата»).
        let settings = ChatSettings {
            max_context_tokens: Some(4000),
            ..ChatSettings::default()
        };
        let session = ChatSession {
            id: "chat-1".to_string(),
            title: "Чат".to_string(),
            messages: Vec::new(),
            updated_at: 0,
            settings,
            history_loaded: true,
        };
        let editor = SettingsEditor::from_chat(&session, &Config::default(), &[], &[], &[]);
        assert_eq!(editor.context_limit, "4000");
    }

    #[test]
    fn settings_editor_cycles_context_strategy_and_saves_it() {
        let settings = ChatSettings::default();
        let session = ChatSession {
            id: "chat-1".to_string(),
            title: "Чат".to_string(),
            messages: Vec::new(),
            updated_at: 0,
            settings,
            history_loaded: true,
        };
        let mut editor = SettingsEditor::from_chat(&session, &Config::default(), &[], &[], &[]);
        assert_eq!(editor.build_context_strategy(), None);
        editor.cycle_context_strategy(1);
        assert_eq!(editor.build_context_strategy(), Some(ContextStrategy::Summary));
        editor.cycle_context_strategy(1);
        assert_eq!(editor.build_context_strategy(), Some(ContextStrategy::SlidingWindow));
        editor.cycle_context_strategy(-1);
        assert_eq!(editor.build_context_strategy(), Some(ContextStrategy::Summary));
    }

    #[test]
    fn settings_editor_cycles_profile_and_saves_it() {
        let settings = ChatSettings::default();
        let session = ChatSession {
            id: "chat-1".to_string(),
            title: "Чат".to_string(),
            messages: Vec::new(),
            updated_at: 0,
            settings,
            history_loaded: true,
        };
        let profiles = vec![
            ProfileChoice { id: "teacher".to_string(), name: "Преподаватель".to_string(), built_in: true },
            ProfileChoice { id: "reviewer".to_string(), name: "Ревьюер".to_string(), built_in: true },
        ];
        let mut editor = SettingsEditor::from_chat(&session, &Config::default(), &[], &[], &profiles);
        assert_eq!(editor.build_profile_id(), None);
        editor.cycle_profile(1);
        assert_eq!(editor.build_profile_id(), Some("teacher".to_string()));
        editor.cycle_profile(1);
        assert_eq!(editor.build_profile_id(), Some("reviewer".to_string()));
        editor.cycle_profile(1);
        assert_eq!(editor.build_profile_id(), None, "перебор возвращается к «без профиля»");
        editor.cycle_profile(-1);
        assert_eq!(editor.build_profile_id(), Some("reviewer".to_string()));
    }

    #[test]
    fn profile_selected_by_typing_id_round_trips_through_chat_settings() {
        let settings = ChatSettings::default();
        let session = ChatSession {
            id: "chat-1".to_string(),
            title: "Чат".to_string(),
            messages: Vec::new(),
            updated_at: 0,
            settings,
            history_loaded: true,
        };
        let mut editor = SettingsEditor::from_chat(&session, &Config::default(), &[], &[], &[]);
        editor.profile_id = "own-profile-id".to_string();
        assert_eq!(editor.build_profile_id(), Some("own-profile-id".to_string()));
    }

    #[test]
    fn existing_chat_profile_is_loaded_into_editor() {
        let settings = ChatSettings { profile_id: Some("psychologist".to_string()), ..ChatSettings::default() };
        let session = ChatSession {
            id: "chat-1".to_string(),
            title: "Чат".to_string(),
            messages: Vec::new(),
            updated_at: 0,
            settings,
            history_loaded: true,
        };
        let editor = SettingsEditor::from_chat(&session, &Config::default(), &[], &[], &[]);
        assert_eq!(editor.profile_id, "psychologist");
        assert_eq!(editor.build_profile_id(), Some("psychologist".to_string()));
    }

    #[test]
    fn memory_layers_fields_appear_only_when_memory_layers_enabled_regardless_of_strategy() {
        let settings = ChatSettings::default();
        let session = ChatSession {
            id: "chat-1".to_string(),
            title: "Чат".to_string(),
            messages: Vec::new(),
            updated_at: 0,
            settings,
            history_loaded: true,
        };
        let mut editor = SettingsEditor::from_chat(&session, &Config::default(), &[], &[], &[]);
        editor.section = SettingsSection::ALL
            .iter()
            .position(|s| *s == SettingsSection::Memory)
            .expect("раздел «Память» существует");
        editor.context_strategy = ContextStrategy::SlidingWindow.as_str().to_string();
        assert!(!editor.visible_fields().contains(&FormatField::MemoryRouterEnabled));
        editor.memory_layers_enabled = "on".to_string();
        let fields = editor.visible_fields();
        assert!(fields.contains(&FormatField::MemoryRouterEnabled));
        assert!(fields.contains(&FormatField::MemoryWorkingMaxEntries));
        assert!(fields.contains(&FormatField::MemoryLongTermMaxEntries));
    }

    #[test]
    fn memory_router_enabled_cycles_three_states_and_saves_it() {
        let settings = ChatSettings::default();
        let session = ChatSession {
            id: "chat-1".to_string(),
            title: "Чат".to_string(),
            messages: Vec::new(),
            updated_at: 0,
            settings,
            history_loaded: true,
        };
        let mut editor = SettingsEditor::from_chat(&session, &Config::default(), &[], &[], &[]);
        assert_eq!(editor.build_memory_router_enabled(), None);
        editor.cycle_memory_router_enabled(1);
        assert_eq!(editor.build_memory_router_enabled(), Some(true));
        editor.cycle_memory_router_enabled(1);
        assert_eq!(editor.build_memory_router_enabled(), Some(false));
        editor.cycle_memory_router_enabled(1);
        assert_eq!(editor.build_memory_router_enabled(), None);
    }

    #[test]
    fn task_state_auto_enabled_field_appears_only_when_task_state_enabled() {
        let settings = ChatSettings::default();
        let session = ChatSession {
            id: "chat-1".to_string(),
            title: "Чат".to_string(),
            messages: Vec::new(),
            updated_at: 0,
            settings,
            history_loaded: true,
        };
        let mut editor = SettingsEditor::from_chat(&session, &Config::default(), &[], &[], &[]);
        editor.section = SettingsSection::ALL
            .iter()
            .position(|s| *s == SettingsSection::Memory)
            .expect("раздел «Память» существует");
        assert!(!editor.visible_fields().contains(&FormatField::TaskStateAutoEnabled));
        editor.task_state_enabled = "on".to_string();
        assert!(editor.visible_fields().contains(&FormatField::TaskStateAutoEnabled));
    }

    #[test]
    fn task_state_enabled_cycles_three_states_and_saves_it() {
        let settings = ChatSettings::default();
        let session = ChatSession {
            id: "chat-1".to_string(),
            title: "Чат".to_string(),
            messages: Vec::new(),
            updated_at: 0,
            settings,
            history_loaded: true,
        };
        let mut editor = SettingsEditor::from_chat(&session, &Config::default(), &[], &[], &[]);
        assert_eq!(editor.build_task_state_enabled(), None);
        editor.cycle_task_state_enabled(1);
        assert_eq!(editor.build_task_state_enabled(), Some(true));
        editor.cycle_task_state_enabled(1);
        assert_eq!(editor.build_task_state_enabled(), Some(false));
        editor.cycle_task_state_enabled(1);
        assert_eq!(editor.build_task_state_enabled(), None);
    }

    #[test]
    fn task_state_auto_enabled_resets_on_ctrl_d() {
        let settings = ChatSettings::default();
        let session = ChatSession {
            id: "chat-1".to_string(),
            title: "Чат".to_string(),
            messages: Vec::new(),
            updated_at: 0,
            settings,
            history_loaded: true,
        };
        let mut editor = SettingsEditor::from_chat(&session, &Config::default(), &[], &[], &[]);
        editor.task_state_enabled = "on".to_string();
        editor.task_state_auto_enabled = "on".to_string();
        editor.section = SettingsSection::ALL
            .iter()
            .position(|s| *s == SettingsSection::Memory)
            .expect("раздел «Память» существует");
        editor.field = editor
            .visible_fields()
            .iter()
            .position(|f| *f == FormatField::TaskStateAutoEnabled)
            .expect("поле автотрекера видно");
        editor.reset_field();
        assert_eq!(editor.build_task_state_auto_enabled(), None);
    }

    // --- Автомат допустимых переходов доступен экрану задачи ---

    #[test]
    fn allowed_next_stages_seeds_task_picker_cursor_options() {
        let picker = TaskPicker::new("chat-1", "Чат");
        assert!(picker.allowed_next_stages().is_empty(), "состояние ещё не загружено");
    }

    fn task_picker_at(stage: &str) -> TaskPicker {
        let mut picker = TaskPicker::new("chat-1", "Чат");
        picker.state = Some(agentclient::TaskState {
            id: "t1".to_string(),
            stage: stage.to_string(),
            step: "шаг".to_string(),
            expected_action: "действие".to_string(),
            paused: false,
            resume_brief: String::new(),
            transitions: Vec::new(),
        });
        picker
    }

    /// Экран должен предлагать выход из `clarification`: до починки список
    /// там был пуст, и выйти из этапа вручную было нельзя
    /// (fix-task-state-clarification-stall, решение 5).
    #[test]
    fn task_picker_offers_transitions_for_clarification_stage() {
        assert_eq!(task_picker_at("planning").allowed_next_stages(), vec!["clarification"]);
        let mut picker = task_picker_at("clarification");
        assert_eq!(picker.allowed_next_stages(), vec!["execution", "planning"]);

        // Позиция 0 — «без смены этапа», дальше идут допустимые рёбра.
        assert_eq!(picker.selected_next_stage(), None);
        picker.cycle_next_stage(1);
        assert_eq!(picker.selected_next_stage(), Some("execution"));
        picker.cycle_next_stage(1);
        assert_eq!(picker.selected_next_stage(), Some("planning"));
        picker.cycle_next_stage(1);
        assert_eq!(picker.selected_next_stage(), None, "список замкнут");
    }

    // --- 4.2 Отказ загрузки виден и объясним ---

    #[test]
    fn failed_list_keeps_reason_with_server_address() {
        let mut state = test_state();
        handle_chats_loaded(
            Ok(vec![summary("chat-1", "Первый", 0)]),
            &mut state,
            &channel(),
        );
        handle_chats_loaded(Err("сервис не ответил".to_string()), &mut state, &channel());

        let reason = state.blocked_reason().expect("причина недоступности");
        assert!(reason.contains("сервис не ответил"), "причина потеряна: {reason}");
        assert!(
            reason.contains(&state.config.effective_server_url()),
            "адрес сервиса не назван: {reason}"
        );
        assert!(state.chats.is_empty());
        assert!(state.panes.is_empty());
    }

    // --- 4.3 Без списка чатов писать некуда ---

    #[test]
    fn loading_and_failed_states_block_actions() {
        let mut state = test_state();
        assert!(state.blocked_reason().is_some(), "во время загрузки писать нельзя");

        state.chats_load = ChatsLoad::Failed("сервис недоступен".to_string());
        assert_eq!(
            state.blocked_reason().as_deref(),
            Some("сервис недоступен"),
            "причина отказа должна объяснять запрет"
        );
    }

    // --- 4.4 История чата приходит отдельным запросом ---

    // Загрузка истории уходит фоновой задачей, поэтому тесту нужен runtime.
    #[tokio::test]
    async fn loaded_history_fills_chat_and_unblocks_input() {
        let mut state = test_state();
        handle_chats_loaded(Ok(vec![summary("chat-1", "Чат", 2)]), &mut state, &channel());
        state.chat_ui.entry("chat-1".to_string()).or_default().history_loading = true;

        handle_history_loaded(
            "chat-1".to_string(),
            Ok(ChatHistory {
                chat: summary("chat-1", "Заголовок сервиса", 2),
                branch_id: None,
                messages: vec![
                    agentclient::StoredMessage {
                        seq: 1,
                        created_at: 1001,
                        message: Message::user("вопрос"),
                    },
                    agentclient::StoredMessage {
                        seq: 2,
                        created_at: 1002,
                        message: Message::assistant("ответ"),
                    },
                ],
            }),
            &mut state,
        );

        assert!(state.chats[0].history_loaded);
        assert_eq!(state.chats[0].title, "Заголовок сервиса");
        assert_eq!(state.chats[0].messages.len(), 2);
        assert!(!state.chat_ui["chat-1"].history_loading);
    }

    // Загрузка истории уходит фоновой задачей, поэтому тесту нужен runtime.
    #[tokio::test]
    async fn failed_history_is_reported_and_leaves_chat_unloaded() {
        let mut state = test_state();
        handle_chats_loaded(Ok(vec![summary("chat-1", "Чат", 2)]), &mut state, &channel());

        handle_history_loaded("chat-1".to_string(), Err("нет связи".to_string()), &mut state);

        assert!(!state.chats[0].history_loaded);
        assert_eq!(state.chat_ui["chat-1"].history_error.as_deref(), Some("нет связи"));
    }

    // --- 4.5 Чат создаёт сервис ---

    #[test]
    fn created_chat_opens_with_service_identifier() {
        let mut state = test_state();
        handle_chats_loaded(Ok(Vec::new()), &mut state, &channel());
        state.creating_chat = true;

        handle_chat_created(Ok(summary("chat-new", "Новый чат", 0)), &mut state);

        assert!(!state.creating_chat);
        assert_eq!(state.chats[0].id, "chat-new");
        assert_eq!(state.active_chat_id().as_deref(), Some("chat-new"));
    }

    // --- 4.8 Отклонённое изменение не применяется ---

    #[test]
    fn rejected_create_leaves_list_untouched() {
        let mut state = test_state();
        handle_chats_loaded(Ok(vec![summary("chat-1", "Первый", 0)]), &mut state, &channel());
        state.creating_chat = true;

        handle_chat_created(Err("400 invalid_request (request_id: req-1)".to_string()), &mut state);

        assert_eq!(state.chats.len(), 1, "список не должен меняться");
        assert!(state.active_notice().expect("уведомление").contains("req-1"));
    }

    #[test]
    fn rejected_update_keeps_confirmed_settings() {
        let mut state = test_state();
        handle_chats_loaded(Ok(vec![summary("chat-1", "Первый", 0)]), &mut state, &channel());

        handle_chat_updated(
            "chat-1".to_string(),
            Err("сервис недоступен (request_id: req-2)".to_string()),
            &mut state,
        );

        assert_eq!(state.chats[0].title, "Первый");
        assert!(state.active_notice().expect("уведомление").contains("req-2"));
    }

    #[test]
    fn confirmed_update_applies_service_values() {
        let mut state = test_state();
        handle_chats_loaded(Ok(vec![summary("chat-1", "Первый", 0)]), &mut state, &channel());

        handle_chat_updated(
            "chat-1".to_string(),
            Ok(summary("chat-1", "Переименован", 0)),
            &mut state,
        );

        assert_eq!(state.chats[0].title, "Переименован");
    }

    // --- 4.7 Удаление подтверждает сервис ---

    #[test]
    fn confirmed_delete_frees_pane_and_allows_empty_list() {
        let mut state = test_state();
        handle_chats_loaded(Ok(vec![summary("chat-1", "Первый", 0)]), &mut state, &channel());

        handle_chat_deleted("chat-1".to_string(), Ok(()), &mut state);

        assert!(state.chats.is_empty());
        assert!(state.panes.is_empty(), "панель освобождена вместе с чатом");
        assert!(state.active_chat_id().is_none());
    }

    #[test]
    fn rejected_delete_keeps_chat_in_list() {
        let mut state = test_state();
        handle_chats_loaded(Ok(vec![summary("chat-1", "Первый", 0)]), &mut state, &channel());

        handle_chat_deleted("chat-1".to_string(), Err("нет связи".to_string()), &mut state);

        assert_eq!(state.chats.len(), 1);
        assert_eq!(state.panes, vec!["chat-1".to_string()]);
    }

    // --- 5.3 Несохранённый обмен помечен и повторяем ---

    #[test]
    fn failed_append_keeps_exchange_for_retry() {
        let mut state = test_state();
        handle_chats_loaded(Ok(vec![summary("chat-1", "Первый", 0)]), &mut state, &channel());
        state.chat_ui.entry("chat-1".to_string()).or_default().unsaved = Some(UnsavedExchange {
            reason: "ожидание".to_string(),
            messages: vec![Message::user("вопрос"), Message::assistant("ответ")],
        });

        handle_exchange_saved(
            "chat-1".to_string(),
            Err("сервис недоступен (request_id: req-3)".to_string()),
            &mut state,
        );

        let unsaved = state.chat_ui["chat-1"].unsaved.as_ref().expect("обмен сохранён для повтора");
        assert_eq!(unsaved.messages.len(), 2, "реплики не должны теряться");
        assert!(unsaved.reason.contains("req-3"), "причина без request_id: {}", unsaved.reason);
        assert!(state.active_notice().expect("уведомление").contains("Ctrl+U"));
    }

    #[test]
    fn successful_append_clears_unsaved_mark() {
        let mut state = test_state();
        handle_chats_loaded(Ok(vec![summary("chat-1", "Первый", 0)]), &mut state, &channel());
        state.chat_ui.entry("chat-1".to_string()).or_default().unsaved = Some(UnsavedExchange {
            reason: "ожидание".to_string(),
            messages: vec![Message::user("вопрос")],
        });

        handle_exchange_saved("chat-1".to_string(), Ok(()), &mut state);

        assert!(state.chat_ui["chat-1"].unsaved.is_none());
    }

    // --- 4.9 Перенос контекста опирается на историю сервиса ---

    #[tokio::test]
    async fn import_uses_loaded_history_and_keeps_source_timestamp() {
        let mut state = test_state();
        handle_chats_loaded(
            Ok(vec![summary("target", "Цель", 0), summary("source", "Источник", 1)]),
            &mut state,
            &channel(),
        );
        handle_history_loaded(
            "source".to_string(),
            Ok(ChatHistory {
                chat: summary("source", "Источник", 1),
                branch_id: None,
                messages: vec![agentclient::StoredMessage {
                    seq: 1,
                    created_at: 1001,
                    message: Message::user("важный контекст"),
                }],
            }),
            &mut state,
        );
        let source_index = state.chat_index("source").expect("чат-источник");
        let source_updated_at = state.chats[source_index].updated_at;

        import_context(&mut state, "target", &["source".to_string()], &channel());

        let target = &state.chats[state.chat_index("target").expect("целевой чат")];
        assert_eq!(target.messages.len(), 1, "контекст переносится одной репликой");
        assert!(target.messages[0].content.contains("важный контекст"));
        assert_eq!(
            state.chats[state.chat_index("source").expect("чат-источник")].updated_at,
            source_updated_at,
            "перенос не меняет время изменения чата-источника"
        );
        assert_eq!(target.title, "Цель", "перенос не меняет заголовок");
    }

    #[tokio::test]
    async fn import_is_refused_while_source_history_is_missing() {
        let mut state = test_state();
        handle_chats_loaded(
            Ok(vec![summary("target", "Цель", 0), summary("source", "Источник", 3)]),
            &mut state,
            &channel(),
        );

        import_context(&mut state, "target", &["source".to_string()], &channel());

        let target = &state.chats[state.chat_index("target").expect("целевой чат")];
        assert!(target.messages.is_empty(), "без истории источника перенос не выполняется");
        assert!(state
            .active_notice()
            .expect("уведомление")
            .contains("не загружена"));
    }

    // --- Список перезагружается, не теряя загруженную историю ---

    // Загрузка истории уходит фоновой задачей, поэтому тесту нужен runtime.
    #[tokio::test]
    async fn reload_keeps_already_loaded_history() {
        let mut state = test_state();
        handle_chats_loaded(Ok(vec![summary("chat-1", "Первый", 1)]), &mut state, &channel());
        handle_history_loaded(
            "chat-1".to_string(),
            Ok(ChatHistory {
                chat: summary("chat-1", "Первый", 1),
                branch_id: None,
                messages: vec![agentclient::StoredMessage {
                    seq: 1,
                    created_at: 1001,
                    message: Message::user("вопрос"),
                }],
            }),
            &mut state,
        );

        handle_chats_loaded(Ok(vec![summary("chat-1", "Первый", 1)]), &mut state, &channel());

        assert!(state.chats[0].history_loaded, "повторный запрос истории не нужен");
        assert_eq!(state.chats[0].messages.len(), 1);
    }

    fn assistant_with_meta(meta: MessageMeta) -> Message {
        let mut message = Message::assistant("ответ");
        message.meta = Some(meta);
        message
    }

    #[test]
    fn chat_token_totals_sums_partial_fields_across_messages() {
        let messages = vec![
            Message::user("вопрос"),
            assistant_with_meta(MessageMeta {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                total_tokens: Some(15),
                reasoning_tokens: None,
                ..Default::default()
            }),
            Message::user("ещё вопрос"),
            assistant_with_meta(MessageMeta {
                prompt_tokens: Some(20),
                completion_tokens: Some(8),
                total_tokens: None,
                reasoning_tokens: Some(3),
                ..Default::default()
            }),
        ];

        let totals = chat_token_totals(&messages);

        assert_eq!(totals.prompt, Some(30));
        assert_eq!(totals.completion, Some(13));
        assert_eq!(totals.reasoning, Some(3));
        // total_tokens второго сообщения отсутствует, но TokenTotals::from_meta
        // достраивает его как prompt+completion, поэтому в итоге складываются
        // оба варианта: явные 15 и достроенные 28.
        assert_eq!(totals.total, Some(43));
    }

    #[test]
    fn chat_token_totals_of_empty_history_has_no_fields() {
        let totals = chat_token_totals(&[]);

        assert_eq!(totals.prompt, None);
        assert_eq!(totals.completion, None);
        assert_eq!(totals.reasoning, None);
        assert_eq!(totals.total, None);
    }

    #[test]
    fn token_counters_is_none_for_empty_totals() {
        assert!(token_counters(&TokenTotals::default()).is_none());
    }

    #[test]
    fn token_counters_orders_parts_prompt_completion_reasoning_total() {
        let totals = TokenTotals {
            prompt: Some(10),
            completion: Some(5),
            reasoning: Some(2),
            total: Some(15),
        };

        let summary = token_counters(&totals).expect("есть телеметрия");

        assert_eq!(summary, "↑ запрос 10 · ↓ ответ 5 · рассужд. 2 · всего 15 ток.");
    }

    #[test]
    fn token_footer_line_is_none_without_any_telemetry() {
        let messages = vec![Message::user("вопрос"), Message::assistant("ответ")];

        assert!(token_footer_line(&messages).is_none());
    }

    #[test]
    fn token_footer_line_combines_exchange_and_chat_totals() {
        let messages = vec![assistant_with_meta(MessageMeta {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: Some(15),
            ..Default::default()
        })];

        let footer = token_footer_line(&messages).expect("есть телеметрия");

        assert!(footer.starts_with("Обмен: "), "footer: {footer}");
        assert!(footer.contains("Чат: "), "footer: {footer}");
    }

    // --- 8.2 Строка стратегии контекста ---

    #[test]
    fn context_status_line_omits_fields_of_other_strategies() {
        use agentcore::config::{ContextObservability, ContextStrategy};

        let context = ContextObservability {
            strategy: Some(ContextStrategy::SlidingWindow),
            sent_messages: Some(6),
            dropped_messages: Some(14),
            ..ContextObservability::default()
        };

        let line = context_status_line(&context);

        assert!(line.contains("отправлено 6"));
        assert!(line.contains("отброшено 14"));
        assert!(!line.contains("факт"), "поля стратегии facts не должны выводиться: {line}");
        assert!(!line.contains("ветка"), "поле стратегии branching не должно выводиться: {line}");
    }

    #[test]
    fn context_status_line_shows_memory_layers_breakdown_alongside_strategy() {
        use agentcore::config::{ContextObservability, ContextStrategy};

        let context = ContextObservability {
            strategy: Some(ContextStrategy::SlidingWindow),
            memory_long_term_entries: Some(2),
            memory_long_term_chars: Some(40),
            memory_working_entries: Some(1),
            memory_working_chars: Some(10),
            memory_short_term_messages: Some(4),
            memory_short_term_chars: Some(80),
            memory_router_applied_set: Some(1),
            memory_router_rejected: Some(1),
            ..ContextObservability::default()
        };

        let line = context_status_line(&context);

        assert!(line.contains("долговременная 2"));
        assert!(line.contains("рабочая 1"));
        assert!(line.contains("краткосрочная 4 сообщ."));
        assert!(line.contains("применено 1"));
        assert!(line.contains("отброшено 1"));
    }

    // --- 8.2 Экран памяти ---

    #[test]
    fn memory_picker_cycles_sections_and_moves_cursor_within_section() {
        let mut picker = MemoryPicker::new("chat-1", "Чат");
        picker.working = vec![
            WorkingMemoryEntry { key: "a".to_string(), value: "1".to_string(), source: "manual".to_string(), updated_at: 1 },
            WorkingMemoryEntry { key: "b".to_string(), value: "2".to_string(), source: "manual".to_string(), updated_at: 2 },
        ];
        assert!(matches!(picker.current_section(), MemorySection::ShortTerm));
        picker.cycle_section(1);
        assert!(matches!(picker.current_section(), MemorySection::Working));
        picker.move_cursor(1);
        assert_eq!(picker.working_cursor, 1);
        assert_eq!(picker.selected_working_key(), Some("b"));
        picker.cycle_section(1);
        assert!(matches!(picker.current_section(), MemorySection::LongTerm));
        picker.cycle_section(1);
        assert!(matches!(picker.current_section(), MemorySection::ShortTerm), "цикл разделов замкнут");
    }

    // --- 8.3 Экран фактов ---

    fn fact(key: &str, value: &str) -> Fact {
        Fact {
            key: key.to_string(),
            value: value.to_string(),
            through_seq: 1,
            updated_at: 0,
        }
    }

    #[test]
    fn facts_loaded_populates_picker() {
        let mut state = test_state();
        state.facts = Some(FactsPicker::new("chat-1", "Чат"));

        handle_facts_loaded(
            "chat-1".to_string(),
            Ok(vec![fact("budget", "200000"), fact("deadline", "март")]),
            &mut state,
        );

        let picker = state.facts.expect("экран фактов открыт");
        assert!(!picker.loading);
        assert_eq!(picker.facts.len(), 2);
        assert!(picker.error.is_none());
    }

    #[test]
    fn facts_load_failure_is_reported_without_closing_picker() {
        let mut state = test_state();
        state.facts = Some(FactsPicker::new("chat-1", "Чат"));

        handle_facts_loaded("chat-1".to_string(), Err("сервис недоступен".to_string()), &mut state);

        let picker = state.facts.expect("экран фактов остаётся открытым");
        assert_eq!(picker.error.as_deref(), Some("сервис недоступен"));
    }

    #[test]
    fn fact_set_updates_existing_key_in_place() {
        let mut state = test_state();
        let mut picker = FactsPicker::new("chat-1", "Чат");
        picker.facts = vec![fact("budget", "200000")];
        picker.editor = Some(FactEditor { key: "budget".to_string(), value: "300000".to_string(), editing_key: false });
        state.facts = Some(picker);

        handle_fact_set("chat-1".to_string(), Ok(fact("budget", "300000")), &mut state);

        let picker = state.facts.expect("экран фактов");
        assert_eq!(picker.facts.len(), 1, "правка не создаёт вторую запись");
        assert_eq!(picker.facts[0].value, "300000");
        assert!(picker.editor.is_none(), "редактор закрывается после сохранения");
    }

    #[test]
    fn fact_deleted_removes_key_from_list() {
        let mut state = test_state();
        let mut picker = FactsPicker::new("chat-1", "Чат");
        picker.facts = vec![fact("budget", "200000"), fact("deadline", "март")];
        state.facts = Some(picker);

        handle_fact_deleted("chat-1".to_string(), Ok("budget".to_string()), &mut state);

        let picker = state.facts.expect("экран фактов");
        assert_eq!(picker.facts.len(), 1);
        assert_eq!(picker.facts[0].key, "deadline");
    }

    // --- 8.4 Экран веток ---

    fn branch(id: &str, name: &str, active: bool) -> Branch {
        Branch {
            id: id.to_string(),
            name: name.to_string(),
            parent_id: None,
            fork_seq: None,
            message_count: 2,
            active,
        }
    }

    #[test]
    fn branches_loaded_populates_picker_with_active_marker() {
        let mut state = test_state();
        state.branches = Some(BranchesPicker::new("chat-1", "Чат"));

        handle_branches_loaded(
            "chat-1".to_string(),
            Ok(vec![branch("root", "root", true), branch("b2", "альтернатива", false)]),
            &mut state,
        );

        let picker = state.branches.expect("экран веток");
        assert!(!picker.loading);
        assert_eq!(picker.branches.len(), 2);
        assert!(picker.branches[0].active);
    }

    #[tokio::test]
    async fn branch_activated_marks_chat_history_for_reload() {
        let mut state = test_state();
        handle_chats_loaded(Ok(vec![summary("chat-1", "Чат", 4)]), &mut state, &channel());
        handle_history_loaded(
            "chat-1".to_string(),
            Ok(ChatHistory {
                chat: summary("chat-1", "Чат", 4),
                branch_id: None,
                messages: vec![agentclient::StoredMessage {
                    seq: 1,
                    created_at: 1001,
                    message: Message::user("вопрос"),
                }],
            }),
            &mut state,
        );
        assert!(state.chats[0].history_loaded);

        handle_branch_activated("chat-1".to_string(), Ok("b2".to_string()), &mut state, &channel());

        assert!(
            !state.chats[0].history_loaded,
            "переключение ветки должно потребовать повторной загрузки истории"
        );
    }

    // --- 2.3/2.5 Режим выбора сообщения ---

    #[test]
    fn shift_selection_stops_at_history_bounds() {
        assert_eq!(shift_selection(2, 5, true), 3);
        assert_eq!(shift_selection(2, 5, false), 1);
        // на краях выбор остаётся на месте, а не идёт по кругу
        assert_eq!(shift_selection(4, 5, true), 4);
        assert_eq!(shift_selection(0, 5, false), 0);
        assert_eq!(shift_selection(0, 0, true), 0);
    }

    #[test]
    fn normalize_selection_clamps_to_history_length() {
        assert_eq!(normalize_selection(Some(7), 3), Some(2));
        assert_eq!(normalize_selection(Some(1), 3), Some(1));
        assert_eq!(normalize_selection(Some(0), 0), None);
        assert_eq!(normalize_selection(None, 3), None);
    }

    #[tokio::test]
    async fn reloaded_shorter_history_clamps_selected_message() {
        let mut state = test_state();
        handle_chats_loaded(Ok(vec![summary("chat-1", "Чат", 1)]), &mut state, &channel());
        state.chat_ui.entry("chat-1".to_string()).or_default().selected_message = Some(5);

        handle_history_loaded(
            "chat-1".to_string(),
            Ok(ChatHistory {
                chat: summary("chat-1", "Чат", 1),
                branch_id: None,
                messages: vec![StoredMessage {
                    seq: 1,
                    created_at: 1001,
                    message: Message::user("вопрос"),
                }],
            }),
            &mut state,
        );

        assert_eq!(
            state.chat_ui["chat-1"].selected_message,
            Some(0),
            "выбор должен упереться в последнее сообщение перезагруженной истории"
        );
    }

    #[tokio::test]
    async fn reloaded_empty_history_drops_selected_message() {
        let mut state = test_state();
        handle_chats_loaded(Ok(vec![summary("chat-1", "Чат", 0)]), &mut state, &channel());
        state.chat_ui.entry("chat-1".to_string()).or_default().selected_message = Some(2);

        handle_history_loaded(
            "chat-1".to_string(),
            Ok(ChatHistory {
                chat: summary("chat-1", "Чат", 0),
                branch_id: None,
                messages: Vec::new(),
            }),
            &mut state,
        );

        assert_eq!(state.chat_ui["chat-1"].selected_message, None);
    }

    fn block(text: &str, height: usize) -> (Vec<Line<'static>>, usize) {
        ((0..height).map(|_| Line::raw(text.to_string())).collect(), height)
    }

    fn window(
        blocks: &[(Vec<Line<'static>>, usize)],
        scroll: usize,
        visible: usize,
    ) -> (Vec<String>, u16) {
        let (lines, offset) = visible_window(
            blocks.iter().map(|(lines, height)| (lines.as_slice(), *height)),
            scroll,
            visible,
        );
        let texts = lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();
        (texts, offset)
    }

    #[test]
    fn wheel_finds_chat_under_cursor() {
        let areas = vec![
            ("left".to_string(), Rect::new(0, 1, 20, 10)),
            ("right".to_string(), Rect::new(20, 1, 20, 10)),
        ];
        assert_eq!(chat_at_position(&areas, 5, 5), Some("left"));
        assert_eq!(chat_at_position(&areas, 25, 5), Some("right"));
        // Левый верхний угол принадлежит области, правый нижний — уже нет.
        assert_eq!(chat_at_position(&areas, 0, 1), Some("left"));
        assert_eq!(chat_at_position(&areas, 20, 11), None);
        // Над боковой панелью и строкой подсказки истории нет.
        assert_eq!(chat_at_position(&areas, 5, 0), None);
        assert_eq!(chat_at_position(&[], 5, 5), None);
    }

    #[test]
    fn window_keeps_everything_when_history_fits() {
        let blocks = [block("a", 2), block("b", 3)];
        let (lines, offset) = window(&blocks, 0, 10);
        assert_eq!(offset, 0);
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn window_skips_blocks_above_scroll() {
        let blocks = [block("a", 4), block("b", 4), block("c", 4)];
        // Прокрутка ровно на границе блока: первый выпадает целиком.
        let (lines, offset) = window(&blocks, 4, 4);
        assert_eq!(offset, 0);
        assert_eq!(lines.first().map(String::as_str), Some("b"));
        assert!(lines.iter().all(|line| line != "a"));
    }

    #[test]
    fn window_offsets_inside_first_visible_block() {
        let blocks = [block("a", 4), block("b", 4)];
        // Прокрутка внутрь первого блока: он остаётся, но со смещением.
        let (lines, offset) = window(&blocks, 2, 4);
        assert_eq!(offset, 2);
        assert_eq!(lines.first().map(String::as_str), Some("a"));
        // Видимое окно (2..6) задевает оба блока.
        assert!(lines.iter().any(|line| line == "b"));
    }

    #[test]
    fn window_stops_below_viewport() {
        let blocks = [block("a", 4), block("b", 4), block("c", 4)];
        let (lines, _) = window(&blocks, 0, 5);
        // Третий блок начинается за нижней границей окна и не собирается.
        assert!(lines.iter().all(|line| line != "c"));
    }

    #[test]
    fn window_ignores_empty_blocks() {
        let blocks = [block("a", 0), block("b", 3)];
        let (lines, offset) = window(&blocks, 1, 2);
        assert_eq!(offset, 1);
        assert_eq!(lines.first().map(String::as_str), Some("b"));
    }
}
