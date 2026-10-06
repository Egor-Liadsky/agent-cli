use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "agentcli", about = "CLI для диалога с облачным AI-агентом")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Задать один вопрос агенту и получить ответ
    Ask {
        /// Текст вопроса
        prompt: String,
    },
    /// Начать интерактивный диалог с агентом
    Chat,
    /// Управление конфигурацией (адрес сервиса, токен, модель)
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Локальные модели через Ollama
    Ollama {
        #[command(subcommand)]
        action: OllamaAction,
    },
    /// Факты чата стратегии `facts` (specs/context-facts): без TUI, по
    /// идентификатору существующего чата
    Facts {
        /// Идентификатор чата, выданный сервисом при создании
        chat_id: String,
        #[command(subcommand)]
        action: FactsAction,
    },
    /// Ветки чата стратегии `branching` (specs/chat-branching): без TUI, по
    /// идентификатору существующего чата
    Branches {
        chat_id: String,
        #[command(subcommand)]
        action: BranchesAction,
    },
    /// Профили владельца (specs/user-profiles): встроенные (`teacher`,
    /// `psychologist`, `reviewer`) и собственные, подключаемые к чату полем
    /// `profile_id` в параметрах чата (TUI, Ctrl+P)
    Profiles {
        #[command(subcommand)]
        action: ProfilesAction,
    },
    /// Сводки активности проектов от демона activity-mcp (адрес и токен —
    /// `config activity`)
    Activity {
        #[command(subcommand)]
        action: ActivityAction,
    },
    /// Пайплайн MCP-инструментов pipeline-mcp: search → summarize →
    /// save_to_file (каталоги — `config pipeline`)
    Pipeline {
        #[command(subcommand)]
        action: PipelineAction,
    },
    /// Индекс документов index-mcp: построить индекс из .docx, искать по
    /// смыслу, посмотреть состояние и модели (настройки — `config index`)
    Index {
        #[command(subcommand)]
        action: IndexAction,
    },
}

// Разбирается один раз при старте: разница размеров вариантов ничего не стоит.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum IndexAction {
    /// Построить индекс из каталога .docx; ход сборки — в stderr, итог — в
    /// stdout. Флаги перекрывают `config index` только для этого запуска
    Build {
        /// Каталог с .docx
        #[arg(long)]
        root: Option<String>,
        /// Файл базы SQLite
        #[arg(long)]
        db: Option<String>,
        /// fixed, structure или all
        #[arg(long)]
        strategy: Option<String>,
        /// Модель эмбеддингов Ollama
        #[arg(long)]
        model: Option<String>,
        /// chars или tokens
        #[arg(long)]
        unit: Option<String>,
        #[arg(long = "chunk-size")]
        chunk_size: Option<usize>,
        #[arg(long)]
        overlap: Option<usize>,
        #[arg(long = "max-section")]
        max_section: Option<usize>,
        #[arg(long = "min-section")]
        min_section: Option<usize>,
        /// Минимальный объём корпуса в символах (умолчание сервера 50000)
        #[arg(long = "min-chars")]
        min_chars: Option<usize>,
    },
    /// Найти в индексе чанки, ближайшие к запросу по смыслу
    Search {
        /// Вопрос или фраза
        query: String,
        /// fixed или structure; по умолчанию — из config index
        #[arg(long)]
        strategy: Option<String>,
        /// Сколько чанков показать (по умолчанию 5, не больше 20)
        #[arg(long = "top-k")]
        top_k: Option<usize>,
        /// Сколько кандидатов взять до фильтрации
        #[arg(long = "candidate-top-k")]
        candidate_top_k: Option<usize>,
        /// Минимальный cosine score
        #[arg(long = "similarity-threshold")]
        similarity_threshold: Option<f32>,
        /// Включить query rewrite
        #[arg(long)]
        rewrite: bool,
        /// Отключить query rewrite из конфигурации
        #[arg(long = "no-rewrite")]
        no_rewrite: bool,
    },
    /// Что лежит в базе: стратегии, число чанков, модель, время сборки
    Status,
    /// Модели Ollama, умеющие эмбеддинги
    Models,
}

#[derive(Subcommand)]
pub enum PipelineAction {
    /// Найти строки с запросом, сделать по ним сводку моделью чата и
    /// сохранить её в файл — одной цепочкой
    Run {
        /// Что искать (подстрока без учёта регистра)
        query: String,
        /// Имя файла результата в каталоге записи
        #[arg(long, default_value = "summary.md")]
        out: String,
        /// Каталог поиска; по умолчанию — из config pipeline
        #[arg(long)]
        root: Option<String>,
        /// Каталог записи; по умолчанию — из config pipeline
        #[arg(long)]
        output: Option<String>,
        /// Сколько строк передать в сводку (по умолчанию 20)
        #[arg(long = "max-results")]
        max_results: Option<u32>,
        /// Перезаписать файл, если он уже есть
        #[arg(long)]
        overwrite: bool,
        /// Не звать модель: сводка экстрактивная, на стороне сервера
        #[arg(long = "no-sampling")]
        no_sampling: bool,
    },
}

