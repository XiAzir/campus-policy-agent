use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const MAX_HEADER_BYTES: usize = 10_000;
pub const MAX_WORK_BUFFER_BYTES: usize = 4 * 1024 * 1024; // 4 MiB
pub const NPY_MAGIC: &[u8; 6] = b"\x93NUMPY";

#[derive(Debug, Clone, PartialEq)]
pub struct NpyHeader {
    pub major: u8,
    pub minor: u8,
    pub descr: String,
    pub fortran_order: bool,
    pub shape: Vec<usize>,
    pub data_offset: u64,
}

#[derive(Debug)]
pub enum NpyError {
    Io(std::io::Error),
    InvalidMagic,
    HeaderTooLarge,
    ParseError(String),
    UnsupportedDtype(String),
    UnsupportedDimension(usize),
    NonFiniteValues,
    ZeroNormRow(usize),
}

impl std::fmt::Display for NpyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO 错误: {}", e),
            Self::InvalidMagic => write!(f, "无效的 NPY 魔数"),
            Self::HeaderTooLarge => write!(f, "NPY 文件头超过 10,000 字节限制"),
            Self::ParseError(msg) => write!(f, "NPY 解析错误: {}", msg),
            Self::UnsupportedDtype(d) => write!(
                f,
                "不支持的 dtype: {}（仅支持 native float32: '<f4' 或 '=f4'）",
                d
            ),
            Self::UnsupportedDimension(d) => write!(f, "不支持的维度: {}（仅支持 2 维矩阵）", d),
            Self::NonFiniteValues => write!(f, "向量数据包含 NaN 或 Inf"),
            Self::ZeroNormRow(r) => write!(f, "第 {} 行向量模长为零或非归一化", r),
        }
    }
}

impl std::error::Error for NpyError {}

impl From<std::io::Error> for NpyError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// 解析并校验 NPY 头信息
pub fn parse_npy_header<R: Read + Seek>(reader: &mut R) -> Result<NpyHeader, NpyError> {
    reader.seek(SeekFrom::Start(0))?;
    let mut magic = [0u8; 6];
    reader.read_exact(&mut magic)?;
    if &magic != NPY_MAGIC {
        return Err(NpyError::InvalidMagic);
    }

    let mut version = [0u8; 2];
    reader.read_exact(&mut version)?;
    let major = version[0];
    let minor = version[1];

    let header_len = if major == 1 {
        let mut len_bytes = [0u8; 2];
        reader.read_exact(&mut len_bytes)?;
        u16::from_le_bytes(len_bytes) as usize
    } else if major == 2 {
        let mut len_bytes = [0u8; 4];
        reader.read_exact(&mut len_bytes)?;
        u32::from_le_bytes(len_bytes) as usize
    } else {
        return Err(NpyError::ParseError(format!(
            "不支持的 NPY 版本: {}.{}",
            major, minor
        )));
    };

    if header_len > MAX_HEADER_BYTES {
        return Err(NpyError::HeaderTooLarge);
    }

    let mut header_bytes = vec![0u8; header_len];
    reader.read_exact(&mut header_bytes)?;
    let header_str = String::from_utf8_lossy(&header_bytes);

    // 解析 Python 字典表示: {'descr': '<f4', 'fortran_order': False, 'shape': (16, 8), }
    let descr = parse_dict_field(&header_str, "descr")
        .ok_or_else(|| NpyError::ParseError("缺少 descr 字段".to_string()))?;

    // Spec 8.1: 目标架构为小端 native float32，拒绝大端 '>f4'、float64 '<f8' 或 object/pickle
    if descr != "<f4" && descr != "=f4" && descr != "f4" {
        return Err(NpyError::UnsupportedDtype(descr));
    }

    let fortran_order = parse_dict_field(&header_str, "fortran_order")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let shape = parse_shape(&header_str)
        .ok_or_else(|| NpyError::ParseError("缺少或无法解析 shape 字段".to_string()))?;

    if shape.len() != 2 {
        return Err(NpyError::UnsupportedDimension(shape.len()));
    }
    if shape[1] == 0 || shape[1] > 8192 {
        return Err(NpyError::ParseError("向量维度必须为 1..8192".into()));
    }

    let data_offset = reader.stream_position()?;
    let expected = shape[0].checked_mul(shape[1]).and_then(|n| n.checked_mul(4))
        .and_then(|n| data_offset.checked_add(n as u64))
        .ok_or_else(|| NpyError::ParseError("矩阵大小溢出".into()))?;
    if reader.seek(SeekFrom::End(0))? != expected {
        return Err(NpyError::ParseError("向量文件长度与 shape 不一致".into()));
    }
    reader.seek(SeekFrom::Start(data_offset))?;

    Ok(NpyHeader {
        major,
        minor,
        descr,
        fortran_order,
        shape,
        data_offset,
    })
}

