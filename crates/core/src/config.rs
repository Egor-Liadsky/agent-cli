use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Адрес сервиса `agentd` по умолчанию, если он не задан в конфиге.
pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:8080";
/// Модель по умолчанию, если она не задана ни в чате, ни в конфиге.
pub const DEFAULT_MODEL: &str = "deepseek-v4-flash";
/// Адрес локального сервера Ollama по умолчанию.
pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Модели, между которыми можно переключаться стрелками в настройках чата.
/// Список — только подсказка: в поле модели можно ввести любое имя,
/// а свой набор задаётся в конфиге полем `models`.
pub const KNOWN_MODELS: [&str; 6] = [
    "deepseek-v4-flash",
    "deepseek-chat",
    "deepseek-reasoner",
    "gpt-4o",
    "gpt-4o-mini",
    "o3-mini",
];

/// Кто отвечает в чате: облачный API или локальная модель через Ollama.
#[derive(Serialize, Deserialize, Default, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    /// Облачная модель через сервис `agentd`: ключ провайдера у сервиса.
    #[default]
    Cloud,
    /// Локальный Ollama: нативный /api/chat, ключ не нужен.
    Ollama,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Cloud, Provider::Ollama];

    pub fn label(self) -> &'static str {
        match self {
            Provider::Cloud => "Облачный API",
            Provider::Ollama => "Ollama (локально)",
        }
    }

    pub fn parse(value: &str) -> Option<Provider> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cloud" | "api" | "облако" => Some(Provider::Cloud),
            "ollama" | "local" | "локально" => Some(Provider::Ollama),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct ResponseFormat {
    /// Описание формата ответа (например: "отвечай маркированным списком")
    pub description: Option<String>,
    /// Ограничение на длину ответа в токенах (max_tokens)
    pub max_length: Option<u32>,
    /// Stop-последовательности: API оборвёт ответ, встретив одну из них
    pub stop: Option<Vec<String>>,
    /// Явная инструкция модели о том, когда завершать ответ
    pub stop_instruction: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct SamplingParams {
    /// Температура сэмплирования (обычно 0.0 - 2.0)
    pub temperature: Option<f32>,
    /// Top-p (nucleus sampling), 0.0 - 1.0
    pub top_p: Option<f32>,
    /// Top-k сэмплирование
    pub top_k: Option<u32>,
    /// Штраф за частоту повторения токенов
    pub frequency_penalty: Option<f32>,
    /// Штраф за присутствие токена в тексте
    pub presence_penalty: Option<f32>,
}

/// Встроенный режим размышления модели (thinking / reasoning_content).
/// Это не стратегия промпта, а параметр запроса к API.
#[derive(Serialize, Deserialize, Default, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingMode {
    /// Ничего не отправлять — модель решает сама (поведение API по умолчанию)
    #[default]
    Auto,
    /// Явно включить размышление
    Enabled,
    /// Явно выключить размышление: ответ без цепочки рассуждений и без
    /// токенов на неё
    Disabled,
}

impl ThinkingMode {
    pub const ALL: [ThinkingMode; 3] = [
        ThinkingMode::Auto,
        ThinkingMode::Enabled,
        ThinkingMode::Disabled,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ThinkingMode::Auto => "Авто (как решит модель)",
            ThinkingMode::Enabled => "Включено",
            ThinkingMode::Disabled => "Выключено",
        }
    }

    /// Значение поля `thinking.type` в запросе. `None` — поле не отправляется,
    /// чтобы не ломать провайдеров, которые его не знают.
    pub fn api_value(self) -> Option<&'static str> {
        match self {
            ThinkingMode::Auto => None,
            ThinkingMode::Enabled => Some("enabled"),
            ThinkingMode::Disabled => Some("disabled"),
        }
    }

    pub fn parse(value: &str) -> Option<ThinkingMode> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" | "авто" => Some(ThinkingMode::Auto),
            "on" | "enabled" | "true" | "вкл" => Some(ThinkingMode::Enabled),
            "off" | "disabled" | "false" | "выкл" => Some(ThinkingMode::Disabled),
            _ => None,
        }
    }
}

/// Стратегия рассуждения агента: подмешивается в системный промпт чата.
#[derive(Serialize, Deserialize, Default, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningMode {
    /// Без дополнительных инструкций — обычный ответ модели
    #[default]
    Default,
    /// Пошаговое решение задачи с явными шагами и выводом
    StepByStep,
    /// Сначала составить качественный промпт для решения задачи, потом решить по нему
    PromptCraft,
    /// Группа экспертов (аналитик, инженер, критик) обсуждает задачу и даёт общий ответ
    ExpertPanel,
}

