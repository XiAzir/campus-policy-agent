use crate::config::Config;
use crate::db::{DbPool, utcnow};
use crate::storage::{hash_file, require_space};
use crate::vectors::validate_npy_file;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};

pub const BACKUP_FORMAT: &str = "campus-policy-backup";
pub const BACKUP_VERSION: u32 = 2;
pub const SUBDIRS: [&str; 4] = ["files", "text", "vectors", "packages"];
pub const MAX_EXPANDED: u64 = 10 * 1024 * 1024 * 1024; // 10 GiB
pub const MAX_ENTRIES: usize = 100_000;
pub const MAX_META: u64 = 16 * 1024 * 1024; // 16 MiB

#[derive(Debug)]
pub enum BackupError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Zip(zip::result::ZipError),
    Validation(String),
    Rollback(String),
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO 错误: {}", e),
            Self::Sqlite(e) => write!(f, "数据库错误: {}", e),
            Self::Zip(e) => write!(f, "压缩包错误: {}", e),
            Self::Validation(msg) => write!(f, "备份校验失败: {}", msg),
            Self::Rollback(msg) => write!(f, "恢复回滚失败: {}", msg),
        }
    }
}

impl std::error::Error for BackupError {}

impl From<std::io::Error> for BackupError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<rusqlite::Error> for BackupError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

