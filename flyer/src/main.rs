 use std::os::unix::fs::PermissionsExt;
 use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use ssh2::{Session, Sftp};
use std::{
    any::Any,
    env,
    fs::{self, File},
    io::{self, stdout, BufRead, Read},
    net::TcpStream,
    path::{Path, PathBuf},
};
use thiserror::Error;
use chrono::prelude::*;
use infer;
use users::{get_user_by_uid, get_group_by_gid};
use regex::Regex;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt; // ← This was missing!

const CHUNK_SIZE: usize = 500;

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
            .map(|t| DateTime::<Local>::from(t)) // ← Fixed: removed unnecessary and_then
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

fn format_permissions_sftp(stat: &ssh2::FileStat) -> String {
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

#[derive(PartialEq, Clone, Copy)]
enum SearchKind { Name, Content }

#[derive(Clone, Copy)]
struct SearchConfig {
    kind: SearchKind,
    max_depth: usize,
}

#[derive(PartialEq)]
enum InputMode { Normal, Search }

struct App {
    fs_ops: Box<dyn FileSystemOperations>,
    current_path: PathBuf,
    entries: Vec<FileEntry>,
    list_state: ListState,
    search_list_state: ListState,
    status_message: Option<String>,
    selected_properties: Option<FileProperties>,
    file_content: Vec<String>,
    input_mode: InputMode,
    search_query: String,
    search_results: Vec<Line<'static>>,
    search_config: Option<SearchConfig>,
}

impl App {
    fn new(fs: Box<dyn FileSystemOperations>) -> Result<Self, AppError> {
        let mut app = Self {
            fs_ops: fs,
            current_path: PathBuf::new(),
            entries: vec![],
            list_state: ListState::default(),
            search_list_state: ListState::default(),
            status_message: None,
            selected_properties: None,
            file_content: vec![],
            input_mode: InputMode::Normal,
            search_query: String::new(),
            search_results: vec![],
            search_config: None,
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
            a.is_dir.cmp(&b.is_dir).reverse().then_with(|| a.path.file_name().unwrap_or_default()
                .to_string_lossy().to_lowercase()
                .cmp(&b.path.file_name().unwrap_or_default().to_string_lossy().to_lowercase()))
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

    fn search_at_depth(&self, path: &Path, re: &Regex, content: bool, out: &mut Vec<Line<'static>>, depth: usize, max_depth: usize) -> Result<(), AppError> {
        if depth > max_depth { return Ok(()); }

        for e in self.fs_ops.read_dir(path)? {
            self.check_interrupt()?;
            let name = e.path.file_name().unwrap().to_string_lossy();
            let full = self.fs_ops.path_to_string(&e.path);

            if e.is_dir {
                if !content && re.is_match(&name) {
                    out.push(Line::from(vec![Span::styled(format!("{full}/"), Style::default().fg(Color::Cyan))]));
                }
                if depth < max_depth {
                    self.search_at_depth(&e.path, re, content, out, depth + 1, max_depth)?;
                }
            } else {
                if !content && re.is_match(&name) {
                    out.push(Line::from(full.clone()));
                }
                if content {
                    if let Ok(head) = self.fs_ops.read_file_head(&e.path) {
                        if infer::get(&head).map_or(true, |k| k.mime_type().starts_with("text")) {
                            let file_lines = self.fs_ops.read_file_chunk(&e.path, 0, 10000).unwrap_or_default();
                            for (i, line_str) in file_lines.iter().enumerate() {
                                if re.is_match(line_str) {
                                    // Allocate owned strings to satisfy 'static
                                    let full_owned = full.clone();
                                    let line_owned = line_str.clone();
                                    let prefix = format!("{}:{:4}: ", full_owned, i + 1);

                                    let mut spans = vec![Span::raw(prefix)];
                                    let mut last = 0;
                                    for m in re.find_iter(&line_owned) {
                                        spans.push(Span::raw(line_owned[last..m.start()].to_owned()));
                                        spans.push(Span::styled(line_owned[m.range()].to_owned(), Style::default().fg(Color::Red)));
                                        last = m.end();
                                    }
                                    spans.push(Span::raw(line_owned[last..].to_owned()));
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

    fn perform_search(&mut self) -> Result<(), AppError> {
        if self.search_query.is_empty() {
            self.clear_search();
            self.input_mode = InputMode::Normal;
            return Ok(());
        }

        let re = Regex::new(&self.search_query)?;
        let config = self.search_config.expect("search config missing");
        let mut results = vec![];

        match self.search_at_depth(&self.current_path, &re, config.kind == SearchKind::Content, &mut results, 0, config.max_depth) {
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

        let text = line.spans.first().map(|s| s.content.as_ref()).unwrap_or("");
        let path_str = if text.contains(':') {
            text.split(':').next().unwrap_or(text)
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
                self.list_state.select(Some(pos));
                self.update_selection();
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
                .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
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

            if !app.search_results.is_empty() {
                let result_items: Vec<ListItem> = app.search_results.iter()
                    .map(|line| ListItem::new(line.clone()))
                    .collect();

                let result_list = List::new(result_items)
                    .block(Block::default().borders(Borders::ALL)
                        .title(format!("Results: {} matches", app.search_results.len())))
                    .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD));

                f.render_stateful_widget(result_list, main[1], &mut app.search_list_state);
            } else {
                let content_lines = if let Some(p) = &app.selected_properties {
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
                    vec![Line::from("Select a file or directory")]
                };

                f.render_widget(
                    Paragraph::new(Text::from(content_lines))
                        .block(Block::default().borders(Borders::ALL).title(" Info ")),
                    main[1],
                );
            }

            let status = if app.input_mode == InputMode::Search {
                let cfg = app.search_config.unwrap();
                let kind = if cfg.kind == SearchKind::Content { "content" } else { "name" };
                let depth = if cfg.max_depth == 0 { "current only" }
                           else if cfg.max_depth == usize::MAX { "unlimited" }
                           else { &format!("depth {}", cfg.max_depth) };
                format!("Search ({kind}, {depth}): {}", app.search_query)
            } else if !app.search_results.is_empty() {
                "↑↓ jk: select | Enter: go to | Esc: clear | q: quit".into()
            } else {
                app.status_message.clone().unwrap_or_else(|| {
                    "q:quit | h l ←→:nav | 0-9:depth name search | /:unlimited name | n:name | c:content".into()
                })
            };

            f.render_widget(Paragraph::new(status), layout[2]);
        })?;

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press { continue; }

                match app.input_mode {
                    InputMode::Normal => match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Char('h') | KeyCode::Left => { let _ = app.leave_dir(); }
                        KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => { let _ = app.enter_dir(); }

                        KeyCode::Char('0') => app.start_search(SearchKind::Name, 0),
                        KeyCode::Char(d @ '1'..='9') => {
                            let depth = d.to_digit(10).unwrap() as usize;
                            app.start_search(SearchKind::Name, depth);
                        }
                        KeyCode::Char('/') => app.start_search(SearchKind::Name, usize::MAX),
                        KeyCode::Char('n') => app.start_search(SearchKind::Name, usize::MAX),
                        KeyCode::Char('c') => app.start_search(SearchKind::Content, usize::MAX),

                        _ => {
                            if !app.search_results.is_empty() {
                                match key.code {
                                    KeyCode::Up | KeyCode::Char('k') => app.navigate_search(false),
                                    KeyCode::Down | KeyCode::Char('j') => app.navigate_search(true),
                                    KeyCode::Enter => { let _ = app.jump_to_selected_result(); }
                                    KeyCode::Esc => app.clear_search(),
                                    _ => {}
                                }
                            } else {
                                match key.code {
                                    KeyCode::Up | KeyCode::Char('k') => app.select_prev(),
                                    KeyCode::Down | KeyCode::Char('j') => app.select_next(),
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
                }
            }
        }
    }

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}
