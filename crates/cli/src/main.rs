mod activity;
mod activity_daemon;
mod agent;
mod chats;
mod clipboard;
mod cli;
mod folder_picker;
mod index;
mod logging;
mod markdown;
mod mcp;
mod pipeline;
mod tool_loop;
mod tui;

use agent::CliAgent;
use agentcore::agent::{Agent, AgentReply, Message, MessageMeta};
use anyhow::Context;
use clap::Parser;
use cli::{
    ActivityAction, ActivityConfigAction, BranchesAction, Cli, Commands, ConfigAction, ContextLimitAction, FactsAction, FormatAction,
    GitToolsAction, IndexAction, IndexConfigAction, OllamaAction, PipelineAction, PipelineConfigAction, ProfilesAction, SamplingAction, SummaryAction,
};
use std::sync::Arc;
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
        Commands::Activity { action } => run_activity(action).await?,
        Commands::Pipeline { action } => run_pipeline_command(action).await?,
        Commands::Index { action } => run_index_command(action).await?,
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
    let reply = if settings.git_tools_active() || config.activity_chat_tools_active() || config.pipeline_active() || config.index_active() {
        ask_with_tools(&agent, &history, &settings, &config).await?
    } else {
        ask_with_spinner(&agent, &history, &settings).await?
    };
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
        ConfigAction::GitTools { action } => run_git_tools_action(action)?,
        ConfigAction::Activity { action } => run_activity_config(action)?,
        ConfigAction::Pipeline { action } => run_pipeline_config(action)?,
        ConfigAction::Index { action } => run_index_config(action)?,
        ConfigAction::SetInvariantsPath { path } => {
            let mut config = load_config()?;
            config.invariants_path = if path.trim().is_empty() { None } else { Some(path) };
            config.save()?;
            println!("{}", style("Путь к файлу инвариантов сохранён.").green().bold());
            print_invariants_path(&config);
        }
        ConfigAction::Invariants => run_config_invariants()?,
    }
    Ok(())
}

fn print_invariants_path(config: &Config) {
    println!(
        "{} {}",
        style("invariants_path:").cyan().bold(),
        config
            .invariants_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<не задан>".to_string())
    );
}