/// Both array layouts are validated in bounded row tiles, without loading the matrix.
pub fn validate_npy_file(path: &Path) -> Result<NpyHeader, NpyError> {
    let mut file = File::open(path)?;
    let header = parse_npy_header(&mut file)?;
    let (rows, dim) = (header.shape[0], header.shape[1]);
    let tile_rows = (MAX_WORK_BUFFER_BYTES / (dim * 4 + 8)).max(1);
    for start in (0..rows).step_by(tile_rows) {
        let count = tile_rows.min(rows - start);
        let mut norms = vec![0.0f64; count];
        let mut bytes = vec![0u8; if header.fortran_order { count * 4 } else { count * dim * 4 }];
        if header.fortran_order {
            for col in 0..dim {
                file.seek(SeekFrom::Start(header.data_offset + ((col * rows + start) * 4) as u64))?;
                file.read_exact(&mut bytes)?;
                for (row, cell) in bytes.chunks_exact(4).enumerate() {
                    let v = f32::from_le_bytes(cell.try_into().unwrap());
                    if !v.is_finite() { return Err(NpyError::NonFiniteValues); }
                    norms[row] += (v as f64).powi(2);
                }
            }
        } else {
            file.seek(SeekFrom::Start(header.data_offset + (start * dim * 4) as u64))?;
            file.read_exact(&mut bytes)?;
            for (index, cell) in bytes.chunks_exact(4).enumerate() {
                let v = f32::from_le_bytes(cell.try_into().unwrap());
                if !v.is_finite() { return Err(NpyError::NonFiniteValues); }
                norms[index / dim] += (v as f64).powi(2);
            }
        }
        for (row, norm) in norms.into_iter().enumerate() {
            if (norm.sqrt() - 1.0).abs() > 0.00101 {
                return Err(NpyError::ZeroNormRow(start + row));
            }
        }
    }
    Ok(header)
}

fn parse_dict_field(header: &str, field: &str) -> Option<String> {
    let pattern = format!("'{}':", field);
    let start_idx = header.find(&pattern)?;
    let rest = &header[start_idx + pattern.len()..].trim_start();
    if rest.starts_with('\'') || rest.starts_with('"') {
        let quote = rest.chars().next()?;
        let end_idx = rest[1..].find(quote)?;
        Some(rest[1..1 + end_idx].to_string())
    } else {
        let end_idx = rest.find([',', '}', ')'])?;
        Some(rest[..end_idx].trim().to_string())
    }
}

fn parse_shape(header: &str) -> Option<Vec<usize>> {
    let start_idx = header.find("'shape':")?;
    let rest = &header[start_idx + "'shape':".len()..].trim_start();
    let open_paren = rest.find('(')?;
    let close_paren = rest.find(')')?;
    let inner = &rest[open_paren + 1..close_paren].trim();
    let parts = inner.split(',');
    let mut dims = Vec::new();
    for p in parts {
        let trimmed = p.trim();
        if !trimmed.is_empty() {
            dims.push(trimmed.parse::<usize>().ok()?);
        }
    }
    Some(dims)
}

