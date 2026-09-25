//! Запуск и остановка демона `activity-mcp` самим клиентом.
//!
//! Демон должен работать круглосуточно и переживать и выход из TUI, и
//! перезагрузку, поэтому клиент не держит его дочерним процессом (как
//! `git-mcp`), а регистрирует в системном супервизоре: LaunchAgent в
//! launchd на macOS, пользовательский unit systemd на Linux. Супервизор
//! запускает демон при входе в систему и поднимает после падения; клиент
//! лишь пишет описание службы из своего конфига и дёргает `launchctl` или
//! `systemctl`.

use crate::activity::{ActivityClient, Endpoint};
use agentcore::config::Config;
use agentcore::logging::ExchangeLog;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const PROGRAM_NAME: &str = "activity-mcp";
/// Путь к бинарнику демона — для сборки из исходников без установки.
pub const PROGRAM_ENV: &str = "AGENTCLI_ACTIVITY_MCP";
/// Метка LaunchAgent; совпадает с `contrib/launchd` репозитория демона,
/// чтобы ручная установка и установка из клиента не жили параллельно.
pub const LAUNCHD_LABEL: &str = "com.github.egor-liadsky.activity-mcp";
pub const SYSTEMD_UNIT: &str = "activity-mcp.service";

/// Первый обход проектов идёт до того, как демон начинает слушать HTTP:
/// на сотне репозиториев это секунды.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const READY_STEP: Duration = Duration::from_millis(500);

/// Где искать демон: путь из `AGENTCLI_ACTIVITY_MCP`, рядом с `agentcli`
/// (туда кладёт `cargo install`), затем по `PATH`. Супервизору нужен
/// абсолютный путь — `PATH` у launchd не тот, что у терминала.
pub fn program() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(PROGRAM_ENV).filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let name = format!("{PROGRAM_NAME}{}", std::env::consts::EXE_SUFFIX);
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&name)))
        .filter(|path| path.is_file());
    let in_path = || {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
            .map(|dir| dir.join(&name))
            .find(|path| path.is_file())
    };
    beside.or_else(in_path).ok_or_else(|| {
        anyhow!(
            "не найден {PROGRAM_NAME}: установите его (cargo install --git \
             https://github.com/Egor-Liadsky/activity-mcp-agent activity-mcp) \
             или укажите путь в {PROGRAM_ENV}"
        )
    })
}

/// `host:port` из адреса MCP. Демон слушает только loopback, поэтому и
/// адрес клиента должен быть loopback — иначе клиент ждал бы демон не там,
/// где тот запустится.
pub fn listen_address(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow!("адрес демона {url} должен начинаться с http://"))?;
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("в адресе демона {url} нет порта"))?;
    port.parse::<u16>().map_err(|_| anyhow!("неверный порт в адресе демона {url}"))?;
    let host = match host {
        "localhost" | "127.0.0.1" => "127.0.0.1",
        "[::1]" => "[::1]",
        other => bail!("демон слушает только loopback, а в адресе {url} хост {other}"),
    };
    Ok(format!("{host}:{port}"))
}

/// Что и с какими аргументами запускать.
#[derive(Debug, Clone, PartialEq)]
pub struct DaemonSpec {
    pub program: PathBuf,
    pub root: PathBuf,
    pub listen: String,
    pub schedule: Option<String>,
    pub token: Option<String>,
}

impl DaemonSpec {
    pub fn from_config(config: &Config) -> Result<Self> {
        let root = config
            .activity_root
            .as_deref()
            .map(str::trim)
            .filter(|root| !root.is_empty())
            .ok_or_else(|| anyhow!("не задан каталог проектов: поле «Каталог проектов» в Ctrl+P или --root"))?;
        let root = expand_home(root);
        if !root.is_dir() {
            bail!("каталог проектов {} не существует", root.display());
        }
        Ok(Self {
            program: program()?,
            root: root.canonicalize().unwrap_or(root),
            listen: listen_address(&config.effective_activity_url())?,
            schedule: config
                .activity_schedule
                .clone()
                .filter(|schedule| !schedule.trim().is_empty()),
            token: config.activity_token.clone().filter(|token| !token.trim().is_empty()),
        })
    }

