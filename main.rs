use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use ssh2::{Session, Sftp, FileStat};
use std::{
    any::Any,
    env,
    fs::{self, File},
    io::{self, stdout, BufReader, BufRead, Read},
    net::TcpStream,
    path::{Path, PathBuf},
    cmp::Ordering,
};
use thiserror::Error;
use chrono::prelude::*;
use infer;
use users::{get_user_by_uid, get_group_by_gid};

/// CONSTANTS
const CHUNK_SIZE: usize = 500;

/// Custom error types
#[derive(Debug, Error)]
enum AppError {
    #[error("IO Error: {0}")]
    Io(#[from] io::Error),
    #[error("SSH Error: {0}")]
    Ssh(#[from] ssh2::Error),
    #[error("Navigation Error: {0}")]
    Navigation(String),
    #[error("Argument Error: {0}")]
    ArgError(String),
}

// ------------------------------------------------------------------
// 1. Abstraction Layer (Trait and File Entry Struct)
// ------------------------------------------------------------------

/// A generic struct to hold file entry info, whether local or remote
#[derive(Debug, Clone)]
struct FileEntry {
    path: PathBuf,
    is_dir: bool,
}

/// A struct to hold file property display information
#[derive(Debug, Clone)]
struct FileProperties {
    type_and_permissions: String,
    user: String,
    group: String,
    size: String,
    modified: String,
}

/// A trait defining the operations our app needs from a filesystem
trait FileSystemOperations: Any {
    fn read_dir(&self, path: &Path) -> Result<Vec<FileEntry>, AppError>;
    fn get_start_path(&self) -> Result<PathBuf, AppError>;
    fn path_to_string(&self, path: &Path) -> String;
    fn download_file(&self, file_entry: &FileEntry, local_dest: &Path) -> Result<(), AppError>;
    fn as_any(&self) -> &dyn Any;
    fn read_file_chunk(&self, path: &Path, start_line: usize, count: usize) -> Result<Vec<String>, AppError>;
    fn read_file_head(&self, path: &Path) -> Result<Vec<u8>, AppError>;
    fn get_file_properties(&self, path: &Path) -> Result<FileProperties, AppError>;
}

// ------------------------------------------------------------------
// 2. Local Filesystem Implementation (RESTORED)
// ------------------------------------------------------------------

struct LocalFileSystem;

fn format_permissions_local(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode();
    let file_type = if metadata.is_dir() { 'd' } else if metadata.is_file() { '-' } else { '?' };

    let user = format!("{}", if mode & 0o400 != 0 { 'r' } else { '-' });
    let user = format!("{}{}", user, if mode & 0o200 != 0 { 'w' } else { '-' });
    let user = format!("{}{}", user, if mode & 0o100 != 0 { 'x' } else { '-' });

    let group = format!("{}", if mode & 0o040 != 0 { 'r' } else { '-' });
    let group = format!("{}{}", group, if mode & 0o020 != 0 { 'w' } else { '-' });
    let group = format!("{}{}", group, if mode & 0o010 != 0 { 'x' } else { '-' });

    let other = format!("{}", if mode & 0o004 != 0 { 'r' } else { '-' });
    let other = format!("{}{}", other, if mode & 0o002 != 0 { 'w' } else { '-' });
    let other = format!("{}{}", other, if mode & 0o001 != 0 { 'x' } else { '-' });

    format!("{}{}{}{}", file_type, user, group, other)
}

impl LocalFileSystem {
    fn read_local_head(path: &Path) -> Result<Vec<u8>, AppError> {
        let mut file = File::open(path)?;
        let mut buffer = vec![0; 512];
        let bytes_read = file.read(&mut buffer)?;
        buffer.truncate(bytes_read);
        Ok(buffer)
    }
}

impl FileSystemOperations for LocalFileSystem {
    fn read_dir(&self, path: &Path) -> Result<Vec<FileEntry>, AppError> {
        fs::read_dir(path)?
            .filter_map(Result::ok)
            .map(|entry| {
                let path = entry.path();
                let is_dir = path.is_dir();
                Ok(FileEntry { path, is_dir })
            })
            .collect()
    }

    fn get_start_path(&self) -> Result<PathBuf, AppError> {
        Ok(env::current_dir()?)
    }

    fn path_to_string(&self, path: &Path) -> String {
        path.to_string_lossy().to_string()
    }

    fn download_file(&self, file_entry: &FileEntry, local_dest: &Path) -> Result<(), AppError> {
        fs::copy(&file_entry.path, local_dest)?;
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn read_file_head(&self, path: &Path) -> Result<Vec<u8>, AppError> {
        LocalFileSystem::read_local_head(path)
    }

    fn read_file_chunk(&self, path: &Path, start_line: usize, count: usize) -> Result<Vec<String>, AppError> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);

        let mut lines = Vec::with_capacity(count);

        for (i, line_result) in reader.lines().enumerate() {
            if i < start_line {
                continue;
            }
            if i >= start_line + count {
                break;
            }
            lines.push(line_result?);
        }

        Ok(lines)
    }

    fn get_file_properties(&self, path: &Path) -> Result<FileProperties, AppError> {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(path)?;

        // Permissions
        let type_and_permissions = format_permissions_local(&metadata);

        // User and Group
        let user = get_user_by_uid(metadata.uid()).map(|u| u.name().to_string_lossy().to_string()).unwrap_or_else(|| metadata.uid().to_string());
        let group = get_group_by_gid(metadata.gid()).map(|g| g.name().to_string_lossy().to_string()).unwrap_or_else(|| metadata.gid().to_string());

        // Size
        let size = metadata.len().to_string();

        // Modified time
        let modified = metadata
            .modified()
            .map(DateTime::<Local>::from)
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|_| "N/A".to_string());

        Ok(FileProperties {
            type_and_permissions,
            user,
            group,
            size,
            modified,
        })
    }
}