/// 在指定候选行范围内进行流式点积计算，返回 (score, row_index)
/// query 向量必须有限且已归一化；工作缓冲最多不超过 4 MiB
pub fn scan_vector_rows<R: Read + Seek>(
    reader: &mut R,
    header: &NpyHeader,
    query_vec: &[f32],
    candidate_rows: &[usize],
) -> Result<Vec<(f32, usize)>, NpyError> {
    let rows = header.shape[0];
    let dim = header.shape[1];

    if dim != query_vec.len() {
        return Err(NpyError::ParseError(format!(
            "查询向量维度 ({}) 与矩阵维度 ({}) 不匹配",
            query_vec.len(),
            dim
        )));
    }

    if candidate_rows.is_empty() || rows == 0 {
        return Ok(Vec::new());
    }

    let mut results = Vec::with_capacity(candidate_rows.len());

    if !header.fortran_order {
        // C-order: 行连续存储。每行占 dim * 4 字节。
        // 计算块行数：max(1, floor(4*1024*1024 / (dim * 4)))
        let row_bytes = dim * 4;
        let block_rows = (MAX_WORK_BUFFER_BYTES / row_bytes).max(1);

        let mut sorted_candidates = candidate_rows.to_vec();
        sorted_candidates.sort_unstable();
        sorted_candidates.dedup();

        let mut current_idx = 0;
        while current_idx < sorted_candidates.len() {
            let chunk_start_row = sorted_candidates[current_idx];
            let chunk_end_row = (chunk_start_row + block_rows).min(rows);

            // 收集位于 [chunk_start_row, chunk_end_row) 范围内的候选行
            let mut batch_rows = Vec::new();
            while current_idx < sorted_candidates.len()
                && sorted_candidates[current_idx] < chunk_end_row
            {
                batch_rows.push(sorted_candidates[current_idx]);
                current_idx += 1;
            }

            if batch_rows.is_empty() {
                break;
            }

            // 读取连续物理块
            let seek_offset = header.data_offset + (chunk_start_row * row_bytes) as u64;
            reader.seek(SeekFrom::Start(seek_offset))?;

            let rows_to_read = chunk_end_row - chunk_start_row;
            let bytes_to_read = rows_to_read * row_bytes;
            let mut byte_buf = vec![0u8; bytes_to_read];
            reader.read_exact(&mut byte_buf)?;

            for &r in &batch_rows {
                let offset_in_block = (r - chunk_start_row) * dim;
                let mut dot_product = 0.0f32;
                for (d, &q) in query_vec.iter().enumerate() {
                    let byte_idx = (offset_in_block + d) * 4;
                    let val = f32::from_le_bytes([
                        byte_buf[byte_idx],
                        byte_buf[byte_idx + 1],
                        byte_buf[byte_idx + 2],
                        byte_buf[byte_idx + 3],
                    ]);
                    if !val.is_finite() {
                        return Err(NpyError::NonFiniteValues);
                    }
                    dot_product += val * q;
                }
                results.push((dot_product, r));
            }
        }
    } else {
        // Fortran-order: 列连续存储。全矩阵由 dim 个长为 rows 的列组成。
        // 第 d 列的第 r 行偏移 = header.data_offset + (d * rows + r) * 4
        // 为保证有界工作内存，按列顺序累加点积：
        let mut accumulators = vec![0.0f32; candidate_rows.len()];

        for (d, &q) in query_vec.iter().enumerate() {
            if q == 0.0 {
                continue;
            }
            let col_byte_offset = header.data_offset + (d * rows * 4) as u64;
            for (idx, &r) in candidate_rows.iter().enumerate() {
                if r >= rows {
                    continue;
                }
                let cell_offset = col_byte_offset + (r * 4) as u64;
                reader.seek(SeekFrom::Start(cell_offset))?;
                let mut cell_bytes = [0u8; 4];
                reader.read_exact(&mut cell_bytes)?;
                let val = f32::from_le_bytes(cell_bytes);
                if !val.is_finite() {
                    return Err(NpyError::NonFiniteValues);
                }
                accumulators[idx] += val * q;
            }
        }

        for (idx, &r) in candidate_rows.iter().enumerate() {
            results.push((accumulators[idx], r));
        }
    }

    Ok(results)
}

/// 跨包向量索引管理器，有界缓存最多 8 个已打开的文件句柄
pub struct VectorIndex {
    vectors_dir: PathBuf,
    // package_id -> (File, NpyHeader)
    handles: std::sync::Mutex<HashMap<i64, (File, NpyHeader)>>,
}

