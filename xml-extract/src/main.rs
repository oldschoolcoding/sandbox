use std::io::{self, BufRead, BufReader, Read, Write};
use std::fs::File;
use std::path::Path;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use quick_xml::{events::Event, Reader, Writer};
use regex::Regex;
use indicatif::{ProgressBar, ProgressStyle};

// Compression crates
use flate2::read::GzDecoder;
use bzip2::read::BzDecoder;
use xz2::read::XzDecoder;

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

    /// Optional input file (supports compressed formats)
    #[arg(value_name = "FILE")]
    file: Option<String>,
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
    reader.trim_text(true);

    let mut out = Vec::new();

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

    Ok(out)
}

/// Wrap reader in decompressor if needed
fn open_reader(file: Option<&str>) -> Result<Box<dyn BufRead>> {
    let reader: Box<dyn Read> = match file {
        Some(path) => {
            let f = File::open(path)?;
            let ext = Path::new(path)
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("");

            match ext {
                "gz" => Box::new(GzDecoder::new(f)),
                "bz2" => Box::new(BzDecoder::new(f)),
                "xz" => Box::new(XzDecoder::new(f)),
                _ => Box::new(f),
            }
        }
        None => Box::new(io::stdin()),
    };

    Ok(Box::new(BufReader::new(reader)))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let pretty = if !cli.pretty && !cli.raw { true } else { cli.pretty };

    let mut reader = open_reader(cli.file.as_deref())?;
    let mut stdout = io::stdout().lock();

    let ts_regex = Regex::new(
        r"(\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:\.\d+)?Z?)",
    )?;

    let mut last_timestamp: Option<String> = None;
    let mut text_buf = String::new();

    // Buffer entire file (for progress bar)
    let mut input = Vec::new();
    reader.read_to_end(&mut input)?;
    let bytes = &input[..];

    let pb = ProgressBar::new(bytes.len() as u64);
    pb.set_style(ProgressStyle::with_template(
        "[{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})"
    )?.progress_chars("=>-"));

    let mut i = 0;

    while i < bytes.len() {
        pb.set_position(i as u64);

        if bytes[i] != b'<' {
            let c = bytes[i] as char;
            text_buf.push(c);
            if let Some(cap) = ts_regex.captures(&text_buf) {
                last_timestamp = Some(cap[1].to_string());
            }
            i += 1;
            continue;
        }

        let slice = &bytes[i..];
        let mut xml_reader = Reader::from_reader(slice);
        xml_reader.trim_text(true);

        let mut buf = Vec::new();
        let mut depth = 0usize;
        let mut consumed = 0usize;
        let mut valid = false;

        loop {
            match xml_reader.read_event_into(&mut buf) {
                Ok(Event::Start(_)) => {
                    depth += 1;
                    consumed = xml_reader.buffer_position();
                }
                Ok(Event::End(_)) => {
                    if depth > 0 { depth -= 1; }
                    consumed = xml_reader.buffer_position();
                    if depth == 0 {
                        valid = true;
                        break;
                    }
                }
                Ok(Event::Empty(_)) => {
                    consumed = xml_reader.buffer_position();
                    valid = true;
                    break;
                }
                Ok(Event::Decl(_))
                | Ok(Event::PI(_))
                | Ok(Event::DocType(_))
                | Ok(Event::Text(_))
                | Ok(Event::CData(_))
                | Ok(Event::Comment(_)) => {
                    consumed = xml_reader.buffer_position();
                }
                Ok(Event::Eof) => break,
                Err(_) => break,
            }
            buf.clear();
        }

        if valid && consumed > 0 {
            let xml = &slice[..consumed];

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

            let out = write_xml(xml, pretty)?;
            stdout.write_all(&out)?;
            stdout.write_all(b"\n\n")?;

            text_buf.clear();
            i += consumed;
        } else {
            text_buf.push('<');
            i += 1;
        }
    }

    pb.finish_with_message("Done");
    Ok(())
}

