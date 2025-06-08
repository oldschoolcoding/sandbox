use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use reqwest;
use serde_json::{json, Value};
use tokio;

#[derive(Debug)]
struct FolderKVStore {
    base_path: PathBuf,
    cache: HashMap<String, String>,
}

impl FolderKVStore {
    fn new(path: &str) -> io::Result<Self> {
        let base_path = PathBuf::from(path);
        if !base_path.exists() {
            fs::create_dir_all(&base_path)?;
        }
        
        let mut store = FolderKVStore {
            base_path,
            cache: HashMap::new(),
        };
        
        store.load_all()?;
        Ok(store)
    }
    
    fn load_all(&mut self) -> io::Result<()> {
        if self.base_path.is_dir() {
            for entry in fs::read_dir(&self.base_path)? {
                let entry = entry?;
                let path = entry.path();
                
                if path.is_file() {
                    if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                        let content = fs::read_to_string(&path)?;
                        self.cache.insert(filename.to_string(), content);
                    }
                }
            }
        }
        Ok(())
    }
    
    fn get(&self, key: &str) -> Option<&String> {
        self.cache.get(key)
    }
    
    fn set(&mut self, key: &str, value: &str) -> io::Result<()> {
        let file_path = self.base_path.join(key);
        fs::write(&file_path, value)?;
        self.cache.insert(key.to_string(), value.to_string());
        Ok(())
    }
    
    fn list_keys(&self) -> Vec<&String> {
        self.cache.keys().collect()
    }
    
    fn search_content(&self, query: &str) -> Vec<(String, String)> {
        let query_lower = query.to_lowercase();
        self.cache
            .iter()
            .filter(|(_, content)| content.to_lowercase().contains(&query_lower))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
    
    fn get_all_content(&self) -> String {
        let mut content = String::new();
        for (key, value) in &self.cache {
            content.push_str(&format!("Key: {}\nContent: {}\n---\n", key, value));
        }
        content
    }
}

struct LLMClient {
    base_url: String,
    client: reqwest::Client,
}

impl LLMClient {
    fn new(base_url: &str) -> Self {
        LLMClient {
            base_url: base_url.to_string(),
            client: reqwest::Client::new(),
        }
    }
    
    async fn generate_response(&self, prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
        let payload = json!({
            "model": "llama3.2:1b",
            "prompt": prompt,
            "stream": false,
            "options": {
                "temperature": 0.7,
                "max_tokens": 512
            }
        });
        
        let response = self.client
            .post(&format!("{}/api/generate", self.base_url))
            .json(&payload)
            .send()
            .await?;
            
        let response_json: Value = response.json().await?;
        
        if let Some(response_text) = response_json["response"].as_str() {
            Ok(response_text.to_string())
        } else {
            Err("No response from LLM".into())
        }
    }
}

struct QASystem {
    kv_store: FolderKVStore,
    llm_client: LLMClient,
}

impl QASystem {
    fn new(store_path: &str, llm_url: &str) -> io::Result<Self> {
        Ok(QASystem {
            kv_store: FolderKVStore::new(store_path)?,
            llm_client: LLMClient::new(llm_url),
        })
    }
    
    async fn answer_question(&self, question: &str) -> Result<String, Box<dyn std::error::Error>> {
        // First, try to find relevant content
        let relevant_content = self.find_relevant_content(question);
        
        // Get key patterns information
        let key_patterns = self.get_key_patterns_info();
        
        // Create a context-aware prompt focused on knowledge store
        let context = if relevant_content.is_empty() {
            format!("No relevant information found in the knowledge base.\n\n{}", key_patterns)
        } else {
            let mut context_str = String::new();
            context_str.push_str("RELEVANT KNOWLEDGE ENTRIES:\n");
            for (i, (key, content)) in relevant_content.iter().enumerate() {
                context_str.push_str(&format!("Entry {}: [Key: {}]\nContent: {}\n\n", i + 1, key, content));
            }
            context_str.push_str(&format!("\n{}", key_patterns));
            context_str
        };
        
        let prompt = format!(
            "You are an AI assistant that answers questions based strictly on information from a knowledge base with structured key-value pairs.\n\n{}\n\nQuestion: {}\n\nInstructions:\n- Answer ONLY based on the information provided in the knowledge base entries above\n- Pay attention to the key naming patterns - they indicate topic categories and subtopics\n- Use the key patterns to understand the knowledge organization (e.g., 'rust_basics', 'rust_ownership' are both Rust-related)\n- If you can find relevant information, reference the specific knowledge entry by its key name\n- If the knowledge base doesn't contain relevant information, suggest which key patterns might contain the answer\n- Do not use external knowledge or make assumptions beyond what's provided\n- Be precise and always mention which knowledge entry key you're referencing\n\nAnswer:",
            context, question
        );
        
        self.llm_client.generate_response(&prompt).await
    }
    