#[derive(Subcommand)]
pub enum ActivityAction {
    /// Показать непрочитанные сводки и отметить их прочитанными
    Digest {
        /// Показать и уже прочитанные (последние 5)
        #[arg(long)]
        all: bool,
        /// Не отмечать показанные сводки прочитанными
        #[arg(long)]
        keep_unread: bool,
    },
    /// Собрать сводку сейчас, вне расписания демона
    Build,
    /// Отметить сводку прочитанной
    Ack { id: i64 },
    /// Проверить связь с демоном и показать наблюдаемые проекты
    Status,
    /// Запустить демон и поставить его на автозапуск (launchd на macOS,
    /// systemd --user на Linux) с каталогом и расписанием из config activity;
    /// уже запущенный — перезапустить с новыми параметрами
    Start,
    /// Остановить демон, запущенный клиентом, и снять его с автозапуска
    Stop,
}

#[derive(Subcommand)]
pub enum ProfilesAction {
    /// Показать доступные профили: встроенные и собственные
    List,
    /// Показать один профиль по идентификатору
    Show { id: String },
    /// Создать собственный профиль из JSON-файла с полями name, persona,
    /// style, format, constraints (непустой обязательно хотя бы один из
    /// persona/style/format/constraints)
    Create {
        #[arg(long)]
        file: String,
    },
}

#[derive(Subcommand)]
pub enum FactsAction {
    /// Показать все факты чата
    List,
    /// Задать значение факта (создаёт ключ или заменяет значение)
    Set { key: String, value: String },
    /// Удалить ключ
    Delete { key: String },
}

#[derive(Subcommand)]
pub enum BranchesAction {
    /// Показать ветки чата с активной веткой
    List,
    /// Создать ветку от сообщения с указанным порядковым номером (`seq` из
    /// `facts list`/`branches list` или из истории чата)
    Create {
        from_seq: i64,
        /// Имя ветки; без значения — «ветка от <from_seq>»
        name: Option<String>,
    },
    /// Переключить активную ветку
    Activate { branch_id: String },
}

#[derive(Subcommand)]
pub enum OllamaAction {
    /// Показать локально скачанные модели (запрос к запущенному Ollama)
    Models,
    /// Сделать локальную модель моделью по умолчанию для новых чатов
    Use {
        /// Имя модели, например gemma4:26b
        model: String,
    },
    /// Задать адрес сервера Ollama (по умолчанию http://localhost:11434)
    SetUrl {
        /// Адрес без /api/chat
        url: String,
    },
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// Сохранить клиентский токен сервиса в конфиг
    SetToken {
        /// Значение токена. Пустая строка — токен не отправляется
        token: String,
    },
    /// Задать модель по умолчанию для новых чатов
    SetModel {
        /// Имя модели, например deepseek-chat
        model: String,
    },
    /// Задать адрес сервиса agentd (например http://127.0.0.1:8080)
    SetUrl {
        /// Адрес сервиса без /v1/chat
        url: String,
    },
    /// Показать модели, разрешённые сервисом (GET /v1/models)
    Models,
    /// Задать провайдера по умолчанию для новых чатов: cloud | ollama
    SetProvider {
        /// cloud — облачный API, ollama — локальные модели
        provider: String,
    },
    /// Показать текущий конфиг (токен маскируется)
    Show,
    /// Настройка формата ответа по умолчанию для новых чатов (кастомный режим)
    Format {
        #[command(subcommand)]
        action: FormatAction,
    },
    /// Настройка параметров сэмплирования (temperature, top_p, top_k и т.д.)
    Sampling {
        #[command(subcommand)]
        action: SamplingAction,
    },
    /// Стратегия рассуждения по умолчанию для новых чатов
    Reasoning {
        /// default | step-by-step | prompt-craft | expert-panel
        /// (без значения — показать текущую)
        mode: Option<String>,
        /// Состав группы экспертов через запятую, например:
        /// "аналитик, инженер, критик". Пустая строка — состав по умолчанию
        #[arg(long, value_delimiter = ',')]
        experts: Option<Vec<String>>,
        /// Встроенный режим thinking у модели: auto | on | off
        #[arg(long)]
        thinking: Option<String>,
    },
    /// Лимит контекста (токены) по умолчанию для НОВЫХ чатов; лимит уже
    /// созданного чата меняется в TUI (Ctrl+P)
    ContextLimit {
        #[command(subcommand)]
        action: ContextLimitAction,
    },
    /// Умолчания компактизации истории (agentd) для НОВЫХ чатов; настройки
    /// уже созданного чата меняются в TUI (Ctrl+P)
    Summary {
        #[command(subcommand)]
        action: SummaryAction,
    },
    /// Умолчания git-инструментов (git-mcp) для НОВЫХ чатов и для
    /// `agentcli ask`; настройки уже созданного чата меняются в TUI (Ctrl+P)
    GitTools {
        #[command(subcommand)]
        action: GitToolsAction,
    },
    /// Подключение к демону activity-mcp: сводки в TUI и инструменты
    /// activity_* для модели
    Activity {
        #[command(subcommand)]
        action: ActivityConfigAction,
    },
    /// Каталоги и переключатель пайплайна pipeline-mcp; инструменты search,
    /// summarize, save_to_file в чатах работают при включённом переключателе
    /// и заданном каталоге поиска
    Pipeline {
        #[command(subcommand)]
        action: PipelineConfigAction,
    },
    /// Индекс документов index-mcp: каталог .docx, база, стратегия и
    /// параметры chunking, модель эмбеддингов. Инструменты index_search и
    /// index_status в чатах работают при заданной базе
    Index {
        #[command(subcommand)]
        action: IndexConfigAction,
    },
    /// Задать путь к файлу инвариантов (`invariants.toml`)
    SetInvariantsPath {
        /// Путь к файлу; пустая строка снимает умолчание
        path: String,
    },
    /// Показать активные инварианты из настроенного файла (источник —
    /// конфигурация, не диалог)
    Invariants,
}