/// Печатает активные инварианты из настроенного файла: источник —
/// конфигурация оператора, не диалог (design.md, «Отдельный тип
/// `InvariantSet`»). Само содержимое чата эта команда не читает и не меняет.
fn run_config_invariants() -> anyhow::Result<()> {
    let config = load_config()?;
    let Some(path) = config.invariants_path() else {
        println!("{}", style("Путь к файлу инвариантов не задан.").yellow());
        return Ok(());
    };
    let set = agentcore::invariants::InvariantSet::load(&path)?;
    if set.is_empty() {
        println!("Инвариантов нет.");
        return Ok(());
    }
    for invariant in &set.invariants {
        println!(
            "{} {} — {}",
            style(format!("[{}]", invariant.id)).cyan().bold(),
            style(format!("({})", invariant.category)).dim(),
            invariant.statement
        );
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

fn run_git_tools_action(action: GitToolsAction) -> anyhow::Result<()> {
    match action {
        GitToolsAction::Set {
            enabled,
            repository,
            allowed_tools,
            max_iterations,
        } => {
            if enabled.is_none() && repository.is_none() && allowed_tools.is_none() && max_iterations.is_none() {
                anyhow::bail!(
                    "укажите хотя бы одно значение: enabled, --repository, --allowed-tools или --max-iterations"
                );
            }
            if let Some(iterations) = max_iterations
                && (iterations == 0 || iterations > agentcore::config::MAX_TOOL_ITERATIONS)
            {
                anyhow::bail!(
                    "--max-iterations должен быть от 1 до {}",
                    agentcore::config::MAX_TOOL_ITERATIONS
                );
            }
            let mut config = load_config()?;
            if let Some(enabled) = enabled {
                config.git_tools_enabled = Some(parse_bool_flag(&enabled)?);
            }
            if let Some(repository) = repository {
                config.git_repository = Some(repository).filter(|path| !path.trim().is_empty());
            }
            if let Some(allowed) = allowed_tools {
                config.git_allowed_tools = Some(parse_tool_list(&allowed)).filter(|list| !list.is_empty());
            }
            if max_iterations.is_some() {
                config.tool_max_iterations = max_iterations;
            }
            config.save()?;
            println!(
                "{}",
                style("Умолчания git-инструментов сохранены.").green().bold()
            );
            print_git_tools(&config);
        }
        GitToolsAction::Clear => {
            let mut config = load_config()?;
            config.git_tools_enabled = None;
            config.git_repository = None;
            config.git_allowed_tools = None;
            config.tool_max_iterations = None;
            config.save()?;
            println!("{}", style("Умолчания git-инструментов сняты.").green().bold());
        }
        GitToolsAction::Show => {
            let config = load_config()?;
            print_git_tools(&config);
        }
    }
    Ok(())
}

fn run_pipeline_config(action: PipelineConfigAction) -> anyhow::Result<()> {
    match action {
        PipelineConfigAction::Set { enabled, root, output } => {
            if enabled.is_none() && root.is_none() && output.is_none() {
                anyhow::bail!("укажите хотя бы одно значение: enabled, --root или --output");
            }
            let mut config = load_config()?;
            if let Some(enabled) = enabled {
                config.pipeline_enabled = Some(parse_bool_flag(&enabled)?);
            }
            if let Some(root) = root {
                config.pipeline_root = Some(root).filter(|root| !root.trim().is_empty());
            }
            if let Some(output) = output {
                config.pipeline_output = Some(output).filter(|output| !output.trim().is_empty());
            }
            config.save()?;
            println!("{}", style("Настройки pipeline-mcp сохранены.").green().bold());
            print_pipeline(&config);
        }
        PipelineConfigAction::Clear => {
            let mut config = load_config()?;
            config.pipeline_root = None;
            config.pipeline_output = None;
            config.pipeline_enabled = None;
            config.save()?;
            println!("{}", style("Настройки pipeline-mcp сняты.").green().bold());
        }
        PipelineConfigAction::Show => print_pipeline(&load_config()?),
    }
    Ok(())
}

fn print_pipeline(config: &Config) {
    println!(
        "{} {}",
        style("инструменты пайплайна в чатах:").cyan().bold(),
        if config.pipeline_active() { "включены" } else { "выключены" }
    );
    println!(
        "{} {}",
        style("переключатель:").cyan().bold(),
        if config.pipeline_switch_on() { "вкл" } else { "выкл" }
    );
    println!(
        "{} {}",
        style("каталог поиска:").cyan().bold(),
        config.pipeline_root.as_deref().unwrap_or("(не задан)")
    );
    println!(
        "{} {}",
        style("каталог записи:").cyan().bold(),
        config.effective_pipeline_output().as_deref().unwrap_or("(не задан)")
    );
}

/// `agentcli pipeline run`: процесс `pipeline-mcp` на время команды и
/// цепочка из трёх шагов. Сводку пишет модель умолчаний конфига через
/// sampling, как написала бы её в новом чате.
async fn run_pipeline_command(action: PipelineAction) -> anyhow::Result<()> {
    let PipelineAction::Run {
        query,
        out,
        root,
        output,
        max_results,
        overwrite,
        no_sampling,
    } = action;
    let config = load_config()?;
    let root = root
        .or_else(|| config.pipeline_root.clone())
        .filter(|root| !root.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("не задан каталог поиска: --root <ПУТЬ> или agentcli config pipeline set --root <ПУТЬ>"))?;
    let output = output
        .filter(|output| !output.trim().is_empty())
        .or_else(|| config.pipeline_output.clone().filter(|output| !output.trim().is_empty()))
        .unwrap_or_else(|| format!("{}/{}", root.trim_end_matches('/'), agentcore::config::DEFAULT_PIPELINE_OUTPUT_DIR));
    let sampler: Option<Arc<dyn pipeline::Sampler>> = if no_sampling {
        None
    } else {
        let agent = CliAgent::from_config(&config, exchange_log())?.with_unauthorized_hint(UNAUTHORIZED_HINT);
        Some(Arc::new(pipeline::AgentSampler {
            agent: Arc::new(agent),
            settings: config.default_chat_settings(),
        }))
    };
    let server = pipeline::PipelineServer::start(&pipeline::server_program(), &root, &output, sampler, exchange_log()).await?;
    let request = pipeline::PipelineRequest {
        query: &query,
        file_name: &out,
        max_results,
        overwrite,
    };
    let result = pipeline::run_pipeline(&server, &request, &StepPrinter).await;
    server.shutdown().await;
    let report = result?;

    let matches = report.search["matches"].as_array().map(Vec::len).unwrap_or(0);
    println!(
        "{} {matches} строк в {} файлах{}",
        style("search:").cyan().bold(),
        report.summary["sources"].as_array().map(Vec::len).unwrap_or(0),
        if report.search["truncated"] == true { " (список обрезан)" } else { "" }
    );
    println!(
        "{} {} строк, метод {}",
        style("summarize:").cyan().bold(),
        report.summary["input_matches"],
        report.summary["method"].as_str().unwrap_or("?")
    );
    if let Some(reason) = report.summary["fallback_reason"].as_str() {
        println!("  {}", style(format!("sampling не использован: {reason}")).yellow());
    }
    println!(
        "{} {} ({} байт, sha256 {})",
        style("save_to_file:").cyan().bold(),
        report.saved["path"].as_str().unwrap_or("?"),
        report.saved["bytes"],
        report.saved["sha256"].as_str().unwrap_or("?")
    );
    println!();
    print_markdown(report.summary["summary"].as_str().unwrap_or_default());
    Ok(())
}

/// Строка о каждом шаге цепочки — в stderr, чтобы итог в stdout оставался
/// чистым.
struct StepPrinter;

impl pipeline::PipelineObserver for StepPrinter {
    fn step(&self, index: usize, name: &str) {
        eprintln!("{} {name}", style(format!("[{index}/3]")).dim());
    }
}

const INDEX_STRATEGIES: [&str; 3] = ["fixed", "structure", "all"];
const INDEX_UNITS: [&str; 2] = ["chars", "tokens"];

/// Значение текстового поля конфига: пустая строка снимает его.
fn set_text(slot: &mut Option<String>, value: Option<String>) {
    if let Some(value) = value {
        *slot = Some(value.trim().to_string()).filter(|value| !value.is_empty());
    }
}

/// Число из флага: 0 снимает значение (размер чанка в 0 всё равно невозможен).
fn set_number(slot: &mut Option<usize>, value: Option<usize>) {
    if let Some(value) = value {
        *slot = Some(value).filter(|value| *value > 0);
    }
}

fn check_choice(what: &str, value: &Option<String>, allowed: &[&str]) -> anyhow::Result<()> {
    match value.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) if !allowed.contains(&value) => {
            anyhow::bail!("неизвестное значение {what}: «{value}». Доступны: {}", allowed.join(", "))
        }
        _ => Ok(()),
    }
}

