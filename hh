use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, BufRead};
use std::path::Path;
use std::process::Command;

fn clear_screen() {
    Command::new("clear").status().ok();
}

// Style for section headers
fn color_header(text: &str) -> String {
    format!("\x1b[1;34m{}\x1b[0m", text) // Bold blue
}

// Style for script names
fn color_script(text: &str) -> String {
    format!("\x1b[0;32m{}\x1b[0m", text) // Green
}

// Style for descriptions
fn color_description(text: &str) -> String {
    format!("\x1b[3;36m{}\x1b[0m", text) // Italic cyan
}

// Store both script name and its description
#[derive(Clone)]
struct ScriptInfo {
    name: String,
    description: String,
}

// Extracts the category and description from each script
fn extract_metadata(file_path: &Path) -> (String, String) {
    let mut keyword = "uncategorized".to_string();
    let mut description = String::new();

    if let Ok(file) = fs::File::open(file_path) {
        let reader = io::BufReader::new(file);
        for line in reader.lines().flatten() {
            if line.contains("KEYWORD:") || line.contains("CATEGORY:") {
                let parts: Vec<&str> = line.splitn(2, ':').collect();
                if parts.len() == 2 {
                    keyword = parts[1].trim().to_lowercase();
                }
            } else if line.contains("DESCRIPTION:") {
                let parts: Vec<&str> = line.splitn(2, ':').collect();
                if parts.len() == 2 {
                    description = parts[1].trim().to_string();
                }
            }

            // Stop early if both found
            if !description.is_empty() && keyword != "uncategorized" {
                break;
            }
        }
    }

    (keyword, description)
}

// Display script name + description, neatly aligned and colored
fn display_in_columns(items: &[ScriptInfo], script_width: usize, desc_width: usize, columns: usize) {
    for (i, item) in items.iter().enumerate() {
        let script_aligned = format!("{:<width$}", &item.name, width = script_width);
        let desc_aligned = format!("{:<width$}", &item.description, width = desc_width);
        print!("{} {}  ", color_script(&script_aligned), color_description(&desc_aligned));

        if (i + 1) % columns == 0 {
            println!();
        }
    }
    if items.len() % columns != 0 {
        println!();
    }
}

fn main() {
    clear_screen();

    let args: Vec<String> = env::args().collect();
    let dir = if args.len() > 1 { &args[1] } else { "." };
    let path = Path::new(dir);

    if !path.is_dir() {
        eprintln!("❌ Error: '{}' is not a directory", dir);
        return;
    }

    let mut sections: HashMap<String, Vec<ScriptInfo>> = HashMap::new();

    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let file_path = entry.path();
            if file_path.extension().map_or(false, |ext| ext == "sh") {
                let file_name = entry.file_name().into_string().unwrap_or_default();
                let (keyword, description) = extract_metadata(&file_path);
                let script = ScriptInfo {
                    name: file_name,
                    description,
                };
                sections.entry(keyword).or_default().push(script);
            }
        }
    }

    let mut sorted_sections: Vec<_> = sections.into_iter().collect();
    sorted_sections.sort_by_key(|(k, _)| k.clone());

    // Gather all scripts to compute max widths
    let all_scripts: Vec<ScriptInfo> = sorted_sections
        .iter()
        .flat_map(|(_, scripts)| scripts.clone())
        .collect();

    let max_script_len = all_scripts
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(0);

    let max_desc_len = all_scripts
        .iter()
        .map(|s| s.description.len())
        .max()
        .unwrap_or(0);

    let padding = 2;
    let script_width = max_script_len + padding;
    let desc_width = max_desc_len + padding;
    let total_width = script_width + desc_width + 2;
    let columns = if total_width == 0 { 1 } else { 100 / total_width }.max(1);

    for (keyword, mut scripts) in sorted_sections {
        scripts.sort_by(|a, b| a.name.cmp(&b.name));
        println!("{}", color_header(&format!("== {} ==", keyword.to_uppercase())));
        display_in_columns(&scripts, script_width, desc_width, columns);
        println!();
    }
}
