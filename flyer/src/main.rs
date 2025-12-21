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
                    let mode_indicator = if app.search_config.map_or(false, |c| c.regex_mode) { "[REGEX]" } else { "[TEXT]" };
                    let kind_indicator = match app.search_config.map(|c| c.kind) {
                        Some(crate::core::app::SearchKind::Name) => "NAME",
                        Some(crate::core::app::SearchKind::Content) => "CONTENT",
                        None => "UNKNOWN",
                    };
                    let title = format!("Search ({}{}) - Ctrl+R: toggle regex, F1-F4: patterns", kind_indicator, mode_indicator);
                    let query_display = format!("{}{}", app.search_query, if app.search_query.is_empty() { "_" } else { "|" });

                    let help_text = "\nF1: word boundary  F2: digits  F3: extension  F4: path separator\nEsc: cancel  Enter: search  Backspace: delete";
                    let full_display = format!("{}{}", query_display, help_text);

                    let para = ratatui::widgets::Paragraph::new(full_display)
                        .block(ratatui::widgets::Block::default().borders(ratatui::widgets::Borders::ALL).title(title));
                    f.render_widget(para, f.size());
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
                        // Navigation
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
                        KeyCode::Char('/') => {
                            app.search_config = Some(crate::core::app::SearchConfig {
                                kind: crate::core::app::SearchKind::Name,
                                regex_mode: false,
                            });
                            app.input_mode = InputMode::Search;
                        }
                        KeyCode::Char('c') => {
                            app.search_config = Some(crate::core::app::SearchConfig {
                                kind: crate::core::app::SearchKind::Content,
                                regex_mode: false,
                            });
                            app.input_mode = InputMode::Search;
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
                                        // Toggle regex mode
                                        if let Some(ref mut config) = app.search_config {
                                            config.regex_mode = !config.regex_mode;
                                        }
                                    }
                                    _ => {}
                                }
                            } else {
                                app.search_query.push(c);
                            }
                        }
                        KeyCode::Backspace => {
                            // Ensure Backspace only removes characters, not adds them
                            let _ = app.search_query.pop();
                        }
                        KeyCode::F(1) => {
                            // Word boundary pattern
                            if app.search_config.map_or(false, |c| c.regex_mode) {
                                app.search_query.push_str(r"\b\w+\b");
                            } else {
                                app.search_query.push_str("*");
                            }
                        }
                        KeyCode::F(2) => {
                            // Digits pattern
                            if app.search_config.map_or(false, |c| c.regex_mode) {
                                app.search_query.push_str(r"\d+");
                            } else {
                                app.search_query.push_str("[0-9]");
                            }
                        }
                        KeyCode::F(3) => {
                            // File extension pattern
                            if app.search_config.map_or(false, |c| c.regex_mode) {
                                app.search_query.push_str(r"\.[a-zA-Z0-9]+$");
                            } else {
                                app.search_query.push_str(".*");
                            }
                        }
                        KeyCode::F(4) => {
                            // Path separator pattern
                            if app.search_config.map_or(false, |c| c.regex_mode) {
                                app.search_query.push_str(r"[/\\]");
                            } else {
                                app.search_query.push_str("/");
                            }
                        }
                        KeyCode::Enter => {
                            if let Err(e) = app.perform_search() {
                                app.status_message = Some(format!("Search failed: {}", get_user_friendly_error(&e)));
                                app.input_mode = InputMode::Normal;
                            }
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