// ------------------------------------------------------------------
// 3. Remote (SFTP) Filesystem Implementation
// ------------------------------------------------------------------

struct SftpFileSystem {
    #[allow(dead_code)]
    session: Session,
    sftp: Sftp,
    remote_host: String,
}

fn format_permissions_sftp(stat: &FileStat) -> String {
    let mode = stat.perm.unwrap_or(0);
    let file_type = if stat.is_dir() { 'd' } else if stat.is_file() { '-' } else { '?' };

    let user = format!("{}", if mode & 0o400 != 0 { 'r' } else { '-' });
    let user = format!("{}{}", user, if mode & 0o200 != 0 { 'w' } else { '-' });
    let user = format!("{}{}", user, if mode & 0o100 != 0 { 'x' } else { '-' });

    let group = format!("{}", if mode & 0o040 != 0 { 'r' } else { '-' });
    let group = format!("{}{}", group, if mode & 0o020 != 0 { 'w' } else { '-' });
    let group = format!("{}{}", group, if mode & 0o010 != 0 { 'x' } else { '-' });

    let other = format!("{}", if mode & 0o004 != 0 { 'r' } else { '-' });
    let other = format!("{}{}", other, if mode & 0o002 != 0 { 'w' } else { '-' });
    let other = format!("{}{}", other, if mode & 0o001 != 0 { 'x' } else { '-' });

    format!("{}{}{}{}", file_type, user, group, other)
}

impl SftpFileSystem {
    fn get_remote_host(&self) -> &str {
        &self.remote_host
    }

    fn new(user: &str, host_port: &str, password: &str) -> Result<Self, AppError> {
        let host = if host_port.contains(':') {
            host_port.to_string()
        } else {
            format!("{}:22", host_port)
        };

        let tcp = TcpStream::connect(&host)?;
        let mut session = Session::new()?;
        session.set_tcp_stream(tcp);
        session.handshake()?;

        // Use password authentication
        session.userauth_password(user, password)?;

        if !session.authenticated() {
            return Err(AppError::Navigation(
                "SSH Authentication failed. Invalid username or password."
                    .to_string(),
            ));
        }

        let sftp = session.sftp()?;
        let display_host = host_port.split(':').next().unwrap_or(host_port);

        Ok(Self {
            session,
            sftp,
            remote_host: display_host.to_string(),
        })
    }
}

