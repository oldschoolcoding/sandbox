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
use crate::network::connection::{ConnectionManager, RemoteConnection, ConnectionStatus};
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
    }
}


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


#[derive(Debug, PartialEq)]
pub enum FocusedPane {
    FileBrowser,
    SearchResults,
}

#[derive(Clone)]
pub struct SearchConfig {
    pub filename_regex: Option<String>,
    pub content_regex: Option<String>,
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
    pub content_scroll: usize, // Scroll position for content pane
    pub content_loaded: bool,  // Whether content is fully loaded
    pub content_total_lines: usize, // Total lines available (for large files)
    pub input_mode: InputMode,
    pub search_query: String,
    pub search_results: Vec<Line<'static>>,
    pub grep_results: Vec<GrepResult>,
    pub search_config: Option<SearchConfig>,
    pub search_field_focus: usize, // 0 = filename, 1 = content
    pub search_receiver: Option<mpsc::UnboundedReceiver<SearchResult>>,
    pub search_task: Option<task::JoinHandle<()>>,
    pub content_cache: HashMap<PathBuf, Vec<String>>,
    pub last_selection_time: Instant,
    pub pending_selection: Option<usize>,
    pub needs_content_update: bool,
    // Connection management
    pub connection_manager: ConnectionManager,
    pub connection_list_state: ListState,
    pub new_connection: RemoteConnection,
    pub last_executed_command: Option<String>,
    // AI integration
    pub ai_client: crate::ai::AIClient,
    pub natural_language_query: String,
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
            content_scroll: 0,
            content_loaded: false,
            content_total_lines: 0,
            input_mode: InputMode::Normal,
            search_query: String::new(),
            search_results: vec![],
            grep_results: vec![],
            search_config: None,
            search_field_focus: 0,
            search_receiver: None,
            search_task: None,
            content_cache: HashMap::new(),
            last_selection_time: Instant::now(),
            pending_selection: None,
            needs_content_update: false,
            connection_manager: ConnectionManager::new()?,
            connection_list_state: ListState::default(),
            new_connection: RemoteConnection {
                name: String::new(),
                username: String::new(),
                host: String::new(),
                port: 22,
                password: String::new(),
                status: ConnectionStatus::Unknown,
            },
            last_executed_command: None,
            ai_client: crate::ai::AIClient::default(),
            natural_language_query: String::new(),
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
            // Auto-load content for the selected item
            self.on_selection_changed();
        }
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
        self.on_selection_changed();
    }

    pub fn select_next(&mut self) {
        let i = match self.table_state.selected() {
            Some(i) => if i + 1 >= self.entries.len() { 0 } else { i + 1 },
            None => 0,
        };
        self.table_state.select(Some(i));
        self.on_selection_changed();
    }

    pub fn on_selection_changed(&mut self) {
        self.mark_content_dirty();
        self.content_scroll = 0; // Reset scroll position

        // Auto-load content for selected file
        if let Some(i) = self.table_state.selected() {
            if let Some(entry) = self.entries.get(i) {
                if !entry.is_dir {
                    // Load file content asynchronously
                    let _ = self.load_file_content_async();
                } else {
                    // Clear content for directories
                    self.file_content.clear();
                    self.content_loaded = true;
                    self.content_total_lines = 0;
                }
            }
        }
    }

    pub fn enter_dir(&mut self) -> Result<(), AppError> {
        if let Some(i) = self.table_state.selected() {
            if self.entries[i].is_dir {
                self.current_path = self.entries[i].path.clone();
                self.refresh_entries()?;
                self.clear_search();
            } else {
                // Load file content for selected file
                self.load_file_content_async()?;
            }
        }
        Ok(())
    }

    pub fn load_file_content_async(&mut self) -> Result<(), AppError> {
        if let Some(i) = self.table_state.selected() {
            if !self.entries[i].is_dir {
                let path = self.entries[i].path.clone();

                // First, try to get basic file info to determine size
                match self.fs_ops.get_file_properties(&path) {
                    Ok(props) => {
                        // Parse size to estimate line count (rough estimate: 50 chars per line)
                        let estimated_lines = props.size.parse::<usize>().unwrap_or(0) / 50;

                        if estimated_lines > 1000 {
                            // Large file - load first chunk only
                            self.load_file_chunk(0, 100)?;
                            self.content_loaded = false;
                            self.content_total_lines = estimated_lines;
                        } else {
                            // Small file - load all content
                            self.load_file_chunk(0, usize::MAX)?;
                            self.content_loaded = true;
                            self.content_total_lines = self.file_content.len();
                        }
                    }
                    Err(_) => {
                        // Fallback: try to load first chunk
                        self.load_file_chunk(0, 100)?;
                        self.content_loaded = false;
                        self.content_total_lines = 100; // Unknown
                    }
                }
            }
        }
        Ok(())
    }

    pub fn load_file_chunk(&mut self, start_line: usize, max_lines: usize) -> Result<(), AppError> {
        if let Some(i) = self.table_state.selected() {
            if !self.entries[i].is_dir {
                match self.fs_ops.read_file_chunk(&self.entries[i].path, start_line, max_lines) {
                    Ok(content) => {
                        self.file_content = content;
                        self.mark_content_dirty();
                    }
                    Err(e) => {
                        self.file_content = vec![format!("Cannot display file content: {}", get_user_friendly_error(&e))];
                        self.mark_content_dirty();
                    }
                }
            }
        }
        Ok(())
    }

    pub fn scroll_content_up(&mut self) {
        if self.content_scroll > 0 {
            self.content_scroll = self.content_scroll.saturating_sub(5);
        }
    }

    pub fn scroll_content_down(&mut self) {
        let max_scroll = self.file_content.len().saturating_sub(20); // Leave some margin
        if self.content_scroll < max_scroll {
            self.content_scroll += 5;
        } else if !self.content_loaded && self.file_content.len() >= 20 {
            // Load more content if available
            let next_start = self.file_content.len();
            let _ = self.load_file_chunk(next_start, 100);
        }
    }

    pub fn leave_dir(&mut self) -> Result<(), AppError> {
        // For remote connections, handle navigation differently
        if self.is_remote() {
            let components: Vec<_> = self.current_path.components().collect();

            if components.len() <= 1 {
                // We're at the initial directory ("."), go up to "/"
                self.current_path = PathBuf::from("/");
                self.refresh_entries()?;
                self.clear_search();
                return Ok(());
            }

            // Pop the last component for deeper navigation
            if self.current_path.pop() {
                // For remote, ensure we don't end up with an empty path
                if self.current_path.as_os_str().is_empty() {
                    self.current_path = PathBuf::from("/");
                }

                self.refresh_entries()?;
                self.clear_search();
            }
            return Ok(());
        }

        // For local filesystem
        if self.current_path.pop() {
            // After popping, ensure we don't go above root
            if self.current_path.as_os_str().is_empty() {
                self.current_path = PathBuf::from("/");
            }

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
                        // Display the executed command
                        self.status_message = Some(format!("Executing: {}", cmd));
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

    pub fn navigate_search(&mut self, next: bool) {
        if next {
            let i = self.search_list_state.selected().map(|i| i + 1).unwrap_or(0);
            let max = self.search_results.len().saturating_sub(1);
            self.search_list_state.select(Some(i.min(max)));
        } else {
            let i = self.search_list_state.selected().and_then(|i| i.checked_sub(1)).unwrap_or(0);
            self.search_list_state.select(Some(i));
        }
    }

    pub fn navigate_connection_list(&mut self, next: bool) {
        let connections = self.connection_manager.get_connections();
        let current = self.connection_list_state.selected().unwrap_or(0);
        let max_index = connections.len().saturating_sub(1);

        let new_index = if next {
            if current >= max_index { 0 } else { current + 1 }
        } else {
            if current == 0 { max_index } else { current - 1 }
        };

        self.connection_list_state.select(Some(new_index));
    }

    pub fn interpret_natural_language_query(&mut self) {
        let query = self.natural_language_query.clone();
        if query.trim().is_empty() {
            self.status_message = Some("Please enter a natural language query first".to_string());
            return;
        }

        self.status_message = Some("Interpreting query with AI...".to_string());

        // Create a new AI client instance for the async task
        let ai_client = crate::ai::AIClient::new(self.ai_client.get_config().clone());

        // Clone query for async task
        let query_clone = query.clone();

        // Perform AI interpretation in background
        tokio::spawn(async move {
            match ai_client.interpret_search_query(&query_clone).await {
                Ok(search_config) => {
                    // In a real implementation, we'd update the app state here
                    // For now, we'll just print the result
                    println!("AI interpreted '{}' -> filename: {:?}, content: {:?}",
                            query_clone, search_config.filename_regex, search_config.content_regex);
                }
                Err(e) => {
                    eprintln!("AI interpretation failed: {}", e);
                }
            }
        });

        // For demo purposes, also use the synchronous fallback
        let interpreted_config = self.interpret_query_fallback(&query);
        self.search_config = Some(interpreted_config);
        self.status_message = Some(format!("AI interpreted query: '{}'", query));
    }

    fn interpret_query_fallback(&self, query: &str) -> SearchConfig {
        // Fallback rule-based interpretation
        let query_lower = query.to_lowercase();

        if query_lower.contains("python") || query_lower.contains("py") {
            SearchConfig {
                filename_regex: Some(".*\\.py$".to_string()),
                content_regex: None,
            }
        } else if query_lower.contains("config") || query_lower.contains("configuration") {
            SearchConfig {
                filename_regex: Some(".*config.*|.*\\.conf.*|.*\\.yml.*|.*\\.yaml.*".to_string()),
                content_regex: None,
            }
        } else if query_lower.contains("error") || query_lower.contains("exception") {
            SearchConfig {
                filename_regex: None,
                content_regex: Some("error|Error|ERROR|exception|Exception".to_string()),
            }
        } else if query_lower.contains("database") || query_lower.contains("db") {
            SearchConfig {
                filename_regex: None,
                content_regex: Some("database|connect|db|mysql|postgres|mongodb".to_string()),
            }
        } else {
            // Default to content search with the query terms
            SearchConfig {
                filename_regex: None,
                content_regex: Some(query.to_string()),
            }
        }
    }

    pub fn test_all_connections(&mut self) {
        // For simplicity, test connections synchronously
        // Set all connections to testing status first
        for connection in self.connection_manager.get_connections_mut() {
            connection.status = ConnectionStatus::Testing;
        }

        self.status_message = Some("Testing all connections...".into());

        // Test connections synchronously (this blocks the UI but is simple)
        // In a real app, this would be done asynchronously
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            self.connection_manager.test_all_connections().await;
        });

        self.status_message = Some("Connection testing completed".into());
    }


    pub fn connect_to_selected(&mut self) -> Result<(), AppError> {
        if let Some(idx) = self.connection_list_state.selected() {
            let connections = self.connection_manager.get_connections();
            if idx >= connections.len() {
                return Err(AppError::Navigation(format!("Invalid connection index: {} (max: {})", idx, connections.len())));
            }
            let conn = &connections[idx];
            let conn_name = conn.name.clone();
            let conn_username = conn.username.clone();
            let conn_host = conn.host.clone();
            let conn_port = conn.port;
            let conn_password = conn.password.clone();

            self.status_message = Some(format!("Connecting to {}@{}:{}...", conn_username, conn_host, conn_port));

            let fs = crate::filesystem::SftpFileSystem::new(
                &conn_username,
                &conn_host,
                conn_port,
                &conn_password,
            )?;
            self.fs_ops = Box::new(fs);
            self.current_path = self.fs_ops.get_start_path()?;
            self.refresh_entries()?;
            self.input_mode = InputMode::Normal;
            self.status_message = Some(format!("Connected to {}", conn_name));
        } else {
            return Err(AppError::Navigation("No connection selected".into()));
        }
        Ok(())
    }

    pub fn start_add_connection(&mut self) {
        // Initialize a new connection with defaults
        self.new_connection = RemoteConnection {
            name: String::new(),
            username: String::new(),
            host: String::new(),
            port: 22,
            password: String::new(),
            status: ConnectionStatus::Unknown,
        };
        // Start with the name field
        self.input_mode = InputMode::AddConnection(AddConnectionStep::Name);
    }

    pub fn delete_selected_connection(&mut self) -> Result<(), AppError> {
        if let Some(idx) = self.connection_list_state.selected() {
            self.connection_manager.delete_connection(idx)?;
            self.connection_manager.save()?;
            // Adjust selection if necessary
            let connections = self.connection_manager.get_connections();
            if connections.is_empty() {
                self.connection_list_state.select(None);
            } else if idx >= connections.len() {
                self.connection_list_state.select(Some(connections.len() - 1));
            }
        }
        Ok(())
    }

    pub fn next_add_connection_step(&mut self) -> bool {
        match self.input_mode {
            InputMode::AddConnection(step) => {
                match step {
                    AddConnectionStep::Name => {
                        if !self.new_connection.name.is_empty() {
                            self.input_mode = InputMode::AddConnection(AddConnectionStep::Username);
                        }
                    }
                    AddConnectionStep::Username => {
                        if !self.new_connection.username.is_empty() {
                            self.input_mode = InputMode::AddConnection(AddConnectionStep::Host);
                        }
                    }
                    AddConnectionStep::Host => {
                        if !self.new_connection.host.is_empty() {
                            self.input_mode = InputMode::AddConnection(AddConnectionStep::Port);
                        }
                    }
                    AddConnectionStep::Port => {
                        self.input_mode = InputMode::AddConnection(AddConnectionStep::Password);
                    }
                    AddConnectionStep::Password => {
                        if !self.new_connection.password.is_empty() {
                            // Connection is complete, add it to the manager
                            if let Err(e) = self.connection_manager.add_connection(self.new_connection.clone()) {
                                // For now, just ignore the error and complete anyway
                                // In a real implementation, you'd want to show an error
                            }
                            if let Err(e) = self.connection_manager.save() {
                                // For now, just ignore the error
                                // In a real implementation, you'd want to show an error
                            }
                            return true; // Connection completed
                        }
                    }
                }
            }
            _ => {}
        }
        false
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

    pub fn cancel_search(&mut self, clear_config: bool) {
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



// Helper: is the current filesystem remote?
pub fn is_remote(&self) -> bool {
    self.fs_ops.as_any().is::<SftpFileSystem>()
}

fn perform_remote_search(&mut self) -> Result<(), AppError> {
    let sftp_fs = match self.fs_ops.as_any().downcast_ref::<SftpFileSystem>() {
        Some(fs) => fs,
        None => return Ok(()),
    };

    let config = self.search_config.as_ref().expect("search config missing");
    let current_path_str = self.current_path.display().to_string();

    // Compile regexes for filename and content
    let filename_re = if let Some(ref filename_regex) = config.filename_regex {
        if filename_regex.is_empty() {
            None
        } else {
            match Regex::new(filename_regex) {
                Ok(re) => Some(re),
                Err(e) => {
                    self.status_message = Some(format!("Invalid filename regex: {}", e));
                    return Ok(());
                }
            }
        }
    } else {
        None
    };

    let content_re = if let Some(ref content_regex) = config.content_regex {
        if content_regex.is_empty() {
            None
        } else {
            match Regex::new(content_regex) {
                Ok(re) => Some(re),
                Err(e) => {
                    self.status_message = Some(format!("Invalid content regex: {}", e));
                    return Ok(());
                }
            }
        }
    } else {
        None
    };

    // If both are None, search everything
    if filename_re.is_none() && content_re.is_none() {
        self.status_message = Some("Please specify at least one search criteria".into());
        return Ok(());
    }

    // Clear previous results and start fresh
    self.search_results.clear();
    self.search_list_state.select(None);

    let mut channel = sftp_fs.session.channel_session()?;

    // Determine search strategy based on what criteria are provided
    if let Some(ref filename_re) = filename_re {
        // Filename search - use find command
        self.status_message = Some("Executing remote filename search...".into());
        let name_pattern = format!(r".*{}.*", filename_re.as_str());

        let cmd = format!(
            "find '{}' -type f -regextype posix-extended -regex '{}'",
            current_path_str,
            name_pattern
        );

        channel.exec(&cmd)?;
    } else if let Some(ref content_re) = content_re {
        // Content-only search - use grep command
        self.status_message = Some("Executing remote content search...".into());
        let cmd = format!(
            "grep -r -n -I --binary-files=without-match --exclude-dir=.git --exclude-dir=.svn --exclude-dir=.hg '{}' '{}'",
            content_re.as_str(),
            current_path_str
        );

        channel.exec(&cmd)?;
    } else {
        // This should not happen due to earlier check, but handle gracefully
        self.status_message = Some("No search criteria specified".into());
        return Ok(());
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

        // Process results based on search type
        if content_re.is_some() && line.contains(':') {
            // Content search result with line numbers (grep format: file:line:content)
            let parts: Vec<&str> = line.splitn(3, ':').collect();
            if parts.len() == 3 {
                let full_path = parts[0];
                let line_num = parts[1];
                let text = parts[2];

                let mut spans = vec![
                    Span::raw(format!("{}:{}: ", full_path, line_num))
                ];

                // Highlight matches in content if we have content regex
                if let Some(ref re) = content_re {
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
                } else {
                    spans.push(Span::raw(text.to_owned()));
                }

                self.search_results.push(Line::from(spans));
            }
        } else {
            // Filename search result (find format: just the path)
            let display_line = line.to_string();
            let styled_line = Line::from(vec![Span::raw(display_line)]);
            self.search_results.push(styled_line);
        }

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
    let config = self.search_config.as_ref().ok_or_else(|| AppError::Navigation("No search configuration".into()))?;

    // Check if at least one search criteria is provided
    let has_filename_criteria = config.filename_regex.as_ref().map_or(false, |s| !s.is_empty());
    let has_content_criteria = config.content_regex.as_ref().map_or(false, |s| !s.is_empty());

    if !has_filename_criteria && !has_content_criteria {
        self.status_message = Some("Please specify at least one search criteria (filename or content)".into());
        self.input_mode = InputMode::Normal;
        return Ok(());
    }

    // Clone necessary data for the async task
    let search_config = Some(config.clone());
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

    // Return to normal mode to show search progress and results
    self.input_mode = InputMode::Normal;

    // Handle search based on connection type
    if is_remote {
        // Extract connection details for async remote search
        let connection_details = self.fs_ops.as_any().downcast_ref::<SftpFileSystem>()
            .map(|fs| (fs.remote_host.clone(), fs.username.clone(), fs.password.clone(), fs.port));

        if let Some((host, username, password, port)) = connection_details {
            // Display what will be searched
            let search_desc = match (has_filename_criteria, has_content_criteria) {
                (true, true) => "filename + content",
                (true, false) => "filename only",
                (false, true) => "content only",
                _ => "unknown"
            };
            self.status_message = Some(format!("Searching {} on {}@{}:{} ...", search_desc, username, host, port));

            let task = task::spawn(async move {
                let _ = perform_remote_search_async(host, username, password, port, String::new(), search_config, current_path, tx).await;
            });
            self.search_task = Some(task);
        } else {
            let _ = tx.send(SearchResult::Error("Could not extract connection details".into()));
            let _ = tx.send(SearchResult::Finished);
        }
    } else {
        // Local search (not implemented yet)
        self.status_message = Some("Local unified search not yet implemented".into());
        let _ = tx.send(SearchResult::Error("Local unified search not yet implemented".into()));
        let _ = tx.send(SearchResult::Finished);
    }
    Ok(())
}

async fn perform_local_search_async(
    search_query: String,
    search_config: Option<SearchConfig>,
    current_path: PathBuf,
    tx: mpsc::UnboundedSender<SearchResult>,
) {
    // Local unified search not yet implemented
    let _ = tx.send(SearchResult::Error("Local unified search not yet implemented".into()));
    let _ = tx.send(SearchResult::Finished);
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