impl ReasoningMode {
    pub const ALL: [ReasoningMode; 4] = [
        ReasoningMode::Default,
        ReasoningMode::StepByStep,
        ReasoningMode::PromptCraft,
        ReasoningMode::ExpertPanel,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ReasoningMode::Default => "По умолчанию",
            ReasoningMode::StepByStep => "Решать пошагово",
            ReasoningMode::PromptCraft => "Составить промпт для решения",
            ReasoningMode::ExpertPanel => "Группа экспертов",
        }
    }

    /// Состав группы экспертов по умолчанию, если пользователь не задал свой.
    pub const DEFAULT_EXPERTS: [&'static str; 3] = [
        "аналитик (уточняет постановку и риски)",
        "инженер (предлагает конкретное решение)",
        "критик (ищет слабые места и предлагает правки)",
    ];

    /// Часть системного промпта для выбранной стратегии.
    /// `experts` учитывается только для режима «Группа экспертов»;
    /// пустой список означает состав по умолчанию.
    pub fn system_prompt(self, experts: &[String]) -> Option<String> {
        match self {
            ReasoningMode::Default => None,
            ReasoningMode::StepByStep => Some(
                "Решай задачу пошагово. Сначала разбей её на пронумерованные шаги, \
                 выполни каждый шаг по порядку, показывая промежуточные выводы, \
                 затем дай итоговый ответ отдельным блоком «Итог»."
                    .to_string(),
            ),
            ReasoningMode::PromptCraft => Some(
                "Сначала составь подробный промпт, который лучше всего описывает задачу: \
                 цель, контекст, ограничения, критерии хорошего решения и формат ответа. \
                 Покажи этот промпт в блоке «Промпт», затем реши задачу по нему \
                 и дай ответ в блоке «Решение»."
                    .to_string(),
            ),
            ReasoningMode::ExpertPanel => {
                let roles: Vec<String> = if experts.is_empty() {
                    Self::DEFAULT_EXPERTS.iter().map(|e| e.to_string()).collect()
                } else {
                    experts.to_vec()
                };
                Some(format!(
                    "Разбери задачу как группа экспертов: {}. Покажи короткую реплику \
                     каждого эксперта под его именем, затем дай согласованный итоговый \
                     ответ в блоке «Итог».",
                    roles.join(", ")
                ))
            }
        }
    }

    /// Разбор значения из CLI.
    pub fn parse(value: &str) -> Option<ReasoningMode> {
        match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "default" | "по-умолчанию" => Some(ReasoningMode::Default),
            "step-by-step" | "steps" | "пошагово" => Some(ReasoningMode::StepByStep),
            "prompt-craft" | "prompt" | "промпт" => Some(ReasoningMode::PromptCraft),
            "expert-panel" | "experts" | "эксперты" => Some(ReasoningMode::ExpertPanel),
            _ => None,
        }
    }
}

/// Стратегия управления контекстом чата на сервисе `agentd`: какой набор
/// сообщений уходит провайдеру при каждом запросе.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextStrategy {
    /// Компактизация пересказом (существующее поведение).
    Summary,
    /// Только последние N сообщений, без пересказа отброшенного.
    SlidingWindow,
    /// Устойчивые факты «ключ-значение» плюс хвост истории.
    Facts,
    /// Ветвление диалога: история собирается по цепочке активной ветки.
    Branching,
}

impl ContextStrategy {
    pub const ALL: [ContextStrategy; 4] = [
        ContextStrategy::Summary,
        ContextStrategy::SlidingWindow,
        ContextStrategy::Facts,
        ContextStrategy::Branching,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ContextStrategy::Summary => "Пересказ (summary)",
            ContextStrategy::SlidingWindow => "Окно последних сообщений",
            ContextStrategy::Facts => "Устойчивые факты",
            ContextStrategy::Branching => "Ветвление диалога",
        }
    }

    /// Имя, которое сервис `agentd` принимает в JSON (`snake_case`).
    pub fn as_str(self) -> &'static str {
        match self {
            ContextStrategy::Summary => "summary",
            ContextStrategy::SlidingWindow => "sliding_window",
            ContextStrategy::Facts => "facts",
            ContextStrategy::Branching => "branching",
        }
    }

    pub fn parse(value: &str) -> Option<ContextStrategy> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "summary" => Some(ContextStrategy::Summary),
            "sliding_window" => Some(ContextStrategy::SlidingWindow),
            "facts" => Some(ContextStrategy::Facts),
            "branching" => Some(ContextStrategy::Branching),
            _ => None,
        }
    }
}