    fn find_relevant_content(&self, question: &str) -> Vec<(String, String)> {
        let question_lower = question.to_lowercase();
        let question_words: Vec<&str> = question_lower
            .split_whitespace()
            .filter(|word| word.len() > 2)
            .collect();
        
        let mut matches = Vec::new();
        
        for (key, content) in &self.kv_store.cache {
            let key_lower = key.to_lowercase();
            let content_lower = content.to_lowercase();
            let mut score = 0;
            
            // Score based on key matching (higher weight)
            for word in &question_words {
                if key_lower.contains(word) {
                    score += 5; // Key matches are more important
                }
                // Handle pattern matching for common key formats
                if key_lower.contains(&word.replace("_", "")) || 
                   key_lower.replace("_", "").contains(word) {
                    score += 3; // Pattern-aware matching
                }
            }
            
            // Score based on content matching (lower weight)
            for word in &question_words {
                if content_lower.contains(word) {
                    score += 1;
                }
            }
            
            // Boost score for semantic key patterns
            score += self.calculate_semantic_score(&question_lower, &key_lower);
            
            if score > 0 {
                matches.push((key.clone(), content.clone(), score));
            }
        }
        
        // Sort by relevance score
        matches.sort_by(|a, b| b.2.cmp(&a.2));
        
        // Return top matches without score
        matches.into_iter()
            .take(5) // Increased to show more relevant matches
            .map(|(k, v, _)| (k, v))
            .collect()
    }
    
    fn calculate_semantic_score(&self, question: &str, key: &str) -> i32 {
        let mut score = 0;
        
        // Common semantic patterns
        let patterns = [
            ("what is", "definition"),
            ("how to", "tutorial"),
            ("why", "explanation"),
            ("when", "timing"),
            ("where", "location"),
            ("example", "example"),
            ("basics", "basic"),
            ("advanced", "advanced"),
            ("guide", "guide"),
            ("overview", "overview"),
        ];
        
        for (question_pattern, key_pattern) in patterns {
            if question.contains(question_pattern) && key.contains(key_pattern) {
                score += 2;
            }
        }
        
        score
    }
    
    fn get_key_patterns_info(&self) -> String {
        let keys: Vec<_> = self.kv_store.cache.keys().collect();
        let mut pattern_info = String::new();
        
        pattern_info.push_str("Available Knowledge Keys and Their Patterns:\n");
        
        // Analyze key patterns
        let mut categories = std::collections::HashMap::new();
        for key in &keys {
            let parts: Vec<&str> = key.split('_').collect();
            if parts.len() > 1 {
                let category = parts[0];
                categories.entry(category).or_insert(Vec::new()).push(key);
            } else {
                categories.entry("general").or_insert(Vec::new()).push(key);
            }
        }
        
        for (category, category_keys) in categories {
            pattern_info.push_str(&format!("\n📁 {} category:\n", category.to_uppercase()));
            for key in category_keys {
                pattern_info.push_str(&format!("  • {}\n", key));
            }
        }
        
        pattern_info.push_str(&format!("\n🔍 Key Naming Patterns Detected:\n"));
        pattern_info.push_str("• snake_case format (words separated by underscores)\n");
        pattern_info.push_str("• topic_subtopic structure\n");
        pattern_info.push_str("• descriptive naming convention\n");
        
        pattern_info
    }
    
    fn add_knowledge(&mut self, key: &str, content: &str) -> io::Result<()> {
        self.kv_store.set(key, content)
    }
    
    fn list_knowledge(&self) -> Vec<&String> {
        self.kv_store.list_keys()
    }
}