fn run_index_config(action: IndexConfigAction) -> anyhow::Result<()> {
    match action {
        IndexConfigAction::Set {
            root,
            db,
            strategy,
            model,
            unit,
            chunk_size,
            overlap,
            max_section,
            min_section,
            ollama_url,
        } => {
            if [&root, &db, &strategy, &model, &unit, &ollama_url].iter().all(|value| value.is_none())
                && [chunk_size, overlap, max_section, min_section].iter().all(Option::is_none)
            {
                anyhow::bail!("укажите хотя бы одно значение: --root, --db, --strategy, --model, --unit, --chunk-size и т. д.");
            }
            check_choice("--strategy", &strategy, &INDEX_STRATEGIES)?;
            check_choice("--unit", &unit, &INDEX_UNITS)?;
            let mut config = load_config()?;
            set_text(&mut config.index_root, root);
            set_text(&mut config.index_db, db);
            set_text(&mut config.index_strategy, strategy);
            set_text(&mut config.index_model, model);
            set_text(&mut config.index_unit, unit);
            set_text(&mut config.index_ollama_url, ollama_url);
            set_number(&mut config.index_chunk_size, chunk_size);
            set_number(&mut config.index_overlap, overlap);
            set_number(&mut config.index_max_section, max_section);
            set_number(&mut config.index_min_section, min_section);
            config.save()?;
            println!("{}", style("Настройки индекса сохранены.").green().bold());
            print_index(&config);
        }
        IndexConfigAction::Clear => {
            let mut config = load_config()?;
            config.index_root = None;
            config.index_db = None;
            config.index_strategy = None;
            config.index_model = None;
            config.index_unit = None;
            config.index_chunk_size = None;
            config.index_overlap = None;
            config.index_max_section = None;
            config.index_min_section = None;
            config.index_ollama_url = None;
            config.save()?;
            println!("{}", style("Настройки индекса сняты.").green().bold());
        }
        IndexConfigAction::Show => print_index(&load_config()?),
    }
    Ok(())
}