/// Что действующая стратегия контекста сделала при сборке истории для этого
/// запроса. Поля, не имеющие смысла для стратегии, отсутствуют — а не несут
/// ноль или `false`.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct ContextObservability {
    /// Стратегия, действовавшая на этом запросе. Отсутствует у ответов
    /// сервисов без выбора стратегии (совместимость со старым контрактом).
    #[serde(default)]
    pub strategy: Option<ContextStrategy>,
    /// Сколько сообщений отправлено провайдеру.
    #[serde(default)]
    pub sent_messages: Option<u32>,
    /// Сколько сохранённых сообщений отброшено стратегией (`sliding_window`,
    /// `facts`).
    #[serde(default)]
    pub dropped_messages: Option<u32>,
    /// Сколько сохранённых сообщений заменено пересказом на этом запросе
    /// (стратегия `summary`).
    #[serde(default)]
    pub replaced_messages: Option<u32>,
    /// Строился ли новый пересказ на этом запросе (стратегия `summary`).
    #[serde(default)]
    pub summary_built: Option<bool>,
    /// Сколько фактов подставлено в запрос (стратегия `facts`).
    #[serde(default)]
    pub facts_applied: Option<u32>,
    /// Обновились ли факты после ответа (стратегия `facts`).
    #[serde(default)]
    pub facts_updated: Option<bool>,
    /// Ветка, из которой собрана история (стратегия `branching`).
    #[serde(default)]
    pub branch_id: Option<String>,
    /// Число записей долговременной памяти в контексте (слоистая память включена).
    #[serde(default)]
    pub memory_long_term_entries: Option<u32>,
    /// Объём долговременной памяти в контексте, в символах (слоистая память включена).
    #[serde(default)]
    pub memory_long_term_chars: Option<u32>,
    /// Число записей рабочей памяти в контексте (слоистая память включена).
    #[serde(default)]
    pub memory_working_entries: Option<u32>,
    /// Объём рабочей памяти в контексте, в символах (слоистая память включена).
    #[serde(default)]
    pub memory_working_chars: Option<u32>,
    /// Число сообщений краткосрочной истории, фактически собранной действующей
    /// стратегией контекста (слоистая память включена).
    #[serde(default)]
    pub memory_short_term_messages: Option<u32>,
    /// Объём этой краткосрочной истории, в символах (слоистая память включена).
    #[serde(default)]
    pub memory_short_term_chars: Option<u32>,
    /// Сколько операций `set` маршрутизатора памяти применено — новые
    /// ключи, счётчики относятся к маршрутизации ПРЕДЫДУЩЕГО сообщения
    /// (слоистая память включена).
    #[serde(default)]
    pub memory_router_applied_set: Option<u32>,
    /// Сколько операций маршрутизатора обновили существующий ключ (слоистая память включена).
    #[serde(default)]
    pub memory_router_applied_update: Option<u32>,
    /// Сколько операций `delete` маршрутизатора применено (слоистая память включена).
    #[serde(default)]
    pub memory_router_applied_delete: Option<u32>,
    /// Сколько операций маршрутизатора отброшено валидацией (слоистая память включена).
    #[serde(default)]
    pub memory_router_rejected: Option<u32>,
    /// Этап активной задачи чата (состояние задачи включено).
    #[serde(default)]
    pub task_stage: Option<String>,
    /// Текущий шаг активной задачи (состояние задачи включено).
    #[serde(default)]
    pub task_step: Option<String>,
    /// Ожидаемое действие активной задачи (состояние задачи включено).
    #[serde(default)]
    pub task_expected_action: Option<String>,
    /// Признак паузы активной задачи (состояние задачи включено).
    #[serde(default)]
    pub task_paused: Option<bool>,
    /// Число переходов, применённых последним завершившимся прогоном
    /// автоматического трекера состояния задачи — по ПРЕДЫДУЩЕМУ сообщению,
    /// трекер фоновый (состояние задачи включено).
    #[serde(default)]
    pub task_tracker_applied: Option<u32>,
    /// Число переходов, отклонённых тем же прогоном трекера (состояние
    /// задачи включено).
    #[serde(default)]
    pub task_tracker_rejected: Option<u32>,
}

