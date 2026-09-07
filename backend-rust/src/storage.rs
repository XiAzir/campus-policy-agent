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

    let sys = sysinfo::Disks::new_with_refreshed_list();
    let free_space = sys
        .iter()
        .find(|d| p.starts_with(d.mount_point()))
        .map(|d| d.available_space())
        .unwrap_or(u64::MAX);

    if free_space < needed + RESERVE_BYTES {
        return Err("磁盘空间不足：无法容纳临时文件、索引与回滚余量".to_string());
    }
    Ok(())
}