impl FileSystemOperations for SftpFileSystem {
    fn read_dir(&self, path: &Path) -> Result<Vec<FileEntry>, AppError> {
        self.sftp
            .readdir(path)?
            .into_iter()
            .map(|(path, stat)| {
                Ok(FileEntry {
                    path,
                    is_dir: stat.is_dir(),
                })
            })
            .collect()
    }

    fn get_start_path(&self) -> Result<PathBuf, AppError> {
        let path = self.sftp.realpath(Path::new("."))?;
        Ok(path)
    }

    fn path_to_string(&self, path: &Path) -> String {
        format!("[{}]:{}", self.remote_host, path.to_string_lossy())
    }

    fn download_file(&self, file_entry: &FileEntry, local_dest: &Path) -> Result<(), AppError> {
        let mut remote_file = self.sftp.open(&file_entry.path)?;
        let mut local_file = File::create(local_dest)?;
        io::copy(&mut remote_file, &mut local_file)?;
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn read_file_head(&self, path: &Path) -> Result<Vec<u8>, AppError> {
        let mut remote_file = self.sftp.open(path)?;
        let mut buffer = vec![0; 512];
        let bytes_read = remote_file.read(&mut buffer)?;
        buffer.truncate(bytes_read);
        Ok(buffer)
    }

    fn read_file_chunk(&self, path: &Path, start_line: usize, count: usize) -> Result<Vec<String>, AppError> {
        let remote_file = self.sftp.open(path)?;
        let reader = BufReader::new(remote_file);

        let mut lines = Vec::with_capacity(count);

        for (i, line_result) in reader.lines().enumerate() {
            if i < start_line {
                continue;
            }
            if i >= start_line + count {
                break;
            }
            lines.push(line_result?);
        }

        Ok(lines)
    }

    fn get_file_properties(&self, path: &Path) -> Result<FileProperties, AppError> {
        let stat = self.sftp.stat(path)?;

        // Permissions
        let type_and_permissions = format_permissions_sftp(&stat);

        // User and Group (SFTP returns UIDs/GIDs as strings if names aren't available)
        let user = stat.uid.unwrap_or(0).to_string();
        let group = stat.gid.unwrap_or(0).to_string();

        // Size
        let size = stat.size.unwrap_or(0).to_string();

        // Modified time (Using the corrected chrono logic)
        let modified = stat
            .mtime
            .map(|t| {
                let fallback_date = Local.timestamp_opt(0, 0).unwrap();

                let datetime = Local.timestamp_opt(t as i64, 0)
                    .single()
                    .unwrap_or(fallback_date);

                datetime.format("%Y-%m-%d %H:%M").to_string()
            })
            .unwrap_or_else(|| "N/A".to_string());

        Ok(FileProperties {
            type_and_permissions,
            user,
            group,
            size,
            modified,
        })
    }
}

// ------------------------------------------------------------------
// 4. Application
// ------------------------------------------------------------------

enum Focus {
    FileList,
    FileContent,
}

struct App {
    fs_ops: Box<dyn FileSystemOperations>,
    current_path: PathBuf,
    entries: Vec<FileEntry>,
    list_state: ListState,
    status_message: Option<String>,

    selected_properties: Option<FileProperties>,

    // Focused File details for chunking
    focused_file: Option<FileEntry>,
    file_content: Vec<String>,
    content_cursor: usize,

