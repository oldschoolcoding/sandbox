use std::{
    any::Any,
    fs,
    io::Read,
    path::{Path, PathBuf},
    os::unix::fs::{MetadataExt, PermissionsExt},
};
use ssh2::Sftp;
use crate::core::error::AppError;

// Constants for Unix file type masks
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    pub is_dir: bool,
}

#[derive(Clone, Debug)]
pub struct FileProperties {
    pub type_and_permissions: String,
    pub user: String,
    pub group: String,
    pub size: String,
    pub modified: String,
}

pub trait FileSystemOperations: Any {
    fn read_dir(&self, p: &Path) -> Result<Vec<FileEntry>, AppError>;
    fn get_start_path(&self) -> Result<PathBuf, AppError>;
    fn path_to_string(&self, p: &Path) -> String;
    #[allow(dead_code)]
    fn open_file(&self, p: &Path) -> Result<Box<dyn Read>, AppError>;
    #[allow(dead_code)]
    fn read_file_head(&self, p: &Path) -> Result<Vec<u8>, AppError>;
    fn read_file_chunk(&self, p: &Path, s: usize, c: usize) -> Result<Vec<String>, AppError>;
    fn get_file_properties(&self, p: &Path) -> Result<FileProperties, AppError>;
    fn as_any(&self) -> &dyn Any;
}

pub struct LocalFileSystem;

fn format_permissions(metadata: &std::fs::Metadata) -> String {
    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode();
        let file_type = metadata.mode() & S_IFMT;
        let t = if file_type == S_IFLNK {
            'l'
        } else if metadata.is_dir() {
            'd'
        } else {
            '-'
        };
        let r = |mask| if mode & mask != 0 { 'r' } else { '-' };
        let w = |mask| if mode & mask != 0 { 'w' } else { '-' };
        let x = |mask| if mode & mask != 0 { 'x' } else { '-' };
        format!(
            "{}{}{}{}{}{}{}{}{}{}",
            t,
            r(0o400), w(0o200), x(0o100),
            r(0o040), w(0o020), x(0o010),
            r(0o004), w(0o002), x(0o001)
        )
    }
    #[cfg(not(unix))]
    {
        "----------".into()
    }
}

impl FileSystemOperations for LocalFileSystem {
    fn read_dir(&self, p: &Path) -> Result<Vec<FileEntry>, AppError> {
        let mut entries: Vec<_> = fs::read_dir(p)?
            .filter_map(|res| res.ok())
            .map(|e| {
                let path = e.path();
                let is_dir = e.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
                FileEntry { path, is_dir }
            })
            .collect();
        entries.sort_by(|a, b| {
            (a.is_dir, &a.path).cmp(&(b.is_dir, &b.path))
        });
        Ok(entries)
    }

    fn get_start_path(&self) -> Result<PathBuf, AppError> {
        Ok(std::env::current_dir()?)
    }

    fn path_to_string(&self, p: &Path) -> String {
        p.display().to_string()
    }

    fn open_file(&self, p: &Path) -> Result<Box<dyn Read>, AppError> {
        Ok(Box::new(std::fs::File::open(p)?))
    }

