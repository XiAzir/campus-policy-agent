use campus_policy_backend::db::DbPool;
use campus_policy_backend::retrieval::{allowed_doc_ids, search};
use campus_policy_backend::vectors::VectorIndex;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;
use sysinfo::{Pid, ProcessesToUpdate, System};
use zip::ZipArchive;

fn get_process_memory_mib() -> f64 {
    let mut sys = System::new();
    let pid = Pid::from_u32(std::process::id());
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    if let Some(proc) = sys.process(pid) {
        (proc.memory() as f64) / 1024.0 / 1024.0
    } else {
        0.0
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("============================================================");
    println!("      M2 阶段：D1 合成数据集检索性能与内存基准初测");
    println!("============================================================");

    let mem_start = get_process_memory_mib();
    println!("初始进程常驻内存 (RSS): {:.2} MiB", mem_start);

    let d1_dir = if Path::new(".local-acceptance/datasets/d1").exists() {
        PathBuf::from(".local-acceptance/datasets/d1")
    } else if Path::new("../.local-acceptance/datasets/d1").exists() {
        PathBuf::from("../.local-acceptance/datasets/d1")
    } else {
        eprintln!("D1 数据集不存在，请先运行 scripts/rust/gen_d1.py --dataset d1");
        return Ok(());
    };

    let temp_bench_dir = tempfile::tempdir()?;
    let bench_path = temp_bench_dir.path();
    let db_path = bench_path.join("campus.db");
    let vectors_dir = bench_path.join("vectors");
    let text_dir = bench_path.join("text");
    std::fs::create_dir_all(&vectors_dir)?;
    std::fs::create_dir_all(&text_dir)?;

    let pool = DbPool::new(&db_path, 2, 64)?;

    println!("\n[1/3] 导入 D1 合成数据包（5个包，共 100 篇文档，10,000 分块）...");
    let import_start = Instant::now();

    for pkg_id in 1..=5 {
        let zip_path = d1_dir.join(format!("d1-pkg-{:03}.zip", pkg_id));
        let file = File::open(&zip_path)?;
        let mut archive = ZipArchive::new(file)?;

        // 1. 抽取向量 vectors.npy -> vectors/pkg-<id>.npy
        {
            let mut vec_entry = archive.by_name("vectors.npy")?;
            let dest_vec = vectors_dir.join(format!("pkg-{}.npy", pkg_id));
            let mut out = File::create(dest_vec)?;
            std::io::copy(&mut vec_entry, &mut out)?;
        }

        // 2. 读取 manifest.json 并插入数据库
        let manifest: serde_json::Value = {
            let mut manifest_entry = archive.by_name("manifest.json")?;
            let mut s = String::new();
            manifest_entry.read_to_string(&mut s)?;
            serde_json::from_str(&s)?
        };

        // 3. 解压文本 text/ 到本地 text 目录
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            let name = entry.name().to_string();
            if name.starts_with("text/") && name.ends_with(".txt") {
                let filename = Path::new(&name).file_name().unwrap();
                let dest = text_dir.join(filename);
                let mut out = File::create(dest)?;
                std::io::copy(&mut entry, &mut out)?;
            }
        }

        // 4. 插入 DB 表
        let docs = manifest["documents"].as_array().unwrap().clone();
        let pkg_sha = format!("pkg-sha256-dummy-{:03}", pkg_id);

        pool.write(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO packages(id, sha256, original_filename, size, imported_at, status, preprocessing_version, embed_model, embed_dim, doc_count, chunk_count)
                 VALUES(?, ?, ?, ?, '2026-09-06T12:00:00Z', 'published', 'v1', 'test-embed', 1024, ?, ?)",
                rusqlite::params![pkg_id, pkg_sha, format!("d1-pkg-{:03}.zip", pkg_id), 8_400_000, docs.len(), 2000],
            )?;

            for d in &docs {
                let doc_hash = d["doc_hash"].as_str().unwrap();
                let doc_uid = format!("uid-{}-{}", pkg_id, &doc_hash[..12]);
                let title = d["title"].as_str().unwrap();
                let doc_type = d["doc_type"].as_str().unwrap();
                let dept = d["department"].as_str().unwrap();
                let eff_date = d["effective_date"].as_str().unwrap();
                let line_count = d["line_count"].as_i64().unwrap();

                tx.execute(
                    "INSERT INTO documents(doc_uid, doc_hash, package_id, title, original_filename, doc_type, department, effective_date, line_count, text_sha256, published_at, deactivated_kind)
                     VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, '2026-09-06T12:00:00Z', '')",
                    rusqlite::params![doc_uid, doc_hash, pkg_id, title, format!("{}.txt", doc_hash), doc_type, dept, eff_date, line_count, doc_hash],
                )?;

                let doc_id = tx.last_insert_rowid();

                for dom in d["domains"].as_array().unwrap() {
                    let tag = dom["tag"].as_str().unwrap();
                    tx.execute(
                        "INSERT INTO doc_tags(doc_id, tag) VALUES(?, ?)",
                        rusqlite::params![doc_id, tag],
                    )?;
                }

                for ch in d["chunks"].as_array().unwrap() {
                    let v_idx = ch["vector_index"].as_i64().unwrap();
                    let text = ch["text"].as_str().unwrap();
                    let ls = ch["line_start"].as_i64().unwrap();
                    let le = ch["line_end"].as_i64().unwrap();
                    let sec_id = ch["section_id"].as_str().unwrap();

                    tx.execute(
                        "INSERT INTO chunks(doc_id, chunk_index, text, line_start, line_end, section_id)
                         VALUES(?, ?, ?, ?, ?, ?)",
                        rusqlite::params![doc_id, v_idx, text, ls, le, sec_id],
                    )?;

                    let chunk_id = tx.last_insert_rowid();

                    let fts_body = campus_policy_backend::tokenizer::tokenize_for_fts(text);
                    tx.execute(
                        "INSERT INTO chunks_fts(rowid, body) VALUES(?, ?)",
                        rusqlite::params![chunk_id, fts_body],
                    )?;

                    tx.execute(
                        "INSERT INTO vector_rows(chunk_id, row_index, doc_id, package_id)
                         VALUES(?, ?, ?, ?)",
                        rusqlite::params![chunk_id, v_idx, doc_id, pkg_id],
                    )?;
                }
            }

            tx.commit()?;
            Ok(())
        })
        .await?;
    }

    println!(
        "D1 数据库灌库完成，耗时: {:.2}s",
        import_start.elapsed().as_secs_f64()
    );
    let mem_after_import = get_process_memory_mib();
    println!("灌库后进程常驻内存 (RSS): {:.2} MiB", mem_after_import);

    println!("\n[2/3] 执行 100 次混合检索 (SQLite FTS5 + 1024 维真实向量 + RRF)...");
    let vectors = VectorIndex::new(&vectors_dir);
    let all_current = allowed_doc_ids(&pool, "current", None, None, None).await?;
    assert_eq!(all_current.len(), 100, "100 份文档均应现行可检索");

    // 构造一个固定种子的归一化 1024 维查询向量
    let mut query_vec = vec![0.0f32; 1024];
    query_vec[0] = 1.0; // 单方向单元向量

    let test_queries = [
        "学生 处分 申诉",
        "奖学金 评定 绩点",
        "社会实践 三下乡 报名",
        "宿舍 安全 违规",
        "创新创业 项目 立项 经费",
    ];

    let mut latencies = Vec::with_capacity(100);
    let search_start = Instant::now();

    for i in 0..100 {
        let q = test_queries[i % test_queries.len()];
        let t0 = Instant::now();
        let hits = search(&pool, &vectors, q, Some(&query_vec), &all_current, 8, None).await?;
        let elapsed = t0.elapsed().as_secs_f64();
        latencies.push(elapsed);
        assert!(!hits.is_empty(), "第 {} 次检索应该返回结果", i);
    }

    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = latencies[50];
    let p95 = latencies[95];
    let p99 = latencies[99];

    println!(
        "100 次混合检索完成，总耗时: {:.2}s",
        search_start.elapsed().as_secs_f64()
    );
    println!(
        "延迟指标: p50 = {:.3}s, p95 = {:.3}s, p99 = {:.3}s",
        p50, p95, p99
    );

    println!("\n[3/3] 内存指标与门槛验证 (对照 Spec 3.3)...");
    let mem_end = get_process_memory_mib();
    println!("检索完成后的进程常驻内存 (RSS): {:.2} MiB", mem_end);
    println!("内存增量 (RSS Delta): {:.2} MiB", mem_end - mem_start);

    // 验收断言：Spec 3.3 门槛: D1 热态检索 p95 <= 3.0 秒，RSS < 160 MiB
    assert!(p95 < 3.0, "p95 延迟超标: {:.3}s > 3.0s", p95);
    assert!(
        mem_end < 160.0,
        "常驻内存 RSS 超标: {:.2} MiB > 160 MiB",
        mem_end
    );

    println!("\n============================================================");
    println!("  M2 基准验收结果: 全部达标 (PASS)");
    println!("  - 10,000 分块 / 1024 维 (39.1 MiB 裸向量) 全库扫描");
    println!("  - p95 混合检索延迟: {:.3}s (规范上限 3.0s)", p95);
    println!("  - 进程常驻 RSS: {:.2} MiB (规范上限 160.0 MiB)", mem_end);
    println!("============================================================");

    Ok(())
}
