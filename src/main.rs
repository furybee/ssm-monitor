use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::Result;
use aws_config::BehaviorVersion;
use aws_sdk_cloudwatch::Client as CwClient;
use aws_sdk_ec2::Client as Ec2Client;
use aws_sdk_ssm::Client as SsmClient;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Tabs, Wrap,
};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const STALE_THRESHOLD_SECS: i64 = 300;

#[derive(Parser)]
#[command(version, about = "TUI monitor for AWS SSM-managed instances", long_about = None)]
struct Cli {}

#[derive(Default, Clone)]
struct AlarmSummary {
    ok: u32,
    alarm: u32,
    insufficient: u32,
}

impl AlarmSummary {
    fn total(&self) -> u32 {
        self.ok + self.alarm + self.insufficient
    }

    fn label(&self) -> String {
        if self.total() == 0 {
            return "-".into();
        }
        if self.alarm > 0 {
            return format!("ALARM({})", self.alarm);
        }
        if self.insufficient > 0 {
            return format!("INSUF({})", self.insufficient);
        }
        format!("OK({})", self.ok)
    }

    fn style(&self) -> Style {
        if self.alarm > 0 {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else if self.insufficient > 0 {
            Style::default().fg(Color::Yellow)
        } else if self.total() > 0 {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    }
}

#[derive(Clone)]
struct AlarmInfo {
    name: String,
    state: String,
    reason: Option<String>,
}

struct Instance {
    id: String,
    name: String,
    status: String,
    platform: String,
    platform_version: String,
    ip_address: String,
    agent_version: String,
    last_ping_secs: Option<i64>,
    alarms: Vec<AlarmInfo>,
}

impl Instance {
    fn alarm_summary(&self) -> AlarmSummary {
        let mut s = AlarmSummary::default();
        for a in &self.alarms {
            match a.state.as_str() {
                "OK" => s.ok += 1,
                "ALARM" => s.alarm += 1,
                "INSUFFICIENT_DATA" => s.insufficient += 1,
                _ => {}
            }
        }
        s
    }

    fn last_ping_str(&self) -> String {
        match self.last_ping_secs {
            Some(s) => format_relative(s),
            None => "never".into(),
        }
    }

    fn is_stale(&self) -> bool {
        if self.status != "Online" {
            return false;
        }
        let Some(s) = self.last_ping_secs else {
            return false;
        };
        (now_secs() - s) > STALE_THRESHOLD_SECS
    }
}

#[derive(PartialEq, Eq, Clone)]
enum Mode {
    Normal,
    Filtering,
    Detail,
    Help,
    ProfilePicker,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum View {
    Favorites,
    All,
}

impl View {
    fn toggle(self) -> Self {
        match self {
            Self::Favorites => Self::All,
            Self::All => Self::Favorites,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Default,
    Alarms,
    Status,
    Name,
}

impl Sort {
    fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Alarms => "Alarms",
            Self::Status => "Status",
            Self::Name => "Name",
        }
    }
    fn next(self) -> Self {
        match self {
            Self::Default => Self::Alarms,
            Self::Alarms => Self::Status,
            Self::Status => Self::Name,
            Self::Name => Self::Default,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum AlarmFilter {
    All,
    Alarming,
    AnyAlarm,
}

impl AlarmFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Alarming => "Alarming",
            Self::AnyAlarm => "Has alarms",
        }
    }
    fn next(self) -> Self {
        match self {
            Self::All => Self::Alarming,
            Self::Alarming => Self::AnyAlarm,
            Self::AnyAlarm => Self::All,
        }
    }
}

const STATUS_CYCLE: [Option<&str>; 4] =
    [None, Some("Online"), Some("ConnectionLost"), Some("Inactive")];

enum DataUpdate {
    Loaded(Vec<Instance>),
    Failed(String),
    ProfileSwitched { profile: String, region: String },
}

enum AppCommand {
    Refresh,
    SwitchProfile(String),
}

struct App {
    all_instances: Vec<Instance>,
    table_state: TableState,
    mode: Mode,
    view: View,
    sort: Sort,
    filter_text: String,
    status_filter_idx: usize,
    alarm_filter: AlarmFilter,
    favorites: HashSet<String>,
    last_message: Option<String>,
    last_refresh: Option<Instant>,
    last_error: Option<String>,
    is_refreshing: bool,
    region: String,
    profile: String,
    profiles: Vec<String>,
    profile_picker_idx: usize,
}

impl App {
    fn new(
        instances: Vec<Instance>,
        favorites: HashSet<String>,
        region: String,
        profile: String,
        profiles: Vec<String>,
    ) -> Self {
        let view = if favorites.is_empty() {
            View::All
        } else {
            View::Favorites
        };
        let mut app = Self {
            all_instances: instances,
            table_state: TableState::default(),
            mode: Mode::Normal,
            view,
            sort: Sort::Default,
            filter_text: String::new(),
            status_filter_idx: 0,
            alarm_filter: AlarmFilter::All,
            favorites,
            last_message: None,
            last_refresh: Some(Instant::now()),
            last_error: None,
            is_refreshing: false,
            region,
            profile,
            profiles,
            profile_picker_idx: 0,
        };
        app.reset_selection();
        app
    }

    fn open_profile_picker(&mut self) {
        self.profiles = list_aws_profiles();
        if self.profiles.is_empty() {
            self.last_error = Some("No profiles found in ~/.aws".into());
            return;
        }
        self.profile_picker_idx = self
            .profiles
            .iter()
            .position(|p| p == &self.profile)
            .unwrap_or(0);
        self.mode = Mode::ProfilePicker;
    }

    fn picker_next(&mut self) {
        if self.profiles.is_empty() {
            return;
        }
        self.profile_picker_idx = (self.profile_picker_idx + 1) % self.profiles.len();
    }

    fn picker_previous(&mut self) {
        if self.profiles.is_empty() {
            return;
        }
        if self.profile_picker_idx == 0 {
            self.profile_picker_idx = self.profiles.len() - 1;
        } else {
            self.profile_picker_idx -= 1;
        }
    }

    fn status_filter(&self) -> Option<&'static str> {
        STATUS_CYCLE[self.status_filter_idx]
    }

    fn matches(&self, i: &Instance) -> bool {
        let needle = self.filter_text.to_lowercase();
        let text_ok = needle.is_empty()
            || i.name.to_lowercase().contains(&needle)
            || i.id.to_lowercase().contains(&needle);
        let status_ok = match self.status_filter() {
            Some(s) => i.status == s,
            None => true,
        };
        let alarm_ok = match self.alarm_filter {
            AlarmFilter::All => true,
            AlarmFilter::Alarming => i.alarm_summary().alarm > 0,
            AlarmFilter::AnyAlarm => i.alarm_summary().total() > 0,
        };
        text_ok && status_ok && alarm_ok
    }

    fn visible(&self) -> Vec<&Instance> {
        let mut v: Vec<&Instance> = self
            .all_instances
            .iter()
            .filter(|i| self.matches(i))
            .filter(|i| match self.view {
                View::Favorites => self.favorites.contains(&i.id),
                View::All => true,
            })
            .collect();
        match self.sort {
            Sort::Default => {
                if self.view == View::All {
                    v.sort_by_key(|i| !self.favorites.contains(&i.id));
                }
            }
            Sort::Alarms => {
                v.sort_by(|a, b| {
                    let sa = a.alarm_summary();
                    let sb = b.alarm_summary();
                    sb.alarm
                        .cmp(&sa.alarm)
                        .then(sb.insufficient.cmp(&sa.insufficient))
                        .then(sb.ok.cmp(&sa.ok))
                });
            }
            Sort::Status => {
                let rank = |s: &str| match s {
                    "ConnectionLost" => 0,
                    "Inactive" => 1,
                    "Online" => 2,
                    _ => 3,
                };
                v.sort_by_key(|i| rank(&i.status));
            }
            Sort::Name => {
                v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            }
        }
        v
    }

    fn switch_view(&mut self, view: View) {
        if self.view == view {
            return;
        }
        let prev_id = self.selected_id();
        self.view = view;
        if let Some(id) = prev_id {
            if let Some(idx) = self.visible().iter().position(|i| i.id == id) {
                self.table_state.select(Some(idx));
                return;
            }
        }
        self.reset_selection();
    }

    fn cycle_sort(&mut self) {
        let prev_id = self.selected_id();
        self.sort = self.sort.next();
        if let Some(id) = prev_id {
            if let Some(idx) = self.visible().iter().position(|i| i.id == id) {
                self.table_state.select(Some(idx));
                return;
            }
        }
        self.reset_selection();
    }

    fn cycle_alarm_filter(&mut self) {
        self.alarm_filter = self.alarm_filter.next();
        self.reset_selection();
    }

    fn selected_id(&self) -> Option<String> {
        let idx = self.table_state.selected()?;
        self.visible().get(idx).map(|i| i.id.clone())
    }

    fn instance_by_id(&self, id: &str) -> Option<&Instance> {
        self.all_instances.iter().find(|i| i.id == id)
    }

    fn next(&mut self) {
        let len = self.visible().len();
        if len == 0 {
            self.table_state.select(None);
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) if i >= len - 1 => 0,
            Some(i) => i + 1,
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    fn previous(&mut self) {
        let len = self.visible().len();
        if len == 0 {
            self.table_state.select(None);
            return;
        }
        let i = match self.table_state.selected() {
            Some(0) | None => len - 1,
            Some(i) => i - 1,
        };
        self.table_state.select(Some(i));
    }

    fn reset_selection(&mut self) {
        if self.visible().is_empty() {
            self.table_state.select(None);
        } else {
            self.table_state.select(Some(0));
        }
    }

    fn cycle_status(&mut self) {
        self.status_filter_idx = (self.status_filter_idx + 1) % STATUS_CYCLE.len();
        self.reset_selection();
    }

    fn toggle_favorite(&mut self) {
        let Some(id) = self.selected_id() else {
            return;
        };
        if self.favorites.contains(&id) {
            self.favorites.remove(&id);
        } else {
            self.favorites.insert(id.clone());
        }
        let _ = save_favorites(&self.favorites);
        if self.view == View::Favorites && self.favorites.is_empty() {
            self.view = View::All;
        }
        let new_idx = self.visible().iter().position(|i| i.id == id);
        self.table_state.select(new_idx);
        if self.table_state.selected().is_none() {
            self.reset_selection();
        }
    }

    fn apply_instances(&mut self, new_list: Vec<Instance>) {
        let prev_id = self.selected_id();
        self.all_instances = new_list;
        if let Some(id) = prev_id {
            if let Some(idx) = self.visible().iter().position(|i| i.id == id) {
                self.table_state.select(Some(idx));
                return;
            }
        }
        self.reset_selection();
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn list_aws_profiles() -> Vec<String> {
    let Ok(home) = std::env::var("HOME") else {
        return vec!["default".into()];
    };
    let mut set = std::collections::BTreeSet::new();

    let parse_sections = |content: &str, with_prefix: bool, set: &mut std::collections::BTreeSet<String>| {
        for line in content.lines() {
            let line = line.trim();
            let Some(inner) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
                continue;
            };
            let inner = inner.trim();
            if inner == "default" {
                set.insert("default".into());
            } else if with_prefix {
                if let Some(name) = inner.strip_prefix("profile ") {
                    set.insert(name.trim().to_string());
                }
            } else {
                set.insert(inner.to_string());
            }
        }
    };

    if let Ok(c) = std::fs::read_to_string(format!("{home}/.aws/config")) {
        parse_sections(&c, true, &mut set);
    }
    if let Ok(c) = std::fs::read_to_string(format!("{home}/.aws/credentials")) {
        parse_sections(&c, false, &mut set);
    }

    if set.is_empty() {
        set.insert("default".into());
    }
    set.into_iter().collect()
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn favorites_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/ssm-monitor/favorites"))
}

fn load_favorites() -> HashSet<String> {
    let Some(path) = favorites_path() else {
        return HashSet::new();
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    contents
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

fn save_favorites(favs: &HashSet<String>) -> Result<()> {
    let Some(path) = favorites_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut sorted: Vec<&String> = favs.iter().collect();
    sorted.sort();
    let body = sorted
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, body)?;
    Ok(())
}

fn format_relative(epoch_secs: i64) -> String {
    let diff = now_secs() - epoch_secs;
    if diff < 0 {
        return "in the future".into();
    }
    if diff < 60 {
        format!("{diff}s ago")
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else {
        format!("{}d ago", diff / 86400)
    }
}

fn format_elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else {
        format!("{}h ago", s / 3600)
    }
}

async fn fetch_ssm_instances(client: &SsmClient) -> Result<Vec<Instance>> {
    let mut out = Vec::new();
    let mut paginator = client.describe_instance_information().into_paginator().send();
    while let Some(page) = paginator.next().await {
        let page = page?;
        for info in page.instance_information_list() {
            let platform_type = info
                .platform_type()
                .map(|p| p.as_str().to_string())
                .unwrap_or_default();
            let platform_name = info.platform_name().unwrap_or("").to_string();
            let platform = match (platform_name.is_empty(), platform_type.is_empty()) {
                (false, false) => format!("{platform_name} ({platform_type})"),
                (false, true) => platform_name,
                (true, false) => platform_type,
                (true, true) => String::new(),
            };
            out.push(Instance {
                id: info.instance_id().unwrap_or("").to_string(),
                name: info
                    .computer_name()
                    .or(info.name())
                    .unwrap_or("")
                    .to_string(),
                status: info
                    .ping_status()
                    .map(|s| s.as_str().to_string())
                    .unwrap_or_default(),
                platform,
                platform_version: info.platform_version().unwrap_or("").to_string(),
                ip_address: info.ip_address().unwrap_or("").to_string(),
                agent_version: info.agent_version().unwrap_or("").to_string(),
                last_ping_secs: info.last_ping_date_time().map(|dt| dt.secs()),
                alarms: Vec::new(),
            });
        }
    }
    Ok(out)
}

async fn fetch_ec2_names(client: &Ec2Client, ids: &[String]) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    if ids.is_empty() {
        return Ok(map);
    }
    for chunk in ids.chunks(200) {
        let mut paginator = client
            .describe_instances()
            .set_instance_ids(Some(chunk.to_vec()))
            .into_paginator()
            .send();
        while let Some(page) = paginator.next().await {
            let page = page?;
            for r in page.reservations() {
                for inst in r.instances() {
                    let Some(id) = inst.instance_id() else {
                        continue;
                    };
                    let name = inst
                        .tags()
                        .iter()
                        .find(|t| t.key() == Some("Name"))
                        .and_then(|t| t.value())
                        .unwrap_or("")
                        .to_string();
                    if !name.is_empty() {
                        map.insert(id.to_string(), name);
                    }
                }
            }
        }
    }
    Ok(map)
}

async fn fetch_alarms(client: &CwClient) -> Result<HashMap<String, Vec<AlarmInfo>>> {
    let mut map: HashMap<String, Vec<AlarmInfo>> = HashMap::new();
    let mut paginator = client.describe_alarms().into_paginator().send();
    while let Some(page) = paginator.next().await {
        let page = page?;
        for alarm in page.metric_alarms() {
            let Some(instance_id) = alarm
                .dimensions()
                .iter()
                .find(|d| d.name() == Some("InstanceId"))
                .and_then(|d| d.value())
            else {
                continue;
            };
            let info = AlarmInfo {
                name: alarm.alarm_name().unwrap_or("").to_string(),
                state: alarm
                    .state_value()
                    .map(|s| s.as_str().to_string())
                    .unwrap_or_default(),
                reason: alarm.state_reason().map(String::from),
            };
            map.entry(instance_id.to_string()).or_default().push(info);
        }
    }
    Ok(map)
}

async fn fetch_instances(
    ssm: &SsmClient,
    ec2: &Ec2Client,
    cw: &CwClient,
) -> Result<Vec<Instance>> {
    let mut instances = fetch_ssm_instances(ssm).await?;
    let ec2_ids: Vec<String> = instances
        .iter()
        .filter(|i| i.id.starts_with("i-"))
        .map(|i| i.id.clone())
        .collect();
    let name_map = fetch_ec2_names(ec2, &ec2_ids).await?;
    let alarm_map = fetch_alarms(cw).await.unwrap_or_default();
    for inst in &mut instances {
        if let Some(real_name) = name_map.get(&inst.id) {
            inst.name = real_name.clone();
        }
        if let Some(list) = alarm_map.get(&inst.id) {
            inst.alarms = list.clone();
        }
    }
    Ok(instances)
}

async fn build_clients_for_profile(profile: &str) -> (SsmClient, Ec2Client, CwClient, String) {
    let config = aws_config::defaults(BehaviorVersion::latest())
        .profile_name(profile)
        .load()
        .await;
    let region = config
        .region()
        .map(|r| r.to_string())
        .unwrap_or_else(|| "unknown".into());
    (
        SsmClient::new(&config),
        Ec2Client::new(&config),
        CwClient::new(&config),
        region,
    )
}

fn spawn_coordinator(
    initial_ssm: SsmClient,
    initial_ec2: Ec2Client,
    initial_cw: CwClient,
    mut cmd_rx: UnboundedReceiver<AppCommand>,
    data_tx: UnboundedSender<DataUpdate>,
) {
    tokio::spawn(async move {
        let mut ssm = initial_ssm;
        let mut ec2 = initial_ec2;
        let mut cw = initial_cw;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(REFRESH_INTERVAL) => {}
                cmd = cmd_rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    apply_command(cmd, &mut ssm, &mut ec2, &mut cw, &data_tx).await;
                }
            }
            // Drain any extra commands that piled up while waking.
            while let Ok(cmd) = cmd_rx.try_recv() {
                apply_command(cmd, &mut ssm, &mut ec2, &mut cw, &data_tx).await;
            }

            let update = match fetch_instances(&ssm, &ec2, &cw).await {
                Ok(list) => DataUpdate::Loaded(list),
                Err(e) => DataUpdate::Failed(e.to_string()),
            };
            if data_tx.send(update).is_err() {
                break;
            }
        }
    });
}

async fn apply_command(
    cmd: AppCommand,
    ssm: &mut SsmClient,
    ec2: &mut Ec2Client,
    cw: &mut CwClient,
    data_tx: &UnboundedSender<DataUpdate>,
) {
    match cmd {
        AppCommand::Refresh => {}
        AppCommand::SwitchProfile(name) => {
            let (new_ssm, new_ec2, new_cw, region) = build_clients_for_profile(&name).await;
            *ssm = new_ssm;
            *ec2 = new_ec2;
            *cw = new_cw;
            let _ = data_tx.send(DataUpdate::ProfileSwitched {
                profile: name,
                region,
            });
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = Cli::parse();
    eprintln!("Loading SSM instances, EC2 tags, and CloudWatch alarms...");
    let config = aws_config::defaults(BehaviorVersion::latest()).load().await;
    let region = config
        .region()
        .map(|r| r.to_string())
        .unwrap_or_else(|| "unknown".into());
    let profile = std::env::var("AWS_PROFILE").unwrap_or_else(|_| "default".into());
    let ssm = SsmClient::new(&config);
    let ec2 = Ec2Client::new(&config);
    let cw = CwClient::new(&config);
    let instances = fetch_instances(&ssm, &ec2, &cw).await?;
    eprintln!("Loaded {} instances.", instances.len());

    let favorites = load_favorites();
    let profiles = list_aws_profiles();

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<AppCommand>();
    let (data_tx, data_rx) = tokio::sync::mpsc::unbounded_channel::<DataUpdate>();
    spawn_coordinator(ssm.clone(), ec2.clone(), cw.clone(), cmd_rx, data_tx);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(instances, favorites, region, profile, profiles);
    let res = run_app(&mut terminal, &mut app, cmd_tx, data_rx);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    res?;
    Ok(())
}

fn run_ssm_session<B: Backend>(
    terminal: &mut Terminal<B>,
    instance_id: &str,
    profile: &str,
) -> io::Result<Option<String>> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    let msg = match Command::new("aws")
        .env("AWS_PROFILE", profile)
        .args(["ssm", "start-session", "--target", instance_id])
        .status()
    {
        Ok(status) if status.success() => None,
        Ok(status) => {
            eprintln!("\nSSM session exited with status {status}. Press Enter to return.");
            let mut buf = String::new();
            let _ = io::stdin().read_line(&mut buf);
            Some(format!("SSM session exited: {status}"))
        }
        Err(e) => {
            eprintln!("\nFailed to launch 'aws ssm start-session': {e}");
            eprintln!("Ensure the aws CLI and the Session Manager plugin are installed.");
            eprintln!("Press Enter to return.");
            let mut buf = String::new();
            let _ = io::stdin().read_line(&mut buf);
            Some(format!("SSM launch failed: {e}"))
        }
    };

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    terminal.hide_cursor()?;
    terminal.clear()?;
    Ok(msg)
}

fn request_refresh(app: &mut App, cmd_tx: &UnboundedSender<AppCommand>) {
    if app.is_refreshing {
        return;
    }
    app.is_refreshing = true;
    let _ = cmd_tx.send(AppCommand::Refresh);
}

fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    cmd_tx: UnboundedSender<AppCommand>,
    mut data_rx: UnboundedReceiver<DataUpdate>,
) -> io::Result<()> {
    loop {
        while let Ok(update) = data_rx.try_recv() {
            match update {
                DataUpdate::Loaded(instances) => {
                    app.apply_instances(instances);
                    app.last_refresh = Some(Instant::now());
                    app.last_error = None;
                    app.is_refreshing = false;
                }
                DataUpdate::Failed(e) => {
                    app.last_error = Some(e);
                    app.is_refreshing = false;
                }
                DataUpdate::ProfileSwitched { profile, region } => {
                    app.profile = profile;
                    app.region = region;
                    app.last_error = None;
                }
            }
        }

        terminal.draw(|f| ui(f, app))?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match app.mode {
            Mode::Normal => match key.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Down | KeyCode::Char('j') => app.next(),
                KeyCode::Up | KeyCode::Char('k') => app.previous(),
                KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
                    app.switch_view(app.view.toggle());
                }
                KeyCode::Char('f') => app.mode = Mode::Filtering,
                KeyCode::Char('s') => app.cycle_status(),
                KeyCode::Char('a') => app.cycle_alarm_filter(),
                KeyCode::Char('o') => app.cycle_sort(),
                KeyCode::Char('b') => app.toggle_favorite(),
                KeyCode::Char('r') => request_refresh(app, &cmd_tx),
                KeyCode::Char('p') => app.open_profile_picker(),
                KeyCode::Char('?') => app.mode = Mode::Help,
                KeyCode::Enter => {
                    if app.selected_id().is_some() {
                        app.mode = Mode::Detail;
                    }
                }
                _ => {}
            },
            Mode::Filtering => match key.code {
                KeyCode::Esc | KeyCode::Enter => app.mode = Mode::Normal,
                KeyCode::Backspace => {
                    app.filter_text.pop();
                    app.reset_selection();
                }
                KeyCode::Char(c) => {
                    app.filter_text.push(c);
                    app.reset_selection();
                }
                _ => {}
            },
            Mode::Detail => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    app.mode = Mode::Normal;
                    app.last_message = None;
                }
                KeyCode::Down | KeyCode::Char('j') => app.next(),
                KeyCode::Up | KeyCode::Char('k') => app.previous(),
                KeyCode::Char('c') => {
                    if let Some(id) = app.selected_id() {
                        let profile = app.profile.clone();
                        match run_ssm_session(terminal, &id, &profile) {
                            Ok(msg) => app.last_message = msg,
                            Err(e) => app.last_message = Some(format!("Error: {e}")),
                        }
                    }
                }
                KeyCode::Char('b') => app.toggle_favorite(),
                KeyCode::Char('r') => request_refresh(app, &cmd_tx),
                KeyCode::Char('?') => app.mode = Mode::Help,
                _ => {}
            },
            Mode::Help => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => {
                    app.mode = Mode::Normal;
                }
                _ => {}
            },
            Mode::ProfilePicker => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('p') => {
                    app.mode = Mode::Normal;
                }
                KeyCode::Down | KeyCode::Char('j') => app.picker_next(),
                KeyCode::Up | KeyCode::Char('k') => app.picker_previous(),
                KeyCode::Enter => {
                    if let Some(name) = app.profiles.get(app.profile_picker_idx).cloned() {
                        if name != app.profile {
                            app.is_refreshing = true;
                            let _ = cmd_tx.send(AppCommand::SwitchProfile(name));
                        }
                    }
                    app.mode = Mode::Normal;
                }
                _ => {}
            },
        }
    }
}