impl From<zip::result::ZipError> for BackupError {
    fn from(e: zip::result::ZipError) -> Self {
        Self::Zip(e)
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BackupFileRecord {
    pub size: u64,
    pub sha256: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BackupCounts {
    pub documents: i64,
    pub chunks: i64,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BackupMeta {
    pub format: String,
    pub version: u32,
    pub created_at: String,
    pub counts: BackupCounts,
    pub files: HashMap<String, BackupFileRecord>,
}

pub fn restore_marker_path(data_dir: &Path) -> PathBuf {
    let parent = data_dir.parent().unwrap_or_else(|| Path::new("."));
    let dir_name = data_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("data");
    parent.join(format!("{}.restore-in-progress", dir_name))
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// 获取数据库所引用的全部不可变物理文件路径集合（相对于 data_dir）
pub fn required_files_from_conn(conn: &Connection) -> Result<HashSet<String>, BackupError> {
    let mut files = HashSet::new();
    files.insert("db.sqlite".to_string());

    let mut doc_stmt = conn.prepare("SELECT doc_hash, original_filename FROM documents")?;
    let mut doc_rows = doc_stmt.query([])?;
    while let Some(row) = doc_rows.next()? {
        let doc_hash: String = row.get(0)?;
        let orig_name: String = row.get(1)?;
        let ext = Path::new(&orig_name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e.to_lowercase()))
            .unwrap_or_default();

        if !is_sha256_hex(&doc_hash) || !matches!(ext.as_str(), ".pdf" | ".docx" | ".txt" | ".md") {
            return Err(BackupError::Validation(
                "备份文档标识或文件类型不合法".to_string(),
            ));
        }
        files.insert(format!("files/{}{}", doc_hash, ext));
        files.insert(format!("text/{}.txt", doc_hash));
    }

    let mut pkg_stmt = conn.prepare("SELECT id, sha256, status FROM packages")?;
    let mut pkg_rows = pkg_stmt.query([])?;
    while let Some(row) = pkg_rows.next()? {
        let pkg_id: i64 = row.get(0)?;
        let sha256: String = row.get(1)?;
        let status: String = row.get(2)?;

        if !is_sha256_hex(&sha256) {
            return Err(BackupError::Validation("备份资料包哈希不合法".to_string()));
        }
        files.insert(format!("packages/{}.zip", sha256));
        let vec_name = if status == "published" {
            format!("pkg-{}", pkg_id)
        } else {
            format!("pkg-draft-{}", &sha256[..16])
        };
        files.insert(format!("vectors/{}.npy", vec_name));
    }

    Ok(files)
}

/// 执行一致性备份导出，生成 format=campus-policy-backup, version=2 的 zip 归档
pub async fn create_backup(
    db: &DbPool,
    config: &Config,
    out_path: &Path,
) -> Result<PathBuf, BackupError> {
    let data_dir = config.data_dir.clone();
    let parent = data_dir.parent().unwrap_or_else(|| Path::new("."));

    // 预检空间：至少为当前 db 大小 2 倍
    let db_path = data_dir.join("campus.db");
    let db_size = db_path
        .metadata()
        .map(|m| m.len())
        .unwrap_or(10 * 1024 * 1024);
    require_space(parent, db_size * 2).map_err(|e| BackupError::Validation(e.to_string()))?;

    let tmp_dir = tempfile::Builder::new()
        .prefix("cpb-backup-")
        .tempdir_in(parent)?;
    let root = tmp_dir.path();

    // 1. 使用 SQLite Backup API 创建 db.sqlite 一致快照
    let snapshot_db = root.join("db.sqlite");
    db.write({
        let snapshot_db = snapshot_db.clone();
        move |conn| {
            let mut dst_conn = Connection::open(&snapshot_db)?;
            let backup = rusqlite::backup::Backup::new(conn, &mut dst_conn)?;
            backup.run_to_completion(256, std::time::Duration::from_millis(50), None)?;
            Ok(())
        }
    })
    .await?;

    // 2. 只读检查快照并收集文件列表
    let (names, counts) = {
        let conn = Connection::open_with_flags(
            &snapshot_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        let names = required_files_from_conn(&conn)?;
        let doc_count: i64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?;
        let chunk_count: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
        (
            names,
            BackupCounts {
                documents: doc_count,
                chunks: chunk_count,
            },
        )
    };

    // 3. 复制或硬链接文件至暂存目录并计算所需大小
    let mut total_files_size = 0u64;
    for name in &names {
        if name == "db.sqlite" {
            total_files_size += snapshot_db.metadata()?.len();
            continue;
        }
        let src = data_dir.join(name);
        if !src.exists() {
            return Err(BackupError::Validation(format!(
                "缺少数据库引用的文件: {}",
                name
            )));
        }
        let dst = root.join(name);
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        if std::fs::hard_link(&src, &dst).is_err() {
            std::fs::copy(&src, &dst)?;
        }
        total_files_size += dst.metadata()?.len();
    }

    require_space(
        parent,
        total_files_size + total_files_size / 100 + 1024 * 1024,
    )
    .map_err(|e| BackupError::Validation(e.to_string()))?;

    // 4. 生成 backup.json 清单
    let mut file_records = HashMap::new();
    for name in &names {
        let file_path = root.join(name);
        let size = file_path.metadata()?.len();
        let sha256 = hash_file(&file_path)?;
        file_records.insert(name.clone(), BackupFileRecord { size, sha256 });
    }

    let meta = BackupMeta {
        format: BACKUP_FORMAT.to_string(),
        version: BACKUP_VERSION,
        created_at: utcnow(),
        counts,
        files: file_records,
    };
    let meta_bytes =
        serde_json::to_vec_pretty(&meta).map_err(|e| BackupError::Validation(e.to_string()))?;

    // 5. 写入最终 ZIP 归档
    let out_file = File::create(out_path)?;
    let mut zip = zip::ZipWriter::new(out_file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    zip.start_file("backup.json", options)?;
    zip.write_all(&meta_bytes)?;

    let mut sorted_names: Vec<&String> = names.iter().collect();
    sorted_names.sort();

    let mut buf = vec![0u8; 1024 * 1024]; // 1 MiB 缓冲区
    for name in sorted_names {
        zip.start_file(name, options)?;
        let mut f = File::open(root.join(name))?;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            zip.write_all(&buf[..n])?;
        }
    }

    zip.finish()?;
    Ok(out_path.to_path_buf())
}

/// 检查并安全验证备份 ZIP 结构和清单，返回 (meta, total_size)
pub fn inspect_backup_archive<R: Read + Seek>(reader: R) -> Result<(BackupMeta, u64), BackupError> {
    let mut zip = zip::ZipArchive::new(reader)?;
    let count = zip.len();
    if count > MAX_ENTRIES {
        return Err(BackupError::Validation("备份条目过多".to_string()));
    }

    let mut total_uncompressed: u64 = 0;
    let mut total_compressed: u64 = 0;
    let mut names = HashSet::new();

    for i in 0..count {
        let file = zip.by_index(i)?;
        let name = file.name().to_string();

        if names.contains(&name) {
            return Err(BackupError::Validation(format!("备份条目重复: {}", name)));
        }
        names.insert(name.clone());

        // 路径安全检查
        if name.contains('\\')
            || name.contains(':')
            || name.starts_with('/')
            || name
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
        {
            return Err(BackupError::Validation(format!(
                "备份包含不合法路径: {}",
                name
            )));
        }

        // 目录白名单检查
        let parts: Vec<&str> = name.split('/').collect();
        let is_valid = name == "backup.json"
            || name == "db.sqlite"
            || (parts.len() == 2 && SUBDIRS.contains(&parts[0]));

        if !is_valid {
            return Err(BackupError::Validation(format!(
                "备份包含白名单外条目: {}",
                name
            )));
        }

        let u_size = file.size();
        let c_size = file.compressed_size();
        total_uncompressed += u_size;
        total_compressed += c_size;
    }

    if total_uncompressed > MAX_EXPANDED {
        return Err(BackupError::Validation(
            "备份解压总规模超过 10 GiB 限制".to_string(),
        ));
    }

    let ratio = total_uncompressed as f64 / total_compressed.max(1) as f64;
    if ratio > 200.0 {
        return Err(BackupError::Validation(
            "备份压缩比超过 200 限制（疑似解压炸弹）".to_string(),
        ));
    }

    // 检查 backup.json
    let meta: BackupMeta = {
        let mut meta_file = zip
            .by_name("backup.json")
            .map_err(|_| BackupError::Validation("备份缺失 backup.json".to_string()))?;

        if meta_file.size() > MAX_META {
            return Err(BackupError::Validation(
                "backup.json 元数据超过 16 MiB 限制".to_string(),
            ));
        }

        let mut meta_bytes = Vec::new();
        meta_file.read_to_end(&mut meta_bytes)?;
        serde_json::from_slice(&meta_bytes)
            .map_err(|e| BackupError::Validation(format!("backup.json 格式错误: {}", e)))?
    };

    if meta.format != BACKUP_FORMAT || meta.version != BACKUP_VERSION {
        return Err(BackupError::Validation(
            "备份格式或版本不支持，请使用新版程序重新导出".to_string(),
        ));
    }

    // 校验清单与条目集合一致性
    let mut expected_entries = names.clone();
    expected_entries.remove("backup.json");

    let inventory_keys: HashSet<String> = meta.files.keys().cloned().collect();
    if inventory_keys != expected_entries {
        return Err(BackupError::Validation(
            "备份清单与实际文件条目不一致".to_string(),
        ));
    }

    for (name, rec) in &meta.files {
        let info = zip.by_name(name)?;
        if info.size() != rec.size || !is_sha256_hex(&rec.sha256) {
            return Err(BackupError::Validation(format!(
                "备份文件大小或哈希记录不合法: {}",
                name
            )));
        }
    }

    Ok((meta, total_uncompressed))
}

/// 深入校验已解压至 root 目录的快照数据
pub fn check_unpacked_snapshot(
    root: &Path,
    config: &Config,
    meta: &BackupMeta,
) -> Result<(), BackupError> {
    let db_path = root.join("db.sqlite");
    if !db_path.exists() {
        return Err(BackupError::Validation("缺失 db.sqlite".to_string()));
    }

    let conn = Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;

    // 1. 表结构校验
    let required_tables = [
        "settings",
        "packages",
        "documents",
        "sections",
        "doc_tags",
        "chunks",
        "chunks_fts",
        "vector_rows",
        "audit_log",
    ];
    for tbl in required_tables {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
            [tbl],
            |r| r.get(0),
        )?;
        if count == 0 {
            return Err(BackupError::Validation(format!(
                "备份数据库表结构不完整，缺少表: {}",
                tbl
            )));
        }
    }

    // 2. 检查 audience_scope 字段
    let mut stmt = conn.prepare("PRAGMA table_info(documents)")?;
    let has_audience_scope = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .any(|col| col.map(|c| c == "audience_scope").unwrap_or(false));
    if !has_audience_scope {
        return Err(BackupError::Validation(
            "备份数据库缺少适用范围字段，请升级原实例后重新导出".to_string(),
        ));
    }

    // 3. SQLite integrity_check 与 foreign_key_check
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if integrity != "ok" {
        return Err(BackupError::Validation(
            "备份数据库完整性校验失败".to_string(),
        ));
    }
    let fk_violations: Vec<String> = conn
        .prepare("PRAGMA foreign_key_check")?
        .query_map([], |r| r.get(0))?
        .filter_map(Result::ok)
        .collect();
    if !fk_violations.is_empty() {
        return Err(BackupError::Validation(
            "备份数据库外键检查失败".to_string(),
        ));
    }

    // 4. 引用文件及哈希校验
    let required = required_files_from_conn(&conn)?;
    for rel_path in &required {
        let full_path = root.join(rel_path);
        if !full_path.is_file() {
            return Err(BackupError::Validation(format!(
                "备份缺少数据库引用的文件: {}",
                rel_path
            )));
        }
        let expected_sha = meta
            .files
            .get(rel_path)
            .map(|r| r.sha256.as_str())
            .ok_or_else(|| BackupError::Validation(format!("清单缺少文件记录: {}", rel_path)))?;
        let actual_sha = hash_file(&full_path)?;
        if actual_sha != expected_sha {
            return Err(BackupError::Validation(format!(
                "备份文件哈希校验失败: {}",
                rel_path
            )));
        }
    }

    // 5. 令牌密钥设置
    let secret: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key='token_secret'",
            [],
            |r| r.get(0),
        )
        .ok();
    let is_valid_secret = secret.as_deref().map(is_sha256_hex).unwrap_or(false);
    if !is_valid_secret {
        return Err(BackupError::Validation(
            "备份缺少有效的令牌签名设置".to_string(),
        ));
    }

    // 6. 逐文档校验：行数与原文哈希
    let mut doc_stmt =
        conn.prepare("SELECT doc_hash, original_filename, text_sha256, line_count FROM documents")?;
    let mut doc_rows = doc_stmt.query([])?;
    while let Some(row) = doc_rows.next()? {
        let doc_hash: String = row.get(0)?;
        let text_sha256: String = row.get(2)?;
        let line_count: i64 = row.get(3)?;

        let text_path = root.join("text").join(format!("{}.txt", doc_hash));
        let actual_sha = hash_file(&text_path)?;
        if actual_sha != text_sha256 {
            return Err(BackupError::Validation(
                "备份原文件或标准化原文哈希不匹配".to_string(),
            ));
        }

        let f = File::open(&text_path)?;
        let lines = BufReader::new(f).lines().count() as i64;
        if lines != line_count {
            return Err(BackupError::Validation("备份原文行号不匹配".to_string()));
        }
    }

    // 7. 关联完整性：chunks <-> vector_rows <-> documents, chunks <-> chunks_fts
    let missing_vec: i64 = conn.query_row(
        "SELECT COUNT(*) FROM chunks c LEFT JOIN vector_rows v ON c.id=v.chunk_id WHERE v.chunk_id IS NULL",
        [],
        |r| r.get(0),
    )?;
    if missing_vec > 0 {
        return Err(BackupError::Validation("备份分块缺少向量关联".to_string()));
    }

    let inconsistent_vec: i64 = conn.query_row(
        "SELECT COUNT(*) FROM vector_rows v \
         LEFT JOIN chunks c ON c.id=v.chunk_id \
         LEFT JOIN documents d ON d.id=c.doc_id \
         WHERE c.id IS NULL OR v.doc_id!=c.doc_id OR v.package_id!=d.package_id",
        [],
        |r| r.get(0),
    )?;
    if inconsistent_vec > 0 {
        return Err(BackupError::Validation("备份向量关联不一致".to_string()));
    }

    let missing_fts: i64 = conn.query_row(
        "SELECT COUNT(*) FROM chunks c LEFT JOIN chunks_fts f ON f.rowid=c.id WHERE f.rowid IS NULL",
        [],
        |r| r.get(0),
    )?;
    if missing_fts > 0 {
        return Err(BackupError::Validation("备份全文索引不完整".to_string()));
    }

    // 8. 逐资料包校验：模型身份、NPY 形状、连续行号与归一化
    let mut pkg_stmt = conn.prepare(
        "SELECT id, sha256, status, embed_model, embed_dim, preprocessing_version, chunk_count FROM packages",
    )?;
    let mut pkg_rows = pkg_stmt.query([])?;
    while let Some(row) = pkg_rows.next()? {
        let pkg_id: i64 = row.get(0)?;
        let sha256: String = row.get(1)?;
        let status: String = row.get(2)?;
        let embed_model: String = row.get(3)?;
        let embed_dim: usize = row.get(4)?;
        let prep_ver: String = row.get(5)?;
        let chunk_count: usize = row.get(6)?;

        if embed_model != config.siliconflow_model
            || embed_dim != config.embed_dims
            || prep_ver != config.preprocessing_version
        {
            return Err(BackupError::Validation(
                "备份向量模型、维度或预处理版本与当前配置不一致".to_string(),
            ));
        }

        let vec_name = if status == "published" {
            format!("pkg-{}", pkg_id)
        } else {
            format!("pkg-draft-{}", &sha256[..16])
        };
        let npy_path = root.join("vectors").join(format!("{}.npy", vec_name));
        let header =
            validate_npy_file(&npy_path).map_err(|e| BackupError::Validation(e.to_string()))?;

        if header.shape != vec![chunk_count, embed_dim] {
            return Err(BackupError::Validation("备份向量形状不匹配".to_string()));
        }

        // 如果是 published，检查 vector_rows 连续性
        if status == "published" {
            let mut vr_stmt = conn.prepare(
                "SELECT row_index FROM vector_rows WHERE package_id=? ORDER BY row_index",
            )?;
            let mut vr_rows = vr_stmt.query([pkg_id])?;
            let mut count = 0;
            while let Some(vr_row) = vr_rows.next()? {
                let row_idx: usize = vr_row.get(0)?;
                if row_idx != count {
                    return Err(BackupError::Validation("备份向量行号不连续".to_string()));
                }
                count += 1;
            }
            if count != chunk_count {
                return Err(BackupError::Validation("备份向量数量不匹配".to_string()));
            }
        }
    }

    Ok(())
}

/// 执行原子目录替换与回滚保护安装备份
pub async fn install_backup(
    db: &DbPool,
    config: &Config,
    unpacked_root: &Path,
    meta: &BackupMeta,
    close_vector_handles: impl Fn(),
) -> Result<BackupCounts, BackupError> {
    let current = config.data_dir.clone();
    let parent = current.parent().unwrap_or_else(|| Path::new("."));

    // 将 db.sqlite 重命名为 campus.db
    let unpacked_sqlite = unpacked_root.join("db.sqlite");
    let unpacked_campus_db = unpacked_root.join("campus.db");
    if unpacked_sqlite.exists() {
        std::fs::rename(&unpacked_sqlite, &unpacked_campus_db)?;
    }

    for sub in SUBDIRS {
        std::fs::create_dir_all(unpacked_root.join(sub))?;
    }

    let uuid_str = hex::encode(rand::random::<[u8; 16]>());
    let dir_name = current
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("data");
    let previous = parent.join(format!("{}.rollback-{}", dir_name, uuid_str));
    let failed = parent.join(format!("{}.failed-{}", dir_name, uuid_str));
    let marker = restore_marker_path(&current);

    // 1. 关闭向量句柄与数据库连接
    close_vector_handles();

    // 2. 写入恢复中标记文件
    use std::io::Write;
    let mut marker_file = File::create(&marker)?;
    marker_file
        .write_all(b"Restore incomplete. Inspect rollback directories before restarting.\n")?;
    marker_file.sync_all()?;
    sync_directory(parent)?;

    // 3. 关闭当前数据库连接
    db.close().await?;

    let mut moved_old = false;
    let mut installed = false;

    let install_result = (|| -> Result<(), BackupError> {
        // 重命名当前数据目录到 rollback 目录
        if current.exists() {
            std::fs::rename(&current, &previous)?;
            moved_old = true;
            sync_directory(parent)?;
        }

        // 将解压验证后的新目录移动到当前位置
        std::fs::rename(unpacked_root, &current)?;
        installed = true;
        sync_directory(parent)?;

        Ok(())
    })();

    if let Err(err) = install_result {
        // 尝试自动回滚
        rollback_install(
            db, &current, &previous, &failed, &marker, moved_old, installed,
        )
        .await?;
        return Err(err);
    }

    // 重开数据库并验证基础查询
    let reopen_res = db.reopen(None).await;
    if let Err(e) = reopen_res {
        // 重开失败，尝试回退
        rollback_install(
            db, &current, &previous, &failed, &marker, moved_old, installed,
        )
        .await?;
        return Err(BackupError::Validation(format!(
            "恢复后重新打开数据库失败: {}",
            e
        )));
    }

    // 验证 SELECT COUNT(*) FROM documents
    let verify_query = db
        .read(|conn| {
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?;
            Ok(count)
        })
        .await;

    if let Err(e) = verify_query {
        rollback_install(
            db, &current, &previous, &failed, &marker, moved_old, installed,
        )
        .await?;
        return Err(BackupError::Validation(format!(
            "恢复后验证查询失败: {}",
            e
        )));
    }

    // 4. 成功后清除标记与旧目录
    std::fs::remove_file(&marker)?;
    sync_directory(parent)?;
    if previous.exists() {
        let _ = std::fs::remove_dir_all(&previous);
    }

    Ok(BackupCounts {
        documents: meta.counts.documents,
        chunks: meta.counts.chunks,
    })
}

async fn rollback_install(
    db: &DbPool,
    current: &Path,
    previous: &Path,
    failed: &Path,
    marker: &Path,
    moved_old: bool,
    installed: bool,
) -> Result<(), BackupError> {
    let result = async {
        db.close().await?;
        if installed {
            std::fs::rename(current, failed)?;
        }
        if moved_old {
            std::fs::rename(previous, current)?;
        }
        // Never create a new empty database while attempting recovery.
        if !current.join("campus.db").is_file() {
            return Err(BackupError::Validation("原数据库缺失".into()));
        }
        db.reopen(None).await?;
        db.read(|conn| {
            conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get::<_, i64>(0))
        })
        .await?;
        std::fs::remove_file(marker)?;
        sync_directory(current.parent().unwrap_or_else(|| Path::new(".")))?;
        Ok::<(), BackupError>(())
    }
    .await;
    result.map_err(|_| BackupError::Rollback("回滚未能完整验证，已保留恢复标记和目录".into()))
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod recovery_regressions {
    use super::*;

    #[tokio::test]
    async fn failed_rollback_preserves_marker_and_does_not_create_database() {
        let tmp = tempfile::tempdir().unwrap();
        let current = tmp.path().join("data");
        let db = DbPool::new(&current.join("campus.db"), 2, 64).unwrap();
        let marker = restore_marker_path(&current);
        std::fs::write(&marker, b"incomplete").unwrap();
        db.close().await.unwrap();
        std::fs::rename(&current, tmp.path().join("original-preserved")).unwrap();
        let result = rollback_install(
            &db,
            &current,
            &tmp.path().join("missing"),
            &tmp.path().join("failed"),
            &marker,
            true,
            false,
        )
        .await;
        assert!(matches!(result, Err(BackupError::Rollback(_))));
        assert!(marker.exists());
        assert!(!current.exists());
        assert!(tmp.path().join("original-preserved/campus.db").exists());
    }

    #[tokio::test]
    async fn invalid_installed_database_rolls_back_to_queryable_original() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::from_env(None);
        config.data_dir = tmp.path().join("data");
        let db = DbPool::new(&config.data_dir.join("campus.db"), 2, 64).unwrap();
        db.setting_set("sentinel".into(), "original".into())
            .await
            .unwrap();
        let unpacked = tmp.path().join("unpacked");
        std::fs::create_dir(&unpacked).unwrap();
        std::fs::write(unpacked.join("db.sqlite"), b"not a database").unwrap();
        let meta: BackupMeta = serde_json::from_value(serde_json::json!({
            "format":BACKUP_FORMAT, "version":BACKUP_VERSION, "created_at":"test",
            "counts":{"documents":0,"chunks":0}, "files":{}
        }))
        .unwrap();
        assert!(
            install_backup(&db, &config, &unpacked, &meta, || {})
                .await
                .is_err()
        );
        assert!(!restore_marker_path(&config.data_dir).exists());
        assert_eq!(
            db.setting_get("sentinel".into()).await.unwrap().as_deref(),
            Some("original")
        );
    }
}