fn print_index(config: &Config) {
    let text = |value: &Option<String>, default: &str| value.clone().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| default.to_string());
    let number = |value: Option<usize>, default: usize| value.map(|v| v.to_string()).unwrap_or_else(|| format!("{default} (умолчание)"));
    let line = |label: &str, value: String| println!("{} {value}", style(format!("{label}:")).cyan().bold());
    line("инструменты индекса в чатах", if config.index_active() { "включены".into() } else { "выключены (не задана база)".into() });
    line("каталог .docx", text(&config.index_root, "(не задан)"));
    line("база", text(&config.index_db, "(не задана)"));
    line("стратегия", text(&config.index_strategy, "all (умолчание)"));
    line("модель", text(&config.index_model, "nomic-embed-text (умолчание)"));
    line("единица размеров", text(&config.index_unit, "chars (умолчание)"));
    line("chunk-size", number(config.index_chunk_size, 1200));
    line("overlap", number(config.index_overlap, 200));
    line("max-section", number(config.index_max_section, 1500));
    line("min-section", number(config.index_min_section, 200));
    line("Ollama", text(&config.index_ollama_url, "http://localhost:11434 (умолчание)"));
}

/// Текст ошибки инструмента индекса — как ошибка команды.
fn index_tool_error(name: &str, message: String) -> anyhow::Error {
    anyhow::anyhow!("{name}: {message}")
}

/// `agentcli index …`: процесс `index-mcp` на время команды. Ход сборки идёт
/// в stderr самого сервера, итог — сюда, в stdout.
async fn run_index_command(action: IndexAction) -> anyhow::Result<()> {
    let mut config = load_config()?;
    let (tool, arguments, progress) = match action {
        IndexAction::Build {
            root,
            db,
            strategy,
            model,
            unit,
            chunk_size,
            overlap,
            max_section,
            min_section,
            min_chars,
        } => {
            check_choice("--strategy", &strategy, &INDEX_STRATEGIES)?;
            check_choice("--unit", &unit, &INDEX_UNITS)?;
            set_text(&mut config.index_root, root);
            set_text(&mut config.index_db, db);
            set_text(&mut config.index_strategy, strategy);
            set_text(&mut config.index_model, model);
            set_text(&mut config.index_unit, unit);
            set_number(&mut config.index_chunk_size, chunk_size);
            set_number(&mut config.index_overlap, overlap);
            set_number(&mut config.index_max_section, max_section);
            set_number(&mut config.index_min_section, min_section);
            let settings = index::IndexSettings::from_config(&config)?;
            let mut arguments = settings.build_arguments().ok_or_else(|| {
                anyhow::anyhow!("не задан каталог с .docx: --root <ПУТЬ> или agentcli config index set --root <ПУТЬ>")
            })?;
            if let Some(min_chars) = min_chars {
                arguments["min_chars"] = serde_json::json!(min_chars);
            }
            (index::INDEX_BUILD, arguments, settings)
        }
        IndexAction::Search { query, strategy, top_k } => {
            check_choice("--strategy", &strategy, &["fixed", "structure"])?;
            set_text(&mut config.index_strategy, strategy);
            let settings = index::IndexSettings::from_config(&config)?;
            let mut arguments = serde_json::json!({ "query": query });
            if let Some(top_k) = top_k {
                arguments["top_k"] = serde_json::json!(top_k);
            }
            (index::INDEX_SEARCH, arguments, settings)
        }
        IndexAction::Status => (index::INDEX_STATUS, serde_json::json!({}), index::IndexSettings::from_config(&config)?),
        // Модели не зависят от базы: без заданной пойдёт умолчание сервера.
        IndexAction::Models => {
            let mut settings = index::IndexSettings::from_config(&config).unwrap_or_default();
            if settings.db.is_empty() {
                settings.db = "index.db".to_string();
            }
            settings.model = None;
            (index::INDEX_MODELS, serde_json::json!({}), settings)
        }
    };
    let server = index::IndexServer::start(&index::server_program(), &progress, index::Progress::Terminal, exchange_log()).await?;
    let result = server.call_json(tool, arguments).await;
    server.shutdown().await;
    let value = result?.map_err(|message| index_tool_error(tool, message))?;
    print_index_result(tool, &value);
    Ok(())
}

