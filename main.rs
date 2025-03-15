use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Read};
use regex::Regex;
use dotenv::dotenv;

mod hardcoded;

#[derive(Debug)]
struct FileStore {
    files: HashMap<String, String>,
}

impl FileStore {
    fn new() -> Self {
        FileStore {
            files: HashMap::new(),
        }
    }

    fn load_from_directory(&mut self, directory: &str) -> io::Result<()> {
        let paths = fs::read_dir(directory)?;

        for path in paths {
            let path = path?.path();
            if path.is_file() {
                if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
                    let key = filename.to_string();
                    let mut file_content = String::new();
                    fs::File::open(&path)?.read_to_string(&mut file_content)?;
                    self.files.insert(key, file_content);
                }
            }
        }
        Ok(())
    }

    fn search_keys(&self, pattern: &str) {
        let re = Regex::new(pattern).unwrap();
        for key in self.files.keys().filter(|key| re.is_match(key)) {
            println!("{}: {}", key, self.files.get(key).unwrap());
        }
    }

    fn search_values(&self, pattern: &str) {
        let re = Regex::new(pattern).unwrap();
        for (key, value) in self.files.iter().filter(|(_, value)| re.is_match(value)) {
            println!("{}: {}", key, value);
        }
    }

    fn insert_hardcoded_files(&mut self) {
        let hardcoded_files = hardcoded::get_hardcoded_files();
        for (key, content) in hardcoded_files {
            self.files.insert(key, content);
        }
    }
}

fn main() -> io::Result<()> {
    // Load environment variables from .env file
    dotenv().ok();

    // Get the directory path from the environment variable
    let directory = env::var("KV_ROOT").expect("KV_ROOT not set in .env file");

    let mut file_store = FileStore::new();

    // Load files from the directory
    file_store.load_from_directory(&directory)?;
    file_store.insert_hardcoded_files();

    // Read search type and pattern from command line arguments
    let args: Vec<String> = env::args().collect();
    let search_type = args.get(1).map_or("bykey", |s| s.as_str());
    let pattern = args.get(2).map_or(".*", |s| s.as_str());

    // Perform search based on the search type
    match search_type {
        "bykey" => {
            println!("Searching by key with pattern: {}", pattern);
            file_store.search_keys(pattern);
        }
        "byvalue" => {
            println!("Searching by value with pattern: {}", pattern);
            file_store.search_values(pattern);
        }
        _ => {
            eprintln!("Invalid search type. Use 'bykey' or 'byvalue'.");
        }
    }

    Ok(())
}