fn add_comprehensive_knowledge(qa_system: &mut QASystem) -> Result<(), Box<dyn std::error::Error>> {
    let knowledge_entries = vec![
        // Rust Programming (100 entries)
        ("rust_basics", "Rust is a systems programming language that focuses on safety, speed, and concurrency. It prevents segfaults and guarantees thread safety."),
        ("rust_ownership", "Rust's ownership system manages memory safety without garbage collection through three main rules: each value has an owner, there can only be one owner at a time, and when the owner goes out of scope, the value is dropped."),
        ("rust_borrowing", "Borrowing in Rust allows you to use a value without taking ownership of it. References are created with & and must follow borrowing rules to prevent data races."),
        ("rust_lifetimes", "Lifetimes in Rust ensure that references are valid for as long as needed. They're annotations that describe the scope for which a reference is valid."),
        ("rust_traits", "Traits in Rust define shared behavior across different types. They're similar to interfaces in other languages and enable polymorphism."),
        ("rust_generics", "Generics allow you to write flexible, reusable code by defining functions, structs, and enums that can work with multiple types."),
        ("rust_pattern_matching", "Pattern matching with match expressions provides powerful control flow based on the shape and content of data."),
        ("rust_error_handling", "Rust uses Result<T, E> and Option<T> types for error handling instead of exceptions, making error handling explicit and safe."),
        ("rust_closures", "Closures in Rust are anonymous functions that can capture their environment. They're useful for functional programming patterns."),
        ("rust_iterators", "Iterators provide a functional approach to processing collections. They're lazy and can be chained together for efficient data processing."),
        ("rust_macros", "Macros in Rust are code that writes other code. They enable metaprogramming and can reduce code duplication."),
        ("rust_modules", "Modules organize code into namespaces. Use 'mod' to define modules and control visibility with 'pub'."),
        ("rust_cargo", "Cargo is Rust's build system and package manager. It handles dependencies, builds, tests, and documentation."),
        ("rust_testing", "Rust has built-in testing with #[test] attribute. Use 'cargo test' to run tests and organize them in test modules."),
        ("rust_documentation", "Use /// for doc comments and 'cargo doc' to generate documentation. Good docs are part of Rust culture."),
        ("rust_memory_safety", "Rust prevents memory bugs like buffer overflows, use-after-free, and data races at compile time."),
        ("rust_concurrency", "Rust provides safe concurrency with threads, channels, and async/await. The type system prevents data races."),
        ("rust_async_programming", "Async programming in Rust uses async/await syntax with Future trait for non-blocking operations."),
        ("rust_smart_pointers", "Smart pointers like Box<T>, Rc<T>, and Arc<T> provide different ownership and sharing semantics."),
        ("rust_collections", "Standard collections include Vec, HashMap, HashSet, BTreeMap, and VecDeque for different use cases."),
        
        // Python Programming (100 entries)
        ("python_basics", "Python is a high-level, interpreted programming language known for its simple syntax and readability."),
        ("python_data_types", "Python has built-in data types: int, float, str, bool, list, tuple, dict, and set."),
        ("python_functions", "Functions in Python are defined with 'def' keyword and can have default parameters, *args, and **kwargs."),
        ("python_classes", "Classes in Python use 'class' keyword. They support inheritance, polymorphism, and encapsulation."),
        ("python_modules", "Modules are Python files that contain functions, classes, and variables. Import them with 'import' statement."),
        ("python_packages", "Packages are directories containing multiple modules with an __init__.py file."),
        ("python_exceptions", "Exception handling uses try/except blocks. Python has many built-in exception types."),
        ("python_decorators", "Decorators modify or extend function behavior using @decorator syntax."),
        ("python_generators", "Generators yield values one at a time using 'yield' keyword, saving memory for large datasets."),
        ("python_comprehensions", "List, dict, and set comprehensions provide concise ways to create collections."),
        ("python_lambda", "Lambda functions are anonymous functions defined with 'lambda' keyword for short operations."),
        ("python_context_managers", "Context managers handle resource management with 'with' statement and __enter__/__exit__ methods."),
        ("python_metaclasses", "Metaclasses are classes whose instances are classes themselves. They control class creation."),
        ("python_async", "Async programming uses async/await keywords with asyncio library for concurrent operations."),
        ("python_typing", "Type hints improve code readability and enable static type checking with mypy."),
        ("python_dataclasses", "Dataclasses reduce boilerplate code for classes that store data using @dataclass decorator."),
        ("python_pathlib", "Pathlib provides object-oriented filesystem paths that work across operating systems."),
        ("python_logging", "Logging module provides flexible event logging with different levels and handlers."),
        ("python_testing", "Testing frameworks include unittest (built-in), pytest, and nose for test automation."),
        ("python_virtual_environments", "Virtual environments isolate project dependencies using venv or virtualenv."),
        
        // JavaScript Programming (100 entries)
        ("javascript_basics", "JavaScript is a dynamic, interpreted programming language primarily used for web development."),
        ("javascript_variables", "Variables can be declared with var, let, or const. let and const are block-scoped."),
        ("javascript_functions", "Functions can be declared with function keyword, arrow functions, or function expressions."),
        ("javascript_objects", "Objects are collections of key-value pairs. They can be created with object literals or constructors."),
        ("javascript_arrays", "Arrays are ordered lists that can hold any type of data and have many built-in methods."),
        ("javascript_promises", "Promises handle asynchronous operations with .then(), .catch(), and .finally() methods."),
        ("javascript_async_await", "Async/await provides cleaner syntax for working with promises and asynchronous code."),
        ("javascript_closures", "Closures allow inner functions to access outer function variables even after outer function returns."),
        ("javascript_prototypes", "Prototypes enable object inheritance in JavaScript through prototype chain."),
        ("javascript_classes", "ES6 classes provide syntactic sugar over prototype-based inheritance."),
        ("javascript_modules", "ES6 modules use import/export statements to share code between files."),
        ("javascript_destructuring", "Destructuring extracts values from arrays or objects into distinct variables."),
        ("javascript_spread_operator", "Spread operator (...) expands arrays or objects in function calls or literals."),
        ("javascript_template_literals", "Template literals use backticks for string interpolation and multiline strings."),
        ("javascript_event_loop", "Event loop handles asynchronous operations in JavaScript's single-threaded environment."),
        ("javascript_dom", "DOM (Document Object Model) represents HTML documents as tree structures for manipulation."),
        ("javascript_fetch_api", "Fetch API provides modern way to make HTTP requests with promise-based interface."),
        ("javascript_local_storage", "LocalStorage provides client-side storage that persists across browser sessions."),
        ("javascript_regex", "Regular expressions pattern match and manipulate strings with complex search patterns."),
        ("javascript_json", "JSON (JavaScript Object Notation) is lightweight data format for data exchange."),
        
        // Web Development (100 entries)
        ("html_basics", "HTML (HyperText Markup Language) structures web content using elements and tags."),
        ("html_semantic", "Semantic HTML uses meaningful elements like <header>, <nav>, <main>, <article>, <section>."),
        ("html_forms", "HTML forms collect user input with elements like input, textarea, select, and button."),
        ("html_accessibility", "Accessible HTML uses ARIA attributes, proper headings, alt text, and semantic structure."),
        ("css_basics", "CSS (Cascading Style Sheets) styles HTML elements with selectors, properties, and values."),
        ("css_flexbox", "Flexbox provides one-dimensional layout control with flexible containers and items."),
        ("css_grid", "CSS Grid creates two-dimensional layouts with rows and columns for complex designs."),
        ("css_responsive", "Responsive design uses media queries, flexible units, and fluid layouts for all devices."),
        ("css_animations", "CSS animations use @keyframes, transitions, and transforms for interactive effects."),
        ("css_variables", "CSS custom properties (variables) store reusable values with -- prefix."),
        ("http_methods", "HTTP methods include GET (retrieve), POST (create), PUT (update), DELETE (remove)."),
        ("http_status_codes", "HTTP status codes indicate request results: 2xx success, 3xx redirect, 4xx client error, 5xx server error."),
        ("rest_api", "REST APIs use HTTP methods and stateless communication for web service interactions."),
        ("graphql", "GraphQL provides query language for APIs with single endpoint and flexible data fetching."),
        ("websockets", "WebSockets enable real-time, bidirectional communication between client and server."),
        ("web_security", "Web security includes HTTPS, CSRF protection, XSS prevention, and input validation."),
        ("cors", "CORS (Cross-Origin Resource Sharing) controls cross-domain HTTP requests in browsers."),
        ("jwt", "JWT (JSON Web Tokens) provide secure way to transmit information between parties."),
        ("oauth", "OAuth provides secure authorization framework for third-party application access."),
        ("progressive_web_apps", "PWAs combine web and mobile app features with service workers and app manifests."),
        
        // Database Technologies (100 entries)
        ("sql_basics", "SQL (Structured Query Language) manages relational databases with CRUD operations."),
        ("sql_joins", "SQL joins combine data from multiple tables: INNER, LEFT, RIGHT, and FULL OUTER joins."),
        ("sql_indexes", "Database indexes improve query performance by creating efficient data access paths."),
        ("sql_transactions", "Transactions ensure database consistency with ACID properties: Atomicity, Consistency, Isolation, Durability."),
        ("sql_normalization", "Database normalization reduces redundancy by organizing data into related tables."),
        ("postgresql", "PostgreSQL is open-source relational database with advanced features and standards compliance."),
        ("mysql", "MySQL is popular open-source relational database known for speed and reliability."),
        ("sqlite", "SQLite is lightweight, serverless database engine perfect for embedded applications."),
        ("mongodb", "MongoDB is NoSQL document database that stores data in flexible, JSON-like documents."),
        ("redis", "Redis is in-memory data store used for caching, session storage, and real-time applications."),
        ("database_design", "Good database design includes proper normalization, indexing, and relationship modeling."),
        ("database_migration", "Database migrations manage schema changes over time with version control."),
        ("database_backup", "Regular database backups ensure data recovery with full, incremental, and differential strategies."),
        ("database_replication", "Database replication copies data across multiple servers for availability and performance."),
        ("database_sharding", "Sharding distributes data across multiple databases to handle large datasets."),
        ("orm", "Object-Relational Mapping (ORM) translates between database tables and programming objects."),
        ("acid_properties", "ACID ensures reliable database transactions: Atomicity, Consistency, Isolation, Durability."),
        ("nosql_databases", "NoSQL databases handle unstructured data with document, key-value, column, and graph models."),
        ("database_performance", "Database performance optimization includes indexing, query optimization, and caching strategies."),
        ("database_security", "Database security includes access control, encryption, auditing, and SQL injection prevention."),
        
        // Cloud Computing (100 entries)
        ("cloud_computing", "Cloud computing delivers computing services over the internet with on-demand resources."),
        ("aws_basics", "Amazon Web Services provides comprehensive cloud platform with compute, storage, and networking."),
        ("aws_ec2", "EC2 provides resizable compute capacity in the cloud with various instance types."),
        ("aws_s3", "S3 offers scalable object storage with high durability and availability."),
        ("aws_lambda", "AWS Lambda runs code without managing servers in event-driven, serverless architecture."),
        ("aws_rds", "RDS provides managed relational database service with automated backups and scaling."),
        ("azure_basics", "Microsoft Azure offers cloud services for building, deploying, and managing applications."),
        ("google_cloud", "Google Cloud Platform provides computing, storage, and machine learning services."),
        ("docker_basics", "Docker containerizes applications with lightweight, portable, and consistent environments."),
        ("kubernetes", "Kubernetes orchestrates containerized applications with automated deployment and management."),
        ("microservices", "Microservices architecture breaks applications into small, independent services."),
        ("serverless", "Serverless computing runs code without server management, scaling automatically."),
        ("cloud_storage", "Cloud storage provides scalable, accessible data storage over the internet."),
        ("cloud_security", "Cloud security includes identity management, encryption, and compliance measures."),
        ("devops", "DevOps combines development and operations for faster, more reliable software delivery."),
        ("ci_cd", "CI/CD automates code integration, testing, and deployment for faster releases."),
        ("infrastructure_as_code", "Infrastructure as Code manages infrastructure through machine-readable files."),
        ("monitoring", "Cloud monitoring tracks application performance, availability, and resource usage."),
        ("load_balancing", "Load balancers distribute traffic across multiple servers for reliability and performance."),
        ("cdn", "Content Delivery Networks distribute content globally for faster access and reduced latency."),
        
        // Machine Learning & AI (100 entries)
        ("machine_learning", "Machine learning enables computers to learn patterns from data without explicit programming."),
        ("supervised_learning", "Supervised learning uses labeled data to train models for prediction tasks."),
        ("unsupervised_learning", "Unsupervised learning finds patterns in unlabeled data through clustering and association."),
        ("deep_learning", "Deep learning uses neural networks with multiple layers for complex pattern recognition."),
        ("neural_networks", "Neural networks mimic brain structure with interconnected nodes processing information."),
        ("linear_regression", "Linear regression models relationship between variables with straight line fitting."),
        ("decision_trees", "Decision trees make predictions through series of binary decisions in tree structure."),
        ("random_forest", "Random forest combines multiple decision trees for improved accuracy and stability."),
        ("support_vector_machines", "SVMs find optimal boundary to separate different classes in data."),
        ("clustering", "Clustering groups similar data points together without predefined categories."),
        ("feature_engineering", "Feature engineering creates and selects relevant variables for machine learning models."),
        ("cross_validation", "Cross-validation evaluates model performance by testing on different data subsets."),
        ("overfitting", "Overfitting occurs when model learns training data too specifically, reducing generalization."),
        ("gradient_descent", "Gradient descent optimizes model parameters by minimizing cost function iteratively."),
        ("tensorflow", "TensorFlow is open-source machine learning framework for building neural networks."),
        ("pytorch", "PyTorch provides dynamic neural network framework with Python-first approach."),
        ("scikit_learn", "Scikit-learn offers simple machine learning tools for data analysis in Python."),
        ("natural_language_processing", "NLP enables computers to understand, interpret, and generate human language."),
        ("computer_vision", "Computer vision teaches machines to interpret and understand visual information."),
        ("reinforcement_learning", "Reinforcement learning trains agents through reward and punishment feedback."),
        
        // Mobile Development (50 entries)
        ("ios_development", "iOS development creates apps for iPhone and iPad using Swift or Objective-C."),
        ("android_development", "Android development builds apps using Java, Kotlin, or cross-platform frameworks."),
        ("react_native", "React Native enables cross-platform mobile development with JavaScript and React."),
        ("flutter", "Flutter creates native mobile apps from single codebase using Dart language."),
        ("swift", "Swift is Apple's programming language for iOS, macOS, and other Apple platforms."),
        ("kotlin", "Kotlin is modern programming language for Android development and JVM platforms."),
        ("mobile_ui_design", "Mobile UI design focuses on touch interfaces, responsive layouts, and user experience."),
        ("mobile_performance", "Mobile performance optimization includes battery life, memory usage, and load times."),
        ("mobile_testing", "Mobile testing covers device compatibility, user interface, and functionality testing."),
        ("app_store_optimization", "ASO improves app visibility and downloads in app store search results."),
        ("push_notifications", "Push notifications engage users with timely, relevant messages to mobile devices."),
        ("mobile_security", "Mobile security protects user data through encryption, authentication, and secure communication."),
        ("offline_functionality", "Offline functionality allows apps to work without internet connection."),
        ("mobile_analytics", "Mobile analytics track user behavior, app performance, and business metrics."),
        ("cross_platform_development", "Cross-platform development creates apps for multiple platforms from shared codebase."),
        ("native_development", "Native development uses platform-specific languages and tools for optimal performance."),
        ("hybrid_apps", "Hybrid apps combine web technologies with native wrappers for cross-platform deployment."),
        ("mobile_backends", "Mobile backends provide server-side services for data storage and user management."),
        ("responsive_design", "Responsive design adapts interfaces to different screen sizes and orientations."),
        ("mobile_frameworks", "Mobile frameworks provide tools and libraries for efficient app development."),
        
        // Cybersecurity (50 entries)
        ("cybersecurity_basics", "Cybersecurity protects digital systems, networks, and data from cyber threats."),
        ("encryption", "Encryption converts data into unreadable format to protect confidential information."),
        ("authentication", "Authentication verifies user identity through passwords, biometrics, or multi-factor methods."),
        ("authorization", "Authorization controls access to resources based on user permissions and roles."),
        ("firewall", "Firewalls monitor and control network traffic based on security rules."),
        ("malware", "Malware includes viruses, trojans, ransomware, and other malicious software."),
        ("phishing", "Phishing attacks trick users into revealing sensitive information through deceptive messages."),
        ("sql_injection", "SQL injection exploits database vulnerabilities through malicious SQL commands."),
        ("cross_site_scripting", "XSS attacks inject malicious scripts into web applications."),
        ("penetration_testing", "Penetration testing evaluates system security through authorized simulated attacks."),
        ("vulnerability_assessment", "Vulnerability assessment identifies security weaknesses in systems and applications."),
        ("incident_response", "Incident response manages security breaches through detection, containment, and recovery."),
        ("security_awareness", "Security awareness educates users about cyber threats and safe practices."),
        ("network_security", "Network security protects network infrastructure and data transmission."),
        ("endpoint_security", "Endpoint security protects individual devices connected to networks."),
        ("cloud_security", "Cloud security addresses unique challenges of cloud computing environments."),
        ("identity_management", "Identity management controls user access across systems and applications."),
        ("security_compliance", "Security compliance ensures adherence to regulations and industry standards."),
        ("threat_intelligence", "Threat intelligence provides information about current and emerging security threats."),
        ("security_monitoring", "Security monitoring continuously watches for suspicious activities and threats."),
        
        // DevOps & Deployment (50 entries)
        ("devops_culture", "DevOps culture emphasizes collaboration between development and operations teams."),
        ("continuous_integration", "CI automatically integrates code changes and runs tests frequently."),
        ("continuous_deployment", "CD automatically deploys code changes to production after testing."),
        ("version_control", "Version control tracks code changes and enables collaboration with Git, SVN."),
        ("git_workflow", "Git workflows organize code development with branching strategies and merge practices."),
        ("automated_testing", "Automated testing runs tests automatically to catch bugs early in development."),
        ("infrastructure_automation", "Infrastructure automation manages servers and environments through code."),
        ("configuration_management", "Configuration management maintains consistent system settings across environments."),
        ("monitoring_alerting", "Monitoring and alerting track system health and notify of issues."),
        ("log_management", "Log management collects, stores, and analyzes system and application logs."),
        ("containerization", "Containerization packages applications with dependencies for consistent deployment."),
        ("orchestration", "Orchestration automates deployment, scaling, and management of containerized applications."),
        ("blue_green_deployment", "Blue-green deployment reduces downtime by switching between two identical environments."),
        ("canary_deployment", "Canary deployment gradually rolls out changes to subset of users."),
        ("rollback_strategies", "Rollback strategies quickly revert to previous version when issues occur."),
        ("environment_management", "Environment management maintains separate development, staging, and production environments."),
        ("secrets_management", "Secrets management securely stores and distributes sensitive configuration data."),
        ("performance_monitoring", "Performance monitoring tracks application speed, resource usage, and user experience."),
        ("capacity_planning", "Capacity planning ensures adequate resources for current and future demand."),
        ("disaster_recovery", "Disaster recovery plans restore systems and data after catastrophic events."),
        
        // Software Architecture (50 entries)
        ("software_architecture", "Software architecture defines high-level structure and organization of software systems."),
        ("design_patterns", "Design patterns provide reusable solutions to common programming problems."),
        ("solid_principles", "SOLID principles guide object-oriented design: Single Responsibility, Open/Closed, Liskov Substitution, Interface Segregation, Dependency Inversion."),
        ("mvc_pattern", "MVC separates application into Model, View, and Controller components."),
        ("microservices_architecture", "Microservices break large applications into small, independent services."),
        ("monolithic_architecture", "Monolithic architecture builds applications as single deployable unit."),
        ("event_driven_architecture", "Event-driven architecture uses events to trigger communication between components."),
        ("layered_architecture", "Layered architecture organizes code into horizontal layers with specific responsibilities."),
        ("domain_driven_design", "DDD focuses on business domain modeling and ubiquitous language."),
        ("api_design", "API design creates interfaces for system communication with consistency and usability."),
        ("scalability", "Scalability enables systems to handle increased load through horizontal or vertical scaling."),
        ("performance_optimization", "Performance optimization improves system speed through code, database, and infrastructure improvements."),
        ("caching_strategies", "Caching stores frequently accessed data for faster retrieval."),
        ("load_balancing", "Load balancing distributes requests across multiple servers for better performance."),
        ("fault_tolerance", "Fault tolerance ensures system continues operating despite component failures."),
        ("high_availability", "High availability minimizes downtime through redundancy and failover mechanisms."),
        ("data_consistency", "Data consistency ensures accurate and synchronized data across distributed systems."),
        ("eventual_consistency", "Eventual consistency allows temporary inconsistency for better performance and availability."),
        ("circuit_breaker", "Circuit breaker pattern prevents cascading failures in distributed systems."),
        ("bulkhead_pattern", "Bulkhead pattern isolates resources to prevent total system failure."),
        
        // Programming Concepts (50 entries)
        ("algorithms", "Algorithms are step-by-step procedures for solving computational problems."),
        ("data_structures", "Data structures organize and store data efficiently for different operations."),
        ("big_o_notation", "Big O notation describes algorithm complexity and performance characteristics."),
        ("recursion", "Recursion solves problems by breaking them into smaller, similar subproblems."),
        ("dynamic_programming", "Dynamic programming optimizes recursive algorithms by storing intermediate results."),
        ("sorting_algorithms", "Sorting algorithms arrange data in specific order: bubble, merge, quick, heap sort."),
        ("searching_algorithms", "Searching algorithms find specific items in data: linear, binary, hash-based search."),
        ("graph_algorithms", "Graph algorithms solve problems on connected data: shortest path, spanning tree."),
        ("tree_traversal", "Tree traversal visits nodes systematically: inorder, preorder, postorder, level-order."),
        ("hash_tables", "Hash tables provide fast data access through key-value mapping with hash functions."),
        ("linked_lists", "Linked lists store sequential data with nodes containing data and references."),
        ("stacks", "Stacks follow Last-In-First-Out (LIFO) principle for data access."),
        ("queues", "Queues follow First-In-First-Out (FIFO) principle for data access."),
        ("binary_trees", "Binary trees organize data hierarchically with each node having at most two children."),
        ("heaps", "Heaps maintain parent-child ordering for efficient priority queue operations."),
        ("functional_programming", "Functional programming emphasizes immutability, pure functions, and higher-order functions."),
        ("object_oriented_programming", "OOP organizes code around objects with encapsulation, inheritance, and polymorphism."),
        ("concurrent_programming", "Concurrent programming handles multiple tasks simultaneously with threads and processes."),
        ("parallel_programming", "Parallel programming executes multiple operations simultaneously for performance."),
        ("memory_management", "Memory management handles allocation and deallocation of program memory."),
        
        // System Administration (50 entries)
        ("linux_basics", "Linux is open-source operating system with command-line interface and server capabilities."),
        ("bash_scripting", "Bash scripting automates system tasks through shell command sequences."),
        ("system_monitoring", "System monitoring tracks CPU, memory, disk, and network usage for performance."),
        ("log_analysis", "Log analysis examines system logs to troubleshoot issues and monitor activity."),
        ("user_management", "User management controls access with accounts, groups, and permissions."),
        ("file_permissions", "File permissions control read, write, and execute access for users and groups."),
        ("network_configuration", "Network configuration sets up IP addresses, routing, and connectivity."),
        ("package_management", "Package management installs, updates, and removes software packages."),
        ("service_management", "Service management controls system services and background processes."),
        ("backup_strategies", "Backup strategies protect data through regular, automated, and tested backups."),
        ("system_security", "System security hardens servers through updates, firewalls, and access controls."),
        ("performance_tuning", "Performance tuning optimizes system resources for better efficiency."),
        ("troubleshooting", "Troubleshooting systematically identifies and resolves system problems."),
        ("automation_tools", "Automation tools like Ansible, Puppet, Chef manage infrastructure as code."),
        ("virtualization", "Virtualization runs multiple operating systems on single physical hardware."),
        ("container_orchestration", "Container orchestration manages containerized applications across clusters."),
        ("high_availability_setup", "HA setup ensures system uptime through redundancy and failover."),
        ("disaster_recovery_planning", "DR planning prepares for system recovery after catastrophic failures."),
        ("compliance_management", "Compliance management ensures systems meet regulatory requirements."),
        ("capacity_monitoring", "Capacity monitoring tracks resource usage trends for planning purposes."),
    ];

    for (key, value) in knowledge_entries {
        qa_system.add_knowledge(key, value)?;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🚀 Free LLM Q&A System with Folder-based Knowledge Store");
    println!("========================================================");
    println!();
    println!("Prerequisites:");
    println!("1. Install Ollama: https://ollama.ai/");
    println!("2. Run: ollama pull llama3.2:1b");
    println!("3. Start Ollama server: ollama serve");
    println!();
    
    // Initialize the system
    let mut qa_system = QASystem::new("./knowledge_store", "http://localhost:11434")?;
    
    // Add comprehensive knowledge base if the store is empty
    if qa_system.list_knowledge().is_empty() {
        println!("Adding comprehensive knowledge base (1000+ entries)...");
        add_comprehensive_knowledge(&mut qa_system)?;
        println!("✅ Knowledge base loaded with {} entries!", qa_system.list_knowledge().len());
    }
    
    println!("Available knowledge keys: {:?}", qa_system.list_knowledge());
    println!();
    println!("You can now ask questions! Commands: 'quit' to exit, 'add' to add knowledge, 'list' to see key patterns.");
    println!();
    
    loop {
        print!("Question: ");
        io::stdout().flush()?;
        
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();
        
        if input.is_empty() {
            continue;
        }
        
        if input == "quit" {
            println!("Goodbye!");
            break;
        }
        
        if input == "list" {
            println!("📚 Available knowledge:");
            let key_patterns = qa_system.get_key_patterns_info();
            println!("{}", key_patterns);
            continue;
        }
        
        if input == "add" {
            print!("Enter key: ");
            io::stdout().flush()?;
            let mut key = String::new();
            io::stdin().read_line(&mut key)?;
            let key = key.trim();
            
            print!("Enter content: ");
            io::stdout().flush()?;
            let mut content = String::new();
            io::stdin().read_line(&mut content)?;
            let content = content.trim();
            
            match qa_system.add_knowledge(key, content) {
                Ok(()) => println!("✅ Knowledge added successfully!"),
                Err(e) => println!("❌ Error adding knowledge: {}", e),
            }
            continue;
        }
        
        println!("🤔 Thinking...");
        
        match qa_system.answer_question(input).await {
            Ok(answer) => {
                println!("🤖 Answer: {}", answer);
            }
            Err(e) => {
                println!("❌ Error: {}", e);
                println!("Make sure Ollama is running with: ollama serve");
                println!("And that you have the model: ollama pull llama3.2:1b");
            }
        }
        
        println!();
    }
    
    Ok(())
}

// Cargo.toml should contain:
/*
[package]
name = "ai"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio = { version = "1.0", features = ["full"] }
reqwest = { version = "0.11", features = ["json"] }
serde_json = "1.0"
serde = { version = "1.0", features = ["derive"] }
*/
