mod agent;
mod chats;
mod clipboard;
mod cli;
mod logging;
mod markdown;
mod tui;

use agent::CliAgent;
use agentcore::agent::{Agent, AgentReply, Message, MessageMeta};
use anyhow::Context;
use clap::Parser;
use cli::{
    BranchesAction, Cli, Commands, ConfigAction, ContextLimitAction, FactsAction, FormatAction,
    OllamaAction, ProfilesAction, SamplingAction, SummaryAction,
};
use agentcore::config::{Config, Provider, ReasoningMode, ThinkingMode};
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use logging::{exchange_log, UNAUTHORIZED_HINT};
use markdown::agent_skin;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Ask { prompt } => run_ask(prompt).await?,
        Commands::Chat => run_chat().await?,
        Commands::Config { action } => match action {
            // Единственная сетевая команда конфига: список моделей отдаёт сервис.
            ConfigAction::Models => run_config_models().await?,
            action => run_config(action)?,
        },
        Commands::Ollama { action } => run_ollama(action).await?,
        Commands::Facts { chat_id, action } => run_facts(chat_id, action).await?,
        Commands::Branches { chat_id, action } => run_branches(chat_id, action).await?,
        Commands::Profiles { action } => run_profiles(action).await?,
    }

    Ok(())
}

async fn run_facts(chat_id: String, action: FactsAction) -> anyhow::Result<()> {
    let config = load_config()?;
    let client = tui::chats_client(&config);
    match action {
        FactsAction::List => {
            let facts = client.facts(&chat_id).await?;
            if facts.is_empty() {
                println!("Фактов нет.");
            }
            for fact in facts {
                println!("{}: {} (через сообщение {})", fact.key, fact.value, fact.through_seq);
            }
        }
        FactsAction::Set { key, value } => {
            let fact = client.set_fact(&chat_id, &key, &value).await?;
            println!("{}: {}", fact.key, fact.value);
        }
        FactsAction::Delete { key } => {
            client.delete_fact(&chat_id, &key).await?;
            println!("Факт «{key}» удалён.");
        }
    }
    Ok(())
}

async fn run_branches(chat_id: String, action: BranchesAction) -> anyhow::Result<()> {
    let config = load_config()?;
    let client = tui::chats_client(&config);
    match action {
        BranchesAction::List => {
            let branches = client.branches(&chat_id).await?;
            for branch in branches {
                let mark = if branch.active { "*" } else { " " };
                println!(
                    "{mark} {} ({}) — {} сообщ.",
                    branch.name, branch.id, branch.message_count
                );
            }
        }
        BranchesAction::Create { from_seq, name } => {
            let name = name.unwrap_or_else(|| format!("ветка от {from_seq}"));
            let branch = client.create_branch(&chat_id, from_seq, &name).await?;
            println!("Создана ветка {} ({}).", branch.name, branch.id);
        }
        BranchesAction::Activate { branch_id } => {
            client.activate_branch(&chat_id, &branch_id).await?;
            println!("Ветка {branch_id} активна.");
        }
    }
    Ok(())
}

/// Поля профиля, читаемые из файла `profiles create --file`
/// (specs/user-profiles, «Ручное управление профилями через HTTP»).
#[derive(serde::Deserialize)]
struct ProfileFile {
    name: String,
    #[serde(default)]
    persona: String,
    #[serde(default)]
    style: String,
    #[serde(default)]
    format: String,
    #[serde(default)]
    constraints: Vec<String>,
}