    // TUI State
    focus: Focus,
}

impl App {
    fn new(fs_ops: Box<dyn FileSystemOperations>) -> Result<Self, AppError> {
        let start_path = fs_ops.get_start_path()?;
        let mut app = Self {
            fs_ops,
            current_path: start_path,
            entries: Vec::new(),
            list_state: ListState::default(),
            status_message: None,
            selected_properties: None,
            focused_file: None,
            file_content: Vec::new(),
            content_cursor: 0,
            focus: Focus::FileList,
        };
        app.list_state.select(Some(0));
        app.refresh_entries()?;
        Ok(app)
    }

    fn is_binary(&self, path: &Path) -> Result<bool, AppError> {
        if path.is_dir() {
            return Ok(false);
        }

        let buffer = self.fs_ops.read_file_head(path)?;
        let kind = infer::get(&buffer);

        // If infer identifies it as anything other than a text-like MIME type, treat it as binary.
        Ok(kind.is_some() && !kind.map(|k| k.mime_type().contains("text")).unwrap_or(false))
    }

    fn get_server_name(&self) -> String {
        if let Some(sftp_fs) = self.fs_ops.as_any().downcast_ref::<SftpFileSystem>() {
            sftp_fs.get_remote_host().to_string()
        } else {
            "local".to_string()
        }
    }

    fn refresh_entries(&mut self) -> Result<(), AppError> {
        self.entries = self.fs_ops.read_dir(&self.current_path)?;

        self.entries.sort_by(|a, b| {
            // 1. Directories first
            match a.is_dir.cmp(&b.is_dir).reverse() {
                Ordering::Equal => {
                    // 2. Then sort case-insensitively by name
                    let a_name = a.path.file_name().unwrap_or_default().to_string_lossy();
                    let b_name = b.path.file_name().unwrap_or_default().to_string_lossy();
                    a_name.to_lowercase().cmp(&b_name.to_lowercase())
                }
                order => order,
            }
        });

        if self.entries.is_empty() {
            self.list_state.select(None);
        } else {
            let new_index = self.list_state
                .selected()
                .unwrap_or(0)
                .min(self.entries.len().saturating_sub(1));
            self.list_state.select(Some(new_index));
        }
        self.update_content_pane();
        Ok(())
    }

    fn get_selected_entry(&self) -> Option<&FileEntry> {
        self.list_state
            .selected()
            .and_then(|i| self.entries.get(i))
    }

    /// Update properties and file content display based on selection
    fn update_content_pane(&mut self) {
        if let Some(entry) = self.get_selected_entry() {
            match self.fs_ops.get_file_properties(&entry.path) {
                Ok(props) => {
                    self.selected_properties = Some(props);
                    self.load_file_chunk(0);
                }
                Err(e) => {
                    self.selected_properties = None;
                    self.file_content = vec![format!("Error fetching properties: {}", e)];
                }
            }
        } else {
            self.selected_properties = None;
            self.file_content.clear();
        }
    }

    fn select_previous(&mut self) {
        self.status_message = None;
        if self.entries.is_empty() { return; }

        let current_index = self.list_state.selected().unwrap_or(0);
        let new_index = if current_index == 0 {
            self.entries.len().saturating_sub(1)
        } else {
            current_index - 1
        };

        self.list_state.select(Some(new_index));
        self.update_content_pane();
    }

    fn select_next(&mut self) {
        self.status_message = None;
        if self.entries.is_empty() { return; }

        let current_index = self.list_state.selected().unwrap_or(0);
        let new_index = if current_index >= self.entries.len().saturating_sub(1) {
            0
        } else {
            current_index + 1
        };

        self.list_state.select(Some(new_index));
        self.update_content_pane();
    }

    fn enter_directory(&mut self) -> Result<(), AppError> {
        self.status_message = None;
        if let Some(entry) = self.get_selected_entry().cloned() {
            if entry.is_dir {
                self.current_path = entry.path;
                self.list_state.select(Some(0));
                self.focused_file = None;
                self.file_content.clear();
                self.content_cursor = 0;
                self.refresh_entries()?;
            }
        }
        Ok(())
    }