fn status_style_for(inst: &Instance) -> Style {
    if inst.is_stale() {
        return Style::default().fg(Color::Yellow);
    }
    match inst.status.as_str() {
        "Online" => Style::default().fg(Color::Green),
        "ConnectionLost" => Style::default().fg(Color::Red),
        "Inactive" => Style::default().fg(Color::DarkGray),
        _ => Style::default().fg(Color::Yellow),
    }
}

fn status_label_for(inst: &Instance) -> String {
    if inst.is_stale() {
        format!("{} (stale)", inst.status)
    } else {
        inst.status.clone()
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    match app.mode {
        Mode::Help => ui_help(f),
        Mode::Detail => ui_detail(f, app),
        Mode::ProfilePicker => {
            ui_list(f, app);
            ui_profile_picker(f, app);
        }
        _ => ui_list(f, app),
    }
}

fn ui_profile_picker(f: &mut Frame, app: &App) {
    let height = (app.profiles.len() as u16).saturating_add(4).min(20);
    let area = centered_rect(50, height, f.area());

    let mut lines: Vec<Line> = Vec::with_capacity(app.profiles.len());
    for (i, name) in app.profiles.iter().enumerate() {
        let is_current = name == &app.profile;
        let is_selected = i == app.profile_picker_idx;
        let marker = if is_current { " ● " } else { "   " };
        let style = if is_selected {
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else if is_current {
            Style::default().fg(Color::Green)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(format!("{marker}{name}"), style)));
    }

    let title = " Switch profile — ↑/↓ · Enter to confirm · Esc to cancel ";
    let widget = Paragraph::new(Text::from(lines)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .style(Style::default().bg(Color::Black)),
    );
    f.render_widget(Clear, area);
    f.render_widget(widget, area);
}

fn top_status_line(app: &App) -> Line<'_> {
    let mut spans = vec![
        Span::raw(" "),
        Span::styled("profile: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            app.profile.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled("region: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            app.region.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
    ];
    if app.is_refreshing {
        spans.push(Span::styled(
            "refreshing...",
            Style::default().fg(Color::Cyan),
        ));
    } else if let Some(t) = app.last_refresh {
        spans.push(Span::styled(
            format!("refreshed {}", format_elapsed(t.elapsed())),
            Style::default().fg(Color::Green),
        ));
    } else {
        spans.push(Span::raw("loaded"));
    }
    if let Some(err) = &app.last_error {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!("⚠ {err}"),
            Style::default().fg(Color::Red),
        ));
    }
    Line::from(spans)
}

