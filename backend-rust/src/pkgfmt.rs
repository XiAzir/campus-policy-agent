use crate::config::Config;
use crate::storage::hash_file;
use crate::vectors::parse_npy_header;
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use zip::ZipArchive;

pub const FORMAT_VERSION: i64 = 1;
pub const MAX_PACKAGE_BYTES: u64 = 200 * 1024 * 1024;
pub const MAX_TOTAL_UNCOMPRESSED: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_RATIO: u64 = 200;
pub const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_TEXT_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_CHUNKS: usize = 20_000;

#[derive(Debug)]
pub enum PackageError {
    SafetyViolation(String),
    ManifestInvalid(String),
    FileValidationFailed(String),
    VectorValidationFailed(String),
    SizeLimitExceeded(String),
    Io(std::io::Error),
    Zip(zip::result::ZipError),
}

impl std::fmt::Display for PackageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SafetyViolation(msg) => write!(f, "安全检查未通过：{}", msg),
            Self::ManifestInvalid(msg) => write!(f, "元数据格式不合法：{}", msg),
            Self::FileValidationFailed(msg) => write!(f, "文件校验失败：{}", msg),
            Self::VectorValidationFailed(msg) => write!(f, "向量校验失败：{}", msg),
            Self::SizeLimitExceeded(msg) => write!(f, "大小超限：{}", msg),
            Self::Io(e) => write!(f, "IO 错误: {}", e),
            Self::Zip(e) => write!(f, "ZIP 解析错误: {}", e),
        }
    }
}

impl std::error::Error for PackageError {}

impl From<std::io::Error> for PackageError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<zip::result::ZipError> for PackageError {
    fn from(e: zip::result::ZipError) -> Self {
        Self::Zip(e)
    }
}

pub struct ValidatedPackage {
    pub manifest: Value,
    pub documents: Vec<Value>,
    pub root: PathBuf,
    pub path: PathBuf,
    pub sha256: String,
    pub size: u64,
    pub total_uncompressed: u64,
}

pub fn check_zip_safety<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<u64, PackageError> {
    let count = archive.len();
    if count > 10_002 {
        return Err(PackageError::SafetyViolation("条目过多".into()));
    }

    let allowed_top = ["manifest.json", "vectors.npy", "files", "text"];
    let allowed_prefixes = ["files/", "text/"];

    let mut seen_names = HashSet::new();
    let mut total_uncompressed: u64 = 0;
    let mut total_compressed: u64 = 0;

    let path_traversal_re = Regex::new(r"^[A-Za-z]:").unwrap();

    for i in 0..count {
        let file = archive.by_index(i)?;
        let name = file.name().to_string();

        if !seen_names.insert(name.clone()) {
            return Err(PackageError::SafetyViolation(format!(
                "存在重复路径: {}",
                name
            )));
        }

        let top_comp = if let Some(idx) = name.find('/') {
            &name[..idx]
        } else {
            &name
        };

        if !allowed_top.contains(&top_comp) {
            return Err(PackageError::SafetyViolation(format!(
                "包含不允许的条目：{}",
                top_comp
            )));
        }

        if name.contains('\\')
            || name.contains(':')
            || name.starts_with('/')
            || path_traversal_re.is_match(&name)
        {
            return Err(PackageError::SafetyViolation(format!(
                "条目路径不合法：{:?}",
                name
            )));
        }

        let parts: Vec<&str> = name.split('/').collect();
        if parts.iter().any(|&p| p == ".." || p == "." || p.is_empty()) {
            return Err(PackageError::SafetyViolation(format!(
                "条目含 .. 穿越组件：{:?}",
                name
            )));
        }

        let in_sub_dir = name != "files" && name != "text" && name.contains('/');
        if in_sub_dir {
            let valid_prefix = allowed_prefixes.iter().any(|p| name.starts_with(p));
            if !valid_prefix {
                return Err(PackageError::SafetyViolation(format!(
                    "条目不在白名单前缀内：{:?}",
                    name
                )));
            }
        }

        let is_symlink = file
            .unix_mode()
            .is_some_and(|mode| (mode & 0o170000) == 0o120000);
        if is_symlink {
            return Err(PackageError::SafetyViolation(format!(
                "条目是符号链接：{:?}",
                name
            )));
        }

        total_uncompressed += file.size();
        total_compressed += file.compressed_size();
    }

    if total_uncompressed > MAX_TOTAL_UNCOMPRESSED {
        return Err(PackageError::SizeLimitExceeded(format!(
            "解压总量超过上限 {}",
            MAX_TOTAL_UNCOMPRESSED
        )));
    }

    let comp_base = total_compressed.max(1);
    if total_uncompressed / comp_base > MAX_RATIO {
        return Err(PackageError::SafetyViolation(
            "压缩比异常（疑似解压炸弹）".into(),
        ));
    }

    Ok(total_uncompressed)
}