#[derive(Subcommand)]
pub enum PipelineConfigAction {
    /// Включить или выключить инструменты и задать каталоги поиска и записи
    Set {
        /// true/false, on/off, yes/no; выключение сохраняет каталоги
        enabled: Option<String>,
        /// Каталог, в котором ищет search; пустая строка снимает его
        #[arg(long)]
        root: Option<String>,
        /// Каталог, куда пишет save_to_file; пустая строка — <root>/pipeline-out
        #[arg(long)]
        output: Option<String>,
    },
    /// Снять оба каталога и переключатель: инструменты пайплайна в чатах
    /// выключаются
    Clear,
    /// Показать каталоги пайплайна
    Show,
}

// Разбирается один раз при старте: разница размеров вариантов ничего не стоит.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum IndexConfigAction {
    /// Задать настройки индекса; пустая строка (у чисел — 0) снимает значение
    Set {
        /// Каталог с .docx
        #[arg(long)]
        root: Option<String>,
        /// Файл базы SQLite; заданная база включает инструменты в чатах
        #[arg(long)]
        db: Option<String>,
        /// fixed, structure или all
        #[arg(long)]
        strategy: Option<String>,
        /// Модель эмбеддингов Ollama (по умолчанию nomic-embed-text)
        #[arg(long)]
        model: Option<String>,
        /// chars или tokens
        #[arg(long)]
        unit: Option<String>,
        #[arg(long = "chunk-size")]
        chunk_size: Option<usize>,
        #[arg(long)]
        overlap: Option<usize>,
        #[arg(long = "max-section")]
        max_section: Option<usize>,
        #[arg(long = "min-section")]
        min_section: Option<usize>,
        /// Адрес Ollama (по умолчанию http://localhost:11434)
        #[arg(long = "ollama-url")]
        ollama_url: Option<String>,
        /// Сколько результатов возвращать после фильтрации
        #[arg(long = "top-k")]
        top_k: Option<usize>,
        /// Сколько кандидатов брать до фильтрации
        #[arg(long = "candidate-top-k")]
        candidate_top_k: Option<usize>,
        /// Минимальный cosine score; пустая строка снимает значение
        #[arg(long = "similarity-threshold")]
        similarity_threshold: Option<String>,
        /// true/false
        #[arg(long)]
        rewrite: Option<String>,
        /// Модель Ollama для query rewrite
        #[arg(long = "rewrite-model")]
        rewrite_model: Option<String>,
    },
    /// Снять все настройки индекса: инструменты в чатах выключаются
    Clear,
    /// Показать настройки индекса
    Show,
}