/// Параметры агента, привязанные к конкретному чату.
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct ChatSettings {
    /// Откуда берётся ответ: облачный API или локальная модель Ollama.
    #[serde(default)]
    pub provider: Provider,
    /// Модель, которой отвечает этот чат. `None` — модель из глобального
    /// конфига, а если и там пусто — `DEFAULT_MODEL`.
    #[serde(default)]
    pub model: Option<String>,
    /// Режим ответа: false — дефолтный, true — кастомный (см. response_format)
    #[serde(default)]
    pub custom_response_mode: bool,
    #[serde(default)]
    pub response_format: ResponseFormat,
    #[serde(default)]
    pub sampling: SamplingParams,
    /// Стратегия рассуждения агента для этого чата
    #[serde(default)]
    pub reasoning: ReasoningMode,
    /// Встроенное размышление модели для этого чата
    #[serde(default)]
    pub thinking: ThinkingMode,
    /// Состав группы экспертов для режима «Группа экспертов».
    /// Пустой список — состав по умолчанию (аналитик, инженер, критик).
    #[serde(default)]
    pub experts: Vec<String>,
    /// Клиентский лимит контекста этого чата в токенах: сужает операторский
    /// лимит сервиса `agentd` (`settings.max_context_tokens` в
    /// `POST /v1/chat`). Не задано — поле не отправляется, действует
    /// операторский лимит сервиса по умолчанию.
    #[serde(default)]
    pub max_context_tokens: Option<u32>,
    /// Включает или выключает компактизацию истории на сервисе `agentd` для
    /// этого чата. `None` — операторское умолчание сервиса
    /// (`AGENTD_SUMMARY_ENABLED`).
    #[serde(default)]
    pub summary_enabled: Option<bool>,
    /// Сколько последних сообщений чата уходят провайдеру дословно при
    /// компактизации. `None` — операторское умолчание сервиса
    /// (`AGENTD_SUMMARY_KEEP_MESSAGES`).
    #[serde(default)]
    pub summary_keep_messages: Option<u32>,
    /// Шаг, с которым сервис перестраивает пересказ (в вытесненных
    /// сообщениях). `None` — операторское умолчание сервиса
    /// (`AGENTD_SUMMARY_STEP_MESSAGES`).
    #[serde(default)]
    pub summary_step_messages: Option<u32>,
    /// Стратегия управления контекстом этого чата. `None` — операторское
    /// умолчание сервиса (`AGENTD_CONTEXT_STRATEGY`).
    #[serde(default)]
    pub context_strategy: Option<ContextStrategy>,
    /// Размер окна последних сообщений для стратегий `sliding_window` и
    /// `facts`. `None` — операторское умолчание сервиса
    /// (`AGENTD_CONTEXT_WINDOW_MESSAGES`).
    #[serde(default)]
    pub context_window_messages: Option<u32>,
    /// Включает или выключает слоистую память (рабочий и долговременный
    /// слои) поверх действующей стратегии контекста. `None` — операторское
    /// умолчание сервиса (`AGENTD_MEMORY_LAYERS_ENABLED`, выключено по
    /// умолчанию).
    #[serde(default)]
    pub memory_layers_enabled: Option<bool>,
    /// Включает или выключает автоматический маршрутизатор записей памяти,
    /// когда слоистая память включена. `None` — операторское умолчание
    /// сервиса (`AGENTD_MEMORY_ROUTER_ENABLED`).
    #[serde(default)]
    pub memory_router_enabled: Option<bool>,
    /// Лимит числа записей рабочей памяти, подставляемых в контекст.
    /// `None` — операторское умолчание сервиса
    /// (`AGENTD_MEMORY_WORKING_MAX_ENTRIES`).
    #[serde(default)]
    pub memory_working_max_entries: Option<u32>,
    /// Лимит числа записей долговременной памяти, подставляемых в контекст.
    /// `None` — операторское умолчание сервиса
    /// (`AGENTD_MEMORY_LONG_TERM_MAX_ENTRIES`).
    #[serde(default)]
    pub memory_long_term_max_entries: Option<u32>,
    /// Профиль, применяемый к этому чату: встроенный (`teacher`,
    /// `psychologist`, `reviewer`) или собственный профиль владельца.
    /// `None` — операторское умолчание сервиса (`AGENTD_DEFAULT_PROFILE`,
    /// пусто — без профиля).
    #[serde(default)]
    pub profile_id: Option<String>,
    /// Включает или выключает состояние задачи (этап, шаг, ожидаемое
    /// действие, пауза) для этого чата. `None` — операторское умолчание
    /// сервиса (`AGENTD_TASK_STATE_ENABLED`, выключено по умолчанию).
    #[serde(default)]
    pub task_state_enabled: Option<bool>,
    /// Включает или выключает автоматический трекер состояния задачи, когда
    /// состояние задачи включено. `None` — операторское умолчание сервиса
    /// (`AGENTD_TASK_STATE_AUTO_ENABLED`).
    #[serde(default)]
    pub task_state_auto_enabled: Option<bool>,
    /// Подключает git-инструменты (`mcp-server-git`) к ходам этого чата.
    /// `None`/`false` — инструменты не подключаются. Сервис поле только
    /// хранит: инструменты запускает и выполняет клиент.
    #[serde(default)]
    pub git_tools_enabled: Option<bool>,
    /// Путь к репозиторию на машине клиента (`--repository` сервера).
    #[serde(default)]
    pub git_repository: Option<String>,
    /// Разрешённые модели инструменты. `None` — только читающие; пишущие
    /// попадают к модели, лишь если перечислены явно.
    #[serde(default)]
    pub git_allowed_tools: Option<Vec<String>>,
    /// Лимит итераций цикла инструментов. `None` —
    /// `DEFAULT_TOOL_MAX_ITERATIONS`, потолок — `MAX_TOOL_ITERATIONS`.
    #[serde(default)]
    pub tool_max_iterations: Option<u32>,
}

/// Лимит итераций цикла инструментов, если чат его не задал.
pub const DEFAULT_TOOL_MAX_ITERATIONS: u32 = 8;
/// Потолок лимита итераций: модель, зациклившаяся на вызовах, не должна
/// жечь запросы без конца даже при неосторожной настройке.
pub const MAX_TOOL_ITERATIONS: u32 = 32;

impl ChatSettings {
    /// Включены ли git-инструменты в этом чате.
    pub fn git_tools_active(&self) -> bool {
        self.git_tools_enabled == Some(true)
    }

    /// Действующий лимит итераций цикла инструментов: от 1 до потолка.
    pub fn effective_tool_max_iterations(&self) -> u32 {
        self.tool_max_iterations
            .unwrap_or(DEFAULT_TOOL_MAX_ITERATIONS)
            .clamp(1, MAX_TOOL_ITERATIONS)
    }