    /// Аргументы демона. Токен идёт файлом, а не аргументом: командная
    /// строка процесса видна всем пользователям машины.
    pub fn arguments(&self, token_file: Option<&Path>) -> Vec<String> {
        let mut args = vec![
            "--root".to_string(),
            self.root.to_string_lossy().into_owned(),
            "--listen".to_string(),
            self.listen.clone(),
        ];
        if let Some(schedule) = &self.schedule {
            args.extend(["--schedule".to_string(), schedule.clone()]);
        }
        if let Some(file) = token_file {
            args.extend(["--token-file".to_string(), file.to_string_lossy().into_owned()]);
        }
        args
    }
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().map(|home| home.join(rest)).unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

fn home() -> Result<PathBuf> {
    dirs::home_dir().context("не удалось определить домашний каталог")
}

/// Токен — в файл рядом с конфигом клиента, только для владельца.
fn write_token_file(token: &str) -> Result<PathBuf> {
    let dir = dirs::config_dir().context("не удалось определить каталог конфигов")?.join("agentcli");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("activity-token");
    std::fs::write(&path, token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// LaunchAgent: запуск при входе, перезапуск после падения, не чаще раза в
/// 30 с. `PATH` берётся у клиента: демону нужен тот же `git`.
pub fn launchd_plist(spec: &DaemonSpec, token_file: Option<&Path>, log: &Path, path_env: &str) -> String {
    let mut args = vec![spec.program.to_string_lossy().into_owned()];
    args.extend(spec.arguments(token_file));
    let args: String = args
        .iter()
        .map(|arg| format!("        <string>{}</string>\n", xml_escape(arg)))
        .collect();
    let log = xml_escape(&log.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Создан agentcli (agentcli activity start или Ctrl+P в TUI). -->
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{args}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>30</integer>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{}</string>
    </dict>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        xml_escape(path_env)
    )
}

/// Аргумент `ExecStart` в кавычках systemd.
fn systemd_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%"))
}

pub fn systemd_unit(spec: &DaemonSpec, token_file: Option<&Path>, path_env: &str) -> String {
    let mut args = vec![spec.program.to_string_lossy().into_owned()];
    args.extend(spec.arguments(token_file));
    let exec: Vec<String> = args.iter().map(|arg| systemd_quote(arg)).collect();
    format!(
        "# Создан agentcli (agentcli activity start или Ctrl+P в TUI).\n\
         [Unit]\n\
         Description=activity-mcp: activity digests of git projects\n\n\
         [Service]\n\
         ExecStart={}\n\
         Environment={}\n\
         Restart=on-failure\n\
         RestartSec=30\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        exec.join(" "),
        systemd_quote(&format!("PATH={path_env}"))
    )
}

/// Супервизор этой ОС.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Supervisor {
    Launchd,
    Systemd,
}

fn supervisor() -> Result<Supervisor> {
    if cfg!(target_os = "macos") {
        Ok(Supervisor::Launchd)
    } else if cfg!(target_os = "linux") {
        Ok(Supervisor::Systemd)
    } else {
        bail!("автозапуск демона поддержан только на macOS и Linux: запустите activity-mcp вручную")
    }
}

fn launchd_plist_path() -> Result<PathBuf> {
    Ok(home()?.join("Library/LaunchAgents").join(format!("{LAUNCHD_LABEL}.plist")))
}

fn systemd_unit_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("не удалось определить каталог конфигов")?
        .join("systemd/user")
        .join(SYSTEMD_UNIT))
}

/// Зарегистрирован ли демон клиентом у супервизора.
pub fn installed() -> bool {
    match supervisor() {
        Ok(Supervisor::Launchd) => launchd_plist_path().is_ok_and(|path| path.is_file()),
        Ok(Supervisor::Systemd) => systemd_unit_path().is_ok_and(|path| path.is_file()),
        Err(_) => false,
    }
}

async fn run(program: &str, args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("не удалось запустить {program}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{program} {} завершился с ошибкой: {}", args.join(" "), stderr.trim())
    }
}

async fn launchd_domain() -> Result<String> {
    Ok(format!("gui/{}", run("id", &["-u"]).await?))
}

