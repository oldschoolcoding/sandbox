// Cargo.toml dependencies:
/*
[package]
name = "cert-manager-tui"
version = "0.1.0"
edition = "2021"

[dependencies]
ratatui = "0.26"
crossterm = "0.27"
tokio = { version = "1", features = ["full"] }
tokio-native-tls = "0.3"
native-tls = "0.2"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
toml = "0.8"
chrono = { version = "0.4", features = ["serde"] }
anyhow = "1.0"
clap = { version = "4.0", features = ["derive"] }
csv = "1.3"
*/

use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect, Alignment},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, ListState},
    Terminal, Frame,
};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use serde::{Deserialize, Serialize};
use std::{io, time::Duration, fs};
use chrono::{DateTime, Utc};
use tokio::net::TcpStream;
use tokio_native_tls::TlsConnector;

// ============================================================================
// Configuration Models
// ============================================================================

#[derive(Debug, Deserialize, Serialize, Clone)]
struct Config {
    servers: Vec<ServerConfig>,
    settings: Settings,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ServerConfig {
    name: String,
    host: String,
    applications: Vec<Application>,
    #[serde(default)]
    migration_to: Option<String>,
    #[serde(default)]
    migration_status: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct Application {
    name: String,
    ports: Vec<u16>,
    #[serde(default)]
    expected_cn: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct Settings {
    #[serde(default = "default_scan_timeout")]
    scan_timeout_secs: u64,
    #[serde(default = "default_warning_days")]
    warning_days: i64,
    #[serde(default = "default_critical_days")]
    critical_days: i64,
    #[serde(default)]
    auto_scan_on_start: bool,
}

fn default_scan_timeout() -> u64 { 5 }
fn default_warning_days() -> i64 { 60 }
fn default_critical_days() -> i64 { 30 }

// ============================================================================
// Certificate Data Models
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CertificateInfo {
    server: String,
    host: String,
    application: String,
    port: u16,
    common_name: String,
    san_names: Vec<String>,
    issuer: String,
    not_before: DateTime<Utc>,
    not_after: DateTime<Utc>,
    days_remaining: i64,
    status: CertStatus,
    last_checked: DateTime<Utc>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
enum CertStatus {
    Ok,
    Warning,
    Critical,
    Expired,
    Error,
}

impl CertStatus {
    fn color(&self) -> Color {
        match self {
            CertStatus::Ok => Color::Green,
            CertStatus::Warning => Color::Yellow,
            CertStatus::Critical => Color::LightRed,
            CertStatus::Expired => Color::Red,
            CertStatus::Error => Color::Magenta,
        }
    }
    
    fn icon(&self) -> &str {
        match self {
            CertStatus::Ok => "✓",
            CertStatus::Warning => "⚠",
            CertStatus::Critical => "⚠",
            CertStatus::Expired => "✗",
            CertStatus::Error => "⚡",
        }
    }
}

// ============================================================================
// Certificate Scanner
// ============================================================================

async fn scan_certificate(
    server: &str,
    host: &str,
    app: &str,
    port: u16,
    timeout: u64,
) -> anyhow::Result<CertificateInfo> {
    let addr = format!("{}:{}", host, port);
    
    // Connect with timeout
    let stream = tokio::time::timeout(
        Duration::from_secs(timeout),
        TcpStream::connect(&addr)
    ).await??;
    
    // TLS handshake
    let connector = TlsConnector::from(
        native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()?
    );
    
    let tls_stream = connector.connect(host, stream).await?;
    
    // Extract certificate
    let cert_der = tls_stream
        .get_ref()
        .peer_certificate()?
        .ok_or_else(|| anyhow::anyhow!("No certificate found"))?
        .to_der()?;
    
    let cert = x509_parser::parse_x509_certificate(&cert_der)
        .map_err(|e| anyhow::anyhow!("Failed to parse certificate: {}", e))?
        .1;
    
    // Extract certificate details
    let subject = cert.subject();
    let common_name = subject
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .unwrap_or("Unknown")
        .to_string();
    
    let issuer = cert.issuer()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .unwrap_or("Unknown")
        .to_string();
    
    // Get SAN (Subject Alternative Names)
    let san_names = cert
        .subject_alternative_name()
        .ok()
        .and_then(|ext| ext)
        .map(|san| {
            san.value.general_names.iter()
                .filter_map(|name| {
                    if let x509_parser::extensions::GeneralName::DNSName(dns) = name {
                        Some(dns.to_string())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    
    // Parse validity dates
    let not_before = DateTime::from_timestamp(cert.validity().not_before.timestamp(), 0)
        .unwrap_or_else(Utc::now);
    let not_after = DateTime::from_timestamp(cert.validity().not_after.timestamp(), 0)
        .unwrap_or_else(Utc::now);
    
    let now = Utc::now();
    let days_remaining = (not_after - now).num_days();
    
    let status = if days_remaining < 0 {
        CertStatus::Expired
    } else if days_remaining < 30 {
        CertStatus::Critical
    } else if days_remaining < 60 {
        CertStatus::Warning
    } else {
        CertStatus::Ok
    };
    
    Ok(CertificateInfo {
        server: server.to_string(),
        host: host.to_string(),
        application: app.to_string(),
        port,
        common_name,
        san_names,
        issuer,
        not_before,
        not_after,
        days_remaining,
        status,
        last_checked: now,
        error: None,
    })
}

async fn scan_all_certificates(config: &Config) -> Vec<CertificateInfo> {
    let mut handles = vec![];
    
    for server in &config.servers {
        for app in &server.applications {
            for &port in &app.ports {
                let server_name = server.name.clone();
                let host = server.host.clone();
                let app_name = app.name.clone();
                let timeout = config.settings.scan_timeout_secs;
                
                let handle = tokio::spawn(async move {
                    match scan_certificate(&server_name, &host, &app_name, port, timeout).await {
                        Ok(cert) => cert,
                        Err(e) => CertificateInfo {
                            server: server_name,
                            host,
                            application: app_name,
                            port,
                            common_name: "Error".to_string(),
                            san_names: vec![],
                            issuer: "N/A".to_string(),
                            not_before: Utc::now(),
                            not_after: Utc::now(),
                            days_remaining: -1,
                            status: CertStatus::Error,
                            last_checked: Utc::now(),
                            error: Some(e.to_string()),
                        }
                    }
                });
                
                handles.push(handle);
            }
        }
    }
    
    let mut results = vec![];
    for handle in handles {
        if let Ok(cert) = handle.await {
            results.push(cert);
        }
    }
    
    // Sort by days remaining (most urgent first)
    results.sort_by_key(|c| c.days_remaining);
    results
}

// ============================================================================
// Application State
// ============================================================================

enum ViewMode {
    Dashboard,
    Details,
    Migrations,
}

struct App {
    config: Config,
    certificates: Vec<CertificateInfo>,
    selected_index: usize,
    list_state: ListState,
    view_mode: ViewMode,
    is_scanning: bool,
    status_message: String,
    filter: FilterMode,
}

#[derive(PartialEq, Debug)]
enum FilterMode {
    All,
    Critical,
    Warning,
    Ok,
    Error,
}

impl App {
    fn new(config: Config) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        
        Self {
            config,
            certificates: vec![],
            selected_index: 0,
            list_state,
            view_mode: ViewMode::Dashboard,
            is_scanning: false,
            status_message: "Press F5 to scan certificates".to_string(),
            filter: FilterMode::All,
        }
    }
    
    fn filtered_certificates(&self) -> Vec<&CertificateInfo> {
        match self.filter {
            FilterMode::All => self.certificates.iter().collect(),
            FilterMode::Critical => self.certificates.iter()
                .filter(|c| c.status == CertStatus::Critical || c.status == CertStatus::Expired)
                .collect(),
            FilterMode::Warning => self.certificates.iter()
                .filter(|c| c.status == CertStatus::Warning)
                .collect(),
            FilterMode::Ok => self.certificates.iter()
                .filter(|c| c.status == CertStatus::Ok)
                .collect(),
            FilterMode::Error => self.certificates.iter()
                .filter(|c| c.status == CertStatus::Error)
                .collect(),
        }
    }
    
    fn next(&mut self) {
        let filtered = self.filtered_certificates();
        if filtered.is_empty() { return; }
        
        let i = match self.list_state.selected() {
            Some(i) => {
                if i >= filtered.len() - 1 { 0 } else { i + 1 }
            }
            None => 0,
        };
        self.list_state.select(Some(i));
        self.selected_index = i;
    }
    
    fn previous(&mut self) {
        let filtered = self.filtered_certificates();
        if filtered.is_empty() { return; }
        
        let i = match self.list_state.selected() {
            Some(i) => {
                if i == 0 { filtered.len() - 1 } else { i - 1 }
            }
            None => 0,
        };
        self.list_state.select(Some(i));
        self.selected_index = i;
    }
    
    fn get_stats(&self) -> (usize, usize, usize, usize, usize) {
        let total = self.certificates.len();
        let critical = self.certificates.iter().filter(|c| c.status == CertStatus::Critical || c.status == CertStatus::Expired).count();
        let warning = self.certificates.iter().filter(|c| c.status == CertStatus::Warning).count();
        let ok = self.certificates.iter().filter(|c| c.status == CertStatus::Ok).count();
        let error = self.certificates.iter().filter(|c| c.status == CertStatus::Error).count();
        (total, critical, warning, ok, error)
    }
    
    fn export_csv(&self) -> anyhow::Result<()> {
        let filename = format!("cert_report_{}.csv", Utc::now().format("%Y%m%d_%H%M%S"));
        let mut wtr = csv::Writer::from_path(&filename)?;
        
        wtr.write_record(&[
            "Server", "Host", "Application", "Port", "Common Name", 
            "Issuer", "Expires", "Days Remaining", "Status"
        ])?;
        
        for cert in &self.certificates {
            wtr.write_record(&[
                &cert.server,
                &cert.host,
                &cert.application,
                &cert.port.to_string(),
                &cert.common_name,
                &cert.issuer,
                &cert.not_after.format("%Y-%m-%d").to_string(),
                &cert.days_remaining.to_string(),
                &format!("{:?}", cert.status),
            ])?;
        }
        
        wtr.flush()?;
        Ok(())
    }
}

// ============================================================================
// UI Rendering
// ============================================================================

fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Header
            Constraint::Length(3),  // Stats
            Constraint::Min(0),     // Main content
            Constraint::Length(2),  // Status bar
        ])
        .split(f.size());
    
    // Header
    let title = Paragraph::new("🔐 Certificate Manager - Multi-Port Scanner")
        .style(Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD))
        .alignment(Alignment::Center)
        .block(Block::default());
    f.render_widget(title, chunks[0]);
    
    // Stats bar
    let (total, critical, warning, ok, error) = app.get_stats();
    let stats = Paragraph::new(format!(
        "Total: {} | 🔴 Critical: {} | 🟡 Warning: {} | 🟢 OK: {} | ⚡ Error: {}",
        total, critical, warning, ok, error
    ))
    .style(Style::default().fg(Color::White).bg(Color::DarkGray))
    .alignment(Alignment::Center)
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(stats, chunks[1]);
    
    // Main content
    match app.view_mode {
        ViewMode::Dashboard => render_dashboard(f, app, chunks[2]),
        ViewMode::Details => render_details(f, app, chunks[2]),
        ViewMode::Migrations => render_migrations(f, app, chunks[2]),
    }
    
    // Status bar
    let status = Paragraph::new(app.status_message.as_str())
        .style(Style::default().fg(Color::Black).bg(Color::Cyan))
        .block(Block::default());
    f.render_widget(status, chunks[3]);
}

fn render_dashboard(f: &mut Frame, app: &App, area: Rect) {
    let filtered = app.filtered_certificates();
    
    let items: Vec<ListItem> = filtered
        .iter()
        .map(|cert| {
            let status_icon = cert.status.icon();
            let status_color = cert.status.color();
            
            let line1 = Line::from(vec![
                Span::styled(format!("{} ", status_icon), Style::default().fg(status_color)),
                Span::styled(&cert.server, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw(" / "),
                Span::styled(&cert.application, Style::default().fg(Color::White)),
                Span::raw(format!(" :{}", cert.port)),
            ]);
            
            let line2 = Line::from(vec![
                Span::raw("   CN: "),
                Span::styled(&cert.common_name, Style::default().fg(Color::Yellow)),
                Span::raw(" | "),
                Span::styled(
                    format!("Expires: {} ({} days)", 
                        cert.not_after.format("%Y-%m-%d"), 
                        cert.days_remaining
                    ),
                    Style::default().fg(status_color)
                ),
            ]);
            
            ListItem::new(vec![line1, line2])
        })
        .collect();
    
    let list = List::new(items)
        .block(Block::default()
            .borders(Borders::ALL)
            .title(format!("Certificates (Filter: {:?}) - ↑↓:Navigate Enter:Details", app.filter))
        )
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ");
    
    f.render_stateful_widget(list, area, &mut app.list_state.clone());
}

fn render_details(f: &mut Frame, app: &App, area: Rect) {
    let filtered = app.filtered_certificates();
    
    if let Some(cert) = filtered.get(app.selected_index) {
        let mut text = vec![
            Line::from(vec![
                Span::styled("Server: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(&cert.server),
            ]),
            Line::from(vec![
                Span::styled("Host: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(&cert.host),
            ]),
            Line::from(vec![
                Span::styled("Application: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(&cert.application),
            ]),
            Line::from(vec![
                Span::styled("Port: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format!("{}", cert.port)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("Common Name: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(&cert.common_name, Style::default().fg(Color::Yellow)),
            ]),
            Line::from(vec![
                Span::styled("Issuer: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(&cert.issuer),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("Valid From: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(cert.not_before.format("%Y-%m-%d %H:%M:%S UTC").to_string()),
            ]),
            Line::from(vec![
                Span::styled("Valid Until: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    cert.not_after.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                    Style::default().fg(cert.status.color())
                ),
            ]),
            Line::from(vec![
                Span::styled("Days Remaining: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!("{}", cert.days_remaining),
                    Style::default().fg(cert.status.color()).add_modifier(Modifier::BOLD)
                ),
            ]),
            Line::from(""),
        ];
        
        if !cert.san_names.is_empty() {
            text.push(Line::from(vec![
                Span::styled("Subject Alternative Names:", Style::default().add_modifier(Modifier::BOLD)),
            ]));
            for san in &cert.san_names {
                text.push(Line::from(format!("  - {}", san)));
            }
        }
        
        if let Some(error) = &cert.error {
            text.push(Line::from(""));
            text.push(Line::from(vec![
                Span::styled("Error: ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::styled(error, Style::default().fg(Color::Red)),
            ]));
        }
        
        let details = Paragraph::new(text)
            .block(Block::default()
                .borders(Borders::ALL)
                .title("Certificate Details (Press Esc to return)")
            )
            .style(Style::default().fg(Color::White));
        
        f.render_widget(details, area);
    }
}

fn render_migrations(f: &mut Frame, app: &App, area: Rect) {
    let migrations: Vec<&ServerConfig> = app.config.servers.iter()
        .filter(|s| s.migration_to.is_some())
        .collect();
    
    let items: Vec<ListItem> = migrations
        .iter()
        .map(|server| {
            let status = server.migration_status.as_deref().unwrap_or("Planned");
            let target = server.migration_to.as_deref().unwrap_or("Unknown");
            
            Line::from(vec![
                Span::styled(&server.name, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw(" → "),
                Span::styled(target, Style::default().fg(Color::Green)),
                Span::raw(" | Status: "),
                Span::styled(status, Style::default().fg(Color::Yellow)),
            ])
        })
        .map(ListItem::new)
        .collect();
    
    let list = List::new(items)
        .block(Block::default()
            .borders(Borders::ALL)
            .title("Server Migrations (Press Esc to return)")
        );
    
    f.render_widget(list, area);
}

// ============================================================================
// Main Application
// ============================================================================

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load configuration
    let config_str = fs::read_to_string("config.toml")
        .unwrap_or_else(|_| create_default_config());
    let config: Config = toml::from_str(&config_str)?;
    
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    
    let mut app = App::new(config.clone());
    
    // Auto-scan if configured
    if config.settings.auto_scan_on_start {
        app.is_scanning = true;
        app.status_message = "Scanning certificates...".to_string();
        terminal.draw(|f| ui(f, &app))?;
        
        app.certificates = scan_all_certificates(&config).await;
        app.is_scanning = false;
        app.status_message = format!("Scan complete! Found {} certificates", app.certificates.len());
    }
    
    // Main loop
    loop {
        terminal.draw(|f| ui(f, &app))?;
        
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Char('Q') => break,
                    KeyCode::F(5) => {
                        if !app.is_scanning {
                            app.is_scanning = true;
                            app.status_message = "Scanning certificates...".to_string();
                            terminal.draw(|f| ui(f, &app))?;
                            
                            app.certificates = scan_all_certificates(&config).await;
                            app.is_scanning = false;
                            app.status_message = format!("Scan complete! Found {} certificates", app.certificates.len());
                        }
                    }
                    KeyCode::F(3) => {
                        match app.export_csv() {
                            Ok(_) => app.status_message = "Exported to CSV successfully!".to_string(),
                            Err(e) => app.status_message = format!("Export failed: {}", e),
                        }
                    }
                    KeyCode::F(4) => {
                        app.view_mode = ViewMode::Migrations;
                    }
                    KeyCode::Char('c') => app.filter = FilterMode::Critical,
                    KeyCode::Char('w') => app.filter = FilterMode::Warning,
                    KeyCode::Char('o') => app.filter = FilterMode::Ok,
                    KeyCode::Char('e') => app.filter = FilterMode::Error,
                    KeyCode::Char('a') => app.filter = FilterMode::All,
                    KeyCode::Up => app.previous(),
                    KeyCode::Down => app.next(),
                    KeyCode::Enter => {
                        if matches!(app.view_mode, ViewMode::Dashboard) {
                            app.view_mode = ViewMode::Details;
                        }
                    }
                    KeyCode::Esc => {
                        app.view_mode = ViewMode::Dashboard;
                    }
                    _ => {}
                }
            }
        }
    }
    
    // Cleanup
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    
    Ok(())
}

// ============================================================================
// Helper Functions
// ============================================================================

fn create_default_config() -> String {
    r#"
[settings]
scan_timeout_secs = 5
warning_days = 60
critical_days = 30
auto_scan_on_start = true

[[servers]]
name = "prod-server-01"
host = "192.168.1.100"
migration_to = "prod-server-05"
migration_status = "In Progress"

[[servers.applications]]
name = "api-gateway"
ports = [443, 8443]

[[servers.applications]]
name = "web-app"
ports = [443]

[[servers]]
name = "dev-server"
host = "192.168.1.101"

[[servers.applications]]
name = "test-api"
ports = [443, 9443]

[[servers]]
name = "staging-server"
host = "10.0.0.50"

[[servers.applications]]
name = "staging-web"
ports = [443]
expected_cn = "staging.example.com"
"#.to_string()
}