    /// Системный промпт выбранной стратегии рассуждения
    pub fn reasoning_prompt(&self) -> Option<String> {
        self.reasoning.system_prompt(&self.experts)
    }

    /// Настройки формата ответа, если включён кастомный режим
    pub fn active_response_format(&self) -> Option<&ResponseFormat> {
        if self.custom_response_mode {
            Some(&self.response_format)
        } else {
            None
        }
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Config {
    /// Адрес сервиса `agentd`. Облачная модель доступна только через него:
    /// ключ провайдера принадлежит сервису и клиенту не известен.
    pub server_url: Option<String>,
    /// Клиентский токен для заголовка `Authorization`. Пустой токен допустим:
    /// сервис с пустым списком токенов аутентификацию не проверяет.
    pub client_token: Option<String>,
    pub model: Option<String>,
    /// Провайдер по умолчанию для новых чатов.
    #[serde(default)]
    pub provider: Provider,
    /// Адрес локального Ollama. Пусто — `DEFAULT_OLLAMA_URL`.
    pub ollama_url: Option<String>,
    /// Локальная модель по умолчанию для чатов с провайдером Ollama.
    pub ollama_model: Option<String>,
    /// Свой список моделей для быстрого переключения в настройках чата.
    /// Пустой список — используется `KNOWN_MODELS`.
    #[serde(default)]
    pub models: Vec<String>,
    /// Режим ответа: false — дефолтный, true — кастомный (см. response_format)
    #[serde(default)]
    pub custom_response_mode: bool,
    #[serde(default)]
    pub response_format: ResponseFormat,
    /// Параметры сэмплирования модели (temperature, top_p, top_k и т.д.)
    #[serde(default)]
    pub sampling: SamplingParams,
    /// Стратегия рассуждения по умолчанию для новых чатов
    #[serde(default)]
    pub reasoning: ReasoningMode,
    /// Режим встроенного размышления модели по умолчанию для новых чатов
    #[serde(default)]
    pub thinking: ThinkingMode,
    /// Состав группы экспертов по умолчанию для новых чатов
    #[serde(default)]
    pub experts: Vec<String>,
    /// Лимит контекста по умолчанию для НОВЫХ чатов: копируется в
    /// `ChatSettings.max_context_tokens` при создании чата
    /// (`default_chat_settings`). Изменение этого поля не влияет на уже
    /// созданные чаты — как и `Config.model`.
    pub max_context_tokens: Option<u32>,
    /// Умолчания компактизации для НОВЫХ чатов, по тому же правилу, что и
    /// `max_context_tokens`: копируются в `ChatSettings.summary_*` при
    /// создании чата и не влияют на уже созданные чаты.
    pub summary_enabled: Option<bool>,
    pub summary_keep_messages: Option<u32>,
    pub summary_step_messages: Option<u32>,
    /// Умолчание стратегии контекста для НОВЫХ чатов, по тому же правилу,
    /// что и `max_context_tokens`.
    #[serde(default)]
    pub context_strategy: Option<ContextStrategy>,
    #[serde(default)]
    pub context_window_messages: Option<u32>,
    /// Путь к файлу инвариантов (`invariants.toml`). Не задан — набор
    /// инвариантов пуст, поведение конвейера не меняется (design.md,
    /// «Формат файла — `invariants.toml`»). Инварианты живут отдельно от
    /// этого конфига и от чатов — здесь хранится только путь к ним, не их
    /// содержимое.
    #[serde(default)]
    pub invariants_path: Option<String>,
    /// Умолчания git-инструментов для НОВЫХ чатов, по тому же правилу, что
    /// и `max_context_tokens`. Их же использует `agentcli ask`, у которого
    /// чата нет.
    #[serde(default)]
    pub git_tools_enabled: Option<bool>,
    #[serde(default)]
    pub git_repository: Option<String>,
    #[serde(default)]
    pub git_allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    pub tool_max_iterations: Option<u32>,
}

impl Config {
    fn path() -> Result<PathBuf> {
        let dir = dirs::config_dir().context("не удалось определить домашнюю директорию конфигов")?;
        Ok(dir.join("agentcli").join("config.toml"))
    }

    pub fn load() -> Result<Config> {
        Ok(Self::load_with_legacy_fields()?.0)
    }

    /// Конфиг вместе со списком найденных в файле полей прежней схемы.
    /// Файл при этом не переписывается: чужие правки в нём терять нельзя.
    pub fn load_with_legacy_fields() -> Result<(Config, Vec<&'static str>)> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok((Config::default(), Vec::new()));
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("не удалось прочитать конфиг {}", path.display()))?;
        Self::parse_with_legacy_fields(&content)
            .with_context(|| format!("не удалось разобрать конфиг {}", path.display()))
    }

    /// Разбор конфига: неизвестные поля игнорируются, поэтому файл прежней
    /// схемы читается без ошибки.
    pub fn parse_with_legacy_fields(content: &str) -> Result<(Config, Vec<&'static str>)> {
        let config: Config = toml::from_str(content)?;
        let table: toml::Value = toml::from_str(content)?;
        let legacy = LEGACY_FIELDS
            .into_iter()
            .filter(|field| table.get(field).is_some())
            .collect();
        Ok((config, legacy))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("не удалось создать директорию {}", parent.display()))?;
        }
        let content = toml::to_string_pretty(self).context("не удалось сериализовать конфиг")?;
        std::fs::write(&path, content)
            .with_context(|| format!("не удалось записать конфиг {}", path.display()))?;
        Ok(())
    }

    /// Значения по умолчанию для новых чатов: глобальный конфиг служит
    /// шаблоном, дальше каждый чат правит свои параметры независимо.
    pub fn default_chat_settings(&self) -> ChatSettings {
        ChatSettings {
            provider: self.provider,
            model: self.default_model_for(self.provider),
            custom_response_mode: self.custom_response_mode,
            response_format: self.response_format.clone(),
            sampling: self.sampling.clone(),
            reasoning: self.reasoning,
            thinking: self.thinking,
            experts: self.experts.clone(),
            max_context_tokens: self.max_context_tokens,
            summary_enabled: self.summary_enabled,
            summary_keep_messages: self.summary_keep_messages,
            summary_step_messages: self.summary_step_messages,
            context_strategy: self.context_strategy,
            context_window_messages: self.context_window_messages,
            memory_layers_enabled: None,
            memory_router_enabled: None,
            memory_working_max_entries: None,
            memory_long_term_max_entries: None,
            profile_id: None,
            task_state_enabled: None,
            task_state_auto_enabled: None,
            git_tools_enabled: self.git_tools_enabled,
            git_repository: self.git_repository.clone(),
            git_allowed_tools: self.git_allowed_tools.clone(),
            tool_max_iterations: self.tool_max_iterations,
        }
    }

    /// Модель по умолчанию для новых чатов и для чатов без своей модели.
    pub fn effective_model(&self) -> String {
        self.model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string())
    }

    /// Модель по умолчанию для выбранного провайдера: у Ollama свой список
    /// моделей, поэтому и модель по умолчанию у неё своя.
    pub fn default_model_for(&self, provider: Provider) -> Option<String> {
        let value = match provider {
            Provider::Cloud => self.model.clone(),
            Provider::Ollama => self.ollama_model.clone(),
        };
        value.filter(|m| !m.trim().is_empty())
    }

    pub fn effective_ollama_url(&self) -> String {
        self.ollama_url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string())
    }

    pub fn effective_server_url(&self) -> String {
        self.server_url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string())
    }

    pub fn client_token(&self) -> String {
        self.client_token.clone().unwrap_or_default()
    }

    /// Список моделей для переключения стрелками: свой из конфига либо
    /// встроенный `KNOWN_MODELS`.
    pub fn model_choices(&self) -> Vec<String> {
        if self.models.is_empty() {
            KNOWN_MODELS.iter().map(|m| m.to_string()).collect()
        } else {
            self.models.clone()
        }
    }

    /// Путь к файлу инвариантов, если он задан в конфиге.
    pub fn invariants_path(&self) -> Option<PathBuf> {
        self.invariants_path
            .as_ref()
            .filter(|p| !p.trim().is_empty())
            .map(PathBuf::from)
    }

    pub fn masked_client_token(&self) -> String {
        match &self.client_token {
            None => "<не задан>".to_string(),
            Some(token) if token.trim().is_empty() => "<не задан>".to_string(),
            Some(token) if token.chars().count() <= 8 => "*".repeat(token.chars().count()),
            Some(token) => {
                let chars: Vec<char> = token.chars().collect();
                let head: String = chars[..4].iter().collect();
                let tail: String = chars[chars.len() - 4..].iter().collect();
                format!("{head}***{tail}")
            }
        }
    }
}