fn ui_list(f: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(4),
    ])
    .split(f.area());

    f.render_widget(Paragraph::new(top_status_line(app)), chunks[0]);

    let fav_count = app.favorites.len();
    let total = app.all_instances.len();
    let tab_titles = vec![
        format!(" ★ Favorites ({fav_count}) "),
        format!(" All Instances ({total}) "),
    ];
    let selected_tab = match app.view {
        View::Favorites => 0,
        View::All => 1,
    };
    let tabs = Tabs::new(tab_titles)
        .select(selected_tab)
        .divider("")
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, chunks[1]);

    let visible = app.visible();
    let shown = visible.len();

    let rows: Vec<Row> = visible
        .iter()
        .map(|i| {
            let is_fav = app.favorites.contains(&i.id);
            let fav_cell = Cell::from(if is_fav { "★" } else { " " })
                .style(Style::default().fg(Color::Yellow));
            let id_cell = Cell::from(i.id.clone());
            let name_cell = Cell::from(i.name.clone());
            let summary = i.alarm_summary();
            let alarm_cell = Cell::from(summary.label()).style(summary.style());
            let status_cell =
                Cell::from(status_label_for(i)).style(status_style_for(i));
            Row::new(vec![fav_cell, id_cell, name_cell, alarm_cell, status_cell])
        })
        .collect();

    let header = Row::new(vec!["", "ID", "Name", "Alarms", "Status"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let view_label = match app.view {
        View::Favorites => "Favorites",
        View::All => "All",
    };
    let title = format!("{view_label}  ·  shown {shown}  ·  sort: {}", app.sort.label());

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Length(20),
            Constraint::Min(20),
            Constraint::Length(14),
            Constraint::Length(22),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(title))
    .row_highlight_style(
        Style::default()
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD),
    );

    f.render_stateful_widget(table, chunks[2], &mut app.table_state);

    if shown == 0 && app.view == View::Favorites {
        let hint = Paragraph::new(
            "\n  No favorites yet — switch to All Instances with → and press f to star one.",
        )
        .style(Style::default().fg(Color::DarkGray));
        let inner = chunks[2].inner(Margin {
            horizontal: 1,
            vertical: 2,
        });
        f.render_widget(hint, inner);
    }

    let status_label = app.status_filter().unwrap_or("All");
    let cursor = if app.mode == Mode::Filtering { "_" } else { "" };
    let mode_label = if app.mode == Mode::Filtering {
        "FILTER"
    } else {
        "NORMAL"
    };
    let line1 = format!(
        " [{mode_label}]  name/id: {}{cursor}    status: {status_label}    alarms: {}",
        app.filter_text,
        app.alarm_filter.label()
    );
    let line2 = " ←/→ view · ↑/↓ nav · ? help".to_string();
    let filter_widget = Paragraph::new(format!("{line1}\n{line2}"))
        .block(Block::default().borders(Borders::ALL).title("Filters"));
    f.render_widget(filter_widget, chunks[3]);
}

