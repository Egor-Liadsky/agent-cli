use crate::agent::{Agent, HttpAgent, Message, Role};
use crate::chats::{self, ChatSession};
use crate::config::{ChatSettings, ReasoningMode, ResponseFormat, SamplingParams};
use crate::markdown::agent_skin;
use ansi_to_tui::IntoText;
use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
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
use std::time::Duration;
use tokio::sync::mpsc;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const BILLY_ART: &str = include_str!("assets/billy.ans");
/// Максимум одновременно открытых на экране чатов (панелей).
const MAX_PANES: usize = 3;

enum ChatEvent {
    Response(String, anyhow::Result<String>),
}

#[derive(PartialEq)]
enum Focus {
    Input,
    Sidebar,
    Settings,
}

#[derive(Clone, Copy, PartialEq)]
enum FormatField {
    Mode,
    Reasoning,
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
    Format,
    Reasoning,
    Sampling,
}

impl SettingsSection {
    const ALL: [SettingsSection; 3] = [
        SettingsSection::Format,
        SettingsSection::Reasoning,
        SettingsSection::Sampling,
    ];

    fn label(self) -> &'static str {
        match self {
            SettingsSection::Format => "Формат ответа",
            SettingsSection::Reasoning => "Рассуждение",
            SettingsSection::Sampling => "Сэмплинг",
        }
    }

    fn fields(self) -> &'static [FormatField] {
        match self {
            SettingsSection::Format => &[
                FormatField::Mode,
                FormatField::Description,
                FormatField::MaxLength,
                FormatField::Stop,
                FormatField::StopInstruction,
            ],
            SettingsSection::Reasoning => &[FormatField::Reasoning, FormatField::Experts],
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

impl FormatField {
    fn label(self) -> &'static str {
        match self {
            FormatField::Mode => "Режим",
            FormatField::Reasoning => "Стратегия рассуждения",
            FormatField::Experts => "Эксперты (через запятую, пусто — состав по умолчанию)",
            FormatField::Description => "Описание формата",
            FormatField::MaxLength => "Макс. длина ответа (токены)",
            FormatField::Stop => "Stop-последовательности (через запятую)",
            FormatField::StopInstruction => "Инструкция завершения ответа",
            FormatField::Temperature => "Temperature",
            FormatField::TopP => "Top-p",
            FormatField::TopK => "Top-k",
            FormatField::FrequencyPenalty => "Frequency penalty",
            FormatField::PresencePenalty => "Presence penalty",
        }
    }

    /// Поля стратегии рассуждения: не зависят от режима формата ответа.
    fn is_reasoning_detail(self) -> bool {
        matches!(self, FormatField::Experts)
    }

    /// Поля-переключатели: редактируются стрелками/пробелом, а не вводом текста.
    fn is_toggle(self) -> bool {
        matches!(self, FormatField::Mode | FormatField::Reasoning)
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
    custom_mode: bool,
    reasoning: ReasoningMode,
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
    fn from_chat(chat: &ChatSession) -> Self {
        let settings = &chat.settings;
        let custom_mode = settings.custom_response_mode;
        let format = settings.response_format.clone();
        let sampling = &settings.sampling;
        Self {
            chat_id: chat.id.clone(),
            chat_title: chat.title.clone(),
            custom_mode,
            reasoning: settings.reasoning,
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
            .filter(|field| {
                *field != FormatField::Experts || self.reasoning == ReasoningMode::ExpertPanel
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

    /// Сбросить текущее поле к значению по умолчанию.
    fn reset_field(&mut self) {
        match self.current_field() {
            Some(FormatField::Mode) => self.custom_mode = false,
            Some(FormatField::Reasoning) => self.reasoning = ReasoningMode::default(),
            _ => {
                if let Some(value) = self.field_value_mut() {
                    value.clear();
                }
            }
        }
    }

    fn field_value_mut(&mut self) -> Option<&mut String> {
        match self.current_field()? {
            FormatField::Mode | FormatField::Reasoning => None,
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

pub async fn run(agent: HttpAgent, default_settings: ChatSettings) -> anyhow::Result<()> {
    let agent = Arc::new(agent);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, agent, default_settings).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
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
    scroll: u16,
    auto_scroll: bool,
    scroll_to_message: Option<usize>,
}

impl Default for ChatUi {
    fn default() -> Self {
        Self {
            input: String::new(),
            pending: false,
            scroll: 0,
            auto_scroll: true,
            scroll_to_message: None,
        }
    }
}

struct AppState {
    chats: Vec<ChatSession>,
    chat_ui: HashMap<String, ChatUi>,
    /// Id чатов, открытых сейчас на экране, по одной панели на элемент.
    panes: Vec<String>,
    active_pane: usize,
    sidebar_selected: usize,
    spinner_frame: usize,
    focus: Focus,
    settings: Option<SettingsEditor>,
    /// Параметры для новых чатов (из глобального конфига).
    default_settings: ChatSettings,
}

impl AppState {
    fn active_chat_id(&self) -> String {
        self.panes[self.active_pane].clone()
    }

    fn chat_index(&self, id: &str) -> usize {
        self.chats
            .iter()
            .position(|c| c.id == id)
            .expect("панель ссылается на существующий чат")
    }

    fn is_pending(&self, id: &str) -> bool {
        self.chat_ui.get(id).map(|u| u.pending).unwrap_or(false)
    }

    fn any_pane_pending(&self) -> bool {
        self.panes.iter().any(|id| self.is_pending(id))
    }
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    agent: Arc<HttpAgent>,
    default_settings: ChatSettings,
) -> anyhow::Result<()> {
    let mut chats: Vec<ChatSession> = chats::list_chats().unwrap_or_default();
    chats.insert(0, ChatSession::new(default_settings.clone()));

    let mut chat_ui: HashMap<String, ChatUi> = HashMap::new();
    for chat in &chats {
        chat_ui.insert(chat.id.clone(), ChatUi::default());
    }
    let first_id = chats[0].id.clone();

    let mut state = AppState {
        chats,
        chat_ui,
        panes: vec![first_id],
        active_pane: 0,
        sidebar_selected: 0,
        spinner_frame: 0,
        focus: Focus::Input,
        settings: None,
        default_settings,
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<ChatEvent>();
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(80));

    loop {
        terminal.draw(|f| render_ui(f, &mut state))?;

        tokio::select! {
            _ = tick.tick() => {
                if state.any_pane_pending() {
                    state.spinner_frame = (state.spinner_frame + 1) % SPINNER_FRAMES.len();
                }
            }
            maybe_event = events.next() => {
                let Some(Ok(event)) = maybe_event else { continue };
                if let Event::Key(key) = event
                    && key.kind == KeyEventKind::Press
                    && matches!(handle_key(key, &mut state, &agent, &tx), LoopControl::Break)
                {
                    break;
                }
            }
            Some(chat_event) = rx.recv() => {
                handle_chat_event(chat_event, &mut state);
            }
        }
    }

    Ok(())
}

/// Глобальные сочетания клавиш, работающие вне зависимости от фокуса/состояния "pending".
fn handle_global_key(key: crossterm::event::KeyEvent, state: &mut AppState) -> Option<LoopControl> {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Some(LoopControl::Break);
    }
    if key.code == KeyCode::Char('n') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if state.focus == Focus::Settings || state.is_pending(&state.active_chat_id()) {
            return Some(LoopControl::Continue);
        }
        let chat = ChatSession::new(state.default_settings.clone());
        let id = chat.id.clone();
        state.chats.insert(0, chat);
        state.chat_ui.insert(id.clone(), ChatUi::default());
        state.panes[state.active_pane] = id;
        state.sidebar_selected = 0;
        state.focus = Focus::Input;
        return Some(LoopControl::Continue);
    }
    if state.focus == Focus::Settings {
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
    agent: &Arc<HttpAgent>,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> LoopControl {
    if let Some(control) = handle_global_key(key, state) {
        return control;
    }
    if key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if state.focus == Focus::Settings {
            state.settings = None;
            state.focus = Focus::Input;
        } else {
            let chat_index = state.chat_index(&state.active_chat_id());
            state.settings = Some(SettingsEditor::from_chat(&state.chats[chat_index]));
            state.focus = Focus::Settings;
        }
        return LoopControl::Continue;
    }
    if key.code == KeyCode::Tab && state.focus != Focus::Settings {
        state.focus = match state.focus {
            Focus::Input => Focus::Sidebar,
            Focus::Sidebar => Focus::Input,
            Focus::Settings => unreachable!(),
        };
        return LoopControl::Continue;
    }

    match state.focus {
        Focus::Settings => handle_settings_key(key, state),
        Focus::Sidebar => handle_sidebar_key(key, state),
        Focus::Input => {
            if state.is_pending(&state.active_chat_id()) {
                LoopControl::Continue
            } else {
                handle_input_key(key, state, agent, tx)
            }
        }
    }
}

fn handle_settings_key(key: crossterm::event::KeyEvent, state: &mut AppState) -> LoopControl {
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
            {
                Ok((format, sampling)) => {
                    let reasoning = editor.reasoning;
                    // состав сохраняем всегда: при возврате к «Группе экспертов»
                    // ранее введённый список не теряется
                    let experts = split_list(&editor.experts);
                    let chat_id = editor.chat_id.clone();
                    state.settings = None;
                    state.focus = Focus::Input;
                    if let Some(chat) = state.chats.iter_mut().find(|c| c.id == chat_id) {
                        chat.settings = ChatSettings {
                            custom_response_mode: format.is_some(),
                            response_format: format.unwrap_or_default(),
                            sampling,
                            reasoning,
                            experts,
                        };
                        let _ = chats::save_chat(chat);
                    }
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
        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
            if editor.pane == SettingsPane::Fields
                && editor.current_field() == Some(FormatField::Mode) =>
        {
            editor.custom_mode = !editor.custom_mode;
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

fn handle_sidebar_key(key: crossterm::event::KeyEvent, state: &mut AppState) -> LoopControl {
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
            let id = state.chats[state.sidebar_selected].id.clone();
            state.panes[state.active_pane] = id;
            state.focus = Focus::Input;
        }
        KeyCode::Char('s') => {
            let id = state.chats[state.sidebar_selected].id.clone();
            if let Some(existing) = state.panes.iter().position(|p| p == &id) {
                state.active_pane = existing;
            } else if state.panes.len() < MAX_PANES {
                state.panes.push(id);
                state.active_pane = state.panes.len() - 1;
            }
            state.focus = Focus::Input;
        }
        KeyCode::Esc => return LoopControl::Break,
        _ => {}
    }
    LoopControl::Continue
}

fn handle_input_key(
    key: crossterm::event::KeyEvent,
    state: &mut AppState,
    agent: &Arc<HttpAgent>,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> LoopControl {
    let chat_id = state.active_chat_id();
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
            let chat_index = state.chat_index(&chat_id);
            state.chats[chat_index]
                .messages
                .push(Message { role: Role::User, content: line });
            state.chats[chat_index].touch();
            let _ = chats::save_chat(&state.chats[chat_index]);
            {
                let ui = state.chat_ui.entry(chat_id.clone()).or_default();
                ui.pending = true;
                ui.auto_scroll = true;
                ui.scroll_to_message = None;
            }

            let agent = agent.clone();
            let hist = state.chats[chat_index].messages.clone();
            let settings = state.chats[chat_index].settings.clone();
            let tx = tx.clone();
            let event_chat_id = chat_id.clone();
            tokio::spawn(async move {
                let result = agent.ask(&hist, &settings).await;
                let _ = tx.send(ChatEvent::Response(event_chat_id, result));
            });
        }
        KeyCode::Esc => return LoopControl::Break,
        KeyCode::Char(c) => {
            state.chat_ui.entry(chat_id).or_default().input.push(c);
        }
        KeyCode::Backspace => {
            state.chat_ui.entry(chat_id).or_default().input.pop();
        }
        KeyCode::Up => {
            let ui = state.chat_ui.entry(chat_id).or_default();
            ui.scroll = ui.scroll.saturating_sub(1);
            ui.auto_scroll = false;
        }
        KeyCode::Down => {
            state.chat_ui.entry(chat_id).or_default().scroll += 1;
        }
        KeyCode::PageUp => {
            let ui = state.chat_ui.entry(chat_id).or_default();
            ui.scroll = ui.scroll.saturating_sub(10);
            ui.auto_scroll = false;
        }
        KeyCode::PageDown => {
            let ui = state.chat_ui.entry(chat_id).or_default();
            ui.scroll = ui.scroll.saturating_add(10);
        }
        _ => {}
    }
    LoopControl::Continue
}

fn handle_chat_event(chat_event: ChatEvent, state: &mut AppState) {
    let ChatEvent::Response(chat_id, result) = chat_event;
    let content = match result {
        Ok(answer) => answer,
        Err(err) => format!("Ошибка: {err}"),
    };
    let Some(chat_index) = state.chats.iter().position(|c| c.id == chat_id) else {
        return;
    };
    state.chats[chat_index]
        .messages
        .push(Message { role: Role::Assistant, content });
    state.chats[chat_index].touch();
    let _ = chats::save_chat(&state.chats[chat_index]);
    let last_index = state.chats[chat_index].messages.len() - 1;
    let ui = state.chat_ui.entry(chat_id).or_default();
    ui.pending = false;
    ui.auto_scroll = false;
    ui.scroll_to_message = Some(last_index);
}

fn render_ui(f: &mut Frame, state: &mut AppState) {
    let outer = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(28), Constraint::Min(20)])
        .split(f.area());

    render_sidebar(f, state, outer[0]);

    let main = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(outer[1]);

    let pane_count = state.panes.len().max(1) as u32;
    let pane_constraints: Vec<Constraint> =
        (0..pane_count).map(|_| Constraint::Ratio(1, pane_count)).collect();
    let pane_areas = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(pane_constraints)
        .split(main[0]);

    for (i, area) in pane_areas.iter().enumerate() {
        render_pane(f, state, i, *area);
    }

    render_help(f, main[1]);

    if let Some(editor) = &state.settings {
        render_settings_popup(f, editor);
    }
}

fn render_pane(f: &mut Frame, state: &mut AppState, pane_idx: usize, area: Rect) {
    let chat_id = state.panes[pane_idx].clone();
    let chat_index = state.chat_index(&chat_id);
    let is_active_pane = pane_idx == state.active_pane;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    render_pane_title(f, state, chat_index, is_active_pane, chunks[0]);
    render_history(f, state, &chat_id, chat_index, chunks[1]);
    render_input(f, state, &chat_id, is_active_pane, chunks[2]);
}

fn render_sidebar(f: &mut Frame, state: &AppState, area: Rect) {
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
            ListItem::new(Line::from(Span::styled(format!("{prefix}{}", chat.title), style)))
        })
        .collect();

    let sidebar_border_style = if state.focus == Focus::Sidebar {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let sidebar = List::new(sidebar_items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(sidebar_border_style)
            .title(" Чаты (Enter — открыть, s — сплит) "),
    );
    f.render_widget(sidebar, area);
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
    let title = Paragraph::new(Line::from(vec![
        Span::styled(" agentcli ", badge_style),
        Span::raw(format!("  {}", state.chats[chat_index].title)),
    ]));
    f.render_widget(title, area);
}

fn render_history(
    f: &mut Frame,
    state: &mut AppState,
    chat_id: &str,
    chat_index: usize,
    area: Rect,
) {
    let pending = state.is_pending(chat_id);
    let scroll_to_message = state.chat_ui.get(chat_id).and_then(|u| u.scroll_to_message);

    let mut lines: Vec<Line> = Vec::new();
    let mut target_line: Option<u16> = None;
    if state.chats[chat_index].messages.is_empty() && state.panes.len() == 1 {
        if let Ok(text) = BILLY_ART.into_text() {
            lines.extend(text.lines);
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "        agent-cli — консольный AI-агент",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::raw(""));
    }
    for (i, entry) in state.chats[chat_index].messages.iter().enumerate() {
        if scroll_to_message == Some(i) {
            target_line = Some(lines.len() as u16);
        }
        let (label, color) = match entry.role {
            Role::User => ("Вы", Color::Green),
            Role::Assistant => ("Агент", Color::Cyan),
        };
        lines.push(Line::from(Span::styled(
            format!("● {label}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
        let rendered = agent_skin().term_text(&entry.content).to_string();
        match rendered.into_text() {
            Ok(text) => lines.extend(text.lines),
            Err(_) => lines.push(Line::raw(entry.content.clone())),
        }
        lines.push(Line::raw(""));
    }
    if pending {
        lines.push(Line::from(Span::styled(
            format!("{} Агент думает...", SPINNER_FRAMES[state.spinner_frame]),
            Style::default().fg(Color::Magenta),
        )));
    }

    let total_lines = lines.len() as u16;
    let visible = area.height.saturating_sub(2);
    let ui = state.chat_ui.entry(chat_id.to_string()).or_default();
    if ui.auto_scroll {
        ui.scroll = total_lines.saturating_sub(visible);
    } else if let Some(target) = target_line {
        ui.scroll = target.min(total_lines.saturating_sub(visible));
        ui.scroll_to_message = None;
    } else {
        ui.scroll = ui.scroll.min(total_lines.saturating_sub(visible));
    }
    let scroll = ui.scroll;

    let history_widget = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::ALL).title(" История "))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(history_widget, area);
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

fn render_help(f: &mut Frame, area: Rect) {
    let help = Paragraph::new(Line::from(Span::styled(
        "Tab — панель · в чатах ←/→ — окно, s — сплит · Ctrl+W — закрыть окно · Ctrl+P — настройки · Ctrl+N — новый чат · Esc/Ctrl+C — выход",
        Style::default().fg(Color::DarkGray),
    )));
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
    let area = centered_rect(88, 24, f.area());
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

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(24), Constraint::Min(0)])
        .split(rows[0]);

    render_settings_sections(f, editor, columns[0]);
    render_settings_fields(f, editor, columns[1]);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " Tab — панель/поле · ↑/↓ — выбор · Ctrl+D — сброс · Ctrl+S — сохранить · Esc — отмена",
            Style::default().fg(Color::DarkGray),
        ))),
        rows[1],
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
            FormatField::Mode => {
                if editor.custom_mode {
                    "Кастомный (◀/▶ или Space — переключить)".to_string()
                } else {
                    "Дефолтный (◀/▶ или Space — переключить)".to_string()
                }
            }
            FormatField::Reasoning => format!(
                "{} (◀/▶ или Space — переключить)",
                editor.reasoning.label()
            ),
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
        let enabled =
            field.is_sampling() || field.is_toggle() || field.is_reasoning_detail() || editor.custom_mode;
        let placeholder = raw.is_empty() && !field.is_toggle();
        let value = if placeholder {
            "не задано — используется значение модели".to_string()
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