/// Поля прежней схемы, когда ключ провайдера хранил клиент. Конфиг с ними
/// читается без ошибки, но клиент один раз предупреждает: ключ больше не
/// используется, а его хранение у клиента ничего не даёт.
pub const LEGACY_FIELDS: [&str; 2] = ["api_key", "base_url"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_url_defaults_to_local_service() {
        let config = Config::default();
        assert_eq!(config.effective_server_url(), DEFAULT_SERVER_URL);
        assert!(config.client_token().is_empty());
    }

    #[test]
    fn token_is_masked() {
        let config = Config {
            client_token: Some("supersecrettoken".to_string()),
            ..Config::default()
        };
        let masked = config.masked_client_token();
        assert!(!masked.contains("secret"), "маска: {masked}");
        assert!(masked.starts_with("supe") && masked.ends_with("oken"));
        assert_eq!(Config::default().masked_client_token(), "<не задан>");
    }

    #[test]
    fn legacy_config_is_read_and_reported() {
        let content = r#"
api_key = "sk-xxx"
base_url = "https://api.deepseek.com"
model = "deepseek-chat"
"#;
        let (config, legacy) = Config::parse_with_legacy_fields(content).expect("конфиг");
        assert_eq!(config.effective_model(), "deepseek-chat");
        // Ключ провайдера читается, но игнорируется: места для него нет.
        assert_eq!(config.effective_server_url(), DEFAULT_SERVER_URL);
        assert_eq!(legacy, vec!["api_key", "base_url"]);
    }

    #[test]
    fn max_context_tokens_defaults_to_none_for_old_config() {
        let content = r#"
server_url = "http://127.0.0.1:9000"
client_token = "t"
"#;
        let (config, _legacy) = Config::parse_with_legacy_fields(content).expect("конфиг");
        assert_eq!(config.max_context_tokens, None);
    }

    #[test]
    fn old_chat_settings_without_field_parse_as_none() {
        let settings: ChatSettings = serde_json::from_str("{}").expect("настройки чата");
        assert_eq!(settings.max_context_tokens, None);
    }

    #[test]
    fn old_chat_settings_without_summary_fields_parse_as_none() {
        let settings: ChatSettings = serde_json::from_str("{}").expect("настройки чата");
        assert_eq!(settings.summary_enabled, None);
        assert_eq!(settings.summary_keep_messages, None);
        assert_eq!(settings.summary_step_messages, None);
    }

    #[test]
    fn old_chat_settings_without_git_tools_fields_parse_as_none() {
        let settings: ChatSettings = serde_json::from_str("{}").expect("настройки чата");
        assert_eq!(settings.git_tools_enabled, None);
        assert_eq!(settings.git_repository, None);
        assert_eq!(settings.git_allowed_tools, None);
        assert_eq!(settings.tool_max_iterations, None);
        assert!(!settings.git_tools_active());
        assert_eq!(settings.effective_tool_max_iterations(), DEFAULT_TOOL_MAX_ITERATIONS);
    }

    #[test]
    fn tool_max_iterations_is_clamped() {
        let mut settings = ChatSettings {
            tool_max_iterations: Some(1000),
            ..ChatSettings::default()
        };
        assert_eq!(settings.effective_tool_max_iterations(), MAX_TOOL_ITERATIONS);
        settings.tool_max_iterations = Some(0);
        assert_eq!(settings.effective_tool_max_iterations(), 1);
    }

    #[test]
    fn new_chat_inherits_git_tools_defaults_from_config() {
        let config = Config {
            git_tools_enabled: Some(true),
            git_repository: Some("/tmp/repo".into()),
            git_allowed_tools: Some(vec!["git_add".into()]),
            tool_max_iterations: Some(4),
            ..Config::default()
        };
        let chat = config.default_chat_settings();
        assert!(chat.git_tools_active());
        assert_eq!(chat.git_repository.as_deref(), Some("/tmp/repo"));
        assert_eq!(chat.git_allowed_tools, Some(vec!["git_add".to_string()]));
        assert_eq!(chat.tool_max_iterations, Some(4));
    }

    #[test]
    fn new_chat_inherits_default_context_limit_from_config() {
        let config = Config {
            max_context_tokens: Some(4000),
            ..Config::default()
        };
        let chat = config.default_chat_settings();
        assert_eq!(chat.max_context_tokens, Some(4000));
    }

    #[test]
    fn changing_config_default_does_not_affect_already_built_chat_settings() {
        let mut config = Config {
            max_context_tokens: Some(4000),
            ..Config::default()
        };
        let chat = config.default_chat_settings();
        config.max_context_tokens = Some(8000);
        assert_eq!(chat.max_context_tokens, Some(4000));
    }

    #[test]
    fn new_chat_inherits_summary_defaults_from_config() {
        let config = Config {
            summary_enabled: Some(true),
            summary_keep_messages: Some(20),
            summary_step_messages: Some(10),
            ..Config::default()
        };
        let chat = config.default_chat_settings();
        assert_eq!(chat.summary_enabled, Some(true));
        assert_eq!(chat.summary_keep_messages, Some(20));
        assert_eq!(chat.summary_step_messages, Some(10));
    }

    #[test]
    fn changing_config_summary_defaults_does_not_affect_already_built_chat_settings() {
        let mut config = Config {
            summary_keep_messages: Some(20),
            ..Config::default()
        };
        let chat = config.default_chat_settings();
        config.summary_keep_messages = Some(40);
        assert_eq!(chat.summary_keep_messages, Some(20));
    }

    #[test]
    fn old_chat_settings_without_context_strategy_fields_parse_as_none() {
        let settings: ChatSettings = serde_json::from_str("{}").expect("настройки чата");
        assert_eq!(settings.context_strategy, None);
        assert_eq!(settings.context_window_messages, None);
    }

    #[test]
    fn old_config_without_context_strategy_fields_parses_as_none() {
        let content = r#"
server_url = "http://127.0.0.1:9000"
"#;
        let (config, _legacy) = Config::parse_with_legacy_fields(content).expect("конфиг");
        assert_eq!(config.context_strategy, None);
        assert_eq!(config.context_window_messages, None);
    }

    #[test]
    fn context_strategy_round_trips_snake_case_json() {
        let settings = ChatSettings {
            context_strategy: Some(ContextStrategy::SlidingWindow),
            ..ChatSettings::default()
        };
        let json = serde_json::to_string(&settings).expect("сериализация");
        assert!(json.contains("\"sliding_window\""));
        let parsed: ChatSettings = serde_json::from_str(&json).expect("разбор");
        assert_eq!(parsed.context_strategy, Some(ContextStrategy::SlidingWindow));
    }

    #[test]
    fn new_chat_inherits_context_strategy_defaults_from_config() {
        let config = Config {
            context_strategy: Some(ContextStrategy::Facts),
            context_window_messages: Some(8),
            ..Config::default()
        };
        let chat = config.default_chat_settings();
        assert_eq!(chat.context_strategy, Some(ContextStrategy::Facts));
        assert_eq!(chat.context_window_messages, Some(8));
    }

    #[test]
    fn changing_config_context_strategy_default_does_not_affect_already_built_chat_settings() {
        let mut config = Config {
            context_strategy: Some(ContextStrategy::Facts),
            ..Config::default()
        };
        let chat = config.default_chat_settings();
        config.context_strategy = Some(ContextStrategy::Branching);
        assert_eq!(chat.context_strategy, Some(ContextStrategy::Facts));
    }

    #[test]
    fn memory_layers_enabled_round_trips_snake_case_json() {
        let settings = ChatSettings {
            memory_layers_enabled: Some(true),
            ..ChatSettings::default()
        };
        let json = serde_json::to_string(&settings).expect("сериализация");
        assert!(json.contains("\"memory_layers_enabled\":true"));
        let parsed: ChatSettings = serde_json::from_str(&json).expect("разбор");
        assert_eq!(parsed.memory_layers_enabled, Some(true));
    }

    #[test]
    fn memory_layers_is_not_a_context_strategy_value() {
        assert_eq!(ContextStrategy::parse("memory_layers"), None);
    }

    #[test]
    fn old_chat_settings_without_memory_fields_parse_as_none() {
        let settings: ChatSettings = serde_json::from_str("{}").expect("настройки чата");
        assert_eq!(settings.memory_layers_enabled, None);
        assert_eq!(settings.memory_router_enabled, None);
        assert_eq!(settings.memory_working_max_entries, None);
        assert_eq!(settings.memory_long_term_max_entries, None);
        assert_eq!(settings.profile_id, None);
        assert_eq!(settings.task_state_enabled, None);
        assert_eq!(settings.task_state_auto_enabled, None);
    }

    #[test]
    fn task_state_enabled_round_trips_snake_case_json() {
        let settings = ChatSettings {
            task_state_enabled: Some(true),
            task_state_auto_enabled: Some(false),
            ..ChatSettings::default()
        };
        let json = serde_json::to_string(&settings).expect("сериализация");
        assert!(json.contains("\"task_state_enabled\":true"));
        assert!(json.contains("\"task_state_auto_enabled\":false"));
        let parsed: ChatSettings = serde_json::from_str(&json).expect("разбор");
        assert_eq!(parsed.task_state_enabled, Some(true));
        assert_eq!(parsed.task_state_auto_enabled, Some(false));
    }

    #[test]
    fn profile_id_round_trips_snake_case_json() {
        let settings = ChatSettings {
            profile_id: Some("teacher".to_string()),
            ..ChatSettings::default()
        };
        let json = serde_json::to_string(&settings).expect("сериализация");
        assert!(json.contains("\"profile_id\":\"teacher\""));
        let parsed: ChatSettings = serde_json::from_str(&json).expect("разбор");
        assert_eq!(parsed.profile_id, Some("teacher".to_string()));
    }

    #[test]
    fn chat_settings_with_legacy_short_term_tail_field_parses_without_error() {
        let settings: ChatSettings =
            serde_json::from_str(r#"{"memory_short_term_tail": 12}"#).expect("настройки чата");
        assert_eq!(settings.memory_layers_enabled, None);
    }

    #[test]
    fn old_config_without_invariants_path_parses_as_none() {
        let content = r#"
server_url = "http://127.0.0.1:9000"
"#;
        let (config, _legacy) = Config::parse_with_legacy_fields(content).expect("конфиг");
        assert_eq!(config.invariants_path(), None);
    }

    #[test]
    fn invariants_path_is_read_from_config() {
        let content = r#"
invariants_path = "/etc/agentcli/invariants.toml"
"#;
        let (config, _legacy) = Config::parse_with_legacy_fields(content).expect("конфиг");
        assert_eq!(
            config.invariants_path(),
            Some(PathBuf::from("/etc/agentcli/invariants.toml"))
        );
    }

    #[test]
    fn blank_invariants_path_is_treated_as_not_set() {
        let config = Config {
            invariants_path: Some("   ".to_string()),
            ..Config::default()
        };
        assert_eq!(config.invariants_path(), None);
    }

    #[test]
    fn new_config_reports_no_legacy_fields() {
        let content = r#"
server_url = "http://127.0.0.1:9000"
client_token = "t"
"#;
        let (config, legacy) = Config::parse_with_legacy_fields(content).expect("конфиг");
        assert_eq!(config.effective_server_url(), "http://127.0.0.1:9000");
        assert!(legacy.is_empty());
    }
}
