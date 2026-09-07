use chrono::Utc;
use rusqlite::{Connection, Result, params};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS packages (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  sha256 TEXT UNIQUE NOT NULL,
  original_filename TEXT NOT NULL,
  size INTEGER NOT NULL,
  imported_at TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('draft','published')),
  preprocessing_version TEXT NOT NULL,
  embed_model TEXT NOT NULL,
  embed_dim INTEGER NOT NULL,
  doc_count INTEGER NOT NULL,
  chunk_count INTEGER NOT NULL,
  meta_overrides TEXT NOT NULL DEFAULT '{}',
  error TEXT
);

CREATE TABLE IF NOT EXISTS documents (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  doc_uid TEXT UNIQUE NOT NULL,
  doc_hash TEXT NOT NULL,
  package_id INTEGER NOT NULL REFERENCES packages(id),
  title TEXT NOT NULL,
  original_filename TEXT NOT NULL,
  doc_type TEXT NOT NULL,
  department TEXT NOT NULL,
  effective_date TEXT,
  audience TEXT NOT NULL DEFAULT '[]',
  audience_scope TEXT NOT NULL DEFAULT '{}',
  notes TEXT NOT NULL DEFAULT '',
  line_count INTEGER NOT NULL,
  page_map TEXT,
  text_sha256 TEXT NOT NULL,
  replaces_doc_id INTEGER REFERENCES documents(id),
  published_at TEXT NOT NULL,
  deactivated_kind TEXT NOT NULL DEFAULT '' CHECK(deactivated_kind IN ('','superseded','manual')),
  deactivated_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_documents_kind ON documents(deactivated_kind);
CREATE INDEX IF NOT EXISTS idx_documents_hash ON documents(doc_hash);

CREATE TABLE IF NOT EXISTS sections (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  doc_id INTEGER NOT NULL REFERENCES documents(id),
  section_id TEXT NOT NULL,
  title TEXT NOT NULL,
  start_line INTEGER NOT NULL,
  end_line INTEGER NOT NULL,
  UNIQUE(doc_id, section_id)
);

CREATE TABLE IF NOT EXISTS doc_tags (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  doc_id INTEGER NOT NULL REFERENCES documents(id),
  section_id TEXT,
  tag TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_doc_tags_doc ON doc_tags(doc_id);
CREATE INDEX IF NOT EXISTS idx_doc_tags_tag ON doc_tags(tag);

CREATE TABLE IF NOT EXISTS chunks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  doc_id INTEGER NOT NULL REFERENCES documents(id),
  chunk_index INTEGER NOT NULL,
  text TEXT NOT NULL,
  line_start INTEGER NOT NULL,
  line_end INTEGER NOT NULL,
  section_id TEXT
);
CREATE INDEX IF NOT EXISTS idx_chunks_doc ON chunks(doc_id);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(body);

CREATE TABLE IF NOT EXISTS vector_rows (
  chunk_id INTEGER PRIMARY KEY REFERENCES chunks(id),
  row_index INTEGER NOT NULL,
  doc_id INTEGER NOT NULL,
  package_id INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_vector_rows_doc ON vector_rows(doc_id);

CREATE TABLE IF NOT EXISTS audit_log (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  at TEXT NOT NULL,
  actor TEXT NOT NULL,
  action TEXT NOT NULL,
  detail TEXT NOT NULL DEFAULT ''
);
"#;

pub fn utcnow() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// 打开并配置 SQLite 连接：WAL、foreign_keys=ON、synchronous=NORMAL、busy_timeout=5000、2MB page cache
fn open_connection(path: &Path, readonly: bool) -> Result<Connection> {
    let conn = if readonly {
        Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?
    } else {
        Connection::open(path)?
    };

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA cache_size = -2000;", // ~2MB cache
    )?;

    Ok(conn)
}

enum JobMessage {
    Execute(Box<dyn FnOnce(&mut Connection) + Send + 'static>),
    Reopen(PathBuf, tokio::sync::oneshot::Sender<Result<()>>),
    Close(tokio::sync::oneshot::Sender<Result<()>>),
}

#[derive(Clone)]
pub struct DbPool {
    writer_tx: SyncSender<JobMessage>,
    reader_txs: Vec<SyncSender<JobMessage>>,
    reader_cursor: Arc<AtomicUsize>,
    pub path: PathBuf,
}

impl DbPool {
    /// 初始化 SQLite 线程池：1 写线程 + max_readers 读线程（建议 2）
    pub fn new(path: &Path, max_readers: usize, queue_cap: usize) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        // 初始化写连接并应用模式
        {
            let conn = open_connection(path, false)?;
            conn.execute_batch(SCHEMA)?;
            // 兼容迁移 audience_scope
            let mut stmt = conn.prepare("PRAGMA table_info(documents)")?;
            let has_audience_scope = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .any(|col| col.map(|c| c == "audience_scope").unwrap_or(false));
            if !has_audience_scope {
                conn.execute_batch(
                    "ALTER TABLE documents ADD COLUMN audience_scope TEXT NOT NULL DEFAULT '{}';",
                )?;
            }
        }

        // 启动写工作线程（1个，队列上限 queue_cap）
        let (writer_tx, writer_rx) = sync_channel::<JobMessage>(queue_cap);
        let write_path = path.to_path_buf();
        thread::Builder::new()
            .name("sqlite-writer".to_string())
            .spawn(move || {
                let mut current_path = write_path;
                let mut conn_opt = open_connection(&current_path, false).ok();
                while let Ok(msg) = writer_rx.recv() {
                    match msg {
                        JobMessage::Execute(job) => {
                            if let Some(ref mut conn) = conn_opt {
                                job(conn);
                            }
                        }
                        JobMessage::Reopen(new_path, resp_tx) => {
                            current_path = new_path;
                            conn_opt = None;
                            match open_connection(&current_path, false) {
                                Ok(new_conn) => {
                                    conn_opt = Some(new_conn);
                                    let _ = resp_tx.send(Ok(()));
                                }
                                Err(e) => {
                                    let _ = resp_tx.send(Err(e));
                                }
                            }
                        }
                        JobMessage::Close(resp_tx) => {
                            conn_opt = None;
                            let _ = resp_tx.send(Ok(()));
                        }
                    }
                }
            })
            .expect("创建写线程失败");

        // 启动读工作线程（max_readers 个，只读连接）
        let mut reader_txs = Vec::with_capacity(max_readers);
        for i in 0..max_readers {
            let (rtx, rrx) = sync_channel::<JobMessage>(queue_cap);
            reader_txs.push(rtx);
            let read_path = path.to_path_buf();
            thread::Builder::new()
                .name(format!("sqlite-reader-{}", i))
                .spawn(move || {
                    let mut current_path = read_path;
                    let mut conn_opt = open_connection(&current_path, true).ok();
                    while let Ok(msg) = rrx.recv() {
                        match msg {
                            JobMessage::Execute(job) => {
                                if let Some(ref mut conn) = conn_opt {
                                    job(conn);
                                }
                            }
                            JobMessage::Reopen(new_path, resp_tx) => {
                                current_path = new_path;
                                conn_opt = None;
                                match open_connection(&current_path, true) {
                                    Ok(new_conn) => {
                                        conn_opt = Some(new_conn);
                                        let _ = resp_tx.send(Ok(()));
                                    }
                                    Err(e) => {
                                        let _ = resp_tx.send(Err(e));
                                    }
                                }
                            }
                            JobMessage::Close(resp_tx) => {
                                conn_opt = None;
                                let _ = resp_tx.send(Ok(()));
                            }
                        }
                    }
                })
                .expect("创建读线程失败");
        }

        Ok(Self {
            writer_tx,
            reader_txs,
            reader_cursor: Arc::new(AtomicUsize::new(0)),
            path: path.to_path_buf(),
        })
    }

    /// 关闭所有数据库连接（释放文件句柄）
    pub async fn close(&self) -> Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.writer_tx.send(JobMessage::Close(tx)).map_err(|_| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                Some("写连接已关闭".to_string()),
            )
        })?;
        let _ = rx.await;

        for rtx in &self.reader_txs {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = rtx.send(JobMessage::Close(tx));
            let _ = rx.await;
        }
        Ok(())
    }

    /// 重新打开所有数据库连接
    pub async fn reopen(&self, new_path: Option<&Path>) -> Result<()> {
        let target_path = new_path
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.path.clone());

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.writer_tx
            .send(JobMessage::Reopen(target_path.clone(), tx))
            .map_err(|_| {
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                    Some("写连接重开失败".to_string()),
                )
            })?;
        rx.await.map_err(|_| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                Some("写连接应答中断".to_string()),
            )
        })??;

        for rtx in &self.reader_txs {
            let (tx, rx) = tokio::sync::oneshot::channel();
            rtx.send(JobMessage::Reopen(target_path.clone(), tx))
                .map_err(|_| {
                    rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                        Some("读连接重开失败".to_string()),
                    )
                })?;
            rx.await.map_err(|_| {
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                    Some("读连接应答中断".to_string()),
                )
            })??;
        }

        Ok(())
    }

    /// 执行读操作
    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let idx = self.reader_cursor.fetch_add(1, Ordering::Relaxed) % self.reader_txs.len();
        let sender = self.reader_txs[idx].clone();

        let job = Box::new(move |conn: &mut Connection| {
            let res = f(conn);
            let _ = result_tx.send(res);
        });

        sender.send(JobMessage::Execute(job)).map_err(|_| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                Some("数据库读通道断开".to_string()),
            )
        })?;

        result_rx.await.map_err(|_| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                Some("读任务应答失败".to_string()),
            )
        })?
    }

    /// 执行写操作
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let sender = self.writer_tx.clone();

        let job = Box::new(move |conn: &mut Connection| {
            let res = f(conn);
            let _ = result_tx.send(res);
        });

        sender.send(JobMessage::Execute(job)).map_err(|_| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                Some("数据库写通道断开".to_string()),
            )
        })?;

        result_rx.await.map_err(|_| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                Some("写任务应答失败".to_string()),
            )
        })?
    }

    /// 获取配置项
    pub async fn setting_get(&self, key: String) -> Result<Option<String>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached("SELECT value FROM settings WHERE key=?")?;
            let mut rows = stmt.query(params![key])?;
            if let Some(row) = rows.next()? {
                Ok(Some(row.get(0)?))
            } else {
                Ok(None)
            }
        })
        .await
    }

    /// 设置配置项
    pub async fn setting_set(&self, key: String, value: String) -> Result<()> {
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO settings(key,value) VALUES(?,?) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, value],
            )?;
            Ok(())
        })
        .await
    }

    /// 写入审计日志（不记正文与密钥）
    pub async fn audit(&self, actor: String, action: String, detail: String) -> Result<()> {
        let now = utcnow();
        let truncated_detail = if detail.len() > 500 {
            detail[..500].to_string()
        } else {
            detail
        };
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO audit_log(at,actor,action,detail) VALUES(?,?,?,?)",
                params![now, actor, action, truncated_detail],
            )?;
            Ok(())
        })
        .await
    }

    /// 查询单篇公开发布文档
    pub async fn get_document_public(&self, doc_uid: String) -> Result<Option<DocumentPublic>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, doc_uid, doc_hash, package_id, title, original_filename, \
                        doc_type, department, effective_date, audience, audience_scope, \
                        notes, line_count, page_map, replaces_doc_id, published_at, \
                        deactivated_kind \
                 FROM documents WHERE doc_uid=?",
            )?;
            let mut rows = stmt.query(params![doc_uid])?;
            if let Some(row) = rows.next()? {
                let doc_id: i64 = row.get(0)?;
                let doc = row_to_document_public(conn, row, doc_id)?;
                Ok(Some(doc))
            } else {
                Ok(None)
            }
        })
        .await
    }

    /// 查询现行公开目录（deactivated_kind=''）
    pub async fn get_catalog(&self) -> Result<Vec<DocumentPublic>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, doc_uid, doc_hash, package_id, title, original_filename, \
                        doc_type, department, effective_date, audience, audience_scope, \
                        notes, line_count, page_map, replaces_doc_id, published_at, \
                        deactivated_kind \
                 FROM documents WHERE deactivated_kind='' ORDER BY title",
            )?;
            let mut rows = stmt.query([])?;
            let mut list = Vec::new();
            while let Some(row) = rows.next()? {
                let doc_id: i64 = row.get(0)?;
                let doc = row_to_document_public(conn, row, doc_id)?;
                list.push(doc);
            }
            Ok(list)
        })
        .await
    }

    /// 管理员查询全量文档（按发布倒序）
    pub async fn get_admin_documents(&self) -> Result<Vec<DocumentPublic>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, doc_uid, doc_hash, package_id, title, original_filename, \
                        doc_type, department, effective_date, audience, audience_scope, \
                        notes, line_count, page_map, replaces_doc_id, published_at, \
                        deactivated_kind \
                 FROM documents ORDER BY published_at DESC",
            )?;
            let mut rows = stmt.query([])?;
            let mut list = Vec::new();
            while let Some(row) = rows.next()? {
                let doc_id: i64 = row.get(0)?;
                let doc = row_to_document_public(conn, row, doc_id)?;
                list.push(doc);
            }
            Ok(list)
        })
        .await
    }

    /// 获取版本链列表
    pub async fn get_version_history(&self, doc_uid: String) -> Result<Vec<VersionItem>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, doc_uid, title, published_at, effective_date, deactivated_kind, package_id, replaces_doc_id \
                 FROM documents WHERE doc_uid=?",
            )?;
            let cur = match stmt.query_row(params![doc_uid], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                ))
            }) {
                Ok(c) => c,
                Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(Vec::new()),
                Err(e) => return Err(e),
            };

            let mut chain = vec![cur.clone()];

            // 向前追溯：被其替代的旧版
            let mut back_replaces_id = cur.7;
            while let Some(old_id) = back_replaces_id {
                let prev = conn.query_row(
                    "SELECT id, doc_uid, title, published_at, effective_date, deactivated_kind, package_id, replaces_doc_id \
                     FROM documents WHERE id=?",
                    params![old_id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, Option<i64>>(7)?,
                        ))
                    },
                );
                match prev {
                    Ok(p) => {
                        back_replaces_id = p.7;
                        chain.insert(0, p);
                    }
                    Err(_) => break,
                }
            }

            // 向后追踪：替代它的新版
            let mut fwd_id = cur.0;
            loop {
                let next = conn.query_row(
                    "SELECT id, doc_uid, title, published_at, effective_date, deactivated_kind, package_id, replaces_doc_id \
                     FROM documents WHERE replaces_doc_id=?",
                    params![fwd_id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, Option<i64>>(7)?,
                        ))
                    },
                );
                match next {
                    Ok(n) => {
                        fwd_id = n.0;
                        chain.push(n);
                    }
                    Err(_) => break,
                }
            }

            let result = chain
                .into_iter()
                .map(|(_, uid, title, pub_at, eff_date, deact, pkg_id, _)| VersionItem {
                    doc_uid: uid,
                    title,
                    published_at: pub_at,
                    effective_date: eff_date,
                    deactivated_kind: deact.clone(),
                    is_current: deact.is_empty(),
                    package_id: pkg_id,
                })
                .collect();

            Ok(result)
        })
        .await
    }

    /// 获取库中数量指标
    pub async fn get_counts(&self) -> Result<Counts> {
        self.read(|conn| {
            let documents: i64 =
                conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?;
            let current: i64 = conn.query_row(
                "SELECT COUNT(*) FROM documents WHERE deactivated_kind=''",
                [],
                |r| r.get(0),
            )?;
            let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
            let packages: i64 =
                conn.query_row("SELECT COUNT(*) FROM packages", [], |r| r.get(0))?;
            Ok(Counts {
                documents,
                current,
                chunks,
                packages,
            })
        })
        .await
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Counts {
    pub documents: i64,
    pub current: i64,
    pub chunks: i64,
    pub packages: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VersionItem {
    pub doc_uid: String,
    pub title: String,
    pub published_at: String,
    pub effective_date: Option<String>,
    pub deactivated_kind: String,
    pub is_current: bool,
    pub package_id: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DocumentPublic {
    pub doc_uid: String,
    pub title: String,
    pub doc_type: String,
    pub department: String,
    pub effective_date: Option<String>,
    pub audience: serde_json::Value,
    pub audience_scope: serde_json::Value,
    pub doc_hash: String,
    pub replaces_doc_uid: Option<String>,
    pub domains: HashMap<String, Vec<String>>,
    pub line_count: i64,
    pub published_at: String,
    pub deactivated_kind: String,
    pub notes: String,
}

fn row_to_document_public(
    conn: &Connection,
    row: &rusqlite::Row,
    doc_id: i64,
) -> Result<DocumentPublic> {
    let doc_uid: String = row.get(1)?;
    let doc_hash: String = row.get(2)?;
    let title: String = row.get(4)?;
    let doc_type: String = row.get(6)?;
    let department: String = row.get(7)?;
    let effective_date: Option<String> = row.get(8)?;
    let audience_raw: String = row.get(9)?;
    let audience_scope_raw: String = row.get(10)?;
    let notes: String = row.get(11)?;
    let line_count: i64 = row.get(12)?;
    let replaces_doc_id: Option<i64> = row.get(14)?;
    let published_at: String = row.get(15)?;
    let deactivated_kind: String = row.get(16)?;

    let replaces_doc_uid: Option<String> = match replaces_doc_id {
        Some(rid) => conn
            .query_row(
                "SELECT doc_uid FROM documents WHERE id=?",
                params![rid],
                |r| r.get(0),
            )
            .ok(),
        None => None,
    };

    let mut tag_stmt = conn.prepare("SELECT tag, section_id FROM doc_tags WHERE doc_id=?")?;
    let mut tag_rows = tag_stmt.query(params![doc_id])?;
    let mut domains: HashMap<String, Vec<String>> = HashMap::new();
    while let Some(trow) = tag_rows.next()? {
        let tag: String = trow.get(0)?;
        let sec_id: Option<String> = trow.get(1)?;
        domains
            .entry(tag)
            .or_default()
            .push(sec_id.unwrap_or_default());
    }

    let audience = serde_json::from_str(&audience_raw).unwrap_or(serde_json::json!([]));
    let audience_scope = serde_json::from_str(&audience_scope_raw).unwrap_or(serde_json::json!({}));

    Ok(DocumentPublic {
        doc_uid,
        title,
        doc_type,
        department,
        effective_date,
        audience,
        audience_scope,
        doc_hash,
        replaces_doc_uid,
        domains,
        line_count,
        published_at,
        deactivated_kind,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_db_pool_init_and_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("campus.db");
        let pool = DbPool::new(&db_path, 2, 64).expect("初始化 DbPool 失败");

        // settings 读写测试
        assert_eq!(
            pool.setting_get("admin_password_hash".into())
                .await
                .unwrap(),
            None
        );

        pool.setting_set("admin_password_hash".into(), "hash123".into())
            .await
            .unwrap();

        assert_eq!(
            pool.setting_get("admin_password_hash".into())
                .await
                .unwrap(),
            Some("hash123".into())
        );

        // 覆盖测试
        pool.setting_set("admin_password_hash".into(), "hash456".into())
            .await
            .unwrap();

        assert_eq!(
            pool.setting_get("admin_password_hash".into())
                .await
                .unwrap(),
            Some("hash456".into())
        );

        // 审计日志写入
        pool.audit("admin".into(), "test_action".into(), "detail info".into())
            .await
            .unwrap();

        let counts = pool.get_counts().await.unwrap();
        assert_eq!(counts.documents, 0);
    }

    #[tokio::test]
    async fn test_read_legacy_database_fixture() {
        let legacy_db_path = Path::new("tests/fixtures/legacy_data/campus.db");
        if !legacy_db_path.exists() {
            eprintln!("legacy_data/campus.db 不存在，跳过测试");
            return;
        }

        let pool = DbPool::new(legacy_db_path, 2, 64).expect("打开 Python legacy campus.db 失败");

        // 1. 验证 settings 可读
        let admin_hash = pool
            .setting_get("admin_password_hash".into())
            .await
            .unwrap();
        assert!(
            admin_hash.is_some(),
            "应读取到 Python 生成的 admin_password_hash"
        );

        // 2. 验证 catalog 现行目录读取
        let catalog = pool.get_catalog().await.unwrap();
        assert_eq!(catalog.len(), 6, "Python 生成的 6 份现行文档应全部读出");

        // 检查其中一份文档字段
        let first = catalog
            .iter()
            .find(|d| d.title.contains("三下乡"))
            .expect("应包含三下乡文档");
        assert_eq!(first.doc_type, "txt");
        assert_eq!(first.department, "团委");
        assert_eq!(first.line_count, 4);

        // 3. 验证单篇文档查询与版本链
        let doc_public = pool
            .get_document_public(first.doc_uid.clone())
            .await
            .unwrap();
        assert!(doc_public.is_some());

        let versions = pool
            .get_version_history(first.doc_uid.clone())
            .await
            .unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].title, first.title);
        assert!(versions[0].is_current);

        // 4. 验证 counts 指标
        let counts = pool.get_counts().await.unwrap();
        assert_eq!(counts.documents, 6);
        assert_eq!(counts.current, 6);
        assert!(counts.chunks > 0);
        assert!(counts.packages > 0);
    }
}