pub fn validate_manifest(manifest: &Value) -> Result<Vec<Value>, PackageError> {
    if manifest.get("format_version").and_then(|v| v.as_i64()) != Some(FORMAT_VERSION) {
        return Err(PackageError::ManifestInvalid(format!(
            "format_version 必须是 {}",
            FORMAT_VERSION
        )));
    }

    let missing_prep_ver = manifest
        .get("preprocessing_version")
        .and_then(|v| v.as_str())
        .is_none_or(|s| s.is_empty());
    if missing_prep_ver {
        return Err(PackageError::ManifestInvalid(
            "preprocessing_version 缺失".into(),
        ));
    }

    let missing_embed_model = manifest
        .get("embed_model")
        .and_then(|v| v.as_str())
        .is_none_or(|s| s.is_empty());
    if missing_embed_model {
        return Err(PackageError::ManifestInvalid("embed_model 缺失".into()));
    }

    let embed_dim = manifest
        .get("embed_dim")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if !(1..=8192).contains(&embed_dim) {
        return Err(PackageError::ManifestInvalid(
            "embed_dim 必须是正整数".into(),
        ));
    }

    let docs = manifest
        .get("documents")
        .and_then(|v| v.as_array())
        .ok_or_else(|| PackageError::ManifestInvalid("documents 必须是非空数组".into()))?;

    if docs.is_empty() {
        return Err(PackageError::ManifestInvalid(
            "documents 必须是非空数组".into(),
        ));
    }

    let hash_re = Regex::new(r"^[0-9a-f]{64}$").unwrap();
    let date_re = Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap();
    let allowed_doc_types = ["pdf", "docx", "md", "txt"];

    let mut seen_hashes = HashSet::new();
    let mut expected_vec = 0;

    for (i, doc) in docs.iter().enumerate() {
        let where_ctx = format!("documents[{}]", i);
        let h = doc
            .get("doc_hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                PackageError::ManifestInvalid(format!("{}.doc_hash 不合法", where_ctx))
            })?;

        if !hash_re.is_match(h) {
            return Err(PackageError::ManifestInvalid(format!(
                "{}.doc_hash 不合法",
                where_ctx
            )));
        }

        if !seen_hashes.insert(h.to_string()) {
            return Err(PackageError::ManifestInvalid(format!(
                "包内重复 doc_hash：{}…",
                &h[..12]
            )));
        }

        let doc_type = doc
            .get("doc_type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                PackageError::ManifestInvalid(format!("{}.doc_type 不合法", where_ctx))
            })?;

        if !allowed_doc_types.contains(&doc_type) {
            return Err(PackageError::ManifestInvalid(format!(
                "{}.doc_type 不合法",
                where_ctx
            )));
        }

        for key in ["title", "original_filename", "department"] {
            let val = doc.get(key).and_then(|v| v.as_str()).unwrap_or("").trim();
            if val.is_empty() {
                return Err(PackageError::ManifestInvalid(format!(
                    "{}.{} 必须是非空字符串",
                    where_ctx, key
                )));
            }
        }

        let filename = doc["original_filename"].as_str().unwrap();
        if filename.contains('/') || filename.contains('\\') || filename.contains(':') {
            return Err(PackageError::ManifestInvalid(format!(
                "{}.original_filename 类型或路径不合法",
                where_ctx
            )));
        }

        let expected_ext = format!(".{}", doc_type);
        if !filename.to_lowercase().ends_with(&expected_ext) {
            return Err(PackageError::ManifestInvalid(format!(
                "{}.original_filename 类型或路径不合法",
                where_ctx
            )));
        }

        if let Some(date_val) = doc.get("effective_date") {
            let is_not_null = !date_val.is_null();
            if is_not_null {
                let d_str = date_val.as_str().ok_or_else(|| {
                    PackageError::ManifestInvalid(format!(
                        "{}.effective_date 必须是 YYYY-MM-DD 或 null",
                        where_ctx
                    ))
                })?;
                if !date_re.is_match(d_str)
                    || chrono::NaiveDate::parse_from_str(d_str, "%Y-%m-%d").is_err()
                {
                    return Err(PackageError::ManifestInvalid("生效日期不存在".into()));
                }
            }
        }

        let aud_arr = doc
            .get("audience")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                PackageError::ManifestInvalid(format!("{}.audience 不合法", where_ctx))
            })?;

        if !aud_arr
            .iter()
            .all(|a| a.as_str().is_some_and(|s| !s.trim().is_empty()))
        {
            return Err(PackageError::ManifestInvalid(format!(
                "{}.audience 不合法",
                where_ctx
            )));
        }

        let line_count = doc.get("line_count").and_then(|v| v.as_i64()).unwrap_or(0);
        if line_count < 1 {
            return Err(PackageError::ManifestInvalid(format!(
                "{}.line_count 不合法",
                where_ctx
            )));
        }

        let text_sha = doc
            .get("text_sha256")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !hash_re.is_match(text_sha) {
            return Err(PackageError::ManifestInvalid(format!(
                "{}.text_sha256 不合法",
                where_ctx
            )));
        }

        let mut section_ids = HashSet::new();
        if let Some(sections) = doc.get("sections").and_then(|v| v.as_array()) {
            for sec in sections {
                let sid = sec.get("section_id").and_then(|v| v.as_str()).unwrap_or("");
                if sid.is_empty() {
                    return Err(PackageError::ManifestInvalid(format!(
                        "{}.sections 不合法",
                        where_ctx
                    )));
                }
                if !section_ids.insert(sid.to_string()) {
                    return Err(PackageError::ManifestInvalid(format!(
                        "{} 章节重复：{}",
                        where_ctx, sid
                    )));
                }
                let start_l = sec.get("start_line").and_then(|v| v.as_i64()).unwrap_or(0);
                let end_l = sec.get("end_line").and_then(|v| v.as_i64()).unwrap_or(0);
                if !(1 <= start_l && start_l <= end_l && end_l <= line_count) {
                    return Err(PackageError::ManifestInvalid(format!(
                        "{} 章节 {} 行区间越界",
                        where_ctx, sid
                    )));
                }
            }
        }

        let chunks = doc
            .get("chunks")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                PackageError::ManifestInvalid(format!("{}.chunks 必须是非空数组", where_ctx))
            })?;

        if chunks.is_empty() {
            return Err(PackageError::ManifestInvalid(format!(
                "{}.chunks 必须是非空数组",
                where_ctx
            )));
        }

        let mut prev_end = 0;
        for ch in chunks {
            let vi = ch
                .get("vector_index")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1);
            if vi != expected_vec {
                return Err(PackageError::ManifestInvalid(format!(
                    "{} chunk vector_index 必须从 0 开始连续",
                    where_ctx
                )));
            }
            expected_vec += 1;

            let start_l = ch.get("line_start").and_then(|v| v.as_i64()).unwrap_or(0);
            let end_l = ch.get("line_end").and_then(|v| v.as_i64()).unwrap_or(0);
            if !(1 <= start_l && start_l <= end_l && end_l <= line_count) {
                return Err(PackageError::ManifestInvalid(format!(
                    "{} chunk {} 行区间越界",
                    where_ctx, vi
                )));
            }

            if start_l < prev_end {
                return Err(PackageError::ManifestInvalid(format!(
                    "{} chunk {} 行区间与前一 chunk 重叠",
                    where_ctx, vi
                )));
            }
            prev_end = end_l;

            let text_val = ch.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if text_val.is_empty() {
                return Err(PackageError::ManifestInvalid(format!(
                    "{} chunk {} text 缺失",
                    where_ctx, vi
                )));
            }

            if let Some(sid) = ch.get("section_id").and_then(|v| v.as_str()) {
                let not_in_sections = !sid.is_empty() && !section_ids.contains(sid);
                if not_in_sections {
                    return Err(PackageError::ManifestInvalid(format!(
                        "{} chunk {} 引用不存在章节 {}",
                        where_ctx, vi, sid
                    )));
                }
            }
        }
    }

    let vec = manifest
        .get("vectors")
        .and_then(|v| v.as_object())
        .ok_or_else(|| PackageError::ManifestInvalid("vectors 缺失".into()))?;

    if vec.get("file").and_then(|v| v.as_str()) != Some("vectors.npy") {
        return Err(PackageError::ManifestInvalid(
            "vectors.file 必须是 vectors.npy".into(),
        ));
    }
    if vec.get("dtype").and_then(|v| v.as_str()) != Some("float32") {
        return Err(PackageError::ManifestInvalid(
            "vectors.dtype 必须是 float32".into(),
        ));
    }
    if vec.get("count").and_then(|v| v.as_i64()) != Some(expected_vec) {
        return Err(PackageError::ManifestInvalid(
            "vectors.count 与 chunk 总数不一致".into(),
        ));
    }
    if expected_vec > MAX_CHUNKS as i64 {
        return Err(PackageError::SizeLimitExceeded(
            "分块数量超限，请分包".into(),
        ));
    }
    if vec.get("dim").and_then(|v| v.as_i64()) != Some(embed_dim) {
        return Err(PackageError::ManifestInvalid(
            "vectors.dim 与 embed_dim 不一致".into(),
        ));
    }

    Ok(docs.clone())
}

