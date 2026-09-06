use crate::audience::match_audience;
use crate::db::DbPool;
use crate::tokenizer::fts_match_query;
use crate::vectors::VectorIndex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const RRF_K: f64 = 60.0;
pub const TOP_K_FTS_FACTOR: usize = 3;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SearchHit {
    pub chunk_id: i64,
    pub score: f64,
    pub doc_uid: String,
    pub doc_hash: String,
    pub title: String,
    pub doc_type: String,
    pub line_start: i64,
    pub line_end: i64,
    pub section_id: Option<String>,
    pub page: Option<i64>,
    pub text: String,
    pub audience: serde_json::Value,
    pub audience_scope: serde_json::Value,
    pub effective_date: Option<String>,
}

/// 计算行号在 page_map（[[mark_line, page_no], ...]）中对应的页码
pub fn page_for_line(page_map: &Option<Vec<[i64; 2]>>, line: i64) -> Option<i64> {
    let marks = page_map.as_ref()?;
    let mut current_page = None;
    for &[mark_line, pg] in marks {
        if mark_line <= line {
            current_page = Some(pg);
        } else {
            break;
        }
    }
    current_page
}

/// 根据可见性（现行/往年）、显式文件限制、领域标签以及学生画像过滤允许检索的 doc_id 集合
pub async fn allowed_doc_ids(
    db: &DbPool,
    year_mode: &str,
    domains: Option<&[String]>,
    doc_uids: Option<&[String]>,
    profile: Option<&HashMap<String, String>>,
) -> Result<HashSet<i64>, rusqlite::Error> {
    let year_mode = year_mode.to_string();
    let domains = domains.map(|d| d.to_vec());
    let doc_uids = doc_uids.map(|u| u.to_vec());
    let profile = profile.cloned();

    db.read(move |conn| {
        let sql = if year_mode == "past" {
            "SELECT id, doc_uid, audience, audience_scope FROM documents WHERE deactivated_kind IN ('', 'superseded')"
        } else {
            "SELECT id, doc_uid, audience, audience_scope FROM documents WHERE deactivated_kind = ''"
        };

        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query([])?;
        let mut doc_list = Vec::new();
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let uid: String = r.get(1)?;
            let aud_raw: String = r.get(2)?;
            let scope_raw: String = r.get(3)?;

            let aud = serde_json::from_str(&aud_raw).unwrap_or(serde_json::json!([]));
            let scope = serde_json::from_str(&scope_raw).unwrap_or(serde_json::json!({}));
            doc_list.push((id, uid, aud, scope));
        }

        // 画像过滤
        if let Some(prof) = &profile {
            let college = prof.get("college").cloned().unwrap_or_default();
            let year = prof.get("entry_year").cloned().unwrap_or_default();
            doc_list.retain(|(_, _, aud, scope)| {
                let (matched, _) = match_audience(aud, scope, &college, &year);
                matched
            });
        }

        // 显式指定文档过滤
        if let Some(uids) = &doc_uids {
            let uid_set: HashSet<&str> = uids.iter().map(|s| s.as_str()).collect();
            let mut ids = HashSet::new();
            for (id, uid, _, _) in &doc_list {
                if uid_set.contains(uid.as_str()) {
                    ids.insert(*id);
                }
            }
            return Ok(ids);
        }

        let mut ids: HashSet<i64> = doc_list.into_iter().map(|(id, ..)| id).collect();

        // 领域标签过滤
        if let Some(doms) = &domains {
            if !doms.is_empty() {
                let marks = doms.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let query_sql = format!(
                    "SELECT DISTINCT doc_id FROM doc_tags WHERE tag IN ({})",
                    marks
                );
                let mut tag_stmt = conn.prepare(&query_sql)?;
                let params_vec: Vec<&dyn rusqlite::ToSql> =
                    doms.iter().map(|d| d as &dyn rusqlite::ToSql).collect();
                let mut tag_rows = tag_stmt.query(&params_vec[..])?;
                let mut matched_doc_ids = HashSet::new();
                while let Some(tr) = tag_rows.next()? {
                    matched_doc_ids.insert(tr.get::<_, i64>(0)?);
                }
                ids.retain(|id| matched_doc_ids.contains(id));
            }
        }

        Ok(ids)
    })
    .await
}