fn print_index_result(tool: &str, value: &serde_json::Value) {
    let head = |text: &str| style(text.to_string()).cyan().bold();
    match tool {
        index::INDEX_BUILD => {
            println!("{} {} (dim {}), база {}", head("модель:"), value["model"].as_str().unwrap_or("?"), value["dim"], value["db"].as_str().unwrap_or("?"));
            for strategy in value["strategies"].as_array().into_iter().flatten() {
                println!(
                    "{} {} чанков из {} файлов ({} символов), эмбеддинг {} мс",
                    head(&format!("{}:", strategy["strategy"].as_str().unwrap_or("?"))),
                    strategy["chunks"],
                    strategy["files"],
                    strategy["chars"],
                    strategy["embed_ms"]
                );
            }
        }
        index::INDEX_SEARCH => {
            println!(
                "{} {} (модель {}, dim {})",
                head("стратегия:"),
                value["strategy"].as_str().unwrap_or("?"),
                value["model"].as_str().unwrap_or("?"),
                value["dim"]
            );
            let hits = value["hits"].as_array().cloned().unwrap_or_default();
            if hits.is_empty() {
                println!("ничего не найдено");
            }
            for (i, hit) in hits.iter().enumerate() {
                println!(
                    "\n{} {:.3}  {}  {}",
                    head(&format!("{}.", i + 1)),
                    hit["score"].as_f64().unwrap_or(0.0),
                    hit["section"].as_str().filter(|s| !s.is_empty()).unwrap_or("(без раздела)"),
                    style(hit["source"].as_str().unwrap_or("?")).dim()
                );
                let text = hit["text"].as_str().unwrap_or_default();
                let preview: String = text.chars().take(300).collect();
                println!("   {}{}", preview.replace('\n', " "), if text.chars().count() > 300 { "…" } else { "" });
            }
        }
        index::INDEX_STATUS => {
            println!("{} {}", head("база:"), value["db"].as_str().unwrap_or("?"));
            if value["exists"] == false {
                println!("файла базы ещё нет: agentcli index build");
                return;
            }
            println!("{} {}", head("модель запросов:"), value["search_model"].as_str().unwrap_or("?"));
            for strategy in value["strategies"].as_array().into_iter().flatten() {
                println!(
                    "{} {} чанков, {} файлов, модель {} (dim {}), собрано {}",
                    head(&format!("{}:", strategy["strategy"].as_str().unwrap_or("?"))),
                    strategy["chunks"],
                    strategy["files"],
                    strategy["model"].as_str().unwrap_or("?"),
                    strategy["dim"],
                    strategy["built_at"].as_str().unwrap_or("?")
                );
                println!("   параметры: {}", strategy["params"]);
            }
        }
        _ => {
            let models = value["models"].as_array().cloned().unwrap_or_default();
            if models.is_empty() {
                println!("Ollama не сообщил моделей с эмбеддингами (ollama pull nomic-embed-text)");
            }
            for model in &models {
                println!(
                    "{}  dim {}  контекст {}",
                    model["name"].as_str().unwrap_or("?"),
                    model["dim"].as_u64().map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
                    model["context_length"].as_u64().map(|v| v.to_string()).unwrap_or_else(|| "?".into())
                );
            }
        }
    }
}