impl VectorIndex {
    pub fn new(vectors_dir: impl AsRef<Path>) -> Self {
        Self {
            vectors_dir: vectors_dir.as_ref().to_path_buf(),
            handles: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn close_all(&self) {
        let mut guard = self.handles.lock().unwrap();
        guard.clear();
    }

    pub fn close_package(&self, package_id: i64) {
        let mut guard = self.handles.lock().unwrap();
        guard.remove(&package_id);
    }

    /// 在 candidates（package_id -> row_indices）中执行点积并收集 Top-K
    pub fn search(
        &self,
        query_vec: &[f32],
        candidates: &HashMap<i64, Vec<usize>>,
        top_k: usize,
    ) -> Result<Vec<(f32, i64, usize)>, NpyError> {
        let mut all_scored = Vec::new();

        for (&pkg_id, rows) in candidates {
            let npy_path = self.vectors_dir.join(format!("pkg-{}.npy", pkg_id));
            if !npy_path.exists() {
                continue;
            }

            let mut guard = self.handles.lock().unwrap();
            // 句柄缓存淘汰：最多 8 个
            let victim = if !guard.contains_key(&pkg_id) && guard.len() >= 8 {
                guard.keys().next().copied()
            } else {
                None
            };
            if let Some(v) = victim {
                guard.remove(&v);
            }

            let entry = if let Some(e) = guard.get_mut(&pkg_id) {
                e
            } else {
                let mut file = File::open(&npy_path)?;
                let header = parse_npy_header(&mut file)?;
                guard.insert(pkg_id, (file, header));
                guard.get_mut(&pkg_id).unwrap()
            };

            let scored = scan_vector_rows(&mut entry.0, &entry.1, query_vec, rows)?;
            for (score, row) in scored {
                all_scored.push((score, pkg_id, row));
            }
        }

        // 有界排序取 Top-K（降序，score 高优先）
        all_scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        all_scored.truncate(top_k);

        Ok(all_scored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;

    #[test]
    fn test_parse_golden_npy_c_and_fortran() {
        let c_path = Path::new("tests/fixtures/retrieval/vectors_dim8_c.npy");
        let mut file_c = File::open(c_path).expect("读取 C-order NPY 失败");
        let header_c = parse_npy_header(&mut file_c).expect("解析 C-order 头失败");
        assert_eq!(header_c.shape, vec![16, 8]);
        assert!(!header_c.fortran_order);

        let f_path = Path::new("tests/fixtures/retrieval/vectors_dim8_fortran.npy");
        let mut file_f = File::open(f_path).expect("读取 Fortran NPY 失败");
        let header_f = parse_npy_header(&mut file_f).expect("解析 Fortran 头失败");
        assert_eq!(header_f.shape, vec![16, 8]);
        assert!(header_f.fortran_order);

        // 加载期望的分值 fixture
        let fixture_path = Path::new("tests/fixtures/retrieval/vector_queries.json");
        let content = fs::read_to_string(fixture_path).expect("读取 query fixture 失败");
        let fixture: Value = serde_json::from_str(&content).unwrap();

        let query: Vec<f32> = fixture["query"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_f64().unwrap() as f32)
            .collect();

        let expected_scores: Vec<f32> = fixture["expected_scores_f32"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_f64().unwrap() as f32)
            .collect();

        let all_rows: Vec<usize> = (0..16).collect();

        let scores_c = scan_vector_rows(&mut file_c, &header_c, &query, &all_rows).unwrap();
        let scores_f = scan_vector_rows(&mut file_f, &header_f, &query, &all_rows).unwrap();

        assert_eq!(scores_c.len(), 16);
        assert_eq!(scores_f.len(), 16);

        for i in 0..16 {
            let (score_c, r_c) = scores_c[i];
            let (score_f, r_f) = scores_f[i];
            assert_eq!(r_c, i);
            assert_eq!(r_f, i);

            let diff_cf = (score_c - score_f).abs();
            assert!(
                diff_cf < 1e-5,
                "C 与 Fortran 布局计算得分必须一致: {}",
                diff_cf
            );

            let diff_expected = (score_c - expected_scores[i]).abs();
            assert!(
                diff_expected < 1e-5,
                "计算得分与预期 golden 偏差超限: row={}, c={}, exp={}",
                i,
                score_c,
                expected_scores[i]
            );
        }
    }

    #[test]
    fn test_reject_bad_npy_samples() {
        let bad_samples = [
            ("bad_big_endian.npy", "大端应被拒绝"),
            ("bad_dtype_float64.npy", "float64 应被拒绝"),
            ("bad_ndim_1.npy", "1维数组应被拒绝"),
            ("bad_has_nan.npy", "包含 NaN 应被拒绝"),
        ];

        for (file_name, desc) in bad_samples {
            let path = Path::new("tests/fixtures/retrieval").join(file_name);
            if !path.exists() {
                continue;
            }
            let mut file = File::open(&path).unwrap();
            let header_res = parse_npy_header(&mut file);
            if let Ok(header) = header_res {
                // 若头能解析（如 has_nan），在 scan_vector_rows 时必须被拦截拒绝
                let dummy_query = vec![0.1f32; header.shape[1]];
                let scan_res = scan_vector_rows(&mut file, &header, &dummy_query, &[0, 1, 2, 3, 4]);
                assert!(scan_res.is_err(), "{}: scan 应该返回 Err", desc);
            } else {
                assert!(header_res.is_err(), "{}: header 应该返回 Err", desc);
            }
        }
    }
}
