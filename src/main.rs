mod agent;
mod chats;
mod clipboard;
mod cli;
mod config;
mod markdown;
mod tui;

use agent::{Agent, AgentReply, HttpAgent, Message, MessageMeta};
use clap::Parser;
use cli::{Cli, Commands, ConfigAction, FormatAction, SamplingAction};
use config::{Config, ReasoningMode, ThinkingMode};
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use markdown::agent_skin;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Ask { prompt } => run_ask(prompt).await?,
        Commands::Chat => run_chat().await?,
        Commands::Config { action } => run_config(action)?,
    }

    Ok(())
}

async fn run_ask(prompt: String) -> anyhow::Result<()> {
    let config = Config::load()?;
    let agent = HttpAgent::from_config(&config)?;
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
    let config = Config::load()?;
    let agent = HttpAgent::from_config(&config)?;
    tui::run(agent).await
}

fn run_config(action: ConfigAction) -> anyhow::Result<()> {
    match action {
        ConfigAction::SetKey { key } => {
            let mut config = Config::load()?;
            config.api_key = Some(key);
            config.save()?;
            println!("{}", style("API key сохранён.").green().bold());
        }
        ConfigAction::Show => show_config()?,
        ConfigAction::Format { action } => run_format_action(action)?,
        ConfigAction::Sampling { action } => run_sampling_action(action)?,
        ConfigAction::Reasoning {
            mode,
            experts,
            thinking,
        } => run_reasoning_action(mode, experts, thinking)?,
    }
    Ok(())
}

fn show_config() -> anyhow::Result<()> {
    let config = Config::load()?;
    println!(
        "{} {}",
        style("api_key: ").cyan().bold(),
        config.masked_api_key()
    );
    println!(
        "{} {}",
        style("base_url:").cyan().bold(),
        config.base_url.as_deref().unwrap_or("<не задан>")
    );
    println!(
        "{} {}",
        style("model:   ").cyan().bold(),
        config.model.as_deref().unwrap_or("<не задан>")
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

fn print_response_format(format: &config::ResponseFormat) {
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

fn print_sampling_params(sampling: &config::SamplingParams) {
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
    agent: &HttpAgent,
    history: &[Message],
    settings: &config::ChatSettings,
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
