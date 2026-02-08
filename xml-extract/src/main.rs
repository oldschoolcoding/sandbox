use std::io::{self, Read, Write};
use std::io::Cursor;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use quick_xml::{events::Event, Reader, Writer};
use regex::Regex;

#[derive(Parser)]
#[command(name = "xml-extract")]
#[command(about = "Extract XML documents from logs (stdin → stdout)")]
struct Cli {
    /// Pretty-print XML output (indented)
    #[arg(long)]
    pretty: bool,

    /// Output raw XML (single line, normalized)
    #[arg(long)]
    raw: bool,

    /// How to emit timestamps
    #[arg(long, value_enum, default_value = "comment")]
    timestamp: TimestampMode,
}

#[derive(ValueEnum, Clone)]
enum TimestampMode {
    Comment,
    Prefix,
    None,
}

/// Write XML with optional pretty-printing
fn write_xml(xml: &[u8], pretty: bool) -> Result<Vec<u8>> {
    let mut reader = Reader::from_reader(xml);
    reader.trim_text(true); // <- works in quick-xml 0.31

    let mut out = Cursor::new(Vec::new());

    let mut writer = if pretty {
        Writer::new_with_indent(&mut out, b' ', 2)
    } else {
        Writer::new(&mut out)
    };

    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Eof => break,
            e => writer.write_event(e)?,
        }
        buf.clear();
    }

    Ok(out.into_inner())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Default: pretty if neither flag is provided
    let pretty = if !cli.pretty && !cli.raw {
        true
    } else {
        cli.pretty
    };

    // Read stdin fully
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;

    let mut stdout = io::stdout().lock();

    // Timestamp regex (ISO-ish)
    let ts_regex = Regex::new(
        r"(\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:\.\d+)?Z?)",
    )?;

    let mut last_timestamp: Option<String> = None;
    let mut text_buf = String::new();

    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Normal text
        if bytes[i] != b'<' {
            let c = bytes[i] as char;
            text_buf.push(c);

            if let Some(cap) = ts_regex.captures(&text_buf) {
                last_timestamp = Some(cap[1].to_string());
            }

            i += 1;
            continue;
        }

        // Try XML parsing
        let slice = &bytes[i..];
        let mut reader = Reader::from_reader(slice);
        reader.trim_text(true); // <- fixed for quick-xml 0.31

        let mut buf = Vec::new();
        let mut depth = 0usize;
        let mut consumed = 0usize;
        let mut valid = false;

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(_)) => {
                    depth += 1;
                    consumed = reader.buffer_position();
                }
                Ok(Event::End(_)) => {
                    if depth > 0 {
                        depth -= 1;
                    }
                    consumed = reader.buffer_position();

                    if depth == 0 {
                        valid = true;
                        break;
                    }
                }
                Ok(Event::Empty(_)) => {
                    consumed = reader.buffer_position();
                    valid = true;
                    break;
                }
                Ok(Event::Decl(_))
                | Ok(Event::PI(_))
                | Ok(Event::DocType(_))
                | Ok(Event::Text(_))
                | Ok(Event::CData(_))
                | Ok(Event::Comment(_)) => {
                    consumed = reader.buffer_position();
                }
                Ok(Event::Eof) => break, // truncated XML
                Err(_) => break,         // malformed XML
            }
            buf.clear();
        }

        if valid && consumed > 0 {
            let xml = &slice[..consumed];

            // Emit timestamp if requested
            if let Some(ts) = &last_timestamp {
                match cli.timestamp {
                    TimestampMode::Comment => {
                        writeln!(stdout, "<!-- timestamp: {} -->", ts)?;
                    }
                    TimestampMode::Prefix => {
                        write!(stdout, "[{}] ", ts)?;
                    }
                    TimestampMode::None => {}
                }
            }

            // Emit XML
            let out = write_xml(xml, pretty)?;
            stdout.write_all(&out)?;
            stdout.write_all(b"\n\n")?;

            text_buf.clear();
            i += consumed; // move past this XML
        } else {
            // Recovery: treat '<' as plain text
            text_buf.push('<');
            i += 1;
        }
    }

    Ok(())
}

