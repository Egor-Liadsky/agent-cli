mod agent;
mod chats;
mod cli;
mod config;
mod markdown;
mod tui;

use agent::{Agent, HttpAgent, Message, Role};
use clap::Parser;
use cli::{Cli, ConfigAction, Commands};
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
            }
        },
    }

    Ok(())
}

fn print_markdown(text: &str) {
    agent_skin().print_text(text);
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
