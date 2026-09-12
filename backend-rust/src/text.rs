//! Bounded source-window reads, isolated from Tokio's reactor.
use std::io::{self, BufRead, BufReader, Read};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use tokio::sync::Semaphore;

pub const MAX_WINDOW_BYTES: usize = 256 * 1024;
static READERS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(2)));

pub async fn read_lines(path: PathBuf, first: i64, last: i64) -> io::Result<Vec<String>> {
    if first < 1 || last < first {
        return Ok(Vec::new());
    }
    let permit = READERS
        .clone()
        .try_acquire_owned()
        .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "原文读取繁忙，请重试"))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let file = std::fs::File::open(path)?;
        if file.metadata()?.len() > crate::pkgfmt::MAX_TEXT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "原文超过单文档大小上限",
            ));
        }
        let mut reader = BufReader::new(file.take(crate::pkgfmt::MAX_TEXT_BYTES + 1));
        let mut output = Vec::new();
        let mut line = String::new();
        let mut used = 0usize;
        for number in 1..=last {
            line.clear();
            let count = reader
                .by_ref()
                .take(MAX_WINDOW_BYTES as u64 + 1)
                .read_line(&mut line)?;
            if count == 0 {
                break;
            }
            if count > MAX_WINDOW_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "单行原文过长，请下载原文件查看",
                ));
            }
            if number < first {
                continue;
            }
            if line.ends_with('\n') {
                line.pop();
                if line.ends_with('\r') {
                    line.pop();
                }
            }
            used = used.saturating_add(line.len());
            if used > MAX_WINDOW_BYTES || output.len() >= 201 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "原文窗口过大，请缩小行号范围或下载原文件",
                ));
            }
            output.push(line.clone());
        }
        Ok(output)
    })
    .await
    .map_err(io::Error::other)?
}