async fn run_profiles(action: ProfilesAction) -> anyhow::Result<()> {
    let config = load_config()?;
    let client = tui::chats_client(&config);
    match action {
        ProfilesAction::List => {
            let profiles = client.profiles().await?;
            for profile in profiles {
                let mark = if profile.built_in { "[встроенный]" } else { "[свой]" };
                println!("{mark} {} — {}", profile.id, profile.name);
            }
        }
        ProfilesAction::Show { id } => {
            let profile = client.profile(&id).await?;
            println!("{} ({})", profile.name, profile.id);
            if !profile.persona.is_empty() {
                println!("Роль: {}", profile.persona);
            }
            if !profile.style.is_empty() {
                println!("Стиль: {}", profile.style);
            }
            if !profile.format.is_empty() {
                println!("Формат ответа: {}", profile.format);
            }
            if !profile.constraints.is_empty() {
                println!("Ограничения:");
                for constraint in &profile.constraints {
                    println!("- {constraint}");
                }
            }
        }
        ProfilesAction::Create { file } => {
            let content = std::fs::read_to_string(&file)
                .with_context(|| format!("не удалось прочитать файл профиля {file}"))?;
            let parsed: ProfileFile = serde_json::from_str(&content)
                .with_context(|| format!("не удалось разобрать файл профиля {file}"))?;
            let created = client
                .create_profile(&parsed.name, &parsed.persona, &parsed.style, &parsed.format, &parsed.constraints)
                .await?;
            println!("Создан профиль {} ({}).", created.name, created.id);
        }
    }
    Ok(())
}

async fn run_ask(prompt: String) -> anyhow::Result<()> {
    let config = load_config()?;
    let agent =
        CliAgent::from_config(&config, exchange_log())?.with_unauthorized_hint(UNAUTHORIZED_HINT);
    let history = vec![Message::user(prompt)];
    let settings = config.default_chat_settings();
    let reply = ask_with_spinner(&agent, &history, &settings).await?;
    if let Some(reasoning) = &reply.reasoning {
        println!("{}", style("Рассуждение:").magenta().bold());
        print_markdown(reasoning);
        println!();
    }
    print_markdown(&reply.content);
    println!("{}", style(format_stats_line(&reply.meta)).dim());
    Ok(())
}

async fn run_chat() -> anyhow::Result<()> {
    let config = load_config()?;
    let agent =
        CliAgent::from_config(&config, exchange_log())?.with_unauthorized_hint(UNAUTHORIZED_HINT);
    tui::run(agent, config).await
}

/// Конфиг вместе с предупреждением о полях прежней схемы. Файл не
/// переписывается: убрать устаревшие поля — решение пользователя.
fn load_config() -> anyhow::Result<Config> {
    let (config, legacy) = Config::load_with_legacy_fields()?;
    if !legacy.is_empty() {
        eprintln!(
            "{} {}",
            style("Внимание:").yellow().bold(),
            style(format!(
                "конфиг содержит устаревшие поля ({}). Ключ провайдера больше не \
                 используется клиентом: облачная модель отвечает через сервис agentd. \
                 Уберите эти поля из файла конфигурации.",
                legacy.join(", ")
            ))
            .yellow()
        );
    }
    Ok(config)
}


fn run_config(action: ConfigAction) -> anyhow::Result<()> {
    match action {
        ConfigAction::SetToken { token } => {
            let mut config = load_config()?;
            config.client_token = Some(token);
            config.save()?;
            println!("{}", style("Клиентский токен сохранён.").green().bold());
        }
        ConfigAction::SetModel { model } => {
            let mut config = load_config()?;
            config.model = Some(model);
            config.save()?;
            println!("{}", style("Модель по умолчанию сохранена.").green().bold());
        }
        ConfigAction::SetUrl { url } => {
            let mut config = load_config()?;
            config.server_url = Some(url);
            config.save()?;
            println!("{}", style("Адрес сервиса сохранён.").green().bold());
        }
        ConfigAction::SetProvider { provider } => {
            let mut config = load_config()?;
            config.provider = Provider::parse(&provider).ok_or_else(|| {
                anyhow::anyhow!("неизвестный провайдер «{provider}». Доступны: cloud, ollama")
            })?;
            config.save()?;
            println!(
                "{} {}",
                style("Провайдер по умолчанию:").green().bold(),
                config.provider.label()
            );
        }
        ConfigAction::Show => show_config()?,
        // Список моделей принадлежит сервису, поэтому команда сетевая.
        ConfigAction::Models => unreachable!("обрабатывается в run_config_async"),
        ConfigAction::Format { action } => run_format_action(action)?,
        ConfigAction::Sampling { action } => run_sampling_action(action)?,
        ConfigAction::Reasoning {
            mode,
            experts,
            thinking,
        } => run_reasoning_action(mode, experts, thinking)?,
        ConfigAction::ContextLimit { action } => run_context_limit_action(action)?,
        ConfigAction::Summary { action } => run_summary_action(action)?,
    }
    Ok(())
}

