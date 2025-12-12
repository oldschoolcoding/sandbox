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
    path::{Path, PathBuf, StripPrefixError},
    os::unix::fs::PermissionsExt,
};
use thiserror::Error;
use chrono::prelude::*;
use infer;
use users::{get_user_by_uid, get_group_by_gid};
use regex::Regex;
use zip::{ZipWriter, write::FileOptions};

const CHUNK_SIZE: usize = 500;

#[derive(Debug, Error)]
enum AppError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("SSH error: {0}")]
    Ssh(#[from] ssh2::Error),
    #[error("Regex error: {0}")]
    Regex(#[from] regex::Error),
    #[error("Zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("Path error: {0}")]
    StripPrefix(#[from] StripPrefixError),
    #[error("Navigation: {0}")]
    Navigation(String),
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

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

fn format_permissions(metadata: &std::fs::Metadata) -> String {
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode();
        let t = if metadata.is_dir() { 'd' } else { '-' };
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
        Ok(fs::read_dir(p)?
            .filter_map(Result::ok)
            .map(|e| FileEntry { path: e.path(), is_dir: e.path().is_dir() })
            .collect())
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
        let reader = BufReader::new(file);
        let mut lines = Vec::with_capacity(count);
        for (i, line) in reader.lines().enumerate() {
            if i < start { continue; }
            if i >= start + count { break; }
            lines.push(line?);
        }
        Ok(lines)
    }

    fn get_file_properties(&self, p: &Path) -> Result<FileProperties, AppError> {
        let m = fs::metadata(p)?;
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
            .and_then(|t| Some(DateTime::<Local>::from(t)))
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
    fn new(user: &str, host_port: &str, password: &str) -> Result<Self, AppError> {
        let addr = if host_port.contains(':') { host_port } else { &format!("{host_port}:22") };
        let tcp = TcpStream::connect(addr)?;
        let mut sess = Session::new()?;
        sess.set_tcp_stream(tcp);
        sess.handshake()?;
        sess.userauth_password(user, password)?;
        if !sess.authenticated() {
            return Err(AppError::Navigation("Authentication failed".into()));
        }
        let sftp = sess.sftp()?;
        let host = host_port.split(':').next().unwrap_or(host_port);
        Ok(Self { session: sess, sftp, remote_host: host.to_string() })
    }
}

fn format_permissions_sftp(stat: &FileStat) -> String {
    let mode = stat.perm.unwrap_or(0);
    let t = if stat.is_dir() { 'd' } else { '-' };
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
        Ok(self.sftp.readdir(p)?
            .into_iter()
            .map(|(p, s)| FileEntry { path: p, is_dir: s.is_dir() })
            .collect())
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
        let reader = BufReader::new(f);
        let mut lines = Vec::with_capacity(count);
        for (i, line) in reader.lines().enumerate() {
            if i < start { continue; }
            if i >= start + count { break; }
            lines.push(line?);
        }
        Ok(lines)
    }

    fn get_file_properties(&self, p: &Path) -> Result<FileProperties, AppError> {
        let s = self.sftp.stat(p)?;
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

#[derive(PartialEq)]
enum InputMode { Normal, Search }

#[derive(PartialEq)]
enum SearchType { Name, Content }

struct App {
    fs_ops: Box<dyn FileSystemOperations>,
    current_path: PathBuf,
    entries: Vec<FileEntry>,
    list_state: ListState,
    status_message: Option<String>,
    selected_properties: Option<FileProperties>,
    file_content: Vec<String>,
    content_cursor: usize,
    input_mode: InputMode,
    search_query: String,
    search_results: Vec<Line<'static>>,
    search_type: Option<SearchType>,
}

impl App {
    fn new(fs: Box<dyn FileSystemOperations>) -> Result<Self, AppError> {
        let mut app = Self {
            fs_ops: fs,
            current_path: PathBuf::new(),
            entries: vec![],
            list_state: ListState::default(),
            status_message: None,
            selected_properties: None,
            file_content: vec![],
            content_cursor: 0,
            input_mode: InputMode::Normal,
            search_query: String::new(),
            search_results: vec![],
            search_type: None,
        };
        app.current_path = app.fs_ops.get_start_path()?;
        app.refresh_entries()?;
        Ok(app)
    }

    fn get_server_name(&self) -> String {
        self.fs_ops.as_any()
            .downcast_ref::<SftpFileSystem>()
            .map(|s| s.remote_host.clone())
            .unwrap_or_else(|| "local".into())
    }

    fn refresh_entries(&mut self) -> Result<(), AppError> {
        self.entries = self.fs_ops.read_dir(&self.current_path)?;
        self.entries.sort_by(|a, b| {
            a.is_dir.cmp(&b.is_dir).reverse().then_with(|| {
                a.path.file_name().unwrap_or_default().to_string_lossy().to_lowercase()
                    .cmp(&b.path.file_name().unwrap_or_default().to_string_lossy().to_lowercase())
            })
        });
        if self.entries.is_empty() {
            self.list_state.select(None);
        } else {
            let i = self.list_state.selected().unwrap_or(0).min(self.entries.len() - 1);
            self.list_state.select(Some(i));
        }
        self.update_selection();
        Ok(())
    }

    fn update_selection(&mut self) {
        self.selected_properties = None;
        self.file_content.clear();
        if let Some(i) = self.list_state.selected() {
            if let Some(e) = self.entries.get(i) {
                self.selected_properties = self.fs_ops.get_file_properties(&e.path).ok();
                if !e.is_dir {
                    self.file_content = self.fs_ops.read_file_chunk(&e.path, 0, CHUNK_SIZE).unwrap_or_default();
                }
            }
        }
    }

    fn select_prev(&mut self) {
        let i = match self.list_state.selected() {
            Some(i) => if i == 0 { self.entries.len().saturating_sub(1) } else { i - 1 },
            None => 0,
        };
        self.list_state.select(Some(i));
        self.update_selection();
    }

    fn select_next(&mut self) {
        let i = match self.list_state.selected() {
            Some(i) => if i + 1 >= self.entries.len() { 0 } else { i + 1 },
            None => 0,
        };
        self.list_state.select(Some(i));
        self.update_selection();
    }

    fn enter_dir(&mut self) -> Result<(), AppError> {
        if let Some(i) = self.list_state.selected() {
            if self.entries[i].is_dir {
                self.current_path = self.entries[i].path.clone();
                self.refresh_entries()?;
            }
        }
        Ok(())
    }

    fn leave_dir(&mut self) -> Result<(), AppError> {
        if self.current_path.parent().is_some() {
            self.current_path.pop();
            self.refresh_entries()?;
        }
        Ok(())
    }

    fn download_zip(&mut self) -> Result<(), AppError> {
        let i = match self.list_state.selected() {
            Some(i) => i,
            None => return Ok(()),
        };
        let entry = &self.entries[i];
        let name = entry.path.file_name().unwrap().to_string_lossy();
        let ts = Local::now().format("%Y%m%d_%H%M%S");
        let zip_name = format!("{}_{}_{}.zip", self.get_server_name(), ts, name);
        let zip_path = PathBuf::from("/tmp").join(zip_name);

        let file = File::create(&zip_path)?;
        let mut zip = ZipWriter::new(file);

        if entry.is_dir {
            let base = entry.path.parent().unwrap_or(Path::new("/"));
            self.recursive_zip(&entry.path, base, &mut zip)?;
        } else {
            zip.start_file::<String, ()>(name.to_string(), FileOptions::default())?;
            io::copy(&mut self.fs_ops.open_file(&entry.path)?, &mut zip)?;
        }
        zip.finish()?;
        self.status_message = Some(format!("ZIP saved: {}", zip_path.display()));
        Ok(())
    }

    fn recursive_zip(&self, path: &Path, base: &Path, zip: &mut ZipWriter<File>) -> Result<(), AppError> {
        for e in self.fs_ops.read_dir(path)? {
            let rel = e.path.strip_prefix(base)?;
            let name = rel.to_str().ok_or(AppError::Navigation("non-utf8 path".into()))?;
            if e.is_dir {
                self.recursive_zip(&e.path, base, zip)?;
            } else {
                zip.start_file::<&str, ()>(name, FileOptions::default())?;
                io::copy(&mut self.fs_ops.open_file(&e.path)?, zip)?;
            }
        }
        Ok(())
    }

    fn start_search(&mut self, kind: SearchType) {
        self.input_mode = InputMode::Search;
        self.search_type = Some(kind);
        self.search_query.clear();
        self.search_results.clear();
    }

    fn perform_search(&mut self) -> Result<(), AppError> {
        if self.search_query.is_empty() {
            self.search_results.clear();
            return Ok(());
        }
        let re = Regex::new(&self.search_query)?;
        let mut results = vec![];
        let content = self.search_type == Some(SearchType::Content);
        self.search_recursive(&self.current_path, &re, content, &mut results)?;
        self.search_results = results;
        self.input_mode = InputMode::Normal;
        Ok(())
    }

    fn search_recursive(&self, path: &Path, re: &Regex, content: bool, out: &mut Vec<Line<'static>>) -> Result<(), AppError> {
        for e in self.fs_ops.read_dir(path)? {
            let name = e.path.file_name().unwrap().to_string_lossy();
            let full = self.fs_ops.path_to_string(&e.path);
            if e.is_dir {
                if !content && re.is_match(&name) {
                    out.push(Line::from(vec![Span::styled(format!("{full}/"), Style::default().fg(Color::Cyan))]));
                }
                self.search_recursive(&e.path, re, content, out)?;
            } else {
                if !content && re.is_match(&name) {
                    out.push(Line::from(full.clone()));
                }
                if content {
                    if let Ok(head) = self.fs_ops.read_file_head(&e.path) {
                        if infer::get(&head).map_or(true, |k| k.mime_type().starts_with("text")) {
                            let lines = self.fs_ops.read_file_chunk(&e.path, 0, 10000).unwrap_or_default();
                            for (i, line) in lines.iter().enumerate() {
                                if re.is_match(line) {
                                    let mut spans = vec![Span::raw(format!("{}:{:4}: ", full, i + 1))];
                                    let mut last = 0;
                                    for m in re.find_iter(line) {
                                        spans.push(Span::raw(line[last..m.start()].to_string()));
                                        spans.push(Span::styled(line[m.range()].to_string(), Style::default().fg(Color::Red)));
                                        last = m.end();
                                    }
                                    spans.push(Span::raw(line[last..].to_string()));
                                    out.push(Line::from(spans));
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn main() -> Result<(), AppError> {
    let args: Vec<String> = env::args().collect();
    let fs_ops: Box<dyn FileSystemOperations> = if args.len() == 4 {
        Box::new(SftpFileSystem::new(&args[1], &args[2], &args[3])?)
    } else {
        Box::new(LocalFileSystem)
    };

    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let mut app = App::new(fs_ops)?;

    loop {
        terminal.draw(|f| {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
                .split(f.area());

            let main = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Ratio(1,2), Constraint::Ratio(1,2)])
                .split(layout[1]);

            f.render_widget(
                Paragraph::new(app.fs_ops.path_to_string(&app.current_path))
                    .block(Block::default().borders(Borders::ALL).title(" Path ")),
                layout[0],
            );

            let items: Vec<ListItem> = app.entries.iter().map(|e| {
                let name = e.path.file_name().unwrap().to_string_lossy();
                ListItem::new(if e.is_dir { format!("({})", name) } else { name.into() })
            }).collect();

            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(" Files "))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

            f.render_stateful_widget(list, main[0], &mut app.list_state);

            let content_lines = if !app.search_results.is_empty() {
                app.search_results.clone()
            } else if let Some(p) = &app.selected_properties {
                let name = app.entries.get(app.list_state.selected().unwrap_or(0))
                    .and_then(|e| e.path.file_name())
                    .map(|n| n.to_string_lossy())
                    .unwrap_or("?".into());
                let mut v = vec![
                    Line::from(vec![Span::styled(format!("--- {} ---", name), Style::default().bold())]),
                    Line::from(format!("Perm : {}", p.type_and_permissions)),
                    Line::from(format!("Size : {} bytes", p.size)),
                    Line::from(format!("Mod  : {}", p.modified)),
                ];
                v.extend(app.file_content.iter().map(|l| Line::from(l.clone())));
                v
            } else {
                vec![Line::from("Select a file or folder")]
            };

            let title = if !app.search_results.is_empty() {
                format!("Search results for \"{}\"", app.search_query)
            } else {
                "Info".into()
            };

            f.render_widget(
                Paragraph::new(Text::from(content_lines))
                    .block(Block::default().borders(Borders::ALL).title(title)),
                main[1],
            );

            let status = if app.input_mode == InputMode::Search {
                let kind = if app.search_type == Some(SearchType::Content) { "content" } else { "name" };
                format!("Search ({}) {}:", kind, app.search_query)
            } else {
                app.status_message.clone().unwrap_or_else(|| "q=quit | d=download ZIP | f=search name | c=search content".into())
            };

            f.render_widget(Paragraph::new(status), layout[2]);
        })?;

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match app.input_mode {
                        InputMode::Normal => match key.code {
                            KeyCode::Char('q') => break,
                            KeyCode::Char('d') => { let _ = app.download_zip(); }
                            KeyCode::Char('f') => app.start_search(SearchType::Name),
                            KeyCode::Char('c') => app.start_search(SearchType::Content),
                            KeyCode::Up | KeyCode::Char('k') => app.select_prev(),
                            KeyCode::Down | KeyCode::Char('j') => app.select_next(),
                            KeyCode::Left | KeyCode::Char('h') => { let _ = app.leave_dir(); }
                            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => { let _ = app.enter_dir(); }
                            _ => {}
                        },
                        InputMode::Search => match key.code {
                            KeyCode::Char(c) => app.search_query.push(c),
                            KeyCode::Backspace => { app.search_query.pop(); }
                            KeyCode::Enter => { let _ = app.perform_search(); }
                            KeyCode::Esc => {
                                app.input_mode = InputMode::Normal;
                                app.search_query.clear();
                                app.search_results.clear();
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}
