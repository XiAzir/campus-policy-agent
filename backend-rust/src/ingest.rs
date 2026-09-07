use crate::config::Config;
use crate::db::{DbPool, utcnow};
use crate::pkgfmt::validate_package_archive;
use crate::tokenizer::tokenize_for_fts;
use rusqlite::params;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const EDITABLE_FIELDS: [&str; 7] = [
    "title",
    "department",
    "effective_date",
    "audience",
    "audience_scope",
    "notes",
    "replaces",
];

#[derive(Debug)]
pub enum IngestError {
    Validation(String),
    Duplicate(String),
    NotFound(String),
    InvalidState(String),
    BadTarget(String),
    Conflict(String),
    DiskFull(String),
    Io(std::io::Error),
    Db(rusqlite::Error),
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(msg) => write!(f, "校验未通过：{}", msg),
            Self::Duplicate(msg) => write!(f, "{}", msg),
            Self::NotFound(msg) => write!(f, "{}", msg),
            Self::InvalidState(msg) => write!(f, "{}", msg),
            Self::BadTarget(msg) => write!(f, "{}", msg),
            Self::Conflict(msg) => write!(f, "{}", msg),
            Self::DiskFull(msg) => write!(f, "磁盘空间不足：{}", msg),
            Self::Io(e) => write!(f, "IO 错误: {}", e),
            Self::Db(e) => write!(f, "数据库错误: {}", e),
        }
    }
}

impl std::error::Error for IngestError {}

impl From<std::io::Error> for IngestError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<rusqlite::Error> for IngestError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

pub fn get_package_zip_path(config: &Config, sha256: &str) -> PathBuf {
    config
        .data_dir
        .join("packages")
        .join(format!("{}.zip", sha256))
}

pub async fn import_package(
    db: &DbPool,
    config: &Config,
    zip_path: &Path,
    original_filename: &str,
) -> Result<i64, IngestError> {
    let pkg = validate_package_archive(zip_path, config, true)
        .map_err(|e| IngestError::Validation(e.to_string()))?;

    let sha256 = pkg.sha256.clone();
    let original_filename = original_filename.to_string();
    let size = pkg.size;

    // 检查重复包哈希
    let sha_check = sha256.clone();
    let exists = db
        .read(move |conn| {
            let mut stmt = conn.prepare("SELECT id FROM packages WHERE sha256=?")?;
            let mut rows = stmt.query([&sha_check])?;
            Ok(rows.next()?.is_some())
        })
        .await
        .map_err(IngestError::Db)?;

    if exists {
        return Err(IngestError::Duplicate(
            "该资料包已导入过（包哈希一致），拒绝重复导入".into(),
        ));
    }

    let packages_dir = config.data_dir.join("packages");
    let vectors_dir = config.data_dir.join("vectors");
    fs::create_dir_all(&packages_dir)?;
    fs::create_dir_all(&vectors_dir)?;

    let dest_zip = get_package_zip_path(config, &sha256);
    let draft_vec = vectors_dir.join(format!("pkg-draft-{}.npy", &sha256[..16]));

    let tmp_zip = dest_zip.with_extension("tmp");
    fs::copy(zip_path, &tmp_zip)?;
    fs::rename(&tmp_zip, &dest_zip)?;

    let root_vec = pkg.root.join("vectors.npy");
    fs::copy(&root_vec, &draft_vec)?;

    let manifest = pkg.manifest;
    let prep_ver = manifest["preprocessing_version"]
        .as_str()
        .unwrap()
        .to_string();
    let embed_model = manifest["embed_model"].as_str().unwrap().to_string();
    let embed_dim = manifest["embed_dim"].as_i64().unwrap();
    let doc_count = pkg.documents.len() as i64;
    let chunk_count = manifest["vectors"]["count"].as_i64().unwrap();

    let sha_ins = sha256.clone();
    let pkg_id = db
        .write(move |conn| {
            let now = utcnow();
            conn.execute(
                "INSERT INTO packages(sha256, original_filename, size, imported_at, status, \
                 preprocessing_version, embed_model, embed_dim, doc_count, chunk_count) \
                 VALUES(?, ?, ?, ?, 'draft', ?, ?, ?, ?, ?)",
                params![
                    sha_ins,
                    original_filename,
                    size as i64,
                    now,
                    prep_ver,
                    embed_model,
                    embed_dim,
                    doc_count,
                    chunk_count
                ],
            )?;
            let id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO audit_log(at, actor, action, detail) VALUES(?, ?, ?, ?)",
                params![now, "admin", "package_import", format!("package={}", id)],
            )?;
            Ok(id)
        })
        .await
        .map_err(IngestError::Db)?;

    // 清理临时解压目录
    let _ = fs::remove_dir_all(&pkg.root);

    Ok(pkg_id)
}