/// 执行混合检索（SQLite FTS5 bm25 + 向量点积 + RRF 融合）
pub async fn search(
    db: &DbPool,
    vectors: &VectorIndex,
    query: &str,
    query_vec: Option<&[f32]>,
    scope_doc_ids: &HashSet<i64>,
    top_k: usize,
    domains: Option<&[String]>,
) -> Result<Vec<SearchHit>, rusqlite::Error> {
    if query.trim().is_empty() || scope_doc_ids.is_empty() {
        return Ok(Vec::new());
    }

    let query_str = query.to_string();
    let scope_ids_vec: Vec<i64> = scope_doc_ids.iter().copied().collect();
    let domains_vec: Vec<String> = domains.unwrap_or(&[]).to_vec();
    let match_expr = fts_match_query(&query_str, 24);

    // 1. FTS 检索
    let fts_hits = if !match_expr.is_empty() {
        let match_copy = match_expr.clone();
        let s_ids = scope_ids_vec.clone();
        let doms = domains_vec.clone();
        let fetch_limit = top_k * TOP_K_FTS_FACTOR;

        db.read(move |conn| {
            let id_marks = s_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let (section_clause, _dom_marks) = if !doms.is_empty() {
                let tags = doms.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                (
                    format!(
                        " AND EXISTS (SELECT 1 FROM doc_tags t LEFT JOIN sections s ON s.doc_id=t.doc_id AND s.section_id=t.section_id \
                         WHERE t.doc_id=c.doc_id AND t.tag IN ({}) AND (t.section_id IS NULL OR \
                         (c.line_start>=s.start_line AND c.line_end<=s.end_line)))",
                        tags
                    ),
                    tags,
                )
            } else {
                (String::new(), String::new())
            };

            let sql = format!(
                "SELECT f.rowid AS chunk_id, bm25(chunks_fts) AS score \
                 FROM chunks_fts f JOIN chunks c ON c.id=f.rowid \
                 WHERE chunks_fts MATCH ? AND c.doc_id IN ({}) \
                 {} \
                 ORDER BY score LIMIT ?",
                id_marks, section_clause
            );

            let mut stmt = conn.prepare(&sql)?;
            let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::new();
            params_vec.push(&match_copy);
            for id in &s_ids {
                params_vec.push(id);
            }
            for d in &doms {
                params_vec.push(d);
            }
            params_vec.push(&fetch_limit);

            let mut rows = stmt.query(&params_vec[..])?;
            let mut hits = Vec::new();
            while let Some(r) = rows.next()? {
                let chunk_id: i64 = r.get(0)?;
                let score: f64 = r.get(1)?;
                hits.push((chunk_id, -score));
            }
            Ok(hits)
        })
        .await
        .unwrap_or_default()
    } else {
        Vec::new()
    };

    // 2. 向量检索
    let mut vec_hits = Vec::new();
    if let Some(q_vec) = query_vec {
        let s_ids = scope_ids_vec.clone();
        let doms = domains_vec.clone();

        let vrows = db
            .read(move |conn| {
                let id_marks = s_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let section_clause = if !doms.is_empty() {
                    let tags = doms.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                    format!(
                        " AND EXISTS (SELECT 1 FROM doc_tags t LEFT JOIN sections s ON s.doc_id=t.doc_id AND s.section_id=t.section_id \
                         WHERE t.doc_id=c.doc_id AND t.tag IN ({}) AND (t.section_id IS NULL OR \
                         (c.line_start>=s.start_line AND c.line_end<=s.end_line)))",
                        tags
                    )
                } else {
                    String::new()
                };

                let sql = format!(
                    "SELECT v.package_id, v.row_index, v.chunk_id \
                     FROM vector_rows v JOIN chunks c ON c.id=v.chunk_id \
                     WHERE v.doc_id IN ({}) {}",
                    id_marks, section_clause
                );

                let mut stmt = conn.prepare(&sql)?;
                let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::new();
                for id in &s_ids {
                    params_vec.push(id);
                }
                for d in &doms {
                    params_vec.push(d);
                }

                let mut rows = stmt.query(&params_vec[..])?;
                let mut list = Vec::new();
                while let Some(r) = rows.next()? {
                    list.push((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as usize, r.get::<_, i64>(2)?));
                }
                Ok(list)
            })
            .await
            .unwrap_or_default();

        let mut candidates: HashMap<i64, Vec<usize>> = HashMap::new();
        let mut by_row: HashMap<(i64, usize), i64> = HashMap::new();
        for (pkg_id, row_idx, chunk_id) in vrows {
            candidates.entry(pkg_id).or_default().push(row_idx);
            by_row.insert((pkg_id, row_idx), chunk_id);
        }

        if let Ok(top_vector_hits) = vectors.search(q_vec, &candidates, top_k * TOP_K_FTS_FACTOR) {
            for (_sim, pkg_id, row_idx) in top_vector_hits {
                if let Some(&cid) = by_row.get(&(pkg_id, row_idx)) {
                    vec_hits.push((cid, 1.0));
                }
            }
        }
    }

    // 3. RRF 排名融合
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for (rank, (cid, _)) in fts_hits.into_iter().enumerate() {
        let entry = scores.entry(cid).or_default();
        *entry += 1.0 / (RRF_K + (rank as f64) + 1.0);
    }
    for (rank, (cid, _)) in vec_hits.into_iter().enumerate() {
        let entry = scores.entry(cid).or_default();
        *entry += 1.0 / (RRF_K + (rank as f64) + 1.0);
    }

    let mut ordered: Vec<(i64, f64)> = scores.into_iter().collect();
    // 降序排序，分高在前
    ordered.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ordered.truncate(top_k);

    if ordered.is_empty() {
        return Ok(Vec::new());
    }

    // 4. 填充分块与文档详细信息
    let ordered_cids: Vec<i64> = ordered.iter().map(|(cid, _)| *cid).collect();
    let s_ids = scope_ids_vec.clone();

    db.read(move |conn| {
        let chunk_marks = ordered_cids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let chunk_sql = format!(
            "SELECT id, doc_id, text, line_start, line_end, section_id FROM chunks WHERE id IN ({})",
            chunk_marks
        );
        let mut chunk_stmt = conn.prepare(&chunk_sql)?;
        let cparams: Vec<&dyn rusqlite::ToSql> = ordered_cids.iter().map(|c| c as &dyn rusqlite::ToSql).collect();
        let mut chunk_rows = chunk_stmt.query(&cparams[..])?;

        struct ChunkInfo {
            doc_id: i64,
            text: String,
            line_start: i64,
            line_end: i64,
            section_id: Option<String>,
        }

        let mut chunks_map = HashMap::new();
        while let Some(cr) = chunk_rows.next()? {
            let cid: i64 = cr.get(0)?;
            chunks_map.insert(
                cid,
                ChunkInfo {
                    doc_id: cr.get(1)?,
                    text: cr.get(2)?,
                    line_start: cr.get(3)?,
                    line_end: cr.get(4)?,
                    section_id: cr.get(5)?,
                },
            );
        }

        let doc_marks = s_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let doc_sql = format!(
            "SELECT id, doc_uid, doc_hash, title, doc_type, effective_date, audience, audience_scope, page_map \
             FROM documents WHERE id IN ({})",
            doc_marks
        );
        let mut doc_stmt = conn.prepare(&doc_sql)?;
        let dparams: Vec<&dyn rusqlite::ToSql> = s_ids.iter().map(|d| d as &dyn rusqlite::ToSql).collect();
        let mut doc_rows = doc_stmt.query(&dparams[..])?;

        struct DocInfo {
            doc_uid: String,
            doc_hash: String,
            title: String,
            doc_type: String,
            effective_date: Option<String>,
            audience: serde_json::Value,
            audience_scope: serde_json::Value,
            page_map: Option<Vec<[i64; 2]>>,
        }

        let mut docs_map = HashMap::new();
        while let Some(dr) = doc_rows.next()? {
            let id: i64 = dr.get(0)?;
            let page_map_raw: Option<String> = dr.get(8)?;
            let page_map: Option<Vec<[i64; 2]>> = page_map_raw
                .and_then(|s| serde_json::from_str(&s).ok());

            docs_map.insert(
                id,
                DocInfo {
                    doc_uid: dr.get(1)?,
                    doc_hash: dr.get(2)?,
                    title: dr.get(3)?,
                    doc_type: dr.get(4)?,
                    effective_date: dr.get(5)?,
                    audience: serde_json::from_str(&dr.get::<_, String>(6)?).unwrap_or(serde_json::json!([])),
                    audience_scope: serde_json::from_str(&dr.get::<_, String>(7)?).unwrap_or(serde_json::json!({})),
                    page_map,
                },
            );
        }

        let mut hits = Vec::new();
        for (cid, score) in ordered {
            let Some(ch) = chunks_map.get(&cid) else { continue };
            let Some(doc) = docs_map.get(&ch.doc_id) else { continue };

            let page = page_for_line(&doc.page_map, ch.line_start);

            hits.push(SearchHit {
                chunk_id: cid,
                score: (score * 1_000_000.0).round() / 1_000_000.0,
                doc_uid: doc.doc_uid.clone(),
                doc_hash: doc.doc_hash.clone(),
                title: doc.title.clone(),
                doc_type: doc.doc_type.clone(),
                line_start: ch.line_start,
                line_end: ch.line_end,
                section_id: ch.section_id.clone(),
                page,
                text: ch.text.clone(),
                audience: doc.audience.clone(),
                audience_scope: doc.audience_scope.clone(),
                effective_date: doc.effective_date.clone(),
            });
        }

        Ok(hits)
    })
    .await
}