/// Регистрирует демон у супервизора и запускает; уже работающий
/// перезапускается с новыми параметрами. Возвращает, где смотреть журнал.
pub async fn start(spec: &DaemonSpec) -> Result<String> {
    let token_file = spec.token.as_deref().map(write_token_file).transpose()?;
    let path_env = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    match supervisor()? {
        Supervisor::Launchd => {
            let plist = launchd_plist_path()?;
            let log = home()?.join("Library/Logs/activity-mcp.log");
            if let Some(parent) = plist.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&plist, launchd_plist(spec, token_file.as_deref(), &log, &path_env))?;
            let domain = launchd_domain().await?;
            // Прежняя регистрация снимается: bootstrap поверх неё отказывает.
            let _ = run("launchctl", &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")]).await;
            run("launchctl", &["bootstrap", &domain, &plist.to_string_lossy()]).await?;
            Ok(format!("launchd, журнал: {}", log.display()))
        }
        Supervisor::Systemd => {
            let unit = systemd_unit_path()?;
            if let Some(parent) = unit.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&unit, systemd_unit(spec, token_file.as_deref(), &path_env))?;
            run("systemctl", &["--user", "daemon-reload"]).await?;
            run("systemctl", &["--user", "enable", SYSTEMD_UNIT]).await?;
            run("systemctl", &["--user", "restart", SYSTEMD_UNIT]).await?;
            Ok(format!("systemd --user, журнал: journalctl --user -u {SYSTEMD_UNIT}"))
        }
    }
}

/// Останавливает демон и снимает регистрацию: без неё он не вернётся при
/// следующем входе в систему.
pub async fn stop() -> Result<()> {
    match supervisor()? {
        Supervisor::Launchd => {
            let plist = launchd_plist_path()?;
            let domain = launchd_domain().await?;
            let unloaded = run("launchctl", &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")]).await;
            let existed = plist.is_file();
            if existed {
                std::fs::remove_file(&plist)?;
            }
            if unloaded.is_err() && !existed {
                bail!("демон не зарегистрирован клиентом: если он запущен вручную, остановите его там же");
            }
        }
        Supervisor::Systemd => {
            let unit = systemd_unit_path()?;
            run("systemctl", &["--user", "disable", "--now", SYSTEMD_UNIT]).await?;
            if unit.is_file() {
                std::fs::remove_file(&unit)?;
                let _ = run("systemctl", &["--user", "daemon-reload"]).await;
            }
        }
    }
    Ok(())
}

/// Состояние демона одной строкой: отвечает ли он и сколько проектов видит.
pub async fn status(endpoint: &Endpoint, log: Arc<ExchangeLog>) -> std::result::Result<String, String> {
    let client = ActivityClient::connect(endpoint, log).await.map_err(|err| err.to_string())?;
    let result = client.call("activity_projects", &json!({})).await;
    client.close().await;
    let (content, is_error) = result.map_err(|err| err.to_string())?;
    let text: String = content
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect();
    if is_error {
        return Err(text);
    }
    let projects = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| value["projects"].as_array().map(|list| list.iter().filter(|p| p["removed"] != true).count()))
        .unwrap_or(0);
    Ok(format!("работает, проектов: {projects}"))
}

/// Ждёт, пока запущенный демон начнёт отвечать.
pub async fn wait_ready(endpoint: &Endpoint, log: Arc<ExchangeLog>) -> std::result::Result<String, String> {
    let started = Instant::now();
    loop {
        match status(endpoint, log.clone()).await {
            Ok(status) => return Ok(status),
            Err(err) if started.elapsed() >= READY_TIMEOUT => {
                return Err(format!(
                    "демон зарегистрирован, но не ответил за {} с: {err}",
                    READY_TIMEOUT.as_secs()
                ));
            }
            Err(_) => tokio::time::sleep(READY_STEP).await,
        }
    }
}