pub async fn preview_package(
    db: &DbPool,
    config: &Config,
    package_id: i64,
) -> Result<Option<Value>, IngestError> {
    let pkg_row = db
        .read(move |conn| {
            let mut stmt = conn.prepare("SELECT * FROM packages WHERE id=?")?;
            let mut rows = stmt.query([package_id])?;
            if let Some(r) = rows.next()? {
                Ok(Some((
                    r.get::<_, i64>("id")?,
                    r.get::<_, String>("sha256")?,
                    r.get::<_, String>("original_filename")?,
                    r.get::<_, i64>("size")?,
                    r.get::<_, String>("imported_at")?,
                    r.get::<_, String>("status")?,
                    r.get::<_, String>("preprocessing_version")?,
                    r.get::<_, String>("embed_model")?,
                    r.get::<_, i64>("embed_dim")?,
                    r.get::<_, i64>("doc_count")?,
                    r.get::<_, i64>("chunk_count")?,
                    r.get::<_, Option<String>>("meta_overrides")?,
                )))
            } else {
                Ok(None)
            }
        })
        .await
        .map_err(IngestError::Db)?;

    let Some((
        id,
        sha256,
        original_filename,
        size,
        imported_at,
        status,
        preprocessing_version,
        embed_model,
        embed_dim,
        doc_count,
        chunk_count,
        meta_overrides_raw,
    )) = pkg_row
    else {
        return Ok(None);
    };

    let zip_path = get_package_zip_path(config, &sha256);
    if !zip_path.exists() {
        return Ok(None);
    }

    let checked = validate_package_archive(&zip_path, config, false)
        .map_err(|e| IngestError::Validation(e.to_string()))?;

    let overrides: HashMap<String, Value> = meta_overrides_raw
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let mut doc_list = Vec::new();
    for d in checked.documents {
        let h = d["doc_hash"].as_str().unwrap();
        let meta = overrides.get(h).cloned().unwrap_or(json!({}));
        let target = meta
            .get("replaces")
            .or_else(|| d.get("replaces"))
            .and_then(|v| v.as_str());

        let target_uid = if let Some(tgt) = target {
            let tgt_str = tgt.to_string();
            db.read(move |conn| {
                let mut stmt =
                    conn.prepare("SELECT doc_uid FROM documents WHERE doc_hash=? OR doc_uid=?")?;
                let mut rows = stmt.query([&tgt_str, &tgt_str])?;
                if let Some(r) = rows.next()? {
                    Ok(Some(r.get::<_, String>(0)?))
                } else {
                    Ok(None)
                }
            })
            .await
            .unwrap_or(None)
        } else {
            None
        };

        let title = meta.get("title").unwrap_or(&d["title"]).clone();
        let department = meta.get("department").unwrap_or(&d["department"]).clone();
        let effective_date = meta
            .get("effective_date")
            .unwrap_or(&d["effective_date"])
            .clone();
        let audience = meta.get("audience").unwrap_or(&d["audience"]).clone();
        let audience_scope = meta
            .get("audience_scope")
            .or_else(|| d.get("audience_scope"))
            .cloned()
            .unwrap_or(json!({}));
        let notes = meta
            .get("notes")
            .or_else(|| d.get("notes"))
            .cloned()
            .unwrap_or(json!(""));

        doc_list.push(json!({
            "doc_hash": h,
            "original_filename": d["original_filename"],
            "doc_type": d["doc_type"],
            "title": title,
            "department": department,
            "effective_date": effective_date,
            "audience": audience,
            "audience_scope": audience_scope,
            "notes": notes,
            "replaces_doc_uid": target_uid,
            "replaces_unresolved": target.is_some() && target_uid.is_none(),
            "domains": d.get("domains").cloned().unwrap_or(json!([])),
            "sections": d.get("sections").cloned().unwrap_or(json!([])),
            "line_count": d["line_count"],
            "chunk_count": d["chunks"].as_array().map_or(0, |c| c.len()),
            "edited": overrides.contains_key(h)
        }));
    }

    let _ = fs::remove_dir_all(&checked.root);

    Ok(Some(json!({
        "id": id,
        "sha256": sha256,
        "original_filename": original_filename,
        "size": size,
        "imported_at": imported_at,
        "status": status,
        "preprocessing_version": preprocessing_version,
        "embed_model": embed_model,
        "embed_dim": embed_dim,
        "doc_count": doc_count,
        "chunk_count": chunk_count,
        "vector_shape": [chunk_count, embed_dim],
        "documents": doc_list
    })))
}

