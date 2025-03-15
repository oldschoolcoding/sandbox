use std::collections::HashMap;

pub fn get_hardcoded_files() -> HashMap<String, String> {
    let mut hardcoded_files = HashMap::new();

    hardcoded_files.insert("config.json".to_string(), r#"{"setting": "value"}"#.to_string());
    hardcoded_files.insert("config.json".to_string(), r#"{"settgggging": "value"}"#.to_string());
    hardcoded_files.insert("readme.txt".to_string(), "This is a readme file.".to_string());
    hardcoded_files.insert("utututututu.csv".to_string(), "name,age\nJohn,30\nJane,25".to_string());

    hardcoded_files
}