fn alarm_state_label(state: &str) -> &'static str {
    match state {
        "OK" => "[OK]   ",
        "ALARM" => "[ALARM]",
        "INSUFFICIENT_DATA" => "[INSUF]",
        _ => "[?]    ",
    }
}

fn alarm_state_style(state: &str) -> Style {
    match state {
        "ALARM" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        "OK" => Style::default().fg(Color::Green),
        "INSUFFICIENT_DATA" => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::DarkGray),
    }
}

fn detail_field<'a>(label: &'a str, value: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{label:<20}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(value),
    ])
}

fn ui_detail(f: &mut Frame, app: &mut App) {
    let Some(id) = app.selected_id() else {
        app.mode = Mode::Normal;
        return;
    };
    let Some(inst) = app.instance_by_id(&id) else {
        app.mode = Mode::Normal;
        return;
    };
    let is_fav = app.favorites.contains(&inst.id);

    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .split(f.area());

    f.render_widget(Paragraph::new(top_status_line(app)), chunks[0]);

    let last_ping_str = inst.last_ping_str();
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));
    lines.push(detail_field("ID:", &inst.id));
    lines.push(detail_field("Name:", &inst.name));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{:<20}", "Status:"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(status_label_for(inst), status_style_for(inst)),
    ]));
    lines.push(detail_field("Platform:", &inst.platform));
    lines.push(detail_field("Platform Version:", &inst.platform_version));
    lines.push(detail_field("IP Address:", &inst.ip_address));
    lines.push(detail_field("Agent Version:", &inst.agent_version));
    lines.push(detail_field("Last Ping:", &last_ping_str));
    lines.push(detail_field(
        "Favorite:",
        if is_fav { "★ yes" } else { "no" },
    ));

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("  Alarms ({})", inst.alarms.len()),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if inst.alarms.is_empty() {
        lines.push(Line::from(Span::styled(
            "    (no alarms attached to this instance)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        let mut alarms = inst.alarms.clone();
        alarms.sort_by_key(|a| match a.state.as_str() {
            "ALARM" => 0,
            "INSUFFICIENT_DATA" => 1,
            "OK" => 2,
            _ => 3,
        });
        for a in &alarms {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(alarm_state_label(&a.state), alarm_state_style(&a.state)),
                Span::raw("  "),
                Span::raw(a.name.clone()),
            ]));
            if let Some(reason) = &a.reason {
                if !reason.is_empty() {
                    lines.push(Line::from(vec![
                        Span::raw("            "),
                        Span::styled(reason.clone(), Style::default().fg(Color::DarkGray)),
                    ]));
                }
            }
        }
    }

    if let Some(msg) = &app.last_message {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(msg.clone(), Style::default().fg(Color::Yellow)),
        ]));
    }

    let title = format!(" Instance Details — {} ({}) ", inst.id, inst.name);
    let body = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    f.render_widget(body, chunks[1]);

    let actions = Paragraph::new(
        " c: SSM session · b: toggle bookmark · ↑/↓: prev/next · r: refresh · ? help · Esc/q: back",
    )
    .block(Block::default().borders(Borders::ALL).title("Actions"));
    f.render_widget(actions, chunks[2]);
}