    fn leave_directory(&mut self) -> Result<(), AppError> {
        self.status_message = None;
        if let Some(parent) = self.current_path.parent() {
            self.current_path = parent.to_path_buf();
            self.list_state.select(Some(0));
            self.focused_file = None;
            self.file_content.clear();
            self.content_cursor = 0;
            self.refresh_entries()?;
        }
        Ok(())
    }

    fn on_enter(&mut self) -> Result<(), AppError> {
        self.status_message = None;
        if let Some(entry) = self.get_selected_entry().cloned() {
            if entry.is_dir {
                self.enter_directory()?;
            } else {
                // When entering a file, load the first chunk and switch focus
                self.focused_file = Some(entry.clone());
                self.load_file_chunk(0);
                self.focus = Focus::FileContent;
            }
        }
        Ok(())
    }

    /// Loads a specific chunk of the file content
    fn load_file_chunk(&mut self, start_line: usize) {
        self.file_content.clear();
        self.content_cursor = start_line; // Update cursor regardless of success/fail

        if let Some(entry) = &self.get_selected_entry() {
            if entry.is_dir {
                self.file_content.clear();
                return;
            }

            match self.is_binary(&entry.path) {
                Ok(true) => {
                    self.file_content = vec![
                        "--- [ BINARY FILE CONTENT NOT DISPLAYED ] ---".to_string(),
                        "Content not displayed.".to_string(),
                        "Press 'd' to download.".to_string(),
                    ];
                }
                Ok(false) => {
                    match self.fs_ops.read_file_chunk(&entry.path, start_line, CHUNK_SIZE) {
                        Ok(lines) => {
                            self.file_content = lines;
                        }
                        Err(e) => {
                            self.file_content = vec![
                                "--- Error reading file ---".to_string(),
                                format!("{}", e)
                            ];
                        }
                    }
                }
                Err(e) => {
                    self.file_content = vec![format!("--- Error checking file type: {} ---", e)];
                }
            }
        }
    }

    fn scroll_content_up(&mut self) {
        // Only scroll if we are showing text content
        if self.file_content.is_empty() || self.file_content[0].contains("[ BINARY FILE") {
            return;
        }

        if self.content_cursor == 0 {
            return;
        }

        let new_cursor = self.content_cursor.saturating_sub(CHUNK_SIZE);
        self.load_file_chunk(new_cursor);
    }

    fn scroll_content_down(&mut self) {
        // Only scroll if we are showing text content
        if self.file_content.is_empty() || self.file_content[0].contains("[ BINARY FILE") {
            return;
        }

        // If the chunk we loaded is smaller than CHUNK_SIZE, we've reached EOF.
        if self.file_content.len() < CHUNK_SIZE {
            return;
        }

        let new_cursor = self.content_cursor.saturating_add(CHUNK_SIZE);
        self.load_file_chunk(new_cursor);
    }