#[cfg(test)]
mod tests {
    use crate::tokenizer::tokenize_for_fts;
    use rusqlite::Connection;
    use serde_json::Value;
    use std::fs;
    use std::path::Path;

    #[test]
    fn test_fts_bm25_golden_alignment() {
        let fixture_path = Path::new("tests/fixtures/retrieval/fts_bm25_golden.json");
        let content = fs::read_to_string(fixture_path).expect("读取 fts_bm25 夹具失败");
        let fixture: Value = serde_json::from_str(&content).unwrap();

        // 建立内存 FTS5 库与语料
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE chunks_fts USING fts5(body);",
        )
        .unwrap();

        let jieba_fixture_path = Path::new("tests/fixtures/tokenize/jieba_golden.json");
        let jieba_content = fs::read_to_string(jieba_fixture_path).expect("读取分词夹具失败");
        let jieba_fixture: Value = serde_json::from_str(&jieba_content).unwrap();

        for (i, item) in jieba_fixture["corpus"].as_array().unwrap().iter().take(12).enumerate() {
            let text = item["text"].as_str().unwrap();
            let body = tokenize_for_fts(text);
            conn.execute(
                "INSERT INTO chunks_fts(rowid, body) VALUES(?, ?)",
                rusqlite::params![i + 1, body],
            )
            .unwrap();
        }