fn run_context_limit_action(action: ContextLimitAction) -> anyhow::Result<()> {
    match action {
        ContextLimitAction::Set { tokens } => {
            if tokens == 0 {
                anyhow::bail!("лимит контекста должен быть больше нуля");
            }
            let mut config = load_config()?;
            config.max_context_tokens = Some(tokens);
            config.save()?;
            println!(
                "{}",
                style("Умолчание лимита контекста для новых чатов сохранено.")
                    .green()
                    .bold()
            );
            print_context_limit(&config);
        }
        ContextLimitAction::Clear => {
            let mut config = load_config()?;
            config.max_context_tokens = None;
            config.save()?;
            println!(
                "{}",
                style("Умолчание лимита контекста для новых чатов снято.")
                    .green()
                    .bold()
            );
        }
        ContextLimitAction::Show => {
            let config = load_config()?;
            print_context_limit(&config);
        }
    }
    Ok(())
}

fn print_context_limit(config: &Config) {
    println!(
        "{} {}",
        style("лимит контекста по умолчанию для новых чатов (токены):")
            .cyan()
            .bold(),
        config
            .max_context_tokens
            .map(|v| v.to_string())
            .unwrap_or_else(|| "<не задан>".to_string())
    );
}

fn run_summary_action(action: SummaryAction) -> anyhow::Result<()> {
    match action {
        SummaryAction::Set {
            enabled,
            keep_messages,
            step_messages,
        } => {
            if enabled.is_none() && keep_messages.is_none() && step_messages.is_none() {
                anyhow::bail!(
                    "укажите хотя бы одно значение: enabled, --keep-messages или --step-messages"
                );
            }
            if keep_messages == Some(0) {
                anyhow::bail!("--keep-messages должен быть больше нуля");
            }
            if step_messages == Some(0) {
                anyhow::bail!("--step-messages должен быть больше нуля");
            }
            let mut config = load_config()?;
            if let Some(enabled) = enabled {
                config.summary_enabled = Some(parse_bool_flag(&enabled)?);
            }
            if keep_messages.is_some() {
                config.summary_keep_messages = keep_messages;
            }
            if step_messages.is_some() {
                config.summary_step_messages = step_messages;
            }
            config.save()?;
            println!(
                "{}",
                style("Умолчания компактизации для новых чатов сохранены.")
                    .green()
                    .bold()
            );
            print_summary(&config);
        }
        SummaryAction::Clear => {
            let mut config = load_config()?;
            config.summary_enabled = None;
            config.summary_keep_messages = None;
            config.summary_step_messages = None;
            config.save()?;
            println!(
                "{}",
                style("Умолчания компактизации для новых чатов сняты.")
                    .green()
                    .bold()
            );
        }
        SummaryAction::Show => {
            let config = load_config()?;
            print_summary(&config);
        }
    }
    Ok(())
}

fn parse_bool_flag(value: &str) -> anyhow::Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => anyhow::bail!("enabled должен быть true/false (или on/off, yes/no), задано: {other}"),
    }
}

fn print_summary(config: &Config) {
    println!(
        "{} {}",
        style("компактизация истории для новых чатов:").cyan().bold(),
        match config.summary_enabled {
            None => "<умолчание сервиса>".to_string(),
            Some(true) => "включена".to_string(),
            Some(false) => "выключена".to_string(),
        }
    );
    println!(
        "{} {}",
        style("дословный хвост компактизации (сообщений):").cyan().bold(),
        config
            .summary_keep_messages
            .map(|v| v.to_string())
            .unwrap_or_else(|| "<умолчание сервиса>".to_string())
    );
    println!(
        "{} {}",
        style("шаг пересказа (сообщений):").cyan().bold(),
        config
            .summary_step_messages
            .map(|v| v.to_string())
            .unwrap_or_else(|| "<умолчание сервиса>".to_string())
    );
}