fn run_activity_config(action: ActivityConfigAction) -> anyhow::Result<()> {
    match action {
        ActivityConfigAction::Set {
            enabled,
            url,
            token,
            poll_secs,
            chat_tools,
            root,
            schedule,
        } => {
            if enabled.is_none()
                && url.is_none()
                && token.is_none()
                && poll_secs.is_none()
                && chat_tools.is_none()
                && root.is_none()
                && schedule.is_none()
            {
                anyhow::bail!(
                    "укажите хотя бы одно значение: enabled, --url, --token, --poll-secs, --chat-tools, --root или --schedule"
                );
            }
            if let Some(secs) = poll_secs
                && secs < agentcore::config::MIN_ACTIVITY_POLL_SECS
            {
                anyhow::bail!("--poll-secs должен быть не меньше {}", agentcore::config::MIN_ACTIVITY_POLL_SECS);
            }
            let mut config = load_config()?;
            if let Some(enabled) = enabled {
                config.activity_enabled = Some(parse_bool_flag(&enabled)?);
            }
            if let Some(url) = url {
                config.activity_url = Some(url).filter(|url| !url.trim().is_empty());
            }
            if let Some(token) = token {
                config.activity_token = Some(token).filter(|token| !token.trim().is_empty());
            }
            if poll_secs.is_some() {
                config.activity_poll_secs = poll_secs;
            }
            if let Some(chat_tools) = chat_tools {
                config.activity_chat_tools = Some(parse_bool_flag(&chat_tools)?);
            }
            if let Some(root) = root {
                config.activity_root = Some(root).filter(|root| !root.trim().is_empty());
            }
            if let Some(schedule) = schedule {
                config.activity_schedule = Some(schedule).filter(|schedule| !schedule.trim().is_empty());
            }
            config.save()?;
            println!("{}", style("Настройки activity-mcp сохранены.").green().bold());
            print_activity(&config);
        }
        ActivityConfigAction::Clear => {
            let mut config = load_config()?;
            config.activity_enabled = None;
            config.activity_url = None;
            config.activity_token = None;
            config.activity_poll_secs = None;
            config.activity_chat_tools = None;
            config.activity_root = None;
            config.activity_schedule = None;
            config.save()?;
            println!("{}", style("Настройки activity-mcp сняты.").green().bold());
        }
        ActivityConfigAction::Show => print_activity(&load_config()?),
    }
    Ok(())
}

fn print_activity(config: &Config) {
    let on_off = |value: bool| if value { "включены" } else { "выключены" };
    println!(
        "{} {}",
        style("сводки активности (activity-mcp):").cyan().bold(),
        on_off(config.activity_active())
    );
    println!("{} {}", style("адрес:").cyan().bold(), config.effective_activity_url());
    println!(
        "{} {}",
        style("токен:").cyan().bold(),
        if config.activity_token.is_some() { "задан" } else { "<нет>" }
    );
    println!(
        "{} {} с",
        style("опрос новых сводок:").cyan().bold(),
        config.effective_activity_poll_secs()
    );
    println!(
        "{} {}",
        style("инструменты activity_* в чатах:").cyan().bold(),
        on_off(config.activity_chat_tools_active())
    );
    println!(
        "{} {}",
        style("каталог проектов для запуска демона:").cyan().bold(),
        config.activity_root.as_deref().unwrap_or("<не задан>")
    );
    println!(
        "{} {}",
        style("расписание сводок:").cyan().bold(),
        config.activity_schedule.as_deref().unwrap_or("<умолчание демона>")
    );
    println!(
        "{} {}",
        style("демон зарегистрирован клиентом:").cyan().bold(),
        if activity_daemon::installed() { "да" } else { "нет" }
    );
}

/// Команды `agentcli activity`: одно соединение с демоном на команду.
async fn run_activity(action: ActivityAction) -> anyhow::Result<()> {
    let config = load_config()?;
    // Запуск и остановка работают без соединения: демона может ещё не быть.
    match action {
        ActivityAction::Start => {
            println!("Регистрирую демон и жду ответа…");
            let status = activity_daemon::start_from_config(&config, exchange_log())
                .await
                .map_err(anyhow::Error::msg)?;
            println!("{} {status}", style("Демон activity-mcp:").green().bold());
            return Ok(());
        }
        ActivityAction::Stop => {
            activity_daemon::stop().await?;
            println!("{}", style("Демон activity-mcp остановлен и снят с автозапуска.").green().bold());
            return Ok(());
        }
        _ => {}
    }
    let endpoint = activity::Endpoint::from_config(&config);
    let client = activity::ActivityClient::connect(&endpoint, exchange_log()).await?;
    let result = run_activity_action(&client, action).await;
    client.close().await;
    result
}

fn print_digest(digest: &activity::Digest) {
    let mark = if digest.acked { " (прочитана)" } else { "" };
    println!(
        "{}",
        style(format!("Сводка #{}{mark}: {} — {}", digest.id, digest.period_from, digest.period_to))
            .cyan()
            .bold()
    );
    print_markdown(&digest.text);
    println!();
}

