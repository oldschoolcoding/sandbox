use std::{
    env,
    fs,
    path::PathBuf,
};
use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use argon2::Argon2;
use argon2::password_hash::{rand_core::RngCore, SaltString};
use serde::{Deserialize, Serialize};
use crate::core::error::AppError;

const CONNECTIONS_FILE: &str = "connections.enc";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteConnection {
    pub name: String,
    pub username: String,
    pub host: String,
    pub port: u16,
    pub password: String,
}

#[derive(Serialize, Deserialize)]
struct ConnectionsData {
    salt: String,
    nonce: Vec<u8>,
    encrypted_data: Vec<u8>,
}

pub struct ConnectionManager {
    connections: Vec<RemoteConnection>,
    file_path: PathBuf,
    master_password: String,
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
            master_password: String::new(),
        })
    }

    fn derive_key(&self, password: &str, salt: &str) -> Result<[u8; 32], AppError> {
        let argon2 = Argon2::default();
        let salt_bytes = salt.as_bytes();
        let mut key = [0u8; 32];

        argon2
            .hash_password_into(password.as_bytes(), salt_bytes, &mut key)
            .map_err(|e| AppError::Password(format!("Key derivation failed: {}", e)))?;

        Ok(key)
    }

    fn encrypt_connections(&self, connections: &[RemoteConnection], password: &str) -> Result<ConnectionsData, AppError> {
        let json = serde_json::to_string(connections)?;

        let salt = SaltString::generate(&mut OsRng);
        let key = self.derive_key(password, salt.as_str())?;

        let cipher = Aes256Gcm::new(key.as_ref().into());
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let encrypted_data = cipher
            .encrypt(nonce, json.as_bytes())
            .map_err(|e| AppError::Encryption(format!("Encryption failed: {}", e)))?;

        Ok(ConnectionsData {
            salt: salt.as_str().to_string(),
            nonce: nonce_bytes.to_vec(),
            encrypted_data,
        })
    }

    fn decrypt_connections(&self, data: &ConnectionsData, password: &str) -> Result<Vec<RemoteConnection>, AppError> {
        let key = self.derive_key(password, &data.salt)?;

        let cipher = Aes256Gcm::new(key.as_ref().into());
        let nonce = Nonce::from_slice(&data.nonce);

        let decrypted = cipher
            .decrypt(nonce, data.encrypted_data.as_ref())
            .map_err(|e| AppError::Encryption(format!("Decryption failed (wrong password?): {}", e)))?;

        let json = String::from_utf8(decrypted)
            .map_err(|e| AppError::Encryption(format!("Invalid UTF-8: {}", e)))?;

        Ok(serde_json::from_str(&json)?)
    }

    pub fn load(&mut self, password: &str) -> Result<bool, AppError> {
        if !self.file_path.exists() {
            self.master_password = password.to_string();
            return Ok(false);
        }

        let contents = fs::read_to_string(&self.file_path)?;
        let data: ConnectionsData = serde_json::from_str(&contents)?;

        self.connections = self.decrypt_connections(&data, password)?;
        self.master_password = password.to_string();
        Ok(true)
    }

    pub fn save(&self) -> Result<(), AppError> {
        let data = self.encrypt_connections(&self.connections, &self.master_password)?;
        let json = serde_json::to_string_pretty(&data)?;
        fs::write(&self.file_path, json)?;
        Ok(())
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