    fn read_file_head(&self, p: &Path) -> Result<Vec<u8>, AppError> {
        let mut f = std::fs::File::open(p)?;
        let mut buf = vec![0; 512];
        let n = f.read(&mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn read_file_chunk(&self, p: &Path, start: usize, count: usize) -> Result<Vec<String>, AppError> {
        use std::io::BufRead;
        let f = std::fs::File::open(p)?;
        let reader = std::io::BufReader::new(f);
        let mut lines = Vec::with_capacity(count);
        for (i, line) in reader.lines().enumerate() {
            if i < start { continue; }
            if i >= start + count { break; }
            lines.push(line?);
        }
        Ok(lines)
    }

    fn get_file_properties(&self, p: &Path) -> Result<FileProperties, AppError> {
        let metadata = p.metadata()?;
        let permissions = format_permissions(&metadata);
        let modified = {
            use chrono::{DateTime, Local};
            let dt: DateTime<Local> = metadata.modified()?.into();
            dt.format("%Y-%m-%d %H:%M").to_string()
        };
        Ok(FileProperties {
            type_and_permissions: permissions,
            user: users::get_user_by_uid(metadata.uid()).map(|u| u.name().to_string_lossy().into_owned()).unwrap_or_else(|| metadata.uid().to_string()),
            group: users::get_group_by_gid(metadata.gid()).map(|g| g.name().to_string_lossy().into_owned()).unwrap_or_else(|| metadata.gid().to_string()),
            size: metadata.len().to_string(),
            modified,
        })
    }

    fn as_any(&self) -> &dyn Any { self }
}

pub struct SftpFileSystem {
    pub session: ssh2::Session,
    pub sftp: Sftp,
    pub remote_host: String,
    pub username: String,
    pub password: String,
    pub port: u16,
}

impl SftpFileSystem {
    pub fn new(user: &str, host: &str, port: u16, password: &str) -> Result<Self, AppError> {
        let addr = format!("{}:{}", host, port);
        let tcp = std::net::TcpStream::connect(&addr)?;
        let mut sess = ssh2::Session::new()?;
        sess.set_tcp_stream(tcp);
        sess.handshake()?;
        sess.userauth_password(user, password)?;
        if !sess.authenticated() {
            return Err(AppError::Navigation("Authentication failed".into()));
        }
        let sftp = sess.sftp()?;
        Ok(Self {
            session: sess,
            sftp,
            remote_host: host.to_string(),
            username: user.to_string(),
            password: password.to_string(),
            port,
        })
    }
}

fn format_permissions_sftp(stat: &ssh2::FileStat) -> String {
    #[cfg(unix)]
    {
        let mode = stat.perm.unwrap_or(0);
        let file_type = mode & S_IFMT;
        let t = if file_type == S_IFLNK {
            'l'
        } else if file_type == S_IFDIR {
            'd'
        } else {
            '-'
        };
        let r = |mask| if mode & mask != 0 { 'r' } else { '-' };
        let w = |mask| if mode & mask != 0 { 'w' } else { '-' };
        let x = |mask| if mode & mask != 0 { 'x' } else { '-' };
        format!(
            "{t}{}{}{}{}{}{}{}{}{}",
            r(0o400), w(0o200), x(0o100),
            r(0o040), w(0o020), x(0o010),
            r(0o004), w(0o002), x(0o001)
        )
    }
    #[cfg(not(unix))]
    {
        "----------".into()
    }
}

impl FileSystemOperations for SftpFileSystem {
    fn read_dir(&self, p: &Path) -> Result<Vec<FileEntry>, AppError> {
        let mut entries = vec![];
        for (path, stat) in self.sftp.readdir(p)? {
            let mode = stat.perm.unwrap_or(0);
            let file_type = mode & S_IFMT;
            let is_dir = file_type == S_IFDIR;
            entries.push(FileEntry { path, is_dir });
        }
        entries.sort_by(|a, b| {
            (a.is_dir, &a.path).cmp(&(b.is_dir, &b.path))
        });
        Ok(entries)
    }

    fn get_start_path(&self) -> Result<PathBuf, AppError> {
        Ok(PathBuf::from("."))
    }

    fn path_to_string(&self, p: &Path) -> String {
        p.display().to_string()
    }

    fn open_file(&self, p: &Path) -> Result<Box<dyn Read>, AppError> {
        Ok(Box::new(self.sftp.open(p)?))
    }

    fn read_file_head(&self, p: &Path) -> Result<Vec<u8>, AppError> {
        let mut f = self.sftp.open(p)?;
        let mut buf = vec![0; 512];
        let n = f.read(&mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn read_file_chunk(&self, p: &Path, start: usize, count: usize) -> Result<Vec<String>, AppError> {
        use std::io::BufRead;
        let f = self.sftp.open(p)?;
        let reader = std::io::BufReader::new(f);
        let mut lines = Vec::with_capacity(count);
        for (i, line) in reader.lines().enumerate() {
            if i < start { continue; }
            if i >= start + count { break; }
            lines.push(line?);
        }
        Ok(lines)
    }

    fn get_file_properties(&self, p: &Path) -> Result<FileProperties, AppError> {
        let stat = self.sftp.stat(p)?;
        let perm = format_permissions_sftp(&stat);
        let modified = {
            use chrono::{DateTime, Local};
            let dt: DateTime<Local> = std::time::SystemTime::UNIX_EPOCH
                .checked_add(std::time::Duration::from_secs(stat.mtime.unwrap_or(0) as u64))
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                .into();
            dt.format("%Y-%m-%d %H:%M").to_string()
        };
        Ok(FileProperties {
            type_and_permissions: perm,
            user: stat.uid.unwrap_or(0).to_string(),
            group: stat.gid.unwrap_or(0).to_string(),
            size: stat.size.unwrap_or(0).to_string(),
            modified,
        })
    }

    fn as_any(&self) -> &dyn Any { self }
}