#[derive(Subcommand)]
pub enum GitToolsAction {
    /// Включить или выключить git-инструменты и задать их параметры
    Set {
        /// true/false, on/off, yes/no
        enabled: Option<String>,
        /// Путь к git-репозиторию на этой машине
        #[arg(long)]
        repository: Option<String>,
        /// Разрешённые пишущие инструменты через запятую, например
        /// "git_add,git_commit"; пустая строка — только читающие
        #[arg(long = "allowed-tools")]
        allowed_tools: Option<String>,
        /// Лимит итераций цикла инструментов (1–32, по умолчанию 8)
        #[arg(long = "max-iterations")]
        max_iterations: Option<u32>,
    },
    /// Снять все умолчания git-инструментов: новые чаты создаются без них
    Clear,
    /// Показать текущие умолчания
    Show,
}

#[derive(Subcommand)]
pub enum ActivityConfigAction {
    /// Включить или выключить сводки и задать параметры подключения
    Set {
        /// true/false, on/off, yes/no
        enabled: Option<String>,
        /// Адрес MCP демона, по умолчанию http://127.0.0.1:7878/mcp; пустая
        /// строка — адрес по умолчанию
        #[arg(long)]
        url: Option<String>,
        /// Bearer-токен (если демон запущен с --token-file); пустая строка
        /// — без токена
        #[arg(long)]
        token: Option<String>,
        /// Как часто TUI спрашивает о новых сводках, секунды (от 10, по
        /// умолчанию 300)
        #[arg(long = "poll-secs")]
        poll_secs: Option<u64>,
        /// Давать модели в чатах читающие инструменты activity_*: true/false
        #[arg(long = "chat-tools")]
        chat_tools: Option<String>,
        /// Каталог с проектами для `activity start`; пустая строка — снять
        #[arg(long)]
        root: Option<String>,
        /// Расписание сводок (cron, 5 полей) для `activity start`; пустая
        /// строка — умолчание демона (0 9,18 * * *)
        #[arg(long)]
        schedule: Option<String>,
    },
    /// Снять все настройки activity-mcp: сводки выключены
    Clear,
    /// Показать текущие настройки
    Show,
}

#[derive(Subcommand)]
pub enum ContextLimitAction {
    /// Задать лимит контекста по умолчанию для новых чатов
    Set {
        /// Положительное число токенов
        tokens: u32,
    },
    /// Снять умолчание: новые чаты создаются без лимита
    Clear,
    /// Показать текущее умолчание для новых чатов
    Show,
}

#[derive(Subcommand)]
pub enum SummaryAction {
    /// Включить или выключить компактизацию по умолчанию для новых чатов
    Set {
        /// true/false, on/off, yes/no
        enabled: Option<String>,
        /// Дословный хвост компактизации (сообщений)
        #[arg(long = "keep-messages")]
        keep_messages: Option<u32>,
        /// Шаг пересказа (сообщений)
        #[arg(long = "step-messages")]
        step_messages: Option<u32>,
    },
    /// Снять все умолчания компактизации: новые чаты используют
    /// операторские значения сервиса
    Clear,
    /// Показать текущие умолчания для новых чатов
    Show,
}

#[derive(Subcommand)]
pub enum SamplingAction {
    /// Задать параметры сэмплирования (обновляет только переданные поля)
    Set {
        /// Температура сэмплирования (обычно 0.0 - 2.0)
        #[arg(long)]
        temperature: Option<f32>,
        /// Top-p (nucleus sampling), 0.0 - 1.0
        #[arg(long = "top-p")]
        top_p: Option<f32>,
        /// Top-k сэмплирование
        #[arg(long = "top-k")]
        top_k: Option<u32>,
        /// Штраф за частоту повторения токенов
        #[arg(long = "frequency-penalty")]
        frequency_penalty: Option<f32>,
        /// Штраф за присутствие токена в тексте
        #[arg(long = "presence-penalty")]
        presence_penalty: Option<f32>,
    },
    /// Сбросить все параметры сэмплирования (вернуться к дефолтным значениям API)
    Reset,
    /// Показать текущие параметры сэмплирования
    Show,
}

#[derive(Subcommand)]
pub enum FormatAction {
    /// Задать параметры формата ответа (обновляет только переданные поля)
    Set {
        /// Описание формата ответа, например: "отвечай маркированным списком"
        #[arg(long)]
        description: Option<String>,
        /// Максимальная длина ответа в токенах
        #[arg(long = "max-length")]
        max_length: Option<u32>,
        /// Stop-последовательности через запятую
        #[arg(long, value_delimiter = ',')]
        stop: Option<Vec<String>>,
        /// Явная инструкция модели о том, когда завершать ответ
        #[arg(long = "stop-instruction")]
        stop_instruction: Option<String>,
    },
    /// Включить кастомный режим ответа
    Enable,
    /// Выключить кастомный режим ответа (вернуться к дефолтному)
    Disable,
    /// Сбросить все настройки формата
    Reset,
    /// Показать текущие настройки формата
    Show,
}
