mod agent;
mod chats;
mod cli;
mod config;
mod markdown;
mod tui;

use agent::{Agent, HttpAgent, Message, Role};
use clap::Parser;
use cli::{Cli, Commands, ConfigAction, FormatAction};
use config::Config;
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use markdown::agent_skin;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Ask { prompt } => {
            let config = Config::load()?;
            let agent = HttpAgent::from_config(&config)?;
            let history = vec![Message {
                role: Role::User,
                content: prompt,
            }];
            let answer = ask_with_spinner(&agent, &history).await?;
            print_markdown(&answer);
        }
        Commands::Chat => {
            let config = Config::load()?;
            let agent = HttpAgent::from_config(&config)?;
            tui::run(agent).await?;
        }
        Commands::Config { action } => match action {
            ConfigAction::SetKey { key } => {
                let mut config = Config::load()?;
                config.api_key = Some(key);
                config.save()?;
                println!("{}", style("API key сохранён.").green().bold());
            }
            ConfigAction::Show => {
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
                    style("режим ответа:").cyan().bold(),
                    if config.custom_response_mode {
                        "кастомный"
                    } else {
                        "дефолтный"
                    }
                );
                print_response_format(&config.response_format);
            }
            ConfigAction::Format { action } => match action {
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
            },
        },
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

async fn ask_with_spinner(agent: &HttpAgent, history: &[Message]) -> anyhow::Result<String> {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    spinner.set_message(style("Агент думает...").magenta().to_string());
    spinner.enable_steady_tick(Duration::from_millis(80));

    let result = agent.ask(history).await;

    spinner.finish_and_clear();
    result
}