/// Команды локального Ollama: список моделей, выбор модели, адрес сервера.
async fn run_ollama(action: OllamaAction) -> anyhow::Result<()> {
    match action {
        OllamaAction::Models => {
            let config = Config::load()?;
            let url = config.effective_ollama_url();
            let models = agentcore::agent::list_ollama_models(&url).await?;
            if models.is_empty() {
                println!(
                    "{}",
                    style("Локальных моделей нет. Скачайте модель: ollama pull <МОДЕЛЬ>").yellow()
                );
                return Ok(());
            }
            let current = config.ollama_model.unwrap_or_default();
            for model in models {
                let marker = if model == current { "●" } else { " " };
                println!("{marker} {model}");
            }
        }
        OllamaAction::Use { model } => {
            let mut config = Config::load()?;
            config.ollama_model = Some(model);
            config.provider = Provider::Ollama;
            config.save()?;
            println!(
                "{}",
                style("Новые чаты будут отвечать локальной моделью через Ollama.")
                    .green()
                    .bold()
            );
        }
        OllamaAction::SetUrl { url } => {
            let mut config = Config::load()?;
            config.ollama_url = Some(url);
            config.save()?;
            println!("{}", style("Адрес Ollama сохранён.").green().bold());
        }
    }
    Ok(())
}

/// Список моделей, разрешённых сервисом. Встроенного списка у клиента больше
/// нет: при недоступном сервисе это ошибка, а не пустой вывод.
async fn run_config_models() -> anyhow::Result<()> {
    let config = load_config()?;
    let models = agentclient::list_models(&config.effective_server_url(), &config.client_token())
        .await
        .with_context(|| {
            format!(
                "не удалось получить список моделей у сервиса {}",
                config.effective_server_url()
            )
        })?;
    if models.is_empty() {
        println!(
            "{}",
            style("Сервис не сообщил ни одной модели: проверьте AGENTD_ALLOWED_MODELS.").yellow()
        );
        return Ok(());
    }
    let current = config.effective_model();
    for model in models {
        let marker = if model == current { "●" } else { " " };
        println!("{marker} {model}");
    }
    Ok(())
}

fn show_config() -> anyhow::Result<()> {
    let config = load_config()?;
    println!(
        "{} {}",
        style("провайдер:").cyan().bold(),
        config.provider.label()
    );
    println!(
        "{} {}",
        style("server_url:  ").cyan().bold(),
        config.effective_server_url()
    );
    println!(
        "{} {}",
        style("client_token:").cyan().bold(),
        config.masked_client_token()
    );
    println!(
        "{} {}",
        style("model:   ").cyan().bold(),
        config.effective_model()
    );
    println!(
        "{} {}",
        style("ollama_url:  ").cyan().bold(),
        config.effective_ollama_url()
    );
    println!(
        "{} {}",
        style("ollama_model:").cyan().bold(),
        config
            .ollama_model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| "<не задана>".to_string())
    );
    println!(
        "{} {}",
        style("режим ответа (по умолчанию для новых чатов):").cyan().bold(),
        if config.custom_response_mode {
            "кастомный"
        } else {
            "дефолтный"
        }
    );
    print_reasoning(&config);
    print_response_format(&config.response_format);
    print_sampling_params(&config.sampling);
    print_context_limit(&config);
    print_summary(&config);
    Ok(())
}