async fn run_activity_action(client: &activity::ActivityClient, action: ActivityAction) -> anyhow::Result<()> {
    match action {
        ActivityAction::Digest { all, keep_unread } => {
            let mut digests = client.digests(!all, if all { 5 } else { 20 }).await?;
            if digests.is_empty() {
                println!("{}", if all { "Сводок пока нет." } else { "Непрочитанных сводок нет." });
            }
            // Старые первыми: читаются в том порядке, в каком шли периоды.
            digests.reverse();
            for digest in &digests {
                print_digest(digest);
                if !digest.acked && !keep_unread {
                    client.ack(digest.id).await?;
                }
            }
        }
        ActivityAction::Build => match client.build_digest().await? {
            Some(digest) => print_digest(&digest),
            None => println!("С прошлой сводки изменений нет."),
        },
        ActivityAction::Start | ActivityAction::Stop => unreachable!("обрабатываются в run_activity"),
        ActivityAction::Ack { id } => {
            client.ack(id).await?;
            println!("Сводка {id} отмечена прочитанной.");
        }
        ActivityAction::Status => {
            let (content, is_error) = client.call("activity_projects", &serde_json::json!({})).await?;
            let text: String = content
                .iter()
                .filter_map(|item| item.get("text").and_then(serde_json::Value::as_str))
                .collect();
            if is_error {
                anyhow::bail!("activity_projects: {text}");
            }
            let parsed: serde_json::Value = serde_json::from_str(&text)?;
            let projects = parsed["projects"].as_array().cloned().unwrap_or_default();
            let active: Vec<&serde_json::Value> = projects.iter().filter(|p| p["removed"] != true).collect();
            println!("{} {}", style("демон доступен, проектов:").green().bold(), active.len());
            if activity_daemon::installed() {
                println!("  (запущен клиентом: agentcli activity stop — остановить)");
            }
            for project in active {
                let branch = project["branch"].as_str().unwrap_or("-");
                let dirty = project["uncommitted_files"].as_i64().unwrap_or(0);
                let dirty = if dirty > 0 { format!(", незакоммичено: {dirty}") } else { String::new() };
                println!("  {} ({branch}{dirty})", project["name"].as_str().unwrap_or("?"));
            }
        }
    }
    Ok(())
}

/// Список инструментов через запятую, без пустых элементов.
fn parse_tool_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