/// Запуск целиком, для TUI и CLI: параметры из конфига, регистрация,
/// ожидание ответа.
pub async fn start_from_config(config: &Config, log: Arc<ExchangeLog>) -> std::result::Result<String, String> {
    let spec = DaemonSpec::from_config(config).map_err(|err| format!("{err:#}"))?;
    let place = start(&spec).await.map_err(|err| format!("{err:#}"))?;
    let status = wait_ready(&Endpoint::from_config(config), log).await?;
    Ok(format!("{status} ({place})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> DaemonSpec {
        DaemonSpec {
            program: PathBuf::from("/opt/bin/activity-mcp"),
            root: PathBuf::from("/Users/я/projects & co"),
            listen: "127.0.0.1:7878".into(),
            schedule: Some("0 9,18 * * *".into()),
            token: None,
        }
    }

    #[test]
    fn listen_address_is_loopback_only() {
        assert_eq!(listen_address("http://127.0.0.1:7878/mcp").unwrap(), "127.0.0.1:7878");
        assert_eq!(listen_address("http://localhost:9000/mcp").unwrap(), "127.0.0.1:9000");
        assert_eq!(listen_address("http://[::1]:7878/mcp").unwrap(), "[::1]:7878");
        assert!(listen_address("http://10.0.0.5:7878/mcp").is_err());
        assert!(listen_address("https://127.0.0.1:7878/mcp").is_err());
        assert!(listen_address("http://127.0.0.1/mcp").is_err());
    }

    #[test]
    fn arguments_carry_schedule_and_token_file() {
        let args = spec().arguments(Some(Path::new("/c/activity-token")));
        assert_eq!(
            args,
            vec![
                "--root",
                "/Users/я/projects & co",
                "--listen",
                "127.0.0.1:7878",
                "--schedule",
                "0 9,18 * * *",
                "--token-file",
                "/c/activity-token"
            ]
        );
        let no_schedule = DaemonSpec { schedule: None, ..spec() };
        assert_eq!(no_schedule.arguments(None).len(), 4);
    }

    #[test]
    fn plist_escapes_arguments() {
        let plist = launchd_plist(&spec(), None, Path::new("/l/a.log"), "/usr/bin:/bin");
        assert!(plist.contains("<string>/opt/bin/activity-mcp</string>"));
        assert!(plist.contains("<string>/Users/я/projects &amp; co</string>"));
        assert!(plist.contains(&format!("<string>{LAUNCHD_LABEL}</string>")));
        assert!(plist.contains("<key>KeepAlive</key>"));
    }

    #[test]
    fn systemd_unit_quotes_arguments() {
        let unit = systemd_unit(&spec(), None, "/usr/bin");
        assert!(unit.contains(r#"ExecStart="/opt/bin/activity-mcp" "--root" "/Users/я/projects & co""#), "{unit}");
        assert!(unit.contains(r#"Environment="PATH=/usr/bin""#));
        assert_eq!(systemd_quote(r#"a"b%c"#), r#""a\"b%%c""#);
    }

    #[test]
    fn spec_requires_existing_root() {
        let missing = Config {
            activity_root: Some("/definitely/not/here".into()),
            ..Config::default()
        };
        let err = DaemonSpec::from_config(&missing).unwrap_err().to_string();
        assert!(err.contains("не существует"), "{err}");
        let err = DaemonSpec::from_config(&Config::default()).unwrap_err().to_string();
        assert!(err.contains("каталог проектов"), "{err}");
    }

    /// Живой цикл через настоящий супервизор: регистрация, ответ демона,
    /// остановка. Бинарник — из `AGENTCLI_ACTIVITY_MCP` (абсолютный путь),
    /// порт 7981, чтобы не задеть демон на порту по умолчанию.
    #[tokio::test]
    #[ignore]
    async fn live_start_and_stop_through_supervisor() {
        let root = std::env::temp_dir().join(format!("agentcli-daemon-live-{}", agentcore::logging::request_id()));
        let project = root.join("app");
        std::fs::create_dir_all(&project).unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&project)
            .status()
            .unwrap();
        assert!(status.success());
        let spec = DaemonSpec {
            program: program().expect("activity-mcp"),
            root: root.canonicalize().unwrap(),
            listen: "127.0.0.1:7981".into(),
            schedule: None,
            token: None,
        };
        let endpoint = Endpoint {
            url: "http://127.0.0.1:7981/mcp".into(),
            token: None,
        };
        let log = Arc::new(ExchangeLog::disabled());
        start(&spec).await.expect("регистрация");
        assert!(installed());
        let status = wait_ready(&endpoint, log.clone()).await;
        stop().await.expect("остановка");
        assert!(!installed());
        assert_eq!(status.as_deref(), Ok("работает, проектов: 1"));
        assert!(status_after_stop(&endpoint, log).await);
        let _ = std::fs::remove_dir_all(root);
    }

    /// После остановки демон перестаёт отвечать (не сразу — даём секунду).
    async fn status_after_stop(endpoint: &Endpoint, log: Arc<ExchangeLog>) -> bool {
        for _ in 0..10 {
            if super::status(endpoint, log.clone()).await.is_err() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        false
    }
}