fn run_format_action(action: FormatAction) -> anyhow::Result<()> {
    match action {
        FormatAction::Set {
            description,
            max_length,
            stop,
            stop_instruction,
        } => {
            let mut config = Config::load()?;
            if description.is_some() {
                config.response_format.description = description;
            }
            if max_length.is_some() {
                config.response_format.max_length = max_length;
            }
            if stop.is_some() {
                config.response_format.stop = stop;
            }
            if stop_instruction.is_some() {
                config.response_format.stop_instruction = stop_instruction;
            }
            config.custom_response_mode = true;
            config.save()?;
            println!(
                "{}",
                style("Настройки формата сохранены, кастомный режим включён.")
                    .green()
                    .bold()
            );
        }
        FormatAction::Enable => {
            let mut config = Config::load()?;
            config.custom_response_mode = true;
            config.save()?;
            println!("{}", style("Кастомный режим ответа включён.").green().bold());
        }
        FormatAction::Disable => {
            let mut config = Config::load()?;
            config.custom_response_mode = false;
            config.save()?;
            println!("{}", style("Кастомный режим ответа выключен.").green().bold());
        }
        FormatAction::Reset => {
            let mut config = Config::load()?;
            config.response_format = Default::default();
            config.custom_response_mode = false;
            config.save()?;
            println!("{}", style("Настройки формата сброшены.").green().bold());
        }
        FormatAction::Show => {
            let config = Config::load()?;
            println!(
                "{} {}",
                style("режим ответа:").cyan().bold(),
                if config.custom_response_mode {
                    "кастомный"
                } else {
                    "дефолтный"
                }
            );
            print_response_format(&config.response_format);
        }
    }
    Ok(())
}

fn run_reasoning_action(
    mode: Option<String>,
    experts: Option<Vec<String>>,
    thinking: Option<String>,
) -> anyhow::Result<()> {
    let mut config = Config::load()?;
    if mode.is_none() && experts.is_none() && thinking.is_none() {
        print_reasoning(&config);
        return Ok(());
    }
    if let Some(value) = thinking {
        config.thinking = ThinkingMode::parse(&value).ok_or_else(|| {
            anyhow::anyhow!("неизвестный режим thinking «{value}». Доступны: auto, on, off")
        })?;
    }
    if let Some(value) = mode {
        config.reasoning = ReasoningMode::parse(&value).ok_or_else(|| {
            anyhow::anyhow!(
                "неизвестная стратегия «{value}». Доступны: default, step-by-step, \
                 prompt-craft, expert-panel"
            )
        })?;
    }
    if let Some(experts) = experts {
        config.experts = experts
            .into_iter()
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
            .collect();
    }
    config.save()?;
    println!("{}", style("Настройки рассуждения сохранены.").green().bold());
    print_reasoning(&config);
    Ok(())
}

fn print_reasoning(config: &Config) {
    println!(
        "{} {}",
        style("стратегия рассуждения:").cyan().bold(),
        config.reasoning.label()
    );
    println!(
        "{} {}",
        style("режим thinking:      ").cyan().bold(),
        config.thinking.label()
    );
    println!(
        "{} {}",
        style("эксперты:            ").cyan().bold(),
        if config.experts.is_empty() {
            format!(
                "<по умолчанию: {}>",
                ReasoningMode::DEFAULT_EXPERTS.join(", ")
            )
        } else {
            config.experts.join(", ")
        }
    );
}

fn run_sampling_action(action: SamplingAction) -> anyhow::Result<()> {
    match action {
        SamplingAction::Set {
            temperature,
            top_p,
            top_k,
            frequency_penalty,
            presence_penalty,
        } => {
            let mut config = Config::load()?;
            if temperature.is_some() {
                config.sampling.temperature = temperature;
            }
            if top_p.is_some() {
                config.sampling.top_p = top_p;
            }
            if top_k.is_some() {
                config.sampling.top_k = top_k;
            }
            if frequency_penalty.is_some() {
                config.sampling.frequency_penalty = frequency_penalty;
            }
            if presence_penalty.is_some() {
                config.sampling.presence_penalty = presence_penalty;
            }
            config.save()?;
            println!("{}", style("Параметры сэмплирования сохранены.").green().bold());
        }
        SamplingAction::Reset => {
            let mut config = Config::load()?;
            config.sampling = Default::default();
            config.save()?;
            println!("{}", style("Параметры сэмплирования сброшены.").green().bold());
        }
        SamplingAction::Show => {
            let config = Config::load()?;
            print_sampling_params(&config.sampling);
        }
    }
    Ok(())
}