fn print_git_tools(config: &Config) {
    println!(
        "{} {}",
        style("git-инструменты (git-mcp):").cyan().bold(),
        if config.git_tools_enabled == Some(true) {
            "включены"
        } else {
            "выключены"
        }
    );
    println!(
        "{} {}",
        style("репозиторий:").cyan().bold(),
        config
            .git_repository
            .clone()
            .unwrap_or_else(|| "<не задан>".to_string())
    );
    println!(
        "{} {}",
        style("разрешённые пишущие инструменты:").cyan().bold(),
        match &config.git_allowed_tools {
            Some(list) if !list.is_empty() => list.join(", "),
            _ => "<нет: только читающие>".to_string(),
        }
    );
    println!(
        "{} {}",
        style("лимит итераций:").cyan().bold(),
        config
            .tool_max_iterations
            .map(|v| v.to_string())
            .unwrap_or_else(|| format!("{} (по умолчанию)", agentcore::config::DEFAULT_TOOL_MAX_ITERATIONS))
    );
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
    print_git_tools(&config);
    print_activity(&config);
    print_invariants_path(&config);
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

/// `ask` отказывает каждому пишущему вызову: команда работает в скриптах и
/// конвейерах, и вопрос в stdin повесил бы их. Флага «разрешить запись без
/// подтверждения» нет намеренно.
struct DenyWrites;

const ASK_WRITE_REFUSAL: &str =
    "пишущие инструменты в режиме ask не выполняются: используйте agentcli chat";

#[async_trait::async_trait]
impl tool_loop::ToolApprover for DenyWrites {
    async fn approve(&self, call: &agentcore::agent::ToolCall) -> bool {
        eprintln!(
            "{} {}",
            style("Внимание:").yellow().bold(),
            style(format!(
                "модель запросила пишущий инструмент {}; в режиме ask запись не выполняется",
                call.name
            ))
            .yellow()
        );
        false
    }

    fn refusal(&self) -> &str {
        ASK_WRITE_REFUSAL
    }
}

/// Строка ожидания показывает текущий инструмент.
struct SpinnerObserver(ProgressBar);

impl tool_loop::TurnObserver for SpinnerObserver {
    fn running(&self, call: &agentcore::agent::ToolCall) {
        self.0
            .set_message(style(format!("Инструмент {}...", call.name)).magenta().to_string());
    }
}

/// Разовый вопрос с инструментами: git-сервер запускается на время
/// команды, к демону activity-mcp — соединение на время команды.
async fn ask_with_tools(
    agent: &CliAgent,
    history: &[Message],
    settings: &agentcore::config::ChatSettings,
    config: &Config,
) -> anyhow::Result<AgentReply> {
    let git = if settings.git_tools_active() {
        let repository = settings.git_repository.clone().unwrap_or_default();
        Some(mcp::GitToolServer::start(&repository, exchange_log()).await?)
    } else {
        None
    };
    let git_tools = git.as_ref().map(|server| mcp::GitTools {
        server: server.clone(),
        allowed_writes: settings.git_allowed_tools.clone(),
    });
    // Демон сводок необязателен: без него вопрос задаётся без его
    // инструментов, а не отклоняется.
    let activity_tools = if config.activity_chat_tools_active() {
        match activity::ActivityTools::connect(&activity::Endpoint::from_config(config), exchange_log()).await {
            Ok(tools) => Some(tools),
            Err(err) => {
                eprintln!(
                    "{} {}",
                    style("Внимание:").yellow().bold(),
                    style(format!("инструменты activity_* недоступны: {err}")).yellow()
                );
                None
            }
        }
    } else {
        None
    };
    // Пайплайн тоже необязателен. Sampling идёт отдельным агентом с теми же
    // настройками: серверу нужен владеющий указатель, а `agent` заимствован.
    let pipeline_tools = if config.pipeline_active() {
        let sampler: Arc<dyn pipeline::Sampler> = Arc::new(pipeline::AgentSampler {
            agent: Arc::new(CliAgent::from_config(config, exchange_log())?.with_unauthorized_hint(UNAUTHORIZED_HINT)),
            settings: settings.clone(),
        });
        match pipeline::PipelineServer::start_from_config(config, Some(sampler), exchange_log()).await {
            Ok(server) => Some(pipeline::PipelineTools { server }),
            Err(err) => {
                eprintln!(
                    "{} {}",
                    style("Внимание:").yellow().bold(),
                    style(format!("инструменты пайплайна недоступны: {err}")).yellow()
                );
                None
            }
        }
    } else {
        None
    };
    // Индекс документов тоже необязателен; `index_build` в `ask` отклоняет
    // `DenyWrites`, поэтому ход сборки показывать некому.
    let index_tools = if config.index_active() {
        let started = match index::IndexSettings::from_config(config) {
            Ok(settings) => index::IndexTools::start(&settings, index::Progress::Discard, exchange_log()).await,
            Err(err) => Err(err),
        };
        match started {
            Ok(tools) => Some(tools),
            Err(err) => {
                eprintln!(
                    "{} {}",
                    style("Внимание:").yellow().bold(),
                    style(format!("инструменты индекса недоступны: {err}")).yellow()
                );
                None
            }
        }
    } else {
        None
    };
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    spinner.set_message(style("Агент думает...").magenta().to_string());
    spinner.enable_steady_tick(Duration::from_millis(80));

    let result = {
        let tools = tool_loop::ToolSet::default()
            .with(git_tools.as_ref().map(|tools| tools as &dyn tool_loop::ToolExecutor))
            .with(activity_tools.as_ref().map(|tools| tools as &dyn tool_loop::ToolExecutor))
            .with(pipeline_tools.as_ref().map(|tools| tools as &dyn tool_loop::ToolExecutor))
            .with(index_tools.as_ref().map(|tools| tools as &dyn tool_loop::ToolExecutor));
        let mut backend = tool_loop::HistoryTurn {
            agent,
            history,
            settings,
        };
        let observer = SpinnerObserver(spinner.clone());
        tool_loop::run_tool_loop(
            &mut backend,
            &tools,
            &DenyWrites,
            settings.effective_tool_max_iterations(),
            &observer,
        )
        .await
    };
    spinner.finish_and_clear();
    if let Some(server) = git {
        server.shutdown().await;
    }
    if let Some(tools) = activity_tools {
        tools.close().await;
    }
    if let Some(tools) = pipeline_tools {
        tools.server.shutdown().await;
    }
    if let Some(tools) = index_tools {
        tools.server.shutdown().await;
    }
    result.map_err(|err| err.error)
}

