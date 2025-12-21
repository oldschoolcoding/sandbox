use ratatui::{
    Frame,
    layout::{Layout, Direction, Constraint, Rect},
    widgets::{List, ListItem, Paragraph, Block, Borders, Row, Cell, Table},
    style::{Style, Color, Modifier},
    text::{Line, Span},
};
use std::path::Path;
use crate::core::app::{App, FocusedPane};
use crate::network::connection::RemoteConnection;

fn get_file_icon_and_color(file_path: &Path, is_dir: bool, is_hidden: bool) -> (&'static str, Color) {
    if is_dir {
        return ("📁", Color::Blue);
    }

    let file_name = file_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    if is_hidden || file_name.starts_with('.') {
        return ("👁️‍🗨️", Color::Gray);
    }

    // Get file extension
    let extension = file_path.extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_lowercase();

    match extension.as_str() {
        // Programming files
        "rs" | "go" | "py" | "js" | "ts" | "java" | "cpp" | "c" | "h" | "hpp" | "cs" | "php" | "rb" | "swift" | "kt" | "scala" => ("💻", Color::Green),
        "html" | "htm" | "xml" | "css" | "scss" | "sass" | "less" => ("🌐", Color::Cyan),
        "json" | "yaml" | "yml" | "toml" | "ini" | "cfg" | "conf" => ("⚙️", Color::Yellow),
        "md" | "txt" | "rst" | "adoc" => ("📝", Color::White),
        "sh" | "bash" | "zsh" | "fish" | "ps1" => ("🐚", Color::Green),

        // Images
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "tiff" | "svg" | "webp" | "ico" => ("🖼️", Color::Magenta),
        "raw" | "cr2" | "nef" | "arw" => ("📸", Color::Magenta),

        // Videos
        "mp4" | "avi" | "mkv" | "mov" | "wmv" | "flv" | "webm" | "m4v" => ("🎥", Color::Red),
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" => ("🎵", Color::Red),

        // Archives
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "tgz" => ("📦", Color::Red),
        "pdf" => ("📄", Color::Red),
        "doc" | "docx" => ("📄", Color::Blue),
        "xls" | "xlsx" | "csv" => ("📊", Color::Green),

        // Executables
        "exe" | "msi" | "dmg" | "pkg" | "deb" | "rpm" | "appimage" => ("⚙️", Color::Green),

        // System files
        "log" | "tmp" | "bak" | "old" => ("📋", Color::Gray),

        // Default
        _ => ("📄", Color::White),
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum AddConnectionStep {
    Name,
    Username,
    Host,
    Port,
    Password,
}

pub fn draw_connection_list(f: &mut Frame, app: &mut App, area: Rect) {
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

pub fn draw_add_connection(f: &mut Frame, app: &App, area: Rect, step: AddConnectionStep) {
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

pub fn draw_password_prompt(f: &mut Frame, app: &App, area: Rect) {
    let text = format!("Master Password: {}_", "*".repeat(app.password_input.len()));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Enter Master Password ");
    let para = Paragraph::new(text).block(block);
    f.render_widget(para, area);
}

pub fn draw_main_ui(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1), // Status bar
        ])
        .split(area);

    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50), // Left pane: file browser
            Constraint::Percentage(50), // Right pane: content/details
        ])
        .split(chunks[0]);

    // Left pane: File browser
    draw_file_browser(f, app, main_chunks[0]);

    // Right pane: Content or details
    draw_content_pane(f, app, main_chunks[1]);

    // Bottom status bar
    draw_status_bar(f, app, chunks[1]);
}

fn draw_file_browser(f: &mut Frame, app: &mut App, area: Rect) {
    let title = if app.is_remote() {
        format!(" Remote: {} ", app.current_path.display())
    } else {
        format!(" Local: {} ", app.current_path.display())
    };

    let mut rows = Vec::new();
    for entry in &app.entries {
        let is_selected = entry.path.file_name() == Some(std::ffi::OsStr::new(".")) ||
                         entry.path.file_name() == Some(std::ffi::OsStr::new(".."));

        let name = if is_selected {
            "..".to_string()
        } else {
            entry.path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        };

        let is_hidden = name.starts_with('.');
        let (icon, color) = get_file_icon_and_color(&entry.path, entry.is_dir, is_hidden);

        // Apply additional styling for hidden files
        let mut style = Style::default().fg(color);
        if is_hidden {
            style = style.add_modifier(Modifier::DIM);
        }

        let display_name = format!("{} {}", icon, name);

        rows.push(Row::new(vec![
            Cell::from(Span::styled(display_name, style)),
        ]));
    }

    let table = ratatui::widgets::Table::new(
        rows,
        [
            Constraint::Percentage(100),
        ]
    )
    .block(Block::default()
        .borders(Borders::ALL)
        .title(title))
    .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    f.render_stateful_widget(table, area, &mut app.table_state);
}