fn help_line(key: &str, desc: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("    "),
        Span::styled(
            format!("{key:<22}"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(desc.to_string()),
    ])
}

fn help_section(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {title}"),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

fn ui_help(f: &mut Frame) {
    let lines = vec![
        Line::from(""),
        help_section("Navigation"),
        help_line("↑/↓  or  j/k", "move selection (also in detail view)"),
        help_line("←/→  or  h/l", "switch view (Favorites / All)"),
        help_line("Enter", "open instance details"),
        help_line("Esc / q", "back / close the current overlay"),
        Line::from(""),
        help_section("Filters & sort"),
        help_line("f", "find by name or id (Esc/Enter to leave, Backspace to delete)"),
        help_line("s", "cycle status filter (All / Online / ConnectionLost / Inactive)"),
        help_line("a", "cycle alarm filter (All / Alarming / Has alarms)"),
        help_line("o", "cycle order/sort (Default / Alarms / Status / Name)"),
        Line::from(""),
        help_section("Actions"),
        help_line("b", "bookmark / toggle favorite on selected instance (persisted)"),
        help_line("r", "refresh data immediately (auto every 30s)"),
        help_line("p", "switch AWS profile (lists ~/.aws/config and ~/.aws/credentials)"),
        help_line("c", "(in detail view) start an SSM session on the selected instance"),
        Line::from(""),
        help_section("Notes"),
        Line::from("    · Favorites are stored at ~/.config/ssm-monitor/favorites"),
        Line::from("    · Data auto-refreshes every 30 seconds in the background"),
        Line::from("    · Online instances with last ping > 5 min show as (stale) in yellow"),
        Line::from("    · SSM session requires the aws CLI and Session Manager plugin"),
    ];
    let p = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Help — press Esc / q / ? to close "),
        );
    f.render_widget(p, f.area());
}