        for case in fixture["cases"].as_array().unwrap() {
            let query = case["query"].as_str().unwrap();
            let match_expr = case["match"].as_str().unwrap();
            let hits = case["hits"].as_array().unwrap();

            let mut stmt = conn
                .prepare("SELECT rowid, bm25(chunks_fts) AS score FROM chunks_fts WHERE chunks_fts MATCH ? ORDER BY score")
                .unwrap();
            let mut rows = stmt.query([match_expr]).unwrap();

            let mut actual_hits = Vec::new();
            while let Some(r) = rows.next().unwrap() {
                let rowid: i64 = r.get(0).unwrap();
                let score: f64 = r.get(1).unwrap();
                actual_hits.push((rowid, score));
            }

            assert_eq!(
                actual_hits.len(),
                hits.len(),
                "query '{}' 命中文档数不一致",
                query
            );

            for (idx, expected) in hits.iter().enumerate() {
                let exp_rowid = expected["rowid"].as_i64().unwrap();
                let exp_score = expected["bm25"].as_f64().unwrap();

                let (act_rowid, act_score) = actual_hits[idx];
                assert_eq!(act_rowid, exp_rowid, "命中 rowid 顺序不一致: query={}", query);

                let diff = (act_score - exp_score).abs();
                assert!(
                    diff < 1e-5,
                    "bm25 分值与 SQLite golden 偏差超限: diff={}, act={}, exp={}",
                    diff,
                    act_score,
                    exp_score
                );
            }
        }
    }
}
