use std::{
    io::{BufRead, BufReader, Read},
    net::TcpStream,
    path::PathBuf,
    time::Duration,
};
use ssh2::Session;
use tokio::{
    sync::mpsc,
    task,
    time,
};
use regex::Regex;
use ratatui::text::Line;
use crate::core::error::AppError;
use crate::core::app::{SearchResult, SearchConfig, SearchKind};

pub async fn perform_remote_search_async(
    host: String,
    username: String,
    password: String,
    port: u16,
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

    // Establish SSH connection
    let sftp_result = task::spawn_blocking(move || {
        let addr = format!("{}:{}", host, port);
        let tcp = TcpStream::connect(&addr)?;
        let mut sess = Session::new()?;
        sess.set_tcp_stream(tcp);
        sess.handshake()?;
        sess.userauth_password(&username, &password)?;
        if !sess.authenticated() {
            return Err(AppError::Navigation("Authentication failed".into()));
        }
        Ok(sess)
    }).await;

    let session = match sftp_result {
        Ok(Ok(sess)) => sess,
        Ok(Err(e)) => {
            let _ = tx.send(SearchResult::Error(format!("SSH connection failed: {}", e)));
            let _ = tx.send(SearchResult::Finished);
            return;
        }
        Err(e) => {
            let _ = tx.send(SearchResult::Error(format!("Failed to spawn blocking task: {}", e)));
            let _ = tx.send(SearchResult::Finished);
            return;
        }
    };

    let current_path_str = current_path.display().to_string();

    // Build command based on search kind - inspired by Yazi's external tool integration
    let cmd_str = match config.kind {
        SearchKind::Name => {
            // Use fd (faster than find) if available, fallback to find
            if check_command_available(&session, "fd") {
                format!("cd '{}' && fd -t f '{}'", current_path_str, search_query)
            } else {
                format!("find '{}' -type f -name '*{}*'", current_path_str, search_query)
            }
        }
        SearchKind::Content => {
            // Prioritize ripgrep (like Yazi), then grep
            if check_command_available(&session, "rg") {
                let _ = tx.send(SearchResult::Status("Using ripgrep for content search".into()));
                format!(
                    "cd '{}' && rg --line-number --binary-files=without-match --hidden --glob '!.git' --glob '!.svn' --glob '!.hg' '{}'",
                    current_path_str, search_query
                )
            } else if check_command_available(&session, "grep") {
                let _ = tx.send(SearchResult::Status("Using grep for content search".into()));
                format!(
                    "cd '{}' && grep -r -n -I --binary-files=without-match --exclude-dir=.git --exclude-dir=.svn --exclude-dir=.hg '{}'",
                    current_path_str, search_query
                )
            } else {
                let _ = tx.send(SearchResult::Error("No grep tools available".into()));
                return;
            }
        }
    };

    let _ = tx.send(SearchResult::Command(cmd_str.clone()));
    let _ = tx.send(SearchResult::Status(format!("Executing: {}", cmd_str)));

    // Execute the command
    let mut channel = match session.channel_session() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(SearchResult::Error(format!("Failed to open SSH channel: {}", e)));
            let _ = tx.send(SearchResult::Finished);
            return;
        }
    };

    match channel.exec(&cmd_str) {
        Ok(_) => {},
        Err(e) => {
            let _ = tx.send(SearchResult::Error(format!("Failed to execute command: {}", e)));
            let _ = tx.send(SearchResult::Finished);
            return;
        }
    }

    let mut stdout = channel.stream(0);
    let mut stderr = channel.stderr();

    let mut buffer = [0; 1024];
    let mut current_line = String::new();

    // Parse output based on search kind
    match config.kind {
        SearchKind::Name => {
            // Parse find output (just file paths)
            loop {
                match stdout.read(&mut buffer) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        for &byte in &buffer[..n] {
                            if byte == b'\n' {
                                if !current_line.trim().is_empty() {
                                    let file_path = current_line.trim().to_string();
                                    let _ = tx.send(SearchResult::GrepMatch {
                                        file_path,
                                        line_number: 0,
                                        line_content: String::new(),
                                        search_term: search_query.clone(),
                                    });
                                }
                                current_line.clear();
                            } else {
                                current_line.push(byte as char);
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(SearchResult::Error(format!("Error reading stdout: {}", e)));
                        break;
                    }
                }

                // Also check stderr for errors
                match stderr.read(&mut buffer) {
                    Ok(n) if n > 0 => {
                        let stderr_msg = String::from_utf8_lossy(&buffer[..n]);
                        if !stderr_msg.trim().is_empty() {
                            let _ = tx.send(SearchResult::Status(format!("stderr: {}", stderr_msg.trim())));
                        }
                    }
                    _ => {}
                }

                // Yield periodically to allow UI updates
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            // Process any remaining line
            if !current_line.trim().is_empty() {
                let file_path = current_line.trim().to_string();
                let _ = tx.send(SearchResult::GrepMatch {
                    file_path,
                    line_number: 0,
                    line_content: String::new(),
                    search_term: search_query.clone(),
                });
            }
        }
        SearchKind::Content => {
            // Parse grep output (format: path:line_number:content)
            loop {
                match stdout.read(&mut buffer) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        for &byte in &buffer[..n] {
                            if byte == b'\n' {
                                if !current_line.trim().is_empty() {
                                    if let Some((file_path, line_number, line_content)) = parse_grep_line(&current_line) {
                                        let _ = tx.send(SearchResult::GrepMatch {
                                            file_path,
                                            line_number,
                                            line_content,
                                            search_term: search_query.clone(),
                                        });
                                    }
                                }
                                current_line.clear();
                            } else {
                                current_line.push(byte as char);
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(SearchResult::Error(format!("Error reading stdout: {}", e)));
                        break;
                    }
                }

                // Check stderr for errors
                match stderr.read(&mut buffer) {
                    Ok(n) if n > 0 => {
                        let stderr_msg = String::from_utf8_lossy(&buffer[..n]);
                        if !stderr_msg.trim().is_empty() {
                            let _ = tx.send(SearchResult::Status(format!("stderr: {}", stderr_msg.trim())));
                        }
                    }
                    _ => {}
                }

                // Yield periodically to allow UI updates
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            // Process any remaining line
            if !current_line.trim().is_empty() {
                if let Some((file_path, line_number, line_content)) = parse_grep_line(&current_line) {
                    let _ = tx.send(SearchResult::GrepMatch {
                        file_path,
                        line_number,
                        line_content,
                        search_term: search_query.clone(),
                    });
                }
            }
        }
    }

    let _ = tx.send(SearchResult::Finished);
}

fn check_command_available(session: &Session, command: &str) -> bool {
    let mut channel = session.channel_session().unwrap();
    if channel.exec(&format!("command -v {}", command)).is_ok() {
        let mut output = String::new();
        let _ = channel.read_to_string(&mut output);
        let _ = channel.wait_close();
        !output.trim().is_empty()
    } else {
        false
    }
}

fn parse_grep_line(line: &str) -> Option<(String, usize, String)> {
    // Parse grep output format: path:line_number:content
    if let Some(colon_pos) = line.find(':') {
        let path = &line[..colon_pos];
        let rest = &line[colon_pos + 1..];

        if let Some(colon_pos2) = rest.find(':') {
            if let Ok(line_number) = rest[..colon_pos2].parse::<usize>() {
                let content = rest[colon_pos2 + 1..].to_string();
                return Some((path.to_string(), line_number, content));
            }
        }
    }
    None
}