pub async fn apply_override(
    db: &DbPool,
    config: &Config,
    package_id: i64,
    doc_hash: &str,
    fields: HashMap<String, Value>,
) -> Result<(), IngestError> {
    for key in fields.keys() {
        if !EDITABLE_FIELDS.contains(&key.as_str()) {
            return Err(IngestError::Validation(format!(
                "字段 {:?} 会影响分块或向量，修改后必须重新生成资料包（本地预处理）",
                vec![key]
            )));
        }
    }

    let pkg_info = db
        .read(move |conn| {
            let mut stmt =
                conn.prepare("SELECT status, sha256, meta_overrides FROM packages WHERE id=?")?;
            let mut rows = stmt.query([package_id])?;
            if let Some(r) = rows.next()? {
                Ok(Some((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                )))
            } else {
                Ok(None)
            }
        })
        .await
        .map_err(IngestError::Db)?;

    let Some((status, sha256, meta_overrides_raw)) = pkg_info else {
        return Err(IngestError::NotFound("资料包不存在".into()));
    };

    if status != "draft" {
        return Err(IngestError::InvalidState(
            "只有草稿状态可以修正元数据".into(),
        ));
    }

    let zip_path = get_package_zip_path(config, &sha256);
    let checked = validate_package_archive(&zip_path, config, false)
        .map_err(|e| IngestError::Validation(e.to_string()))?;

    let doc_in_pkg = checked.documents.iter().any(|d| d["doc_hash"] == doc_hash);
    let _ = fs::remove_dir_all(&checked.root);

    if !doc_in_pkg {
        return Err(IngestError::NotFound("该文档不在草稿中".into()));
    }

    let mut ov: HashMap<String, Value> = meta_overrides_raw
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let doc_entry = ov.entry(doc_hash.to_string()).or_insert_with(|| json!({}));
    if let Some(obj) = doc_entry.as_object_mut() {
        for (k, v) in fields.iter() {
            obj.insert(k.clone(), v.clone());
        }
    }

    let ov_str = serde_json::to_string(&ov).unwrap_or_default();
    let fields_keys: Vec<String> = fields.keys().cloned().collect();
    let d_hash = doc_hash.to_string();

    db.write(move |conn| {
        conn.execute(
            "UPDATE packages SET meta_overrides=? WHERE id=?",
            params![ov_str, package_id],
        )?;
        let now = utcnow();
        conn.execute(
            "INSERT INTO audit_log(at, actor, action, detail) VALUES(?, ?, ?, ?)",
            params![
                now,
                "admin",
                "package_meta_edit",
                format!(
                    "package={} doc={}… fields={:?}",
                    package_id,
                    &d_hash[..12.min(d_hash.len())],
                    fields_keys
                )
            ],
        )?;
        Ok(())
    })
    .await
    .map_err(IngestError::Db)?;

    Ok(())
}

