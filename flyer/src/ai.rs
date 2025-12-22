use serde::{Deserialize, Serialize};
use crate::core::app::SearchConfig;

// ============================================================================
// AI Provider Configuration
// ============================================================================

#[derive(Clone, Debug)]
pub enum AIProvider {
    Ollama,
    Groq,
}

#[derive(Clone, Debug)]
pub struct AIConfig {
    pub provider: AIProvider,
    pub groq_api_key: Option<String>,
    pub ollama_url: String,
    pub ollama_model: String,
    pub groq_model: String,
}

impl Default for AIConfig {
    fn default() -> Self {
        Self {
            provider: AIProvider::Ollama, // Default to local Ollama
            groq_api_key: std::env::var("GROQ_API_KEY").ok(),
            ollama_url: "http://localhost:11434".to_string(),
            ollama_model: "llama3.2:3b".to_string(),
            groq_model: "llama-3.3-70b-versatile".to_string(),
        }
    }
}

// ============================================================================
// Ollama Request/Response
// ============================================================================

#[derive(Serialize)]
struct OllamaRequest {
    model: String,
    prompt: String,
    stream: bool,
    format: String,
}

#[derive(Deserialize)]
struct OllamaResponse {
    response: String,
}

// ============================================================================
// Groq Request/Response (OpenAI-compatible)
// ============================================================================

#[derive(Serialize)]
struct GroqRequest {
    model: String,
    messages: Vec<GroqMessage>,
    temperature: f32,
}

#[derive(Serialize)]
struct GroqMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct GroqResponse {
    choices: Vec<GroqChoice>,
}

#[derive(Deserialize)]
struct GroqChoice {
    message: GroqMessageResponse,
}

#[derive(Deserialize)]
struct GroqMessageResponse {
    content: String,
}

// ============================================================================
// AI Response Parsing
// ============================================================================

#[derive(Deserialize)]
struct AISearchCriteria {
    filename_pattern: Option<String>,
    content_pattern: Option<String>,
    explanation: String,
}

// ============================================================================
// AI Client Implementation
// ============================================================================

pub struct AIClient {
    client: reqwest::Client,
    config: AIConfig,
}