pub fn validate_package_archive(
    zip_path: &Path,
    config: &Config,
    check_identity: bool,
) -> Result<ValidatedPackage, PackageError> {
    let file = File::open(zip_path)?;
    let size = file.metadata()?.len();
    if size > (config.max_package_mb as u64 * 1024 * 1024).min(MAX_PACKAGE_BYTES) {
        return Err(PackageError::SizeLimitExceeded(
            "包大小超过单包上限，请分包".into(),
        ));
    }

    let sha256 = hash_file(zip_path)?;
    let mut archive = ZipArchive::new(file)?;
    let total_uncompressed = check_zip_safety(&mut archive)?;

    let manifest: Value;
    {
        let mut manifest_file = archive.by_name("manifest.json").map_err(|_| {
            PackageError::SafetyViolation("缺少 manifest.json 或 vectors.npy".into())
        })?;
        if manifest_file.size() > MAX_MANIFEST_BYTES {
            return Err(PackageError::SizeLimitExceeded(
                "manifest 大小超限，请分包".into(),
            ));
        }

        let mut manifest_str = String::new();
        manifest_file.read_to_string(&mut manifest_str)?;
        manifest = serde_json::from_str(&manifest_str)
            .map_err(|_| PackageError::ManifestInvalid("manifest 顶层必须是对象".into()))?;
    }

    let docs = validate_manifest(&manifest)?;

    if check_identity {
        let m_model = manifest
            .get("embed_model")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let m_dim = manifest
            .get("embed_dim")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let m_ver = manifest
            .get("preprocessing_version")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if (m_model, m_dim as usize, m_ver)
            != (
                &config.siliconflow_model,
                config.embed_dims,
                &config.preprocessing_version,
            )
        {
            return Err(PackageError::ManifestInvalid(
                "向量模型、维度或预处理版本与服务配置不匹配，必须重新生成资料包".into(),
            ));
        }
    }

    // 解压到独立临时目录
    let parent = config.data_dir.parent().unwrap_or(&config.data_dir);
    let tmp_dir = tempfile::Builder::new()
        .prefix("cpb-pkg-")
        .tempdir_in(parent)?;
    let root = tmp_dir.keep();

    archive.extract(&root)?;

    // 校验向量文件
    let vec_path = root.join("vectors.npy");
    if !vec_path.exists() {
        return Err(PackageError::FileValidationFailed(
            "缺少 vectors.npy".into(),
        ));
    }

    let mut vfile = File::open(&vec_path)?;
    let header = parse_npy_header(&mut vfile)
        .map_err(|e| PackageError::VectorValidationFailed(format!("NPY 解析失败: {}", e)))?;

    let expected_count = manifest["vectors"]["count"].as_u64().unwrap() as usize;
    let expected_dim = manifest["vectors"]["dim"].as_u64().unwrap() as usize;
    if header.shape != vec![expected_count, expected_dim] {
        return Err(PackageError::VectorValidationFailed(
            "向量 shape 与清单不匹配".into(),
        ));
    }

    // 逐份文档校验原始文件哈希、标准化文本行与分块逐字符一致性
    for doc in &docs {
        let h = doc["doc_hash"].as_str().unwrap();
        let orig_ext = Path::new(doc["original_filename"].as_str().unwrap())
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| format!(".{}", s.to_lowercase()))
            .unwrap_or_default();

        let orig_path = root.join("files").join(format!("{}{}", h, orig_ext));
        let text_path = root.join("text").join(format!("{}.txt", h));

        if !orig_path.exists() || !text_path.exists() {
            return Err(PackageError::FileValidationFailed(
                "缺少原文件或标准化文本".into(),
            ));
        }

        let orig_hash = hash_file(&orig_path)?;
        let text_hash = hash_file(&text_path)?;

        if orig_hash != h || text_hash != doc["text_sha256"].as_str().unwrap() {
            return Err(PackageError::FileValidationFailed(
                "原文件或标准化原文哈希不匹配".into(),
            ));
        }

        let tfile = File::open(&text_path)?;
        let reader = BufReader::new(tfile);
        let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

        if lines.len() as i64 != doc["line_count"].as_i64().unwrap() {
            return Err(PackageError::FileValidationFailed(
                "标准化原文行数不匹配".into(),
            ));
        }

        if let Some(chunks) = doc.get("chunks").and_then(|v| v.as_array()) {
            for ch in chunks {
                let start_l = ch["line_start"].as_i64().unwrap() as usize;
                let end_l = ch["line_end"].as_i64().unwrap() as usize;
                let expected_chunk_text = lines[start_l - 1..end_l].join("\n");
                if ch["text"].as_str().unwrap() != expected_chunk_text {
                    return Err(PackageError::FileValidationFailed(
                        "分块 text 与标准化原文不一致".into(),
                    ));
                }
            }
        }
    }

    Ok(ValidatedPackage {
        manifest,
        documents: docs,
        root,
        path: zip_path.to_path_buf(),
        sha256,
        size,
        total_uncompressed,
    })
}