fn draw_content_pane(f: &mut Frame, app: &App, area: Rect) {
    // Split the right pane into two sections: properties (top) and content (bottom)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(10), // Properties section (fixed height)
            Constraint::Min(0),     // Content section (remaining space)
        ])
        .split(area);

    // Top section: Properties
    draw_properties_section(f, app, chunks[0]);

    // Bottom section: Content
    draw_content_section(f, app, chunks[1]);
}

fn draw_properties_section(f: &mut Frame, app: &App, area: Rect) {
    let selected_idx = app.table_state.selected().unwrap_or(0);
    if let Some(entry) = app.entries.get(selected_idx) {
        if let Ok(props) = app.fs_ops.get_file_properties(&entry.path) {
            let file_name = entry.path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Unknown");

            let (icon, color) = get_file_icon_and_color(&entry.path, entry.is_dir, file_name.starts_with('.'));

            let content = format!(
                "{} {}  {}\nSize: {}\nUser: {}  Group: {}\nModified: {}\nPermissions: {}",
                icon,
                Span::styled(file_name, Style::default().fg(color).add_modifier(Modifier::BOLD)).to_string(),
                if entry.is_dir { "Directory" } else { "File" },
                props.size,
                props.user,
                props.group,
                props.modified,
                props.type_and_permissions
            );

            let para = Paragraph::new(content)
                .block(Block::default()
                    .borders(Borders::ALL)
                    .title(" Properties ")
                    .border_style(Style::default().fg(Color::Blue)));
            f.render_widget(para, area);
        } else {
            let para = Paragraph::new("Unable to read file properties")
                .block(Block::default()
                    .borders(Borders::ALL)
                    .title(" Properties ")
                    .border_style(Style::default().fg(Color::Red)));
            f.render_widget(para, area);
        }
    } else {
        let para = Paragraph::new("No file selected")
            .block(Block::default()
                .borders(Borders::ALL)
                .title(" Properties ")
                .border_style(Style::default().fg(Color::Gray)));
        f.render_widget(para, area);
    }
}

fn draw_content_section(f: &mut Frame, app: &App, area: Rect) {
    let selected_idx = app.table_state.selected().unwrap_or(0);
    if let Some(entry) = app.entries.get(selected_idx) {
        if entry.is_dir {
            // For directories, show a brief description
            let file_name = entry.path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Unknown");

            let content = format!(
                "📁 Directory: {}\n\nThis is a directory containing files and subdirectories.\n\nPress Enter to navigate into this directory.\nPress h to go back to parent directory.",
                file_name
            );

            let para = Paragraph::new(content)
                .block(Block::default()
                    .borders(Borders::ALL)
                    .title(" Directory Content ")
                    .border_style(Style::default().fg(Color::Blue)))
                .wrap(ratatui::widgets::Wrap { trim: true });
            f.render_widget(para, area);
        } else {
            // For files, show content if available
            if !app.file_content.is_empty() {
                let content = app.file_content.join("\n");
                let para = Paragraph::new(content)
                    .block(Block::default()
                        .borders(Borders::ALL)
                        .title(" File Content ")
                        .border_style(Style::default().fg(Color::Green)))
                    .wrap(ratatui::widgets::Wrap { trim: true });
                f.render_widget(para, area);
            } else {
                // File not loaded yet
                let file_name = entry.path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("Unknown");

                let (icon, color) = get_file_icon_and_color(&entry.path, false, file_name.starts_with('.'));

                let content = format!(
                    "{} {}\n\nFile content not loaded.\n\nPress Enter to load and view file content.\nThis will display text files or show a preview for other file types.",
                    icon,
                    Span::styled(file_name, Style::default().fg(color).add_modifier(Modifier::BOLD)).to_string()
                );

                let para = Paragraph::new(content)
                    .block(Block::default()
                        .borders(Borders::ALL)
                        .title(" File Content ")
                        .border_style(Style::default().fg(Color::Yellow)))
                    .wrap(ratatui::widgets::Wrap { trim: true });
                f.render_widget(para, area);
            }
        }
    } else {
        let para = Paragraph::new("No file selected")
            .block(Block::default()
                .borders(Borders::ALL)
                .title(" Content ")
                .border_style(Style::default().fg(Color::Gray)));
        f.render_widget(para, area);
    }
}

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let status = if let Some(msg) = &app.status_message {
        msg.clone()
    } else {
        let entry_count = app.entries.len();
        let selected = app.table_state.selected().map(|i| i + 1).unwrap_or(0);
        format!("{}/{} entries | Press 'r' for remote connections, '/' for search, 'q' to quit", selected, entry_count)
    };

    let para = Paragraph::new(status)
        .style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(para, area);
}