impl AIClient {
    pub fn new(config: AIConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
        }
    }

    pub async fn interpret_search_query(&self, query: &str) -> Result<SearchConfig, Box<dyn std::error::Error + Send + Sync>> {
        let prompt = self.build_search_prompt(query);

        // Try Ollama first (local)
        match self.config.provider {
            AIProvider::Ollama => {
                match self.interpret_with_ollama(&prompt).await {
                    Ok(result) => return Ok(result),
                    Err(e) => {
                        eprintln!("Ollama failed: {}, falling back to Groq", e);
                        // Fall back to Groq if Ollama fails and API key is available
                        if self.config.groq_api_key.is_some() {
                            match self.interpret_with_groq(&prompt).await {
                                Ok(result) => return Ok(result),
                                Err(groq_err) => {
                                    return Err(format!("Both Ollama and Groq failed. Ollama: {}, Groq: {}", e, groq_err).into());
                                }
                            }
                        } else {
                            return Err(format!("Ollama failed and no Groq API key configured: {}", e).into());
                        }
                    }
                }
            }
            AIProvider::Groq => {
                match self.interpret_with_groq(&prompt).await {
                    Ok(result) => return Ok(result),
                    Err(e) => {
                        return Err(format!("Groq failed: {}", e).into());
                    }
                }
            }
        }
    }

    fn build_search_prompt(&self, query: &str) -> String {
        format!(
            r#"You are a smart file search assistant. Convert natural language queries into structured search criteria.

Given the query: "{query}"

Respond with a JSON object containing:
- filename_pattern: regex pattern for filename matching (null if not applicable)
- content_pattern: regex pattern for content matching (null if not applicable)
- explanation: brief explanation of the interpretation

Examples:
Query: "find all Python files"
{{"filename_pattern": ".*\\.py$", "content_pattern": null, "explanation": "Search for Python files by extension"}}

Query: "find files with database connections"
{{"filename_pattern": null, "content_pattern": "database|connect|db|mysql|postgres|mongodb", "explanation": "Search for files containing database-related terms"}}

Query: "find config files with API keys"
{{"filename_pattern": ".*config.*|.*\\.conf.*|.*\\.yml.*|.*\\.yaml.*", "content_pattern": "api.*key|secret.*key|token", "explanation": "Search config files for API key patterns"}}

Query: "show me text files with error messages"
{{"filename_pattern": ".*\\.txt$|.*\\.log$", "content_pattern": "error|Error|ERROR|exception|Exception", "explanation": "Search text/log files for error patterns"}}

Only respond with the JSON object, no additional text."#,
            query = query
        )
    }

    async fn interpret_with_ollama(&self, prompt: &str) -> Result<SearchConfig, Box<dyn std::error::Error + Send + Sync>> {
        let request = OllamaRequest {
            model: self.config.ollama_model.clone(),
            prompt: prompt.to_string(),
            stream: false,
            format: "json".to_string(),
        };

        let response = self.client
            .post(&format!("{}/api/generate", self.config.ollama_url))
            .json(&request)
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(format!("Ollama API error: {}", response.status()).into());
        }

        let ollama_response: OllamaResponse = response.json().await?;
        let json_str = ollama_response.response.trim();

        // Parse the JSON response
        let criteria: AISearchCriteria = serde_json::from_str(json_str)?;

        Ok(SearchConfig {
            filename_regex: criteria.filename_pattern,
            content_regex: criteria.content_pattern,
        })
    }

    async fn interpret_with_groq(&self, prompt: &str) -> Result<SearchConfig, Box<dyn std::error::Error + Send + Sync>> {
        let api_key = self.config.groq_api_key.as_ref()
            .ok_or("Groq API key not configured")?;

        let request = GroqRequest {
            model: self.config.groq_model.clone(),
            messages: vec![GroqMessage {
                role: "user".to_string(),
                content: prompt.to_string(),
            }],
            temperature: 0.3,
        };

        let response = self.client
            .post("https://api.groq.com/openai/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&request)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(format!("Groq API error: {}", response.status()).into());
        }

        let groq_response: GroqResponse = response.json().await?;

        if groq_response.choices.is_empty() {
            return Err("No response from Groq".into());
        }

        let content = &groq_response.choices[0].message.content;

        // Remove markdown formatting if present
        let cleaned = content.trim()
            .trim_start_matches("```json")
            .trim_end_matches("```")
            .trim();

        let criteria: AISearchCriteria = serde_json::from_str(cleaned)?;

        Ok(SearchConfig {
            filename_regex: criteria.filename_pattern,
            content_regex: criteria.content_pattern,
        })
    }

    pub async fn check_connection(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        match self.config.provider {
            AIProvider::Ollama => {
                let response = self.client
                    .get(&format!("{}/api/tags", self.config.ollama_url))
                    .send()
                    .await?;

                if response.status().is_success() {
                    Ok(format!("Ollama ({})", self.config.ollama_model))
                } else {
                    Err(format!("Ollama connection failed: {}", response.status()).into())
                }
            }
            AIProvider::Groq => {
                if self.config.groq_api_key.is_some() {
                    Ok(format!("Groq ({})", self.config.groq_model))
                } else {
                    Err("Groq API key not configured".into())
                }
            }
        }
    }

    pub fn switch_provider(&mut self) {
        self.config.provider = match self.config.provider {
            AIProvider::Ollama => AIProvider::Groq,
            AIProvider::Groq => AIProvider::Ollama,
        };
    }

    pub fn get_current_provider(&self) -> &AIProvider {
        &self.config.provider
    }

    pub fn get_provider_name(&self) -> String {
        match self.config.provider {
            AIProvider::Ollama => format!("Ollama ({})", self.config.ollama_model),
            AIProvider::Groq => format!("Groq ({})", self.config.groq_model),
        }
    }

    pub fn get_config(&self) -> &AIConfig {
        &self.config
    }
}

impl Default for AIClient {
    fn default() -> Self {
        Self::new(AIConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ai_client_creation() {
        let config = AIConfig::default();
        let client = AIClient::new(config);
        assert!(matches!(client.config.provider, AIProvider::Ollama));
    }

    #[test]
    fn test_default_client() {
        let client = AIClient::default();
        assert!(matches!(client.config.provider, AIProvider::Ollama));
        assert_eq!(client.config.ollama_model, "llama3.2:3b");
        assert_eq!(client.config.groq_model, "llama-3.3-70b-versatile");
    }

    #[test]
    fn test_provider_switching() {
        let mut client = AIClient::default();
        assert!(matches!(client.get_current_provider(), AIProvider::Ollama));

        client.switch_provider();
        assert!(matches!(client.get_current_provider(), AIProvider::Groq));

        client.switch_provider();
        assert!(matches!(client.get_current_provider(), AIProvider::Ollama));
    }

    #[test]
    fn test_provider_name() {
        let client = AIClient::default();
        let name = client.get_provider_name();
        assert!(name.contains("Ollama"));
        assert!(name.contains("llama3.2:3b"));
    }

    #[tokio::test]
    async fn test_ai_connection() {
        let client = AIClient::default();
        let result = client.check_connection().await;
        // This test will pass if Ollama is running, fail gracefully if not
        match result {
            Ok(provider) => println!("AI connection successful: {}", provider),
            Err(e) => println!("AI connection error: {}", e),
        }
        // We don't assert here since AI services may not be running during CI
    }
}
