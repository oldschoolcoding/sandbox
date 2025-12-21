mod core;
mod filesystem;
mod network;
mod ui;
mod search;

use std::io::stdout;
use crossterm::{
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{backend::CrosstermBackend, Terminal, style::Style};
use crossterm::event::{KeyCode, KeyModifiers};
use crate::core::app::{App, FocusedPane};
use crate::core::InputMode;
use crate::core::error::AppError;

fn get_user_friendly_error(error: &AppError) -> String {
    match error {
        AppError::Io(io_err) => match io_err.kind() {
            std::io::ErrorKind::PermissionDenied => "Permission denied - you don't have access to this directory".to_string(),
            std::io::ErrorKind::NotFound => "Directory not found".to_string(),
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

#[tokio::main]
async fn main() -> Result<(), crate::core::error::AppError> {
    let mut app = App::new_local()?;
    
    // Prompt for master password to load connections
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    
    app.input_mode = InputMode::Password;
    
    loop {
        terminal.draw(|f| {
            ui::draw_password_prompt(f, &app, f.size());
        })?;

        if crossterm::event::poll(std::time::Duration::from_millis(16))? {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                if key.kind != crossterm::event::KeyEventKind::Press {
                    continue;
                }

            match app.input_mode {
                    InputMode::Password => match key.code {
                        crossterm::event::KeyCode::Char(c) => {
                            app.password_input.push(c);
                        }
                        crossterm::event::KeyCode::Backspace => {
                            app.password_input.pop();
                        }
                        crossterm::event::KeyCode::Enter => {
                            if app.connection_manager.load(&app.password_input).is_ok() {
                                app.input_mode = InputMode::Normal;
                                break;
        } else {
                                app.password_input.clear();
                            }
                        }
                        crossterm::event::KeyCode::Esc => {
                    app.input_mode = InputMode::Normal;
                    break;
                }
                _ => {}
                            }
                            _ => {}
                }
            }
        }
    }

    app.password_input.clear();

    // Main application loop
    loop {
        // Process any incoming search results
        app.process_search_results();

        terminal.draw(|f| {
            match app.input_mode {
                InputMode::Normal => {
                    ui::draw_main_ui(f, &mut app, f.size());
                }
                InputMode::ConnectionList => {
                    ui::draw_connection_list(f, &mut app, f.size());
                }
                InputMode::AddConnection(step) => {
                    ui::draw_add_connection(f, &app, f.size(), step);
                }
                InputMode::Search => {
                    use ratatui::widgets::{Paragraph, Block, Borders};
                    use ratatui::layout::{Layout, Direction, Constraint, Rect};

                    let chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([
                            Constraint::Length(3), // Filename regex field
                            Constraint::Length(3), // Content regex field
                            Constraint::Min(0),    // Help text
                        ])
                        .split(f.size());

                    // Filename regex field
                    let filename_title = if app.search_field_focus == 0 {
                        "Filename Regex (focused)"
                    } else {
                        "Filename Regex"
                    };
                    let filename_value = app.search_config.as_ref()
                        .and_then(|c| c.filename_regex.as_ref())
                        .map(|s| if s.is_empty() { "_" } else { s })
                        .unwrap_or("_");

                    let filename_para = Paragraph::new(filename_value)
                        .block(Block::default()
                            .borders(Borders::ALL)
                            .title(filename_title)
                            .border_style(if app.search_field_focus == 0 {
                                ratatui::style::Style::default().fg(ratatui::style::Color::Yellow)
                            } else {
                                ratatui::style::Style::default()
                            }));
                    f.render_widget(filename_para, chunks[0]);

                    // Content regex field
                    let content_title = if app.search_field_focus == 1 {
                        "Content Regex (focused)"
        } else {
                        "Content Regex"
                    };
                    let content_value = app.search_config.as_ref()
                        .and_then(|c| c.content_regex.as_ref())
                        .map(|s| if s.is_empty() { "_" } else { s })
                        .unwrap_or("_");

                    let content_para = Paragraph::new(content_value)
                        .block(Block::default()
                            .borders(Borders::ALL)
                            .title(content_title)
                            .border_style(if app.search_field_focus == 1 {
                                ratatui::style::Style::default().fg(ratatui::style::Color::Yellow)
                            } else {
                                ratatui::style::Style::default()
                            }));
                    f.render_widget(content_para, chunks[1]);

                    // Help text
                    let help_text = "Tab: switch fields  F1-F4: insert patterns  Ctrl+R: test regex\nEnter: search  Esc: cancel  Backspace: delete";
                    let help_para = Paragraph::new(help_text)
                        .block(Block::default().borders(Borders::ALL).title("Help"));
                    f.render_widget(help_para, chunks[2]);
                }
                _ => {
                    ui::draw_main_ui(f, &mut app, f.size());
                }
            }
        })?;

        if crossterm::event::poll(std::time::Duration::from_millis(16))? {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                if key.kind != crossterm::event::KeyEventKind::Press {
                    continue;
                }

                match app.input_mode {
                    InputMode::Normal => match key.code {
                        // Ctrl+Arrow keys for pane switching (must come before regular arrow keys)
                        KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if !app.search_results.is_empty() {
                                app.focused_pane = FocusedPane::FileBrowser;
                            }
                        }
                        KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if !app.search_results.is_empty() {
                                app.focused_pane = FocusedPane::SearchResults;
                            }
                        }
                        KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if !app.search_results.is_empty() {
                                app.focused_pane = FocusedPane::FileBrowser;
                            }
                        }
                        KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if !app.search_results.is_empty() {
                                app.focused_pane = FocusedPane::SearchResults;
                            }
                        }

                        // Regular navigation (without Ctrl modifier)
                        KeyCode::Char('j') | KeyCode::Down => {
                            if !app.search_results.is_empty() && app.focused_pane == FocusedPane::SearchResults {
                                app.navigate_search(true);
    } else {
                                app.select_next();
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            if !app.search_results.is_empty() && app.focused_pane == FocusedPane::SearchResults {
                                app.navigate_search(false);
                    } else {
                                app.select_prev();
                            }
                        }
                        KeyCode::Char('h') | KeyCode::Left => {
                            if let Err(e) = app.leave_dir() {
                                let error_msg = match &e {
                                    AppError::Navigation(msg) => msg.clone(),
                                    _ => format!("Cannot go up: {}", get_user_friendly_error(&e)),
                                };
                                app.status_message = Some(error_msg);
                            }
                        }
                        KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => {
                            if !app.search_results.is_empty() && app.focused_pane == FocusedPane::SearchResults {
                                let _ = app.jump_to_grep_result();
        } else {
                                if let Err(e) = app.enter_dir() {
                                    app.status_message = Some(format!("Cannot enter directory: {}", get_user_friendly_error(&e)));
                                }
                            }
                        }

                        // Search
                        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.search_config = Some(crate::core::app::SearchConfig {
                                filename_regex: Some(String::new()),
                                content_regex: Some(String::new()),
                            });
                            app.input_mode = InputMode::Search;
                            app.search_field_focus = 0; // Start with filename field
                        }

                        // Remote connections
                        KeyCode::Char('r') => {
                            app.input_mode = InputMode::ConnectionList;
                        }

                        // Content scrolling (when a file is selected)
                        KeyCode::PageUp => {
                            app.scroll_content_up();
                        }
                        KeyCode::PageDown => {
                            app.scroll_content_down();
                        }
                        KeyCode::Char('u') => {
                            if key.modifiers.contains(KeyModifiers::CONTROL) {
                                app.scroll_content_up();
                            }
                        }
                        KeyCode::Char('d') => {
                            if key.modifiers.contains(KeyModifiers::CONTROL) {
                                app.scroll_content_down();
                            }
                        }

                        // Pane switching
                        KeyCode::Tab => {
                            if !app.search_results.is_empty() {
                                app.focused_pane = match app.focused_pane {
                                    FocusedPane::FileBrowser => FocusedPane::SearchResults,
                                    FocusedPane::SearchResults => FocusedPane::FileBrowser,
                                };
                            }
                        }

                        // Quit
                        KeyCode::Char('q') | KeyCode::Esc => {
                            if !app.search_results.is_empty() {
                                app.clear_search();
                    } else {
                                break;
                            }
                        }
                        _ => {}
                    }
                    InputMode::ConnectionList => match key.code {
                        KeyCode::Char('j') | KeyCode::Down => {
                            app.navigate_connection_list(true);
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            app.navigate_connection_list(false);
                            }
                KeyCode::Enter => {
                            if let Err(e) = app.connect_to_selected() {
                                app.status_message = Some(format!("Connection failed: {}", get_user_friendly_error(&e)));
                            }
                        }
                        KeyCode::Char('a') => {
                            app.start_add_connection();
                        }
                        KeyCode::Char('d') => {
                            if let Err(e) = app.delete_selected_connection() {
                                app.status_message = Some(format!("Delete failed: {}", get_user_friendly_error(&e)));
                    }
                }
                KeyCode::Esc => {
                    app.input_mode = InputMode::Normal;
                }
                _ => {}
            }
                    InputMode::AddConnection(step) => match key.code {
                        KeyCode::Enter => {
                            if app.next_add_connection_step() {
                                // Connection completed
                                        app.input_mode = InputMode::ConnectionList;
                                    }
                        }
                        KeyCode::Char(c) => {
                            app.update_new_connection(step, c);
                        }
                        KeyCode::Backspace => {
                            app.backspace_new_connection(step);
                            }
                            KeyCode::Esc => {
                                app.input_mode = InputMode::ConnectionList;
                            }
                                    _ => {}
                                }
                    InputMode::Search => match key.code {
                        KeyCode::Char(c) => {
                            // Handle special key combinations
                            if key.modifiers.contains(KeyModifiers::CONTROL) {
                                match c {
                                    'r' => {
                                        // Test regex (could show matches in a sample)
                                        // For now, just show a message
                                        app.status_message = Some("Regex testing not yet implemented".to_string());
                                    }
                                    _ => {}
                                }
        } else {
                                // Add character to the focused field
                                if let Some(ref mut config) = app.search_config {
                                    let field = if app.search_field_focus == 0 {
                                        &mut config.filename_regex
        } else {
                                        &mut config.content_regex
                                    };
                                    if let Some(ref mut field_value) = field {
                                        field_value.push(c);
                                    }
                                }
                            }
                        }
                        KeyCode::Backspace => {
                            // Remove character from the focused field
                            if let Some(ref mut config) = app.search_config {
                                let field = if app.search_field_focus == 0 {
                                    &mut config.filename_regex
        } else {
                                    &mut config.content_regex
                                };
                                if let Some(ref mut field_value) = field {
                                    let _ = field_value.pop();
                                }
                            }
                        }
                        KeyCode::Tab => {
                            // Switch between filename and content fields
                            app.search_field_focus = 1 - app.search_field_focus;
                        }
                        KeyCode::F(1) => {
                            // Word boundary pattern
                            if let Some(ref mut config) = app.search_config {
                                let pattern = r"\b\w+\b";
                                let field = if app.search_field_focus == 0 {
                                    &mut config.filename_regex
        } else {
                                    &mut config.content_regex
                                };
                                if let Some(ref mut field_value) = field {
                                    field_value.push_str(pattern);
                                }
                            }
                        }
                        KeyCode::F(2) => {
                            // Digits pattern
                            if let Some(ref mut config) = app.search_config {
                                let pattern = r"\d+";
                                let field = if app.search_field_focus == 0 {
                                    &mut config.filename_regex
    } else {
                                    &mut config.content_regex
                                };
                                if let Some(ref mut field_value) = field {
                                    field_value.push_str(pattern);
                                }
                            }
                        }
                        KeyCode::F(3) => {
                            // File extension pattern
                            if let Some(ref mut config) = app.search_config {
                                let pattern = if app.search_field_focus == 0 {
                                    r"\.[a-zA-Z0-9]+$"
        } else {
                                    r"\.[a-zA-Z0-9]+"
                                };
                                let field = if app.search_field_focus == 0 {
                                    &mut config.filename_regex
        } else {
                                    &mut config.content_regex
                                };
                                if let Some(ref mut field_value) = field {
                                    field_value.push_str(pattern);
                                }
                            }
                        }
                        KeyCode::F(4) => {
                            // Path separator pattern
                            if let Some(ref mut config) = app.search_config {
                                let pattern = r"[/\\]";
                                let field = if app.search_field_focus == 0 {
                                    &mut config.filename_regex
                                } else {
                                    &mut config.content_regex
                                };
                                if let Some(ref mut field_value) = field {
                                    field_value.push_str(pattern);
                                }
                            }
                        }
                KeyCode::Enter => {
                    if let Err(e) = app.perform_search() {
                        app.status_message = Some(format!("Search failed: {}", get_user_friendly_error(&e)));
                        app.input_mode = InputMode::Normal;
                    }
                    // perform_search() already sets input_mode to Normal on success
                }
                KeyCode::Esc => {
                    app.input_mode = InputMode::Normal;
                            app.cancel_search(true);
                }
                _ => {}
            }
                                    _ => {}
                }
            }
        }
    }

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}