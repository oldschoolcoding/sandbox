use std::os::unix::fs::PermissionsExt;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Row, Cell, Table, TableState},
};
use ssh2::{Session, Sftp};
use std::{
    any::Any,
    collections::HashMap,
    env,
    fs::{self, File},
    io::{self, stdout, BufRead, Read},
    net::TcpStream,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use thiserror::Error;
use chrono::prelude::*;
use infer;
use users::{get_user_by_uid, get_group_by_gid};
use regex::Regex;
use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use argon2::Argon2;
use argon2::password_hash::{rand_core::RngCore, SaltString};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

const CHUNK_SIZE: usize = 500;
const DEBOUNCE_MS: u64 = 150;
const MAX_CACHE_SIZE: usize = 50;
const CONNECTIONS_FILE: &str = "connections.enc";

// Constants for Unix file type masks
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;
const S_IFREG: u32 = 0o100000;

#[derive(Debug, Error)]
enum AppError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("SSH error: {0}")]
    Ssh(#[from] ssh2::Error),
    #[error("Regex error: {0}")]
    Regex(#[from] regex::Error),
    #[error("Navigation: {0}")]
    Navigation(String),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Encryption error: {0}")]
    Encryption(String),
    #[error("Password error: {0}")]
    Password(String),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct RemoteConnection {
    name: String,
    username: String,
    host: String,
    port: u16,
    password: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ConnectionsData {
    salt: String,
    nonce: Vec<u8>,
    encrypted_data: Vec<u8>,
}

struct ConnectionManager {
    connections: Vec<RemoteConnection>,
    file_path: PathBuf,
    master_password: String,
}

impl ConnectionManager {
    fn new() -> Result<Self, AppError> {
        let exe_path = env::current_exe()?;
        let exe_dir = exe_path.parent().ok_or_else(|| {
            AppError::Io(io::Error::new(io::ErrorKind::NotFound, "Cannot find exe directory"))
        })?;
        let file_path = exe_dir.join(CONNECTIONS_FILE);

        Ok(Self {
            connections: Vec::new(),
            file_path,
            master_password: String::new(),
        })
    }

    fn derive_key(&self, password: &str, salt: &str) -> Result<[u8; 32], AppError> {
        let argon2 = Argon2::default();
        let salt_bytes = salt.as_bytes();
        let mut key = [0u8; 32];
        
        argon2
            .hash_password_into(password.as_bytes(), salt_bytes, &mut key)
            .map_err(|e| AppError::Password(format!("Key derivation failed: {}", e)))?;
        
        Ok(key)
    }

    fn encrypt_connections(&self, connections: &[RemoteConnection], password: &str) -> Result<ConnectionsData, AppError> {
        let json = serde_json::to_string(connections)?;
        
        let salt = SaltString::generate(&mut OsRng);
        let key = self.derive_key(password, salt.as_str())?;
        
        let cipher = Aes256Gcm::new(key.as_ref().into());
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        
        let encrypted_data = cipher
            .encrypt(nonce, json.as_bytes())
            .map_err(|e| AppError::Encryption(format!("Encryption failed: {}", e)))?;
        
        Ok(ConnectionsData {
            salt: salt.as_str().to_string(),
            nonce: nonce_bytes.to_vec(),
            encrypted_data,
        })
    }

    fn decrypt_connections(&self, data: &ConnectionsData, password: &str) -> Result<Vec<RemoteConnection>, AppError> {
        let key = self.derive_key(password, &data.salt)?;
        
        let cipher = Aes256Gcm::new(key.as_ref().into());
        let nonce = Nonce::from_slice(&data.nonce);
        
        let decrypted = cipher
            .decrypt(nonce, data.encrypted_data.as_ref())
            .map_err(|e| AppError::Encryption(format!("Decryption failed (wrong password?): {}", e)))?;
        
        let json = String::from_utf8(decrypted)
            .map_err(|e| AppError::Encryption(format!("Invalid UTF-8: {}", e)))?;
        
        Ok(serde_json::from_str(&json)?)
    }

    fn load(&mut self, password: &str) -> Result<bool, AppError> {
        if !self.file_path.exists() {
            self.master_password = password.to_string();
            return Ok(false);
        }

        let contents = fs::read_to_string(&self.file_path)?;
        let data: ConnectionsData = serde_json::from_str(&contents)?;
        
        self.connections = self.decrypt_connections(&data, password)?;
        self.master_password = password.to_string();
        Ok(true)
    }

    fn save(&self) -> Result<(), AppError> {
        let data = self.encrypt_connections(&self.connections, &self.master_password)?;
        let json = serde_json::to_string_pretty(&data)?;
        fs::write(&self.file_path, json)?;
        Ok(())
    }

    fn add_connection(&mut self, conn: RemoteConnection) -> Result<(), AppError> {
        self.connections.push(conn);
        self.save()
    }

    fn delete_connection(&mut self, index: usize) -> Result<(), AppError> {
        if index < self.connections.len() {
            self.connections.remove(index);
            self.save()?;
        }
        Ok(())
    }

    fn get_connections(&self) -> &[RemoteConnection] {
        &self.connections
    }
}

#[derive(Clone, Debug)]
struct FileEntry {
    path: PathBuf,
    is_dir: bool,
}

#[derive(Clone, Debug)]
struct FileProperties {
    type_and_permissions: String,
    user: String,
    group: String,
    size: String,
    modified: String,
}

trait FileSystemOperations: Any {
    fn read_dir(&self, p: &Path) -> Result<Vec<FileEntry>, AppError>;
    fn get_start_path(&self) -> Result<PathBuf, AppError>;
    fn path_to_string(&self, p: &Path) -> String;
    fn open_file(&self, p: &Path) -> Result<Box<dyn Read>, AppError>;
    fn read_file_head(&self, p: &Path) -> Result<Vec<u8>, AppError>;
    fn read_file_chunk(&self, p: &Path, s: usize, c: usize) -> Result<Vec<String>, AppError>;
    fn get_file_properties(&self, p: &Path) -> Result<FileProperties, AppError>;
    fn as_any(&self) -> &dyn Any;
}

struct LocalFileSystem;

fn format_permissions(metadata: &std::fs::Metadata) -> String {
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode();
        let file_type = metadata.mode() & S_IFMT;
        let t = if file_type == S_IFLNK {
            'l'
        } else if metadata.is_dir() {
            'd'
        } else {
            '-'
        };
        let r = |mask| if mode & mask != 0 { 'r' } else { '-' };
        let w = |mask| if mode & mask != 0 { 'w' } else { '-' };
        let x = |mask| if mode & mask != 0 { 'x' } else { '-' };
        format!(
            "{}{}{}{}{}{}{}{}{}{}",
            t,
            r(0o400), w(0o200), x(0o100),
            r(0o040), w(0o020), x(0o010),
            r(0o004), w(0o002), x(0o001)
        )
    }
    #[cfg(not(unix))]
    {
        "----------".into()
    }
}

impl FileSystemOperations for LocalFileSystem {
    fn read_dir(&self, p: &Path) -> Result<Vec<FileEntry>, AppError> {
        let mut entries: Vec<_> = fs::read_dir(p)?
            .filter_map(|res| res.ok())
            .map(|e| {
                let path = e.path();
                let is_dir = path.is_dir();
                FileEntry { path, is_dir }
            })
            .collect();
        entries.sort_by(|a, b| {
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                a.path.file_name().unwrap_or_default()
                    .to_string_lossy().to_lowercase()
                    .cmp(&b.path.file_name().unwrap_or_default().to_string_lossy().to_lowercase())
            })
        });
        Ok(entries)
    }

    fn get_start_path(&self) -> Result<PathBuf, AppError> {
        Ok(env::current_dir()?)
    }

    fn path_to_string(&self, p: &Path) -> String {
        p.display().to_string()
    }

    fn open_file(&self, p: &Path) -> Result<Box<dyn Read>, AppError> {
        Ok(Box::new(File::open(p)?))
    }

    fn read_file_head(&self, p: &Path) -> Result<Vec<u8>, AppError> {
        let mut f = File::open(p)?;
        let mut buf = vec![0; 512];
        let n = f.read(&mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn read_file_chunk(&self, p: &Path, start: usize, count: usize) -> Result<Vec<String>, AppError> {
        let file = File::open(p)?;
        let reader = std::io::BufReader::new(file);
        let mut lines = Vec::with_capacity(count);
        for (i, line) in reader.lines().enumerate() {
            if i < start { continue; }
            if i >= start + count { break; }
            lines.push(line?);
        }
        Ok(lines)
    }

    fn get_file_properties(&self, p: &Path) -> Result<FileProperties, AppError> {
        let m = fs::symlink_metadata(p)?;
        let perm = format_permissions(&m);
        let user = if cfg!(unix) {
            get_user_by_uid(m.uid())
                .map(|u| u.name().to_string_lossy().into_owned())
                .unwrap_or_else(|| m.uid().to_string())
        } else {
            "user".into()
        };
        let group = if cfg!(unix) {
            get_group_by_gid(m.gid())
                .map(|g| g.name().to_string_lossy().into_owned())
                .unwrap_or_else(|| m.gid().to_string())
        } else {
            "group".into()
        };
        let modified = m.modified()
            .ok()
            .map(|t| DateTime::<Local>::from(t))
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or("N/A".into());

        Ok(FileProperties {
            type_and_permissions: perm,
            user,
            group,
            size: m.len().to_string(),
            modified,
        })
    }

    fn as_any(&self) -> &dyn Any { self }
}

struct SftpFileSystem {
    session: Session,
    sftp: Sftp,
    remote_host: String,
}

impl SftpFileSystem {
    fn new(user: &str, host: &str, port: u16, password: &str) -> Result<Self, AppError> {
        let addr = format!("{}:{}", host, port);
        let tcp = TcpStream::connect(&addr)?;
        let mut sess = Session::new()?;
        sess.set_tcp_stream(tcp);
        sess.handshake()?;
        sess.userauth_password(user, password)?;
        if !sess.authenticated() {
            return Err(AppError::Navigation("Authentication failed".into()));
        }
        let sftp = sess.sftp()?;
        Ok(Self { session: sess, sftp, remote_host: host.to_string() })
    }
}

fn format_permissions_sftp(stat: &ssh2::FileStat) -> String {
    let mode = stat.perm.unwrap_or(0);
    let file_type = mode & S_IFMT;
    let t = if file_type == S_IFLNK {
        'l'
    } else if file_type == S_IFDIR {
        'd'
    } else {
        '-'
    };
    let r = |mask| if mode & mask != 0 { 'r' } else { '-' };
    let w = |mask| if mode & mask != 0 { 'w' } else { '-' };
    let x = |mask| if mode & mask != 0 { 'x' } else { '-' };
    format!(
        "{t}{}{}{}{}{}{}{}{}{}",
        r(0o400), w(0o200), x(0o100),
        r(0o040), w(0o020), x(0o010),
        r(0o004), w(0o002), x(0o001)
    )
}

impl FileSystemOperations for SftpFileSystem {
    fn read_dir(&self, p: &Path) -> Result<Vec<FileEntry>, AppError> {
        let mut entries = vec![];
        for (path, stat) in self.sftp.readdir(p)? {
            let mode = stat.perm.unwrap_or(0);
            let file_type = mode & S_IFMT;
            let is_dir = file_type == S_IFDIR;
            entries.push(FileEntry { path, is_dir });
        }
        entries.sort_by(|a, b| {
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                a.path.file_name().unwrap_or_default()
                    .to_string_lossy().to_lowercase()
                    .cmp(&b.path.file_name().unwrap_or_default().to_string_lossy().to_lowercase())
            })
        });
        Ok(entries)
    }

    fn get_start_path(&self) -> Result<PathBuf, AppError> {
        Ok(self.sftp.realpath(Path::new("."))?)
    }

    fn path_to_string(&self, p: &Path) -> String {
        format!("[{}]:{}", self.remote_host, p.display())
    }

    fn open_file(&self, p: &Path) -> Result<Box<dyn Read>, AppError> {
        Ok(Box::new(self.sftp.open(p)?))
    }

    fn read_file_head(&self, p: &Path) -> Result<Vec<u8>, AppError> {
        let mut f = self.sftp.open(p)?;
        let mut buf = vec![0; 512];
        let n = f.read(&mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn read_file_chunk(&self, p: &Path, start: usize, count: usize) -> Result<Vec<String>, AppError> {
        let f = self.sftp.open(p)?;
        let reader = std::io::BufReader::new(f);
        let mut lines = Vec::with_capacity(count);
        for (i, line) in reader.lines().enumerate() {
            if i < start { continue; }
            if i >= start + count { break; }
            lines.push(line?);
        }
        Ok(lines)
    }

    fn get_file_properties(&self, p: &Path) -> Result<FileProperties, AppError> {
        let s = self.sftp.lstat(p)?;
        let perm = format_permissions_sftp(&s);
        let modified = s.mtime
            .map(|t| Local.timestamp_opt(t as i64, 0).single().unwrap().format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or("N/A".into());
        Ok(FileProperties {
            type_and_permissions: perm,
            user: s.uid.unwrap_or(0).to_string(),
            group: s.gid.unwrap_or(0).to_string(),
            size: s.size.unwrap_or(0).to_string(),
            modified,
        })
    }

    fn as_any(&self) -> &dyn Any { self }
}

#[derive(PartialEq, Clone, Copy)]
enum SearchKind { Name, Content }

#[derive(Clone, Copy)]
struct SearchConfig {
    kind: SearchKind,
    max_depth: usize,
}

#[derive(PartialEq)]
enum InputMode { 
    Normal, 
    Search, 
    Password,
    ConnectionList,
    AddConnection(AddConnectionStep),
}

#[derive(PartialEq, Clone, Copy)]
enum AddConnectionStep {
    Name,
    Username,
    Host,
    Port,
    Password,
}

struct App {
    fs_ops: Box<dyn FileSystemOperations>,
    current_path: PathBuf,
    entries: Vec<FileEntry>,
    table_state: TableState,
    search_list_state: ListState,
    status_message: Option<String>,
    file_content: Vec<String>,
    input_mode: InputMode,
    search_query: String,
    search_results: Vec<Line<'static>>,
    search_config: Option<SearchConfig>,
    content_cache: HashMap<PathBuf, Vec<String>>,
    last_selection_time: Instant,
    pending_selection: Option<usize>,
    needs_content_update: bool,
    // Connection management
    connection_manager: ConnectionManager,
    connection_list_state: ListState,
    password_input: String,
    new_connection: RemoteConnection,
}

impl App {
    fn new_local() -> Result<Self, AppError> {
        Self::new(Box::new(LocalFileSystem))
    }

    fn new(fs: Box<dyn FileSystemOperations>) -> Result<Self, AppError> {
        let mut app = Self {
            fs_ops: fs,
            current_path: PathBuf::new(),
            entries: vec![],
            table_state: TableState::default(),
            search_list_state: ListState::default(),
            status_message: None,
            file_content: vec![],
            input_mode: InputMode::Normal,
            search_query: String::new(),
            search_results: vec![],
            search_config: None,
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

    fn select_prev(&mut self) {
        let i = match self.table_state.selected() {
            Some(i) => if i == 0 { self.entries.len().saturating_sub(1) } else { i - 1 },
            None => 0,
        };
        self.table_state.select(Some(i));
        self.mark_content_dirty();
    }

    fn select_next(&mut self) {
        let i = match self.table_state.selected() {
            Some(i) => if i + 1 >= self.entries.len() { 0 } else { i + 1 },
            None => 0,
        };
        self.table_state.select(Some(i));
        self.mark_content_dirty();
    }

    fn enter_dir(&mut self) -> Result<(), AppError> {
        if let Some(i) = self.table_state.selected() {
            if self.entries[i].is_dir {
                self.current_path = self.entries[i].path.clone();
                self.refresh_entries()?;
                self.clear_search();
            }
        }
        Ok(())
    }

    fn leave_dir(&mut self) -> Result<(), AppError> {
        if self.current_path.pop() {
            self.refresh_entries()?;
            self.clear_search();
        }
        Ok(())
    }

    fn clear_search(&mut self) {
        self.search_results.clear();
        self.search_list_state.select(None);
    }

    fn start_search(&mut self, kind: SearchKind, max_depth: usize) {
        self.input_mode = InputMode::Search;
        self.search_config = Some(SearchConfig { kind, max_depth });
        self.search_query.clear();
        self.clear_search();
    }

    fn check_interrupt(&self) -> Result<(), AppError> {
        if event::poll(std::time::Duration::from_millis(0))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press && k.code == KeyCode::Esc {
                    return Err(AppError::Navigation("Search interrupted".into()));
                }
            }
        }
        Ok(())
    }

    fn add_name_match(&self, out: &mut Vec<Line<'static>>, full: &str, is_dir: bool) {
        let path_display = if is_dir {
            format!("{full}/")
        } else {
            full.to_owned()
        };
        let styled = if is_dir {
            Span::styled(path_display, Style::default().fg(Color::Cyan))
        } else {
            Span::raw(path_display)
        };
        out.push(Line::from(vec![styled]));
    }

    fn add_content_match(&self, out: &mut Vec<Line<'static>>, full: &str, line_num: usize, line_text: &str, re: &Regex) {
        let prefix = format!("{}:{:4}: ", full, line_num + 1);
        let mut spans = vec![Span::raw(prefix)];

        let mut last = 0;
        for m in re.find_iter(line_text) {
            spans.push(Span::raw(line_text[last..m.start()].to_owned()));
            spans.push(Span::styled(line_text[m.range()].to_owned(), Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)));
            last = m.end();
        }
        spans.push(Span::raw(line_text[last..].to_owned()));

        out.push(Line::from(spans));
    }

    fn search_at_depth(&self, path: &Path, re: &Regex, content_search: bool, out: &mut Vec<Line<'static>>, depth: usize, max_depth: usize) -> Result<(), AppError> {
        if depth > max_depth { return Ok(()); }

        for e in self.fs_ops.read_dir(path)? {
            self.check_interrupt()?;
            let name = e.path.file_name().unwrap().to_string_lossy();
            let full = self.fs_ops.path_to_string(&e.path);

            if e.is_dir {
                if re.is_match(&name) {
                    self.add_name_match(out, &full, true);
                }
                if depth < max_depth {
                    self.search_at_depth(&e.path, re, content_search, out, depth + 1, max_depth)?;
                }
            } else {
                if !content_search && re.is_match(&name) {
                    self.add_name_match(out, &full, false);
                }

                if content_search {
                    if let Ok(head) = self.fs_ops.read_file_head(&e.path) {
                        if infer::get(&head).map_or(true, |k| k.mime_type().starts_with("text/")) {
                            let lines = self.fs_ops.read_file_chunk(&e.path, 0, 10000).unwrap_or_default();
                            for (i, line) in lines.iter().enumerate() {
                                if re.is_match(line) {
                                    self.add_content_match(out, &full, i, line, re);
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn perform_search(&mut self) -> Result<(), AppError> {
        if self.search_query.is_empty() {
            self.clear_search();
            self.input_mode = InputMode::Normal;
            return Ok(());
        }

        let re = match Regex::new(&self.search_query) {
            Ok(re) => re,
            Err(e) => {
                self.status_message = Some(format!("Invalid regex: {}", e));
                self.input_mode = InputMode::Normal;
                self.clear_search();
                return Ok(());
            }
        };
        let config = self.search_config.expect("search config missing");
        let content_search = config.kind == SearchKind::Content;
        let mut results = vec![];

        match self.search_at_depth(&self.current_path, &re, content_search, &mut results, 0, config.max_depth) {
            Ok(()) => {
                self.search_results = results;
                self.search_list_state.select(if self.search_results.is_empty() { None } else { Some(0) });
                self.input_mode = InputMode::Normal;
                Ok(())
            }
            Err(e) => {
                if let AppError::Navigation(ref msg) = e {
                    if msg == "Search interrupted" {
                        self.status_message = Some("Search interrupted".into());
                        self.input_mode = InputMode::Normal;
                        return Ok(());
                    }
                }
                Err(e)
            }
        }
    }

    fn navigate_search(&mut self, down: bool) {
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

    fn start_add_connection(&mut self) {
        self.new_connection = RemoteConnection {
            name: String::new(),
            username: String::new(),
            host: String::new(),
            port: 22,
            password: String::new(),
        };
        self.input_mode = InputMode::AddConnection(AddConnectionStep::Name);
    }

    fn connect_to_selected(&mut self) -> Result<(), AppError> {
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

    fn delete_selected_connection(&mut self) -> Result<(), AppError> {
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

    fn navigate_connection_list(&mut self, down: bool) {
        let connections = self.connection_manager.get_connections();
        if connections.is_empty() { return; }
        let len = connections.len();
        let i = match self.connection_list_state.selected() {
            Some(i) => if down { (i + 1) % len } else { if i == 0 { len - 1 } else { i - 1 } },
            None => if down { 0 } else { len - 1 },
        };
        self.connection_list_state.select(Some(i));
    }
}

fn draw_connection_list(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(area);

    let connections = app.connection_manager.get_connections();
    let items: Vec<ListItem> = connections
        .iter()
        .map(|c| {
            let line = Line::from(vec![
                Span::styled(format!("{:20}", c.name), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(format!("{}@{}:{}", c.username, c.host, c.port), Style::default().fg(Color::Yellow)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default()
            .borders(Borders::ALL)
            .title(" Saved Connections (Enter: connect | d: delete | a: add | Esc: cancel) "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD));

    f.render_stateful_widget(list, chunks[0], &mut app.connection_list_state);

    let help = Paragraph::new("↑↓: navigate | Enter: connect | d: delete | a: add new | Esc: cancel")
        .style(Style::default().bg(Color::DarkGray));
    f.render_widget(help, chunks[1]);
}

fn draw_add_connection(f: &mut Frame, app: &App, area: Rect, step: AddConnectionStep) {
    let text = match step {
        AddConnectionStep::Name => {
            format!("Connection Name: {}_", app.new_connection.name)
        }
        AddConnectionStep::Username => {
            format!("Username: {}_", app.new_connection.username)
        }
        AddConnectionStep::Host => {
            format!("Host: {}_", app.new_connection.host)
        }
        AddConnectionStep::Port => {
            format!("Port: {}_", app.new_connection.port)
        }
        AddConnectionStep::Password => {
            format!("Password: {}_", "*".repeat(app.new_connection.password.len()))
        }
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Add New Connection (Enter: next | Esc: cancel) ");

    let para = Paragraph::new(text).block(block);
    f.render_widget(para, area);
}

fn draw_password_prompt(f: &mut Frame, app: &App, area: Rect) {
    let text = format!("Master Password: {}_", "*".repeat(app.password_input.len()));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Enter Master Password ");
    let para = Paragraph::new(text).block(block);
    f.render_widget(para, area);
}

fn main() -> Result<(), AppError> {
    let mut app = App::new_local()?;
    
    // Prompt for master password to load connections
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    
    app.input_mode = InputMode::Password;
    
    loop {
        terminal.draw(|f| {
            let area = f.size();
            let centered = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Percentage(40),
                    Constraint::Length(3),
                    Constraint::Percentage(40),
                ])
                .split(area);
            
            let h_centered = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(30),
                    Constraint::Percentage(40),
                    Constraint::Percentage(30),
                ])
                .split(centered[1]);
            
            draw_password_prompt(f, &app, h_centered[1]);
        })?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press { continue; }
            
            match key.code {
                KeyCode::Char(c) => app.password_input.push(c),
                KeyCode::Backspace => { app.password_input.pop(); }
                KeyCode::Enter => {
                    match app.connection_manager.load(&app.password_input) {
                        Ok(_) => {
                            app.input_mode = InputMode::Normal;
                            break;
                        }
                        Err(e) => {
                            app.status_message = Some(format!("Error: {}", e));
                            app.password_input.clear();
                        }
                    }
                }
                KeyCode::Esc => {
                    app.input_mode = InputMode::Normal;
                    break;
                }
                _ => {}
            }
        }
    }

    app.password_input.clear();

    // Main loop
    loop {
        app.update_content_if_needed();

        terminal.draw(|f| {
            match app.input_mode {
                InputMode::ConnectionList => {
                    draw_connection_list(f, &mut app, f.size());
                }
                InputMode::AddConnection(step) => {
                    let area = f.size();
                    let centered = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([
                            Constraint::Percentage(40),
                            Constraint::Length(5),
                            Constraint::Percentage(40),
                        ])
                        .split(area);
                    
                    let h_centered = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(25),
                            Constraint::Percentage(50),
                            Constraint::Percentage(25),
                        ])
                        .split(centered[1]);
                    
                    draw_add_connection(f, &app, h_centered[1], step);
                }
                _ => {
                    let layout = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)])
                        .split(f.size());

                    let main = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                        .split(layout[1]);

                    f.render_widget(
                        Paragraph::new(app.fs_ops.path_to_string(&app.current_path))
                            .block(Block::default().borders(Borders::ALL).title(" Directory ")),
                        layout[0],
                    );

                    let header = ["Permissions", "User", "Group", "Size", "Modified", "Name"]
                        .into_iter()
                        .map(Cell::from)
                        .collect::<Row<'static>>()
                        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
                        .height(1);

                    let rows = app.entries.iter().enumerate().map(|(i, e)| {
                        let name = e.path.file_name().unwrap_or_default().to_string_lossy();
                        let display_name = if e.is_dir { format!("{}/", name) } else { name.to_string() };

                        let props = app.fs_ops.get_file_properties(&e.path).ok();

                        let perms = props.as_ref().map(|p| p.type_and_permissions.clone()).unwrap_or_else(|| "-".to_string());
                        let user = props.as_ref().map(|p| p.user.clone()).unwrap_or_else(|| "-".to_string());
                        let group = props.as_ref().map(|p| p.group.clone()).unwrap_or_else(|| "-".to_string());
                        let size = props.as_ref().map(|p| p.size.clone()).unwrap_or_else(|| "-".to_string());
                        let mod_time = props.as_ref().map(|p| p.modified.clone()).unwrap_or_else(|| "-".to_string());

                        let name_style = if e.is_dir {
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };

                        let selected_style = if Some(i) == app.table_state.selected() {
                            Style::default().add_modifier(Modifier::REVERSED)
                        } else {
                            Style::default()
                        };

                        Row::new(vec![
                            Cell::from(perms),
                            Cell::from(user),
                            Cell::from(group),
                            Cell::from(size),
                            Cell::from(mod_time),
                            Cell::from(Span::styled(display_name, name_style)),
                        ])
                        .style(selected_style)
                    });

                    let widths = [
                        Constraint::Length(11),
                        Constraint::Length(10),
                        Constraint::Length(10),
                        Constraint::Length(12),
                        Constraint::Length(16),
                        Constraint::Fill(1),
                    ];

                    let table = Table::new(rows, widths)
                        .header(header)
                        .block(Block::default().borders(Borders::ALL).title(" Files "))
                        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                        .column_spacing(1);

                    f.render_stateful_widget(table, main[0], &mut app.table_state);

                    if !app.search_results.is_empty() {
                        let result_items: Vec<ListItem> = app.search_results.iter()
                            .map(|line| ListItem::new(line.clone()))
                            .collect();

                        let result_list = List::new(result_items)
                            .block(Block::default().borders(Borders::ALL)
                                .title(format!(" Search Results ({} matches) ", app.search_results.len())))
                            .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD));

                        f.render_stateful_widget(result_list, main[1], &mut app.search_list_state);
                    } else {
                        let content_lines = if app.file_content.is_empty() {
                            vec![Line::from("Select a file to preview or a directory for info")]
                        } else {
                            let selected_name = app.entries.get(app.table_state.selected().unwrap_or(0))
                                .and_then(|e| e.path.file_name())
                                .map(|n| n.to_string_lossy())
                                .unwrap_or("?".into());

                            let mut v = vec![
                                Line::from(vec![Span::styled(format!(" Preview: {} ", selected_name), Style::default().bold().fg(Color::Green))]),
                                Line::from(""),
                            ];
                            v.extend(app.file_content.iter().map(|l| Line::from(l.clone())));
                            v
                        };

                        f.render_widget(
                            Paragraph::new(Text::from(content_lines))
                                .block(Block::default().borders(Borders::ALL).title(" Preview ")),
                            main[1],
                        );
                    }

                    let status = if app.input_mode == InputMode::Search {
                        let cfg = app.search_config.unwrap();
                        let kind = if cfg.kind == SearchKind::Content { "content" } else { "name" };
                        let depth = if cfg.max_depth == usize::MAX { "unlimited" } else { &format!("depth {}", cfg.max_depth) };
                        format!("SEARCH ({} | {}): {}", kind, depth, app.search_query)
                    } else if !app.search_results.is_empty() {
                        "j/k ↑↓: navigate results | Enter: jump to | Esc: clear results | r: remote connections | q: quit".into()
                    } else if let Some(msg) = &app.status_message {
                        msg.clone()
                    } else {
                        "h: back | l/Enter: enter | j/k ↑↓: select | r: remote connections | /: search name | c: search content | q: quit".into()
                    };

                    f.render_widget(Paragraph::new(status).style(Style::default().bg(Color::DarkGray)), layout[2]);
                }
            }
        })?;

        if event::poll(std::time::Duration::from_millis(16))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press { continue; }

                match &app.input_mode {
                    InputMode::Normal => match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Char('r') => app.show_connection_list(),
                        KeyCode::Char('h') => { let _ = app.leave_dir(); }
                        KeyCode::Char('l') | KeyCode::Enter => { let _ = app.enter_dir(); }
                        KeyCode::Char('/') => app.start_search(SearchKind::Name, usize::MAX),
                        KeyCode::Char('c') => app.start_search(SearchKind::Content, usize::MAX),
                        KeyCode::Char(c) if ('0'..='9').contains(&c) => {
                            let depth = c.to_digit(10).unwrap() as usize;
                            app.start_search(SearchKind::Name, depth);
                        }
                        KeyCode::Char(c) => {
                            let symbols = ")!@#$%^&*(";
                            if let Some(pos) = symbols.chars().position(|s| s == c) {
                                let depth = pos;
                                app.start_search(SearchKind::Content, depth);
                            }
                        }
                        _ => {
                            if !app.search_results.is_empty() {
                                match key.code {
                                    KeyCode::Char('j') | KeyCode::Down => app.navigate_search(true),
                                    KeyCode::Char('k') | KeyCode::Up => app.navigate_search(false),
                                    KeyCode::Enter => { let _ = app.jump_to_selected_result(); }
                                    KeyCode::Esc => app.clear_search(),
                                    _ => {}
                                }
                            } else {
                                match key.code {
                                    KeyCode::Char('j') | KeyCode::Down => app.select_next(),
                                    KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
                                    _ => {}
                                }
                            }
                        }
                    },
                    InputMode::Search => match key.code {
                        KeyCode::Char(c) => app.search_query.push(c),
                        KeyCode::Backspace => { app.search_query.pop(); }
                        KeyCode::Enter => { let _ = app.perform_search(); }
                        KeyCode::Esc => {
                            app.input_mode = InputMode::Normal;
                            app.search_query.clear();
                            app.clear_search();
                        }
                        _ => {}
                    },
                    InputMode::ConnectionList => match key.code {
                        KeyCode::Char('j') | KeyCode::Down => app.navigate_connection_list(true),
                        KeyCode::Char('k') | KeyCode::Up => app.navigate_connection_list(false),
                        KeyCode::Enter => { let _ = app.connect_to_selected(); }
                        KeyCode::Char('d') => { let _ = app.delete_selected_connection(); }
                        KeyCode::Char('a') => app.start_add_connection(),
                        KeyCode::Esc => app.input_mode = InputMode::Normal,
                        _ => {}
                    },
                    InputMode::AddConnection(step) => {
                        let current_step = *step;
                        match key.code {
                            KeyCode::Char(c) => {
                                match current_step {
                                    AddConnectionStep::Name => app.new_connection.name.push(c),
                                    AddConnectionStep::Username => app.new_connection.username.push(c),
                                    AddConnectionStep::Host => app.new_connection.host.push(c),
                                    AddConnectionStep::Port => {
                                        if c.is_ascii_digit() {
                                            let mut port_str = app.new_connection.port.to_string();
                                            port_str.push(c);
                                            if let Ok(port) = port_str.parse::<u16>() {
                                                app.new_connection.port = port;
                                            }
                                        }
                                    }
                                    AddConnectionStep::Password => app.new_connection.password.push(c),
                                }
                            }
                            KeyCode::Backspace => {
                                match current_step {
                                    AddConnectionStep::Name => { app.new_connection.name.pop(); }
                                    AddConnectionStep::Username => { app.new_connection.username.pop(); }
                                    AddConnectionStep::Host => { app.new_connection.host.pop(); }
                                    AddConnectionStep::Port => {
                                        let mut port_str = app.new_connection.port.to_string();
                                        port_str.pop();
                                        app.new_connection.port = port_str.parse().unwrap_or(22);
                                    }
                                    AddConnectionStep::Password => { app.new_connection.password.pop(); }
                                }
                            }
                            KeyCode::Enter => {
                                match current_step {
                                    AddConnectionStep::Name => {
                                        app.input_mode = InputMode::AddConnection(AddConnectionStep::Username);
                                    }
                                    AddConnectionStep::Username => {
                                        app.input_mode = InputMode::AddConnection(AddConnectionStep::Host);
                                    }
                                    AddConnectionStep::Host => {
                                        app.input_mode = InputMode::AddConnection(AddConnectionStep::Port);
                                    }
                                    AddConnectionStep::Port => {
                                        app.input_mode = InputMode::AddConnection(AddConnectionStep::Password);
                                    }
                                    AddConnectionStep::Password => {
                                        if let Err(e) = app.connection_manager.add_connection(app.new_connection.clone()) {
                                            app.status_message = Some(format!("Failed to save: {}", e));
                                        } else {
                                            app.status_message = Some("Connection saved!".into());
                                        }
                                        app.input_mode = InputMode::ConnectionList;
                                    }
                                }
                            }
                            KeyCode::Esc => {
                                app.input_mode = InputMode::ConnectionList;
                            }
                            _ => {}
                        }
                    },
                    InputMode::Password => {}
                }
            }
        }
    }

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}