pub async fn discard_package(
    db: &DbPool,
    config: &Config,
    package_id: i64,
) -> Result<(), IngestError> {
    let pkg_info = db
        .read(move |conn| {
            let mut stmt = conn.prepare("SELECT status, sha256 FROM packages WHERE id=?")?;
            let mut rows = stmt.query([package_id])?;
            if let Some(r) = rows.next()? {
                Ok(Some((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            } else {
                Ok(None)
            }
        })
        .await
        .map_err(IngestError::Db)?;

    let Some((status, sha256)) = pkg_info else {
        return Err(IngestError::NotFound("只有存在的草稿可以丢弃".into()));
    };

    if status != "draft" {
        return Err(IngestError::InvalidState("只有存在的草稿可以丢弃".into()));
    }

    db.write(move |conn| {
        conn.execute("DELETE FROM packages WHERE id=?", [package_id])?;
        let now = utcnow();
        conn.execute(
            "INSERT INTO audit_log(at, actor, action, detail) VALUES(?, ?, ?, ?)",
            params![
                now,
                "admin",
                "package_discard",
                format!("package={}", package_id)
            ],
        )?;
        Ok(())
    })
    .await
    .map_err(IngestError::Db)?;

    let draft_vec = config
        .data_dir
        .join("vectors")
        .join(format!("pkg-draft-{}.npy", &sha256[..16]));
    let _ = fs::remove_file(draft_vec);
    let zip_path = get_package_zip_path(config, &sha256);
    let _ = fs::remove_file(zip_path);

    Ok(())
}

pub async fn publish_package(
    db: &DbPool,
    config: &Config,
    package_id: i64,
    replacements: HashMap<String, Option<String>>,
) -> Result<(), IngestError> {
    let pkg_info = db
        .read(move |conn| {
            let mut stmt =
                conn.prepare("SELECT status, sha256, meta_overrides FROM packages WHERE id=?")?;
            let mut rows = stmt.query([package_id])?;
            if let Some(r) = rows.next()? {
                Ok(Some((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                )))
            } else {
                Ok(None)
            }
        })
        .await
        .map_err(IngestError::Db)?;

    let Some((status, sha256, meta_overrides_raw)) = pkg_info else {
        return Err(IngestError::NotFound("资料包不存在".into()));
    };

    if status != "draft" {
        return Err(IngestError::InvalidState("资料包不是草稿状态".into()));
    }

    let zip_path = get_package_zip_path(config, &sha256);
    let checked = validate_package_archive(&zip_path, config, true)
        .map_err(|e| IngestError::Validation(e.to_string()))?;

    let overrides: HashMap<String, Value> = meta_overrides_raw
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    // 校验替代目标映射合法性
    let pkg_doc_hashes: HashSet<&str> = checked
        .documents
        .iter()
        .map(|d| d["doc_hash"].as_str().unwrap())
        .collect();
    for key in replacements.keys() {
        if !pkg_doc_hashes.contains(key.as_str()) {
            let _ = fs::remove_dir_all(&checked.root);
            return Err(IngestError::Validation("替代映射包含包外文档".into()));
        }
    }

    // 校验替代目标是否存在且不重复
    let mut targets_set = HashSet::new();
    for doc in &checked.documents {
        let h = doc["doc_hash"].as_str().unwrap();
        let target = replacements.get(h).cloned().flatten().or_else(|| {
            overrides
                .get(h)
                .and_then(|m| m.get("replaces"))
                .or_else(|| doc.get("replaces"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });

        if let Some(tgt) = target {
            let tgt_str = tgt.clone();
            let matches = db
                .read(move |conn| {
                    let mut stmt =
                        conn.prepare("SELECT id FROM documents WHERE doc_uid=? OR doc_hash=?")?;
                    let mut rows = stmt.query([&tgt_str, &tgt_str])?;
                    let mut ids = Vec::new();
                    while let Some(r) = rows.next()? {
                        ids.push(r.get::<_, i64>(0)?);
                    }
                    Ok(ids)
                })
                .await
                .map_err(IngestError::Db)?;

            if matches.len() != 1 {
                let _ = fs::remove_dir_all(&checked.root);
                return Err(IngestError::BadTarget(
                    "替代目标不存在或不唯一，请使用文档标识".into(),
                ));
            }

            let old_id = matches[0];
            if targets_set.contains(&old_id) {
                let _ = fs::remove_dir_all(&checked.root);
                return Err(IngestError::Conflict(
                    "同一旧版已有替代版本，请选择当前链尾版本".into(),
                ));
            }

            let has_replaced = db
                .read(move |conn| {
                    let mut stmt =
                        conn.prepare("SELECT id FROM documents WHERE replaces_doc_id=?")?;
                    let mut rows = stmt.query([old_id])?;
                    Ok(rows.next()?.is_some())
                })
                .await
                .map_err(IngestError::Db)?;

            if has_replaced {
                let _ = fs::remove_dir_all(&checked.root);
                return Err(IngestError::Conflict(
                    "同一旧版已有替代版本，请选择当前链尾版本".into(),
                ));
            }

            targets_set.insert(old_id);
        }

        // 校验是否与历史行号冲突（同 doc_hash 不同 text_sha256）
        let h_str = h.to_string();
        let existing_text_sha = db
            .read(move |conn| {
                let mut stmt =
                    conn.prepare("SELECT text_sha256 FROM documents WHERE doc_hash=?")?;
                let mut rows = stmt.query([&h_str])?;
                if let Some(r) = rows.next()? {
                    Ok(Some(r.get::<_, String>(0)?))
                } else {
                    Ok(None)
                }
            })
            .await
            .map_err(IngestError::Db)?;

        if let Some(txt_sha) = existing_text_sha {
            let matches_history = txt_sha == doc["text_sha256"].as_str().unwrap();
            if !matches_history {
                let _ = fs::remove_dir_all(&checked.root);
                return Err(IngestError::Conflict(
                    "同一原文件已存在不同标准化原文，不能覆盖历史引用".into(),
                ));
            }
        }
    }

    // 拷贝文件到正式目录
    let files_dir = config.data_dir.join("files");
    let text_dir = config.data_dir.join("text");
    let vectors_dir = config.data_dir.join("vectors");
    fs::create_dir_all(&files_dir)?;
    fs::create_dir_all(&text_dir)?;

    let final_vec = vectors_dir.join(format!("pkg-{}.npy", package_id));
    let draft_vec = vectors_dir.join(format!("pkg-draft-{}.npy", &sha256[..16]));

    let root_files = checked.root.join("files");
    if root_files.exists() {
        for entry in fs::read_dir(root_files)? {
            let entry = entry?;
            let dest = files_dir.join(entry.file_name());
            if !dest.exists() {
                fs::copy(entry.path(), dest)?;
            }
        }
    }

    let root_text = checked.root.join("text");
    if root_text.exists() {
        for entry in fs::read_dir(root_text)? {
            let entry = entry?;
            let dest = text_dir.join(entry.file_name());
            if !dest.exists() {
                fs::copy(entry.path(), dest)?;
            }
        }
    }

    fs::copy(checked.root.join("vectors.npy"), &final_vec)?;
    let _ = fs::remove_file(&draft_vec);

    // 事务写入数据库
    let docs = checked.documents.clone();
    let num_docs = docs.len();

    db.write(move |conn| {
        let tx = conn.transaction()?;
        let now = utcnow();

        for d in docs {
            let h = d["doc_hash"].as_str().unwrap();
            let meta = overrides.get(h).cloned().unwrap_or(json!({}));
            let new_uid = hex::encode(rand::random::<[u8; 16]>());

            let target = replacements
                .get(h)
                .cloned()
                .flatten()
                .or_else(|| {
                    meta.get("replaces")
                        .or_else(|| d.get("replaces"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                });

            let mut replaces_id = None;
            if let Some(tgt) = target {
                let mut stmt = tx.prepare("SELECT id, deactivated_kind FROM documents WHERE doc_uid=? OR doc_hash=?")?;
                let mut rows = stmt.query([&tgt, &tgt])?;
                if let Some(old) = rows.next()? {
                    let old_id: i64 = old.get(0)?;
                    let old_deact: String = old.get(1)?;
                    replaces_id = Some(old_id);
                    if old_deact.is_empty() {
                        tx.execute(
                            "UPDATE documents SET deactivated_kind='superseded', deactivated_at=? WHERE id=?",
                            params![now, old_id],
                        )?;
                    }
                }
            }

            let title = meta.get("title").unwrap_or(&d["title"]).as_str().unwrap().to_string();
            let original_filename = d["original_filename"].as_str().unwrap().to_string();
            let doc_type = d["doc_type"].as_str().unwrap().to_string();
            let department = meta.get("department").unwrap_or(&d["department"]).as_str().unwrap().to_string();
            let effective_date = meta.get("effective_date").unwrap_or(&d["effective_date"]).as_str().map(|s| s.to_string());
            let audience = serde_json::to_string(meta.get("audience").unwrap_or(&d["audience"])).unwrap();
            let audience_scope = serde_json::to_string(meta.get("audience_scope").or_else(|| d.get("audience_scope")).unwrap_or(&json!({}))).unwrap();
            let notes = meta.get("notes").or_else(|| d.get("notes")).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let line_count = d["line_count"].as_i64().unwrap();
            let page_map = d.get("page_map").map(|p| serde_json::to_string(p).unwrap());
            let text_sha256 = d["text_sha256"].as_str().unwrap().to_string();

            tx.execute(
                "INSERT INTO documents(doc_uid, doc_hash, package_id, title, original_filename, \
                 doc_type, department, effective_date, audience, audience_scope, notes, line_count, \
                 page_map, text_sha256, replaces_doc_id, published_at, deactivated_kind) \
                 VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, '')",
                params![
                    new_uid,
                    h,
                    package_id,
                    title,
                    original_filename,
                    doc_type,
                    department,
                    effective_date,
                    audience,
                    audience_scope,
                    notes,
                    line_count,
                    page_map,
                    text_sha256,
                    replaces_id,
                    now
                ],
            )?;

            let doc_id = tx.last_insert_rowid();

            if let Some(sections) = d.get("sections").and_then(|v| v.as_array()) {
                for sec in sections {
                    tx.execute(
                        "INSERT INTO sections(doc_id, section_id, title, start_line, end_line) VALUES(?, ?, ?, ?, ?)",
                        params![
                            doc_id,
                            sec["section_id"].as_str().unwrap(),
                            sec["title"].as_str().unwrap(),
                            sec["start_line"].as_i64().unwrap(),
                            sec["end_line"].as_i64().unwrap()
                        ],
                    )?;
                }
            }

            if let Some(domains) = d.get("domains").and_then(|v| v.as_array()) {
                for dom in domains {
                    let tag = dom["tag"].as_str().unwrap();
                    let sids = dom.get("section_ids").and_then(|s| s.as_array());
                    if let Some(sids_arr) = sids {
                        for sid in sids_arr {
                            tx.execute(
                                "INSERT INTO doc_tags(doc_id, section_id, tag) VALUES(?, ?, ?)",
                                params![doc_id, sid.as_str().unwrap(), tag],
                            )?;
                        }
                    } else {
                        tx.execute(
                            "INSERT INTO doc_tags(doc_id, section_id, tag) VALUES(?, NULL, ?)",
                            params![doc_id, tag],
                        )?;
                    }
                }
            }

            if let Some(chunks) = d.get("chunks").and_then(|v| v.as_array()) {
                for ch in chunks {
                    let text = ch["text"].as_str().unwrap();
                    let fts_body = tokenize_for_fts(text);
                    let vi = ch["vector_index"].as_i64().unwrap();
                    let l_start = ch["line_start"].as_i64().unwrap();
                    let l_end = ch["line_end"].as_i64().unwrap();
                    let s_id = ch.get("section_id").and_then(|v| v.as_str());

                    tx.execute(
                        "INSERT INTO chunks(doc_id, chunk_index, text, line_start, line_end, section_id) \
                         VALUES(?, ?, ?, ?, ?, ?)",
                        params![doc_id, vi, text, l_start, l_end, s_id],
                    )?;

                    let chunk_id = tx.last_insert_rowid();

                    tx.execute(
                        "INSERT INTO chunks_fts(rowid, body) VALUES(?, ?)",
                        params![chunk_id, fts_body],
                    )?;

                    tx.execute(
                        "INSERT INTO vector_rows(chunk_id, row_index, doc_id, package_id) VALUES(?, ?, ?, ?)",
                        params![chunk_id, vi, doc_id, package_id],
                    )?;
                }
            }
        }

        tx.execute(
            "UPDATE packages SET status='published' WHERE id=?",
            [package_id],
        )?;

        tx.execute(
            "INSERT INTO audit_log(at, actor, action, detail) VALUES(?, ?, ?, ?)",
            params![now, "admin", "package_publish", format!("package={} docs={}", package_id, num_docs)],
        )?;

        tx.commit()?;
        Ok(())
    })
    .await
    .map_err(IngestError::Db)?;

    let _ = fs::remove_dir_all(&checked.root);
    Ok(())
}

pub async fn deactivate_document(db: &DbPool, doc_uid: &str) -> Result<(), IngestError> {
    let uid = doc_uid.to_string();
    db.write(move |conn| {
        let mut stmt =
            conn.prepare("SELECT id, deactivated_kind FROM documents WHERE doc_uid=?")?;
        let mut rows = stmt.query([&uid])?;
        let Some(r) = rows.next()? else {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        };
        let id: i64 = r.get(0)?;
        let kind: String = r.get(1)?;
        if kind == "manual" {
            return Ok(false);
        }
        let now = utcnow();
        conn.execute(
            "UPDATE documents SET deactivated_kind='manual', deactivated_at=? WHERE id=?",
            params![now, id],
        )?;
        conn.execute(
            "INSERT INTO audit_log(at, actor, action, detail) VALUES(?, ?, ?, ?)",
            params![now, "admin", "document_deactivate", format!("doc={}", uid)],
        )?;
        Ok(true)
    })
    .await
    .map_err(|e| {
        if let rusqlite::Error::QueryReturnedNoRows = e {
            IngestError::NotFound("资料不存在".into())
        } else {
            IngestError::Db(e)
        }
    })?
    .then_some(())
    .ok_or_else(|| IngestError::InvalidState("该资料已处于手动停用状态".into()))
}

pub async fn enable_document(db: &DbPool, doc_uid: &str) -> Result<(), IngestError> {
    let uid = doc_uid.to_string();
    let res = db
        .write(move |conn| {
            let mut stmt =
                conn.prepare("SELECT id, deactivated_kind FROM documents WHERE doc_uid=?")?;
            let mut rows = stmt.query([&uid])?;
            let Some(r) = rows.next()? else {
                return Ok(Err("NOT_FOUND".to_string()));
            };
            let id: i64 = r.get(0)?;
            let kind: String = r.get(1)?;
            if kind.is_empty() {
                return Ok(Ok(()));
            }

            let mut newer_stmt =
                conn.prepare("SELECT doc_uid, title FROM documents WHERE replaces_doc_id=?")?;
            let mut newer_rows = newer_stmt.query([id])?;
            if let Some(nr) = newer_rows.next()? {
                let n_uid: String = nr.get(0)?;
                let n_title: String = nr.get(1)?;
                return Ok(Err(format!(
                    "该版本已被《{}》（{}…）替代，须先解除替代关系再启用",
                    n_title,
                    &n_uid[..8.min(n_uid.len())]
                )));
            }

            let now = utcnow();
            conn.execute(
                "UPDATE documents SET deactivated_kind='', deactivated_at=NULL WHERE id=?",
                [id],
            )?;
            conn.execute(
                "INSERT INTO audit_log(at, actor, action, detail) VALUES(?, ?, ?, ?)",
                params![now, "admin", "document_enable", format!("doc={}", uid)],
            )?;
            Ok(Ok(()))
        })
        .await
        .map_err(IngestError::Db)?;

    match res {
        Ok(()) => Ok(()),
        Err(msg) if msg == "NOT_FOUND" => Err(IngestError::NotFound("资料不存在".into())),
        Err(msg) => Err(IngestError::Conflict(msg)),
    }
}

pub async fn unlink_replacement(db: &DbPool, doc_uid: &str) -> Result<(), IngestError> {
    let uid = doc_uid.to_string();
    let res = db
        .write(move |conn| {
            let mut stmt =
                conn.prepare("SELECT id, replaces_doc_id FROM documents WHERE doc_uid=?")?;
            let mut rows = stmt.query([&uid])?;
            let Some(r) = rows.next()? else {
                return Ok(Err("NOT_FOUND".to_string()));
            };
            let id: i64 = r.get(0)?;
            let replaces_id: Option<i64> = r.get(1)?;
            if replaces_id.is_none() {
                return Ok(Err("NO_RELATION".to_string()));
            }

            let now = utcnow();
            conn.execute("UPDATE documents SET replaces_doc_id=NULL WHERE id=?", [id])?;
            conn.execute(
                "INSERT INTO audit_log(at, actor, action, detail) VALUES(?, ?, ?, ?)",
                params![now, "admin", "document_unlink", format!("doc={}", uid)],
            )?;
            Ok(Ok(()))
        })
        .await
        .map_err(IngestError::Db)?;

    match res {
        Ok(()) => Ok(()),
        Err(msg) if msg == "NOT_FOUND" => Err(IngestError::NotFound("资料不存在".into())),
        Err(msg) if msg == "NO_RELATION" => Err(IngestError::Conflict("该资料没有替代关系".into())),
        Err(msg) => Err(IngestError::Conflict(msg)),
    }
}
