use std::{
    any::Any,
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio::task;
use ratatui::{
    layout::{Constraint, Layout, Direction, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Row, Cell, Table, TableState, ListState},
    Frame,
};
use crossterm::event::{KeyCode, KeyModifiers};
use regex::Regex;
use users;
use super::error::AppError;
use crate::filesystem::{FileSystemOperations, FileEntry, FileProperties, SftpFileSystem, LocalFileSystem};
use crate::network::connection::{ConnectionManager, RemoteConnection};
use crate::ui::AddConnectionStep;
use crate::search::perform_remote_search_async;

fn get_user_friendly_error(error: &AppError) -> String {
    match error {
        AppError::Io(io_err) => match io_err.kind() {
            std::io::ErrorKind::PermissionDenied => "Permission denied - you don't have access to this file".to_string(),
            std::io::ErrorKind::NotFound => "File not found".to_string(),
            std::io::ErrorKind::IsADirectory => "Cannot read file as directory".to_string(),
            std::io::ErrorKind::NotADirectory => "Cannot enter - this is not a directory".to_string(),
            _ => format!("I/O error: {}", io_err),
        },
        AppError::Navigation(msg) => msg.clone(),
        AppError::Ssh(err) => format!("SSH connection error: {}", err),
        AppError::Regex(err) => format!("Search pattern error: {}", err),
        AppError::Serialization(err) => format!("Data error: {}", err),
        AppError::Encryption(msg) => format!("Security error: {}", msg),
        AppError::Password(msg) => format!("Authentication error: {}", msg),
    }
}

const CONNECTIONS_FILE: &str = "connections.enc";

#[derive(Debug)]
pub enum SearchResult {
    Found(Line<'static>),
    GrepMatch {
        file_path: String,
        line_number: usize,
        line_content: String,
        search_term: String,
    },
    Finished,
    Error(String),
    Status(String),
    Command(String),
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum SearchKind { Name, Content }

#[derive(Debug, PartialEq)]
pub enum FocusedPane {
    FileBrowser,
    SearchResults,
}

#[derive(Clone, Copy)]
pub struct SearchConfig {
    pub kind: SearchKind,
}

#[derive(Debug, Clone)]
pub struct GrepResult {
    pub file_path: String,
    pub line_number: usize,
    pub line_content: String,
    pub search_term: String,
}

#[derive(PartialEq)]
pub enum InputMode {
    Normal,
    Search,
    Password,
    ConnectionList,
    AddConnection(AddConnectionStep),
}

// Constants for Unix file type masks
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

const DEBOUNCE_MS: u64 = 100;
const CHUNK_SIZE: usize = 50;
const MAX_CACHE_SIZE: usize = 100;

pub struct App {
    pub fs_ops: Box<dyn FileSystemOperations>,
    pub current_path: PathBuf,
    pub entries: Vec<FileEntry>,
    pub table_state: TableState,
    pub search_list_state: ListState,
    pub focused_pane: FocusedPane,
    pub status_message: Option<String>,
    pub file_content: Vec<String>,
    pub input_mode: InputMode,
    pub search_query: String,
    pub search_results: Vec<Line<'static>>,
    pub grep_results: Vec<GrepResult>,
    pub search_config: Option<SearchConfig>,
    pub search_receiver: Option<mpsc::UnboundedReceiver<SearchResult>>,
    pub search_task: Option<task::JoinHandle<()>>,
    pub content_cache: HashMap<PathBuf, Vec<String>>,
    pub last_selection_time: Instant,
    pub pending_selection: Option<usize>,
    pub needs_content_update: bool,
    // Connection management
    pub connection_manager: ConnectionManager,
    pub connection_list_state: ListState,
    pub password_input: String,
    pub new_connection: RemoteConnection,
    pub last_executed_command: Option<String>,
}

impl App {
    pub fn new_local() -> Result<Self, AppError> {
        Self::new(Box::new(LocalFileSystem))
    }

    fn new(fs: Box<dyn FileSystemOperations>) -> Result<Self, AppError> {
        let mut app = Self {
            fs_ops: fs,
            current_path: PathBuf::new(),
            entries: vec![],
            table_state: TableState::default(),
            search_list_state: ListState::default(),
            focused_pane: FocusedPane::FileBrowser,
            status_message: None,
            file_content: vec![],
            input_mode: InputMode::Normal,
            search_query: String::new(),
            search_results: vec![],
            grep_results: vec![],
            search_config: None,
            search_receiver: None,
            search_task: None,
            content_cache: HashMap::new(),
            last_selection_time: Instant::now(),
            pending_selection: None,
            needs_content_update: false,
            connection_manager: ConnectionManager::new()?,
            connection_list_state: ListState::default(),
            password_input: String::new(),
            new_connection: RemoteConnection {
                name: String::new(),
                username: String::new(),
                host: String::new(),
                port: 22,
                password: String::new(),
            },
            last_executed_command: None,
        };
        app.current_path = app.fs_ops.get_start_path()?;
        app.refresh_entries()?;
        Ok(app)
    }

    fn refresh_entries(&mut self) -> Result<(), AppError> {
        self.entries = self.fs_ops.read_dir(&self.current_path)?;
        self.content_cache.clear();
        
        if self.entries.is_empty() {
            self.table_state.select(None);
        } else {
            let i = self.table_state.selected().unwrap_or(0).min(self.entries.len() - 1);
            self.table_state.select(Some(i));
        }
        self.mark_content_dirty();
        Ok(())
    }

    fn mark_content_dirty(&mut self) {
        self.last_selection_time = Instant::now();
        self.needs_content_update = true;
        if let Some(i) = self.table_state.selected() {
            self.pending_selection = Some(i);
        }
    }

    fn update_content_if_needed(&mut self) {
        if !self.needs_content_update {
            return;
        }

        if self.last_selection_time.elapsed() < Duration::from_millis(DEBOUNCE_MS) {
            return;
        }

        self.needs_content_update = false;
        self.file_content.clear();

        if let Some(i) = self.table_state.selected() {
            if let Some(e) = self.entries.get(i) {
                if !e.is_dir {
                    if let Some(cached) = self.content_cache.get(&e.path) {
                        self.file_content = cached.clone();
                    } else {
                        if let Ok(content) = self.fs_ops.read_file_chunk(&e.path, 0, CHUNK_SIZE) {
                            self.file_content = content.clone();
                            
                            if self.content_cache.len() >= MAX_CACHE_SIZE {
                                if let Some(first_key) = self.content_cache.keys().next().cloned() {
                                    self.content_cache.remove(&first_key);
                                }
                            }
                            
                            self.content_cache.insert(e.path.clone(), content);
                        }
                    }
                }
            }
        }
    }

    pub fn select_prev(&mut self) {
        let i = match self.table_state.selected() {
            Some(i) => if i == 0 { self.entries.len().saturating_sub(1) } else { i - 1 },
            None => 0,
        };
        self.table_state.select(Some(i));
        self.mark_content_dirty();
    }

    pub fn select_next(&mut self) {
        let i = match self.table_state.selected() {
            Some(i) => if i + 1 >= self.entries.len() { 0 } else { i + 1 },
            None => 0,
        };
        self.table_state.select(Some(i));
        self.mark_content_dirty();
    }

    pub fn enter_dir(&mut self) -> Result<(), AppError> {
        if let Some(i) = self.table_state.selected() {
            if self.entries[i].is_dir {
                self.current_path = self.entries[i].path.clone();
                self.refresh_entries()?;
                self.clear_search();
            } else {
                // Load file content for selected file
                self.load_file_content()?;
            }
        }
        Ok(())
    }

    pub fn load_file_content(&mut self) -> Result<(), AppError> {
        if let Some(i) = self.table_state.selected() {
            if !self.entries[i].is_dir {
                // Try to read the file content
                match self.fs_ops.read_file_chunk(&self.entries[i].path, 0, 100) {
                    Ok(content) => {
                        self.file_content = content;
                        self.mark_content_dirty();
                    }
                    Err(e) => {
                        // If we can't read as text, just show an error message
                        self.file_content = vec![format!("Cannot display file content: {}", get_user_friendly_error(&e))];
                        self.mark_content_dirty();
                    }
                }
            }
        }
        Ok(())
    }

    pub fn leave_dir(&mut self) -> Result<(), AppError> {
        if self.current_path.pop() {
            self.refresh_entries()?;
            self.clear_search();
        }
        Ok(())
    }

    pub fn clear_search(&mut self) {
        self.search_results.clear();
        self.grep_results.clear();
        self.search_list_state.select(None);
    }

    pub fn jump_to_grep_result(&mut self) -> Result<(), AppError> {
        if let Some(idx) = self.search_list_state.selected() {
            if let Some(result) = self.grep_results.get(idx) {
                let file_path = result.file_path.clone();
                let line_number = result.line_number;

                // Navigate to the directory containing the file
                if let Some(parent) = Path::new(&file_path).parent() {
                    self.current_path = parent.to_path_buf();
                    self.refresh_entries()?;
                }

                // Find and select the file in the current directory
                if let Some(file_name) = Path::new(&file_path).file_name() {
                    for (i, entry) in self.entries.iter().enumerate() {
                        if entry.path.file_name() == Some(file_name) {
                            self.table_state.select(Some(i));
                            break;
                        }
                    }
                }

                // Load the file content and jump to the specific line if it's a content search
                if line_number > 0 {
                    // TODO: Implement load_file_content method
                    // self.load_file_content()?;
                    // Could add line highlighting/scrolling here
                }

                // Switch back to file browser pane
                self.focused_pane = FocusedPane::FileBrowser;
            }
        }
        Ok(())
    }

    pub fn process_search_results(&mut self) {
        if let Some(rx) = &mut self.search_receiver {
            while let Ok(result) = rx.try_recv() {
                match result {
                    SearchResult::GrepMatch { file_path, line_number, line_content, search_term } => {
                        self.grep_results.push(GrepResult {
                            file_path: file_path.clone(),
                            line_number,
                            line_content: line_content.clone(),
                            search_term: search_term.clone(),
                        });

                        // Create a display line for the UI (Yazi-style formatting)
                        let display_line = if line_number > 0 {
                            format!("{}:{}:{}", file_path, line_number, line_content.trim())
                        } else {
                            file_path.clone()
                        };

                        // Highlight the search term with red color (like Yazi)
                        let spans = if let Ok(re) = Regex::new(&regex::escape(&search_term)) {
                            let mut spans = vec![];
                            let mut last = 0;
                            for mat in re.find_iter(&display_line) {
                                spans.push(Span::raw(display_line[last..mat.start()].to_string()));
                                spans.push(Span::styled(
                                    display_line[mat.range()].to_string(),
                                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                                ));
                                last = mat.end();
                            }
                            spans.push(Span::raw(display_line[last..].to_string()));
                            spans
                        } else {
                            vec![Span::raw(display_line)]
                        };

                        self.search_results.push(Line::from(spans));

                        // Auto-focus on first result (like Yazi's spot mode)
                        if self.grep_results.len() == 1 {
                            self.search_list_state.select(Some(0));
                            self.focused_pane = FocusedPane::SearchResults;
                        }
                    }
                    SearchResult::Found(line) => {
                        self.search_results.push(line);
                        if self.search_results.len() == 1 {
                            self.search_list_state.select(Some(0));
                            self.focused_pane = FocusedPane::SearchResults;
                        }
                    }
                    SearchResult::Finished => {
                        self.status_message = Some(format!("Found {} matches", self.grep_results.len()));
                    }
                    SearchResult::Error(msg) => {
                        self.status_message = Some(format!("Search error: {}", msg));
                    }
                    SearchResult::Status(msg) => {
                        self.status_message = Some(msg);
                    }
                    SearchResult::Command(cmd) => {
                        // Store the command for potential display
                        self.last_executed_command = Some(cmd);
                    }
                }
            }
        }

        // Clean up finished receiver (Yazi-style async cleanup)
        if let Some(rx) = &mut self.search_receiver {
            if rx.is_closed() {
                self.search_receiver = None;
                self.search_task = None;
                if self.status_message.is_none() {
                    self.status_message = Some(format!("Found {} matches", self.grep_results.len()));
                }
            }
        }
    }

    fn cancel_search(&mut self, clear_config: bool) {
        // Abort the search task if running
        if let Some(task) = self.search_task.take() {
            task.abort();
        }

        // Clear the receiver
        self.search_receiver = None;

        self.clear_search();
        if clear_config {
            self.search_config = None;
            self.search_query.clear();
        }
        self.status_message = Some("Search cancelled".into());
    }

    fn start_search(&mut self, kind: SearchKind) {
        self.input_mode = InputMode::Search;
        self.search_config = Some(SearchConfig { kind });
        self.search_query.clear();
        self.clear_search();
    }


// Helper: is the current filesystem remote?
pub fn is_remote(&self) -> bool {
    self.fs_ops.as_any().is::<SftpFileSystem>()
}

fn perform_remote_search(&mut self) -> Result<(), AppError> {
    let sftp_fs = match self.fs_ops.as_any().downcast_ref::<SftpFileSystem>() {
        Some(fs) => fs,
        None => return Ok(()),
    };

    let config = self.search_config.expect("search config missing");
    let current_path_str = self.current_path.display().to_string();

    let re = match Regex::new(&regex::escape(&self.search_query)) {
        Ok(r) => r,
        Err(e) => {
            self.status_message = Some(format!("Invalid regex: {}", e));
            return Ok(());
        }
    };

    // Clear previous results and start fresh
    self.search_results.clear();
    self.search_list_state.select(None);

    let mut channel = sftp_fs.session.channel_session()?;

    if config.kind == SearchKind::Name {
        // Fast name search with find -regex
        self.status_message = Some("Executing remote find command...".into());
        let name_pattern = format!(r".*{}.*", regex::escape(&self.search_query));

        let cmd = format!(
            "find '{}' -type f -regextype posix-extended -regex '{}'",
            current_path_str,
            name_pattern
        );

        channel.exec(&cmd)?;
    } else {
        // Content search with grep
        self.status_message = Some("Executing remote grep command...".into());
        let cmd = format!(
            "grep -r -n -I --binary-files=without-match --exclude-dir=.git --exclude-dir=.svn --exclude-dir=.hg '{}' '{}'",
            self.search_query,
            current_path_str
        );

        channel.exec(&cmd)?;
    }

    self.status_message = Some("Reading remote results...".into());
    let mut output = String::new();
    channel.read_to_string(&mut output)?;
    channel.wait_close()?;

    let stdout = output.trim();
    if stdout.is_empty() {
        return Ok(());
    }

    self.status_message = Some("Processing remote results...".into());
    let lines: Vec<&str> = stdout.lines().collect();

    for (i, line) in lines.into_iter().enumerate() {
        let line = line.trim();
        if line.is_empty() { continue; }

        if config.kind == SearchKind::Content && line.contains(':') {
            let parts: Vec<&str> = line.splitn(3, ':').collect();
            if parts.len() == 3 {
                let full_path = parts[0];
                let line_num = parts[1];
                let text = parts[2];

                let mut spans = vec![
                    Span::raw(format!("{}:{}: ", full_path, line_num))
                ];

                let mut last = 0;
                for m in re.find_iter(text) {
                    spans.push(Span::raw(text[last..m.start()].to_owned()));
                    spans.push(Span::styled(
                        text[m.range()].to_owned(),
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ));
                    last = m.end();
                }
                spans.push(Span::raw(text[last..].to_owned()));

                self.search_results.push(Line::from(spans));

                // Update selection for first result
                if self.search_results.len() == 1 {
                    self.search_list_state.select(Some(0));
                    self.focused_pane = FocusedPane::SearchResults;
                }
                continue;
            }
        }

        // Name matches or malformed lines
        let is_dir = line.ends_with('/');
        let display = if is_dir { format!("{}/", line) } else { line.to_string() };
        let styled = if is_dir {
            Span::styled(display, Style::default().fg(Color::Cyan))
        } else {
            Span::raw(display)
        };
        self.search_results.push(Line::from(vec![styled]));

        // Update selection for first result
        if self.search_results.len() == 1 {
            self.search_list_state.select(Some(0));
            self.focused_pane = FocusedPane::SearchResults;
        }

        // Update progress message periodically
        if i % 10 == 0 && i > 0 {
            self.status_message = Some(format!("Processing results... ({}/{} found)", self.search_results.len(), i));
        }
    }

    // Clear progress message
    self.status_message = Some(format!("Found {} matches", self.search_results.len()));

    Ok(())
}




// Replace the old search_at_depth + perform_search logic
pub fn perform_search(&mut self) -> Result<(), AppError> {
    if self.search_query.is_empty() {
        self.clear_search();
        self.input_mode = InputMode::Normal;
        return Ok(());
    }

    // Clone necessary data for the async task BEFORE cancelling
    let search_query = self.search_query.clone();
    let search_config = self.search_config.clone();
    let current_path = self.current_path.clone();
    let is_remote = self.is_remote();

    // Cancel any existing search
    self.cancel_search(false);

    // Clear previous results and start fresh
    self.search_results.clear();
    self.search_list_state.select(None);

    // Create channel for search results
    let (tx, rx) = mpsc::unbounded_channel();
    self.search_receiver = Some(rx);

    // Handle search based on connection type
    if is_remote {
        // Extract connection details for async remote search
        let connection_details = self.fs_ops.as_any().downcast_ref::<SftpFileSystem>()
            .map(|fs| (fs.remote_host.clone(), fs.username.clone(), fs.password.clone(), fs.port));

        if let Some((host, username, password, port)) = connection_details {
            let task = task::spawn(async move {
                perform_remote_search_async(host, username, password, port, search_query, search_config, current_path.clone(), tx);
            });
            self.search_task = Some(task);
        } else {
            self.status_message = Some("Remote connection details not available".into());
            self.input_mode = InputMode::Normal;
        }
    } else {
        // Local search in async task
        let search_task_handle = task::spawn(async move {
            Self::perform_local_search_async(search_query, search_config, current_path, tx).await;
        });
        self.search_task = Some(search_task_handle);
    }
    self.input_mode = InputMode::Normal;
    Ok(())
}

async fn perform_local_search_async(
    search_query: String,
    search_config: Option<SearchConfig>,
    current_path: PathBuf,
    tx: mpsc::UnboundedSender<SearchResult>,
) {
    let config = match search_config {
        Some(c) => c,
        None => {
            let _ = tx.send(SearchResult::Error("No search config provided".into()));
            return;
        }
    };

    let current_path_str = current_path.display().to_string();

    if config.kind == SearchKind::Name {
        // Create a regex pattern that matches any file whose name contains the search query
        let name_pattern = format!(r".*{}.*", regex::escape(&search_query));

        let cmd = format!(
            "find '{}' -type f -regextype posix-extended -regex '{}'",
            current_path_str,
            name_pattern
        );

        let output = match tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .await
        {
            Ok(o) => o,
            Err(e) => {
                let _ = tx.send(SearchResult::Error(format!("Command failed: {}", e)));
                return;
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);

        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() { continue; }

            let is_dir = line.ends_with('/');
            let display = if is_dir { format!("{}/", line) } else { line.to_string() };
            let styled = if is_dir {
                Span::styled(display.clone(), Style::default().fg(Color::Cyan))
            } else {
                Span::raw(display)
            };
            let _ = tx.send(SearchResult::Found(Line::from(vec![styled])));
        }
    } else {
        // Use local grep command
        let cmd = format!(
            "grep -r -n -I --binary-files=without-match --exclude-dir=.git --exclude-dir=.svn --exclude-dir=.hg '{}' '{}'",
            search_query,
            current_path_str
        );

        let output = match tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .await
        {
            Ok(o) => o,
            Err(e) => {
                let _ = tx.send(SearchResult::Error(format!("Command failed: {}", e)));
                return;
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);

        let re = match Regex::new(&regex::escape(&search_query)) {
            Ok(r) => r,
            Err(_) => {
                let _ = tx.send(SearchResult::Error("Invalid regex".into()));
                return;
            }
        };

        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() { continue; }

            if line.contains(':') {
                let parts: Vec<&str> = line.splitn(3, ':').collect();
                if parts.len() == 3 {
                    let full_path = parts[0];
                    let line_num = parts[1];
                    let text = parts[2];

                    let mut spans = vec![
                        Span::raw(format!("{}:{}: ", full_path, line_num))
                    ];

                    let mut last = 0;
                    for m in re.find_iter(text) {
                        spans.push(Span::raw(text[last..m.start()].to_owned()));
                        spans.push(Span::styled(
                            text[m.range()].to_owned(),
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        ));
                        last = m.end();
                    }
                    spans.push(Span::raw(text[last..].to_owned()));

                    let _ = tx.send(SearchResult::Found(Line::from(spans)));
                    continue;
                }
            }

            // Name matches or malformed lines
            let is_dir = line.ends_with('/');
            let display = if is_dir { format!("{}/", line) } else { line.to_string() };
            let styled = if is_dir {
                Span::styled(display.clone(), Style::default().fg(Color::Cyan))
            } else {
                Span::raw(display)
            };
            let _ = tx.send(SearchResult::Found(Line::from(vec![styled])));
        }
    }

    let _ = tx.send(SearchResult::Finished);
}


// Keep search_at_depth only for local fallback


    pub fn navigate_search(&mut self, down: bool) {
        if self.search_results.is_empty() { return; }
        let len = self.search_results.len();
        let i = match self.search_list_state.selected() {
            Some(i) => if down { (i + 1) % len } else { if i == 0 { len - 1 } else { i - 1 } },
            None => if down { 0 } else { len - 1 },
        };
        self.search_list_state.select(Some(i));
    }

    fn jump_to_selected_result(&mut self) -> Result<(), AppError> {
        let idx = match self.search_list_state.selected() {
            Some(i) => i,
            None => return Ok(()),
        };
        let line = match self.search_results.get(idx) {
            Some(l) => l,
            None => return Ok(()),
        };

        let text = line.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        let path_str = if text.contains(": ") {
            text.split(": ").next().unwrap_or(&text)
        } else {
            text.trim_end_matches('/')
        };
        let target_path = PathBuf::from(path_str);

        if let Some(parent) = target_path.parent() {
            if parent != self.current_path {
                self.current_path = parent.to_owned();
                self.refresh_entries()?;
            }
            if let Some(pos) = self.entries.iter().position(|e| e.path == target_path) {
                self.table_state.select(Some(pos));
                self.mark_content_dirty();
            }
        }
        Ok(())
    }

    fn show_connection_list(&mut self) {
        self.input_mode = InputMode::ConnectionList;
        if !self.connection_manager.get_connections().is_empty() {
            self.connection_list_state.select(Some(0));
        }
    }

    pub fn start_add_connection(&mut self) {
        self.new_connection = RemoteConnection {
            name: String::new(),
            username: String::new(),
            host: String::new(),
            port: 22,
            password: String::new(),
        };
        self.input_mode = InputMode::AddConnection(AddConnectionStep::Name);
    }

    pub fn connect_to_selected(&mut self) -> Result<(), AppError> {
        if let Some(idx) = self.connection_list_state.selected() {
            let conn = self.connection_manager.get_connections().get(idx).cloned();
            if let Some(conn) = conn {
                let fs = SftpFileSystem::new(
                    &conn.username,
                    &conn.host,
                    conn.port,
                    &conn.password,
                )?;
                self.fs_ops = Box::new(fs);
                self.current_path = self.fs_ops.get_start_path()?;
                self.refresh_entries()?;
                self.input_mode = InputMode::Normal;
                self.status_message = Some(format!("Connected to {}", conn.name));
            }
        }
        Ok(())
    }

    pub fn delete_selected_connection(&mut self) -> Result<(), AppError> {
        if let Some(idx) = self.connection_list_state.selected() {
            self.connection_manager.delete_connection(idx)?;
            let connections = self.connection_manager.get_connections();
            if connections.is_empty() {
                self.connection_list_state.select(None);
            } else if idx >= connections.len() {
                self.connection_list_state.select(Some(connections.len() - 1));
            }
            self.status_message = Some("Connection deleted".into());
        }
        Ok(())
    }

    pub fn navigate_connection_list(&mut self, down: bool) {
        let connections = self.connection_manager.get_connections();
        if connections.is_empty() { return; }
        let len = connections.len();
        let i = match self.connection_list_state.selected() {
            Some(i) => if down { (i + 1) % len } else { if i == 0 { len - 1 } else { i - 1 } },
            None => if down { 0 } else { len - 1 },
        };
        self.connection_list_state.select(Some(i));
    }

    pub fn next_add_connection_step(&mut self) -> bool {
        match self.input_mode {
            InputMode::AddConnection(step) => match step {
                AddConnectionStep::Name => {
                    if !self.new_connection.name.is_empty() {
                        self.input_mode = InputMode::AddConnection(AddConnectionStep::Username);
                        false
                    } else {
                        false
                    }
                }
                AddConnectionStep::Username => {
                    if !self.new_connection.username.is_empty() {
                        self.input_mode = InputMode::AddConnection(AddConnectionStep::Host);
                        false
                    } else {
                        false
                    }
                }
                AddConnectionStep::Host => {
                    if !self.new_connection.host.is_empty() {
                        self.input_mode = InputMode::AddConnection(AddConnectionStep::Port);
                        false
                    } else {
                        false
                    }
                }
                AddConnectionStep::Port => {
                    self.input_mode = InputMode::AddConnection(AddConnectionStep::Password);
                    false
                }
                AddConnectionStep::Password => {
                    if !self.new_connection.password.is_empty() {
                        // Add the connection
                        if let Err(e) = self.connection_manager.add_connection(self.new_connection.clone()) {
                            self.status_message = Some(format!("Failed to add connection: {}", e));
                        } else {
                            self.status_message = Some("Connection added successfully".to_string());
                        }
                        true
                    } else {
                        false
                    }
                }
            }
            _ => false,
        }
    }

    pub fn update_new_connection(&mut self, step: AddConnectionStep, c: char) {
        match step {
            AddConnectionStep::Name => self.new_connection.name.push(c),
            AddConnectionStep::Username => self.new_connection.username.push(c),
            AddConnectionStep::Host => self.new_connection.host.push(c),
            AddConnectionStep::Port => {
                if c.is_ascii_digit() {
                    self.new_connection.port = self.new_connection.port * 10 + c.to_digit(10).unwrap() as u16;
                }
            }
            AddConnectionStep::Password => self.new_connection.password.push(c),
        }
    }

    pub fn backspace_new_connection(&mut self, step: AddConnectionStep) {
        match step {
            AddConnectionStep::Name => { self.new_connection.name.pop(); }
            AddConnectionStep::Username => { self.new_connection.username.pop(); }
            AddConnectionStep::Host => { self.new_connection.host.pop(); }
            AddConnectionStep::Port => {
                self.new_connection.port /= 10;
            }
            AddConnectionStep::Password => { self.new_connection.password.pop(); }
        }
    }
}

