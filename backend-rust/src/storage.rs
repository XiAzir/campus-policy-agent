use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

pub const BLOCK_BYTES: usize = 1024 * 1024;
pub const RESERVE_BYTES: u64 = 128 * 1024 * 1024; // 128 MiB 保留余量

pub fn hash_file(path: impl AsRef<Path>) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; BLOCK_BYTES];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn require_space(path: impl AsRef<Path>, needed: u64) -> Result<(), String> {
    let p = path.as_ref();
    if let Err(e) = std::fs::create_dir_all(p) {
        return Err(format!("创建目录失败: {}", e));
    }

    let free_space = disk_space(p).map_err(|e| e.to_string())?.0;
    if free_space < needed.saturating_add(RESERVE_BYTES) {
        return Err("磁盘空间不足：无法容纳临时文件、索引与回滚余量".to_string());
    }
    Ok(())
}

pub fn disk_space(path: &Path) -> io::Result<(u64, u64)> {
    let p = path.canonicalize()?;
    let sys = sysinfo::Disks::new_with_refreshed_list();
    sys
        .iter()
        .filter(|d| p.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().components().count())
        .map(|d| (d.available_space(), d.total_space()))
        .ok_or_else(|| io::Error::other("无法确定数据盘剩余空间"))
}

pub fn directory_bytes(path: &Path) -> io::Result<u64> {
    fn visit(path: &Path, depth: usize) -> io::Result<u64> {
        if depth > 32 { return Err(io::Error::other("数据目录层级超过上限")); }
        let mut total = 0u64;
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            // Never follow links outside the data directory.
            total = total.saturating_add(if kind.is_dir() { visit(&entry.path(), depth + 1)? }
                else if kind.is_file() { entry.metadata()?.len() } else { 0 });
        }
        Ok(total)
    }
    visit(path, 0)
}

pub fn cpu_seconds() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        static TICKS: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();
        let ticks = TICKS.get_or_init(|| {
            let output = std::process::Command::new("getconf").arg("CLK_TCK").output().ok()?;
            if !output.status.success() { return None; }
            let value = String::from_utf8(output.stdout).ok()?.trim().parse::<f64>().ok()?;
            (value > 0.0).then_some(value)
        });
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        let (_, fields) = stat.rsplit_once(") ")?;
        let fields: Vec<_> = fields.split_whitespace().collect();
        let user = fields.get(11)?.parse::<u64>().ok()?;
        let system = fields.get(12)?.parse::<u64>().ok()?;
        Some((user + system) as f64 / (*ticks)?)
    }
    #[cfg(not(target_os = "linux"))]
    { None }
}

#[derive(Default)]
pub struct StagedFiles { paths: Vec<std::path::PathBuf> }

impl StagedFiles {
    pub fn copy(&mut self, source: &Path, dest: &Path) -> io::Result<()> {
        if dest.exists() {
            if hash_file(source)? != hash_file(dest)? {
                return Err(io::Error::other("已有文件内容与资料包不一致"));
            }
            return Ok(());
        }
        let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(dest)?;
        self.paths.push(dest.to_owned());
        io::copy(&mut File::open(source)?, &mut out)?;
        out.sync_all()
    }
    pub fn commit(mut self) { self.paths.clear(); }
}

impl Drop for StagedFiles {
    fn drop(&mut self) {
        for path in &self.paths { let _ = std::fs::remove_file(path); }
    }
}