    fn download_selected_file(&mut self) -> Result<(), AppError> {
        let entry = self.get_selected_entry().cloned().ok_or_else(|| {
            AppError::Navigation("No file selected for download.".to_string())
        })?;

        if entry.is_dir {
            self.status_message = Some("Cannot download a directory.".to_string());
            return Ok(());
        }

        let file_name_os = entry.path.file_name().ok_or_else(|| {
            AppError::Navigation("Cannot get file name.".to_string())
        })?;

        let original_file_name = file_name_os.to_string_lossy();

        let now: DateTime<Local> = Local::now();
        let timestamp = now.format("%Y%m%d%H%M%S").to_string();
        let server_name = self.get_server_name();

        // Ensure a unique, clear name for the downloaded file
        let prefixed_file_name = format!(
            "{}_{}_{}",
            timestamp,
            server_name,
            original_file_name
        );

        // Download always to a temporary directory in the local system
        let mut dest_path = PathBuf::from("/tmp");
        dest_path.push(prefixed_file_name);

        self.fs_ops.download_file(&entry, &dest_path)?;

        self.status_message = Some(format!(
            "Downloaded '{}' to {}",
            original_file_name,
            dest_path.to_string_lossy()
        ));
        Ok(())
    }
}

// ------------------------------------------------------------------
// 5. Main function (Handles Local Default or SFTP Remote)
// ------------------------------------------------------------------

fn main() -> Result<(), AppError> {
    let args: Vec<String> = env::args().collect();
    let fs_ops: Box<dyn FileSystemOperations>;

    // Check if arguments were provided
    if args.len() > 3 {
        // We expect: cargo run <user> <host[:port]> <password>
        let user = &args[1];
        let host_port = &args[2];
        let password = &args[3];

        println!("Connecting to {}@{} via SFTP...", user, host_port);
        fs_ops = Box::new(SftpFileSystem::new(user, host_port, password)?);
        println!("Connection successful! Starting TUI...");

    } else if args.len() > 1 {
        // If args were provided but not enough
        return Err(AppError::ArgError("To connect remotely, provide arguments: <user> <host[:port]> <password>".to_string()));
    }
    else {
        // DEFAULT: Use local file system
        println!("Starting with Local File System (Default).");
        fs_ops = Box::new(LocalFileSystem);
    }

    let mut terminal = setup_terminal()?;
    let mut app = App::new(fs_ops)?;

    loop {
        terminal.draw(|frame| ui(frame, &mut app))?;

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') => break,
                        // 's' switches focus between the file list and the content view
                        KeyCode::Char('s') => app.focus = match app.focus {
                            Focus::FileList => Focus::FileContent,
                            Focus::FileContent => Focus::FileList,
                        },
                        // 'd' downloads the currently selected file
                        KeyCode::Char('d') => app.download_selected_file()?,
                        _ => match app.focus {
                            Focus::FileList => match key.code {
                                KeyCode::Up | KeyCode::Char('k') => app.select_previous(),
                                KeyCode::Down | KeyCode::Char('j') => app.select_next(),
                                KeyCode::Left | KeyCode::Char('h') => app.leave_directory()?,
                                KeyCode::Right | KeyCode::Char('l') => app.enter_directory()?,
                                KeyCode::Enter => app.on_enter()?,
                                _ => {}
                            },
                            Focus::FileContent => match key.code {
                                // Optimized scrolling uses chunk size
                                KeyCode::Up | KeyCode::Char('k') => app.scroll_content_up(),
                                KeyCode::Down | KeyCode::Char('j') => app.scroll_content_down(),
                                // Quick page up/down for faster navigation
                                KeyCode::PageUp => {
                                    for _ in 0..10 { app.scroll_content_up(); }
                                }
                                KeyCode::PageDown => {
                                    for _ in 0..10 { app.scroll_content_down(); }
                                }
                                KeyCode::Esc | KeyCode::Enter => app.focus = Focus::FileList,
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }

    restore_terminal()?;
    Ok(())
}

// ------------------------------------------------------------------
// 6. UI and Terminal functions
// ------------------------------------------------------------------

fn setup_terminal() -> Result<Terminal<impl Backend>, AppError> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    stdout.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal() -> Result<(), AppError> {
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

fn format_entry(entry: &FileEntry) -> Line<'static> {
    let path = &entry.path;
    let name_os = path
        .file_name()
        .map_or_else(|| PathBuf::from(".."), |s| PathBuf::from(s.to_os_string()));

    let mut display_name = name_os.file_name().unwrap_or_default().to_string_lossy().to_string();

    if entry.is_dir {
        // Folder convention: (name)/
        display_name = format!("({})", display_name);
        display_name.push('/');
    }

    Line::from(Span::raw(display_name))
}

fn create_properties_section(props: &FileProperties, entry_name: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled("--- Properties for: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(entry_name.to_owned()),
            Span::raw(" ---"),
        ]),
        Line::from(Span::raw("")),
        Line::from(vec![
            Span::styled("Permissions: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!("{} (rwx for user/group/other)", props.type_and_permissions)),
        ]),
        Line::from(vec![
            Span::styled("Owner: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(props.user.clone()),
        ]),
        Line::from(vec![
            Span::styled("Group: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(props.group.clone()),
        ]),
        Line::from(vec![
            Span::styled("Size: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!("{} bytes", props.size)),
        ]),
        Line::from(vec![
            Span::styled("Modified: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(props.modified.clone()),
        ]),
        Line::from(Span::raw("")),
        Line::from(Span::styled("--- File/Folder Content ---", Style::default().add_modifier(Modifier::UNDERLINED))),
    ]
}

fn ui(frame: &mut Frame, app: &mut App) {
    let outer_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.size());

    let main_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Ratio(1, 2),
            Constraint::Ratio(1, 2),
        ])
        .split(outer_layout[1]);

    // --- Header (outer_layout[0]) ---
    let path_str = app.fs_ops.path_to_string(&app.current_path);
    let title_suffix = if app.fs_ops.as_any().downcast_ref::<SftpFileSystem>().is_some() {
        "Current Remote Path (SFTP)"
    } else {
        "Current Local Path (Default)"
    };

    let header =
        Paragraph::new(path_str).block(Block::default().borders(Borders::ALL).title(title_suffix));
    frame.render_widget(header, outer_layout[0]);

    // --- Body: File List (main_layout[0]) ---
    let items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|entry| ListItem::new(format_entry(entry)))
        .collect();

    let list_border_color = if matches!(app.focus, Focus::FileList) { Color::Yellow } else { Color::White };

    let list_title = "Entries";

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).border_style(list_border_color).title(list_title))
        .highlight_style(ratatui::prelude::Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol(">> ");

    frame.render_stateful_widget(list, main_layout[0], &mut app.list_state);

    // --- Body: File Content (main_layout[1]) ---
    let content_border_color = if matches!(app.focus, Focus::FileContent) { Color::Yellow } else { Color::White };

    let mut content_lines = Vec::new();
    let content_title;

    // 1. Add properties section
    if let Some(props) = &app.selected_properties {
        let entry_name = app.get_selected_entry()
            .and_then(|e| e.path.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "N/A".to_string());

        content_lines.extend(create_properties_section(props, &entry_name));

        content_title = if app.get_selected_entry().map(|e| e.is_dir).unwrap_or(false) {
            format!("Properties: {}", entry_name)
        } else {
            let file_display_name = app.get_selected_entry()
                .and_then(|e| e.path.file_name())
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();

            if app.file_content.is_empty() || app.file_content[0].contains("[ BINARY FILE") {
                 format!("File/Content: {}", file_display_name)
            } else {
                 format!("File/Content: {} (Line {})", file_display_name, app.content_cursor)
            }
        };
    } else {
        content_title = "File/Content (Select an item)".to_string();
    }

    // 2. Add file content (only present if it's a non-binary file)
    if !app.get_selected_entry().map(|e| e.is_dir).unwrap_or(true) {
        content_lines.extend(app.file_content.iter().map(|s| Line::from(Span::raw(s.clone()))));
    }

    if content_lines.is_empty() {
        content_lines.push(Line::from(Span::raw("Navigate to a directory or select a file/folder to view its properties.")));
    }


    let content_widget = Paragraph::new(Text::from(content_lines))
        .block(Block::default().borders(Borders::ALL).border_style(content_border_color).title(content_title))
        .wrap(ratatui::widgets::Wrap { trim: true });

    frame.render_widget(content_widget, main_layout[1]);


    // --- Status Bar (outer_layout[2]) ---
    let status_text = app.status_message.as_deref().unwrap_or("Keys: 'q' to quit, 's' to toggle focus, 'd' to download. UP/DOWN for quick file content scroll.");
    let status_bar = Paragraph::new(status_text)
        .style(ratatui::prelude::Style::default());
    frame.render_widget(status_bar, outer_layout[2]);
}



