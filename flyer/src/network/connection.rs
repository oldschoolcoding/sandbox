use std::{
    env,
    fs,
    path::PathBuf,
};
use serde::{Deserialize, Serialize};
use tokio::time::{timeout, Duration};
use crate::core::error::AppError;

const CONNECTIONS_FILE: &str = "connections.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub enum ConnectionStatus {
    #[default]
    Unknown,
    Testing,
    Valid,
    Invalid,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteConnection {
    pub name: String,
    pub username: String,
    pub host: String,
    pub port: u16,
    pub password: String,
    #[serde(default)]
    pub status: ConnectionStatus,
}


#[derive(Clone)]
pub struct ConnectionManager {
    connections: Vec<RemoteConnection>,
    file_path: PathBuf,
}

impl ConnectionManager {
    pub fn new() -> Result<Self, AppError> {
        let exe_path = env::current_exe()?;
        let exe_dir = exe_path.parent().ok_or_else(|| {
            AppError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "Cannot find exe directory"))
        })?;
        let file_path = exe_dir.join(CONNECTIONS_FILE);

        Ok(Self {
            connections: Vec::new(),
            file_path,
        })
    }


    pub fn load(&mut self) -> Result<bool, AppError> {
        if !self.file_path.exists() {
            return Ok(false);
        }

        let contents = fs::read_to_string(&self.file_path)?;
        let mut connections: Vec<RemoteConnection> = serde_json::from_str(&contents)?;

        // Initialize all connections with Unknown status
        for conn in &mut connections {
            conn.status = ConnectionStatus::Unknown;
        }
        self.connections = connections;
        Ok(true)
    }

    pub fn save(&self) -> Result<(), AppError> {
        let json = serde_json::to_string_pretty(&self.connections)?;
        fs::write(&self.file_path, json)?;
        Ok(())
    }

    pub async fn test_connection(connection: &RemoteConnection) -> ConnectionStatus {
        let test_result = timeout(Duration::from_secs(5), async {
            match crate::filesystem::SftpFileSystem::new(
                &connection.username,
                &connection.host,
                connection.port,
                &connection.password,
            ) {
                Ok(_) => ConnectionStatus::Valid,
                Err(_) => ConnectionStatus::Invalid,
            }
        }).await;

        match test_result {
            Ok(status) => status,
            Err(_) => ConnectionStatus::Invalid, // Timeout
        }
    }

    pub fn get_connections_mut(&mut self) -> &mut Vec<RemoteConnection> {
        &mut self.connections
    }


    pub async fn test_all_connections(&mut self) {
        // Set all connections to testing status
        for connection in &mut self.connections {
            connection.status = ConnectionStatus::Testing;
        }

        // Test connections sequentially with timeout
        for connection in &mut self.connections {
            connection.status = Self::test_connection(connection).await;
        }
    }

    pub fn add_connection(&mut self, conn: RemoteConnection) -> Result<(), AppError> {
        self.connections.push(conn);
        self.save()
    }

    pub fn delete_connection(&mut self, index: usize) -> Result<(), AppError> {
        if index < self.connections.len() {
            self.connections.remove(index);
            self.save()?;
        }
        Ok(())
    }

    pub fn get_connections(&self) -> &[RemoteConnection] {
        &self.connections
    }
}