fn print_markdown(text: &str) {
    agent_skin().print_text(text);
}

fn print_response_format(format: &agentcore::config::ResponseFormat) {
    println!(
        "{} {}",
        style("описание:      ").cyan().bold(),
        format.description.as_deref().unwrap_or("<не задано>")
    );
    println!(
        "{} {}",
        style("макс. длина:   ").cyan().bold(),
        format
            .max_length
            .map(|v| v.to_string())
            .unwrap_or_else(|| "<не задано>".to_string())
    );
    println!(
        "{} {}",
        style("stop:          ").cyan().bold(),
        format
            .stop
            .as_ref()
            .map(|v| v.join(", "))
            .unwrap_or_else(|| "<не задано>".to_string())
    );
    println!(
        "{} {}",
        style("stop-инструкция:").cyan().bold(),
        format.stop_instruction.as_deref().unwrap_or("<не задано>")
    );
}

fn print_sampling_params(sampling: &agentcore::config::SamplingParams) {
    println!(
        "{} {}",
        style("temperature:       ").cyan().bold(),
        sampling
            .temperature
            .map(|v| v.to_string())
            .unwrap_or_else(|| "<не задано>".to_string())
    );
    println!(
        "{} {}",
        style("top_p:             ").cyan().bold(),
        sampling
            .top_p
            .map(|v| v.to_string())
            .unwrap_or_else(|| "<не задано>".to_string())
    );
    println!(
        "{} {}",
        style("top_k:             ").cyan().bold(),
        sampling
            .top_k
            .map(|v| v.to_string())
            .unwrap_or_else(|| "<не задано>".to_string())
    );
    println!(
        "{} {}",
        style("frequency_penalty: ").cyan().bold(),
        sampling
            .frequency_penalty
            .map(|v| v.to_string())
            .unwrap_or_else(|| "<не задано>".to_string())
    );
    println!(
        "{} {}",
        style("presence_penalty:  ").cyan().bold(),
        sampling
            .presence_penalty
            .map(|v| v.to_string())
            .unwrap_or_else(|| "<не задано>".to_string())
    );
}

/// Однострочная сводка: время отправки/получения, длительность, токены, скорость.
fn format_stats_line(meta: &MessageMeta) -> String {
    let mut parts = Vec::new();
    // Модель берётся из ответа: сервис мог ответить не запрошенной моделью.
    if let Some(model) = &meta.model {
        parts.push(model.clone());
    }
    if let (Some(sent), Some(received)) = (meta.sent_at, meta.received_at) {
        parts.push(format!(
            "{} → {}",
            format_clock(sent),
            format_clock(received)
        ));
    }
    if let Some(ms) = meta.duration_ms {
        parts.push(format!("{:.1} с", ms as f64 / 1000.0));
    }
    match (meta.prompt_tokens, meta.completion_tokens) {
        (Some(prompt), Some(completion)) => {
            parts.push(format!("токены ↑{prompt} ↓{completion}"))
        }
        (Some(prompt), None) => parts.push(format!("токены ↑{prompt}")),
        (None, Some(completion)) => parts.push(format!("токены ↓{completion}")),
        (None, None) => {}
    }
    if let Some(total) = meta.total_tokens {
        parts.push(format!("всего {total}"));
    }
    if let Some(reasoning) = meta.reasoning_tokens {
        parts.push(format!("рассуждение {reasoning}"));
    }
    if let Some(speed) = meta.tokens_per_second() {
        parts.push(format!("{speed:.0} ток/с"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        parts.join(" · ")
    }
}

fn format_clock(timestamp: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_default()
}

async fn ask_with_spinner(
    agent: &CliAgent,
    history: &[Message],
    settings: &agentcore::config::ChatSettings,
) -> anyhow::Result<AgentReply> {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    spinner.set_message(style("Агент думает...").magenta().to_string());
    spinner.enable_steady_tick(Duration::from_millis(80));

    let result = agent.ask(history, settings).await;

    spinner.finish_and_clear();
    result
}
