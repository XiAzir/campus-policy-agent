use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const MAX_HEADER_BYTES: usize = 10_000;
/// Shared ceiling for scan/validation scratch buffers (not the matrix size).
pub const MAX_WORK_BUFFER_BYTES: usize = 256 * 1024;
pub const MAX_TOP_K: usize = 256;
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
            Self::Io(e) => write!(f, "IO 错误: {e}"),
            Self::InvalidMagic => write!(f, "无效的 NPY 魔数"),
            Self::HeaderTooLarge => write!(f, "NPY 文件头超过 10,000 字节限制"),
            Self::ParseError(msg) => write!(f, "NPY 解析错误: {msg}"),
            Self::UnsupportedDtype(d) => write!(f, "不支持的 dtype: {d}（仅支持小端 float32）"),
            Self::UnsupportedDimension(d) => write!(f, "不支持的维度: {d}（仅支持 2 维矩阵）"),
            Self::NonFiniteValues => write!(f, "向量数据包含 NaN 或 Inf"),
            Self::ZeroNormRow(r) => write!(f, "第 {r} 行向量模长为零或非归一化"),
        }
    }
}
impl std::error::Error for NpyError {}
impl From<std::io::Error> for NpyError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
fn invalid(message: &str) -> NpyError {
    NpyError::ParseError(message.into())
}

// A small literal parser, not substring matching or Python evaluation. Duplicate keys,
// malformed booleans, mismatched parentheses and integer overflow are all rejected.
struct HeaderParser<'a> {
    text: &'a str,
    pos: usize,
}
impl<'a> HeaderParser<'a> {
    fn ws(&mut self) {
        while self
            .text
            .as_bytes()
            .get(self.pos)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.pos += 1;
        }
    }
    fn eat(&mut self, byte: u8) -> bool {
        self.ws();
        if self.text.as_bytes().get(self.pos) == Some(&byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, byte: u8) -> Result<(), NpyError> {
        if self.eat(byte) {
            Ok(())
        } else {
            Err(invalid("NPY 字典语法错误"))
        }
    }
    fn quoted(&mut self) -> Result<&'a str, NpyError> {
        self.ws();
        let quote = *self
            .text
            .as_bytes()
            .get(self.pos)
            .ok_or_else(|| invalid("字符串缺失"))?;
        if quote != b'\'' && quote != b'"' {
            return Err(invalid("字段必须加引号"));
        }
        self.pos += 1;
        let start = self.pos;
        while let Some(&byte) = self.text.as_bytes().get(self.pos) {
            if byte == quote {
                let value = &self.text[start..self.pos];
                self.pos += 1;
                return Ok(value);
            }
            if byte == b'\\' || !byte.is_ascii() {
                return Err(invalid("不支持的字段转义或字符"));
            }
            self.pos += 1;
        }
        Err(invalid("字符串未闭合"))
    }
    fn shape(&mut self) -> Result<Vec<usize>, NpyError> {
        self.expect(b'(')?;
        let mut shape = Vec::with_capacity(2);
        while !self.eat(b')') {
            self.ws();
            let start = self.pos;
            while self
                .text
                .as_bytes()
                .get(self.pos)
                .is_some_and(u8::is_ascii_digit)
            {
                self.pos += 1;
            }
            let value = self.text[start..self.pos]
                .parse::<usize>()
                .map_err(|_| invalid("shape 必须为非负整数且不得溢出"))?;
            shape.push(value);
            if shape.len() > 2 {
                return Err(NpyError::UnsupportedDimension(shape.len()));
            }
            if self.eat(b')') {
                break;
            }
            self.expect(b',')?;
        }
        if shape.len() != 2 {
            return Err(NpyError::UnsupportedDimension(shape.len()));
        }
        Ok(shape)
    }
}

pub fn parse_npy_header<R: Read + Seek>(reader: &mut R) -> Result<NpyHeader, NpyError> {
    reader.seek(SeekFrom::Start(0))?;
    let mut magic = [0u8; 6];
    reader.read_exact(&mut magic)?;
    if &magic != NPY_MAGIC {
        return Err(NpyError::InvalidMagic);
    }
    let mut version = [0u8; 2];
    reader.read_exact(&mut version)?;
    let [major, minor] = version;
    if minor != 0 || !matches!(major, 1 | 2) {
        return Err(invalid("仅支持 NPY 1.0 / 2.0"));
    }
    let header_len = if major == 1 {
        let mut bytes = [0u8; 2];
        reader.read_exact(&mut bytes)?;
        u16::from_le_bytes(bytes) as usize
    } else {
        let mut bytes = [0u8; 4];
        reader.read_exact(&mut bytes)?;
        u32::from_le_bytes(bytes) as usize
    };
    if header_len > MAX_HEADER_BYTES {
        return Err(NpyError::HeaderTooLarge);
    }
    let mut bytes = vec![0u8; header_len];
    reader.read_exact(&mut bytes)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid("NPY 头不是合法 UTF-8"))?;
    let mut parser = HeaderParser { text, pos: 0 };
    let (mut descr, mut order, mut shape) = (None, None, None);
    parser.expect(b'{')?;
    while !parser.eat(b'}') {
        let key = parser.quoted()?;
        parser.expect(b':')?;
        match key {
            "descr" if descr.is_none() => descr = Some(parser.quoted()?.to_owned()),
            "fortran_order" if order.is_none() => {
                parser.ws();
                let rest = &text[parser.pos..];
                if rest.starts_with("True") {
                    order = Some(true);
                    parser.pos += 4;
                } else if rest.starts_with("False") {
                    order = Some(false);
                    parser.pos += 5;
                } else {
                    return Err(invalid("fortran_order 必须为 True 或 False"));
                }
            }
            "shape" if shape.is_none() => shape = Some(parser.shape()?),
            _ => return Err(invalid("未知或重复的 NPY 字段")),
        }
        if parser.eat(b'}') {
            break;
        }
        parser.expect(b',')?;
    }
    parser.ws();
    if parser.pos != text.len() {
        return Err(invalid("NPY 字典后存在多余内容"));
    }
    let descr = descr.ok_or_else(|| invalid("缺少 descr"))?;
    if descr != "<f4" && !(cfg!(target_endian = "little") && (descr == "=f4" || descr == "f4")) {
        return Err(NpyError::UnsupportedDtype(descr));
    }
    let header = NpyHeader {
        major,
        minor,
        descr,
        fortran_order: order.ok_or_else(|| invalid("缺少 fortran_order"))?,
        shape: shape.ok_or_else(|| invalid("缺少 shape"))?,
        data_offset: reader.stream_position()?,
    };
    let expected = checked_layout(&header)?;
    if reader.seek(SeekFrom::End(0))? != expected {
        return Err(invalid("向量文件长度与 shape 不一致"));
    }
    reader.seek(SeekFrom::Start(header.data_offset))?;
    Ok(header)
}

fn checked_layout(header: &NpyHeader) -> Result<u64, NpyError> {
    if header.shape.len() != 2 {
        return Err(NpyError::UnsupportedDimension(header.shape.len()));
    }
    let (rows, dim) = (header.shape[0], header.shape[1]);
    if !(1..=8192).contains(&dim) {
        return Err(invalid("向量维度必须为 1..8192"));
    }
    rows.checked_mul(dim)
        .and_then(|n| n.checked_mul(4))
        .and_then(|n| header.data_offset.checked_add(n as u64))
        .ok_or_else(|| invalid("矩阵大小溢出"))
}

/// Validate either array layout in bounded row tiles, without loading the matrix.
pub fn validate_npy_file(path: &Path) -> Result<NpyHeader, NpyError> {
    let mut file = File::open(path)?;
    let header = parse_npy_header(&mut file)?;
    let (rows, dim) = (header.shape[0], header.shape[1]);
    let tile_rows = (MAX_WORK_BUFFER_BYTES / (dim * 4 + 8)).max(1);
    let mut norms = Vec::new();
    let mut bytes = Vec::new();
    for start in (0..rows).step_by(tile_rows) {
        let count = tile_rows.min(rows - start);
        norms.clear();
        norms.resize(count, 0.0f64);
        bytes.resize(
            if header.fortran_order {
                count * 4
            } else {
                count * dim * 4
            },
            0,
        );
        if header.fortran_order {
            for col in 0..dim {
                file.seek(SeekFrom::Start(
                    header.data_offset + ((col * rows + start) * 4) as u64,
                ))?;
                file.read_exact(&mut bytes)?;
                for (row, cell) in bytes.as_chunks::<4>().0.iter().enumerate() {
                    let value = f32::from_le_bytes(*cell);
                    if !value.is_finite() {
                        return Err(NpyError::NonFiniteValues);
                    }
                    norms[row] += (value as f64).powi(2);
                }
            }
        } else {
            file.seek(SeekFrom::Start(
                header.data_offset + (start * dim * 4) as u64,
            ))?;
            file.read_exact(&mut bytes)?;
            for (index, cell) in bytes.as_chunks::<4>().0.iter().enumerate() {
                let value = f32::from_le_bytes(*cell);
                if !value.is_finite() {
                    return Err(NpyError::NonFiniteValues);
                }
                norms[index / dim] += (value as f64).powi(2);
            }
        }
        for (row, norm) in norms.iter().enumerate() {
            if (norm.sqrt() - 1.0).abs() > 0.00101 {
                return Err(NpyError::ZeroNormRow(start + row));
            }
        }
    }
    Ok(header)
}

/// Compatibility API: returns one score per unique valid candidate. Production retrieval
/// uses the visitor below so it never allocates a score vector proportional to the corpus.
pub fn scan_vector_rows<R: Read + Seek>(
    reader: &mut R,
    header: &NpyHeader,
    query: &[f32],
    candidates: &[usize],
) -> Result<Vec<(f32, usize)>, NpyError> {
    let mut results = Vec::new();
    scan_vector_rows_into(reader, header, query, candidates, |score, row| {
        results.push((score, row))
    })?;
    Ok(results)
}

fn scan_vector_rows_into<R: Read + Seek>(
    reader: &mut R,
    header: &NpyHeader,
    query: &[f32],
    candidates: &[usize],
    mut visit: impl FnMut(f32, usize),
) -> Result<(), NpyError> {
    checked_layout(header)?;
    let (rows, dim) = (header.shape[0], header.shape[1]);
    if query.len() != dim {
        return Err(invalid("查询向量维度与矩阵不匹配"));
    }
    if query.iter().any(|v| !v.is_finite()) {
        return Err(NpyError::NonFiniteValues);
    }
    if candidates.iter().any(|&r| r >= rows) {
        return Err(invalid("候选行越界"));
    }
    let sorted: Cow<'_, [usize]> = if candidates.windows(2).all(|w| w[0] < w[1]) {
        Cow::Borrowed(candidates)
    } else {
        let mut sorted = candidates.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        Cow::Owned(sorted)
    };
    let row_bytes = dim * 4;
    let tile_rows = if header.fortran_order {
        MAX_WORK_BUFFER_BYTES / 8
    } else {
        MAX_WORK_BUFFER_BYTES / row_bytes
    }
    .max(1);
    let mut bytes = Vec::new();
    let mut accumulators = Vec::new();
    let mut cursor = 0;
    while cursor < sorted.len() {
        let start = sorted[cursor];
        let end = start.saturating_add(tile_rows).min(rows);
        let mut next = cursor + 1;
        while next < sorted.len() && sorted[next] < end {
            next += 1;
        }
        let batch = &sorted[cursor..next];
        let span = batch[batch.len() - 1] - start + 1;
        if header.fortran_order {
            bytes.resize(span * 4, 0);
            accumulators.clear();
            accumulators.resize(batch.len(), 0.0f32);
            for (col, &q) in query.iter().enumerate() {
                reader.seek(SeekFrom::Start(
                    header.data_offset + ((col * rows + start) * 4) as u64,
                ))?;
                reader.read_exact(&mut bytes)?;
                for (idx, &row) in batch.iter().enumerate() {
                    let offset = (row - start) * 4;
                    let value = f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
                    if !value.is_finite() {
                        return Err(NpyError::NonFiniteValues);
                    }
                    accumulators[idx] += value * q;
                }
            }
            for (&row, &score) in batch.iter().zip(&accumulators) {
                if !score.is_finite() {
                    return Err(NpyError::NonFiniteValues);
                }
                visit(score, row);
            }
        } else {
            bytes.resize(span * row_bytes, 0);
            reader.seek(SeekFrom::Start(
                header.data_offset + (start * row_bytes) as u64,
            ))?;
            reader.read_exact(&mut bytes)?;
            for &row in batch {
                let offset = (row - start) * row_bytes;
                let mut score = 0.0f32;
                for (cell, &q) in bytes[offset..offset + row_bytes]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(query)
                {
                    let value = f32::from_le_bytes(*cell);
                    if !value.is_finite() {
                        return Err(NpyError::NonFiniteValues);
                    }
                    score += value * q;
                }
                if !score.is_finite() {
                    return Err(NpyError::NonFiniteValues);
                }
                visit(score, row);
            }
        }
        cursor = next;
    }
    Ok(())
}

#[derive(Debug)]
struct ScoredRow {
    score: f32,
    package: i64,
    row: usize,
    chunk: i64,
}
impl PartialEq for ScoredRow {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for ScoredRow {}
impl PartialOrd for ScoredRow {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
// The worst retained candidate is at the top of this max heap.
impl Ord for ScoredRow {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| self.package.cmp(&other.package))
            .then_with(|| self.row.cmp(&other.row))
            .then_with(|| self.chunk.cmp(&other.chunk))
    }
}
pub(crate) struct VectorTopK {
    heap: BinaryHeap<ScoredRow>,
    limit: usize,
}
impl VectorTopK {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            heap: BinaryHeap::new(),
            limit: limit.min(MAX_TOP_K),
        }
    }
    pub(crate) fn push(&mut self, score: f32, package: i64, row: usize, chunk: i64) {
        if self.limit == 0 {
            return;
        }
        let item = ScoredRow {
            score,
            package,
            row,
            chunk,
        };
        if self.heap.len() < self.limit {
            self.heap.push(item);
        } else if self.heap.peek().is_some_and(|worst| item < *worst) {
            *self.heap.peek_mut().unwrap() = item;
        }
    }
    pub(crate) fn finish(self) -> Vec<(f32, i64, usize, i64)> {
        let mut hits = self.heap.into_vec();
        hits.sort();
        hits.into_iter()
            .map(|v| (v.score, v.package, v.row, v.chunk))
            .collect()
    }
}

/// Cloning shares a bounded cache, rather than copying matrices or file cursors.
#[derive(Clone)]
pub struct VectorIndex {
    vectors_dir: PathBuf,
    handles: Arc<Mutex<HashMap<i64, (File, NpyHeader)>>>,
}
impl VectorIndex {
    pub fn new(vectors_dir: impl AsRef<Path>) -> Self {
        Self {
            vectors_dir: vectors_dir.as_ref().to_owned(),
            handles: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    pub fn close_all(&self) {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
    pub fn close_package(&self, package: i64) {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&package);
    }
    pub(crate) fn scan_package(
        &self,
        package: i64,
        query: &[f32],
        rows: &[usize],
        visit: impl FnMut(f32, usize),
    ) -> Result<(), NpyError> {
        let mut handles = self.handles.lock().map_err(|_| invalid("向量句柄锁异常"))?;
        if !handles.contains_key(&package) {
            if handles.len() >= 8
                && let Some(victim) = handles.keys().copied().min()
            {
                handles.remove(&victim);
            }
            let mut file = File::open(self.vectors_dir.join(format!("pkg-{package}.npy")))?;
            let header = parse_npy_header(&mut file)?;
            handles.insert(package, (file, header));
        }
        let (file, header) = handles.get_mut(&package).unwrap();
        scan_vector_rows_into(file, header, query, rows, visit)
    }
    /// Top-K is bounded independently of corpus size; equal scores use package/row order.
    pub fn search(
        &self,
        query: &[f32],
        candidates: &HashMap<i64, Vec<usize>>,
        top_k: usize,
    ) -> Result<Vec<(f32, i64, usize)>, NpyError> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        if top_k > MAX_TOP_K {
            return Err(invalid("top_k 超过上限 256"));
        }
        let mut top = VectorTopK::new(top_k);
        for (&package, rows) in candidates {
            if rows.is_empty() {
                continue;
            }
            self.scan_package(package, query, rows, |score, row| {
                top.push(score, package, row, 0)
            })?;
        }
        Ok(top
            .finish()
            .into_iter()
            .map(|(s, p, r, _)| (s, p, r))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;

    #[test]
    fn validates_values_layout_and_exact_length() {
        for name in ["vectors_dim8_c.npy", "vectors_dim8_fortran.npy"] {
            let source = Path::new("tests/fixtures/retrieval").join(name);
            assert!(validate_npy_file(&source).is_ok());
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("bad.npy");
            let bytes = fs::read(source).unwrap();
            fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
            assert!(validate_npy_file(&path).is_err());
            let mut bad = bytes.clone();
            let header = parse_npy_header(&mut std::io::Cursor::new(&bytes)).unwrap();
            let offset = header.data_offset as usize;
            bad[offset..offset + 4].copy_from_slice(&f32::NAN.to_le_bytes());
            fs::write(&path, &bad).unwrap();
            assert!(validate_npy_file(&path).is_err());
            bad[offset..].fill(0);
            fs::write(&path, &bad).unwrap();
            assert!(validate_npy_file(&path).is_err());
        }
    }

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
    #[test]
    fn malformed_headers_are_errors_not_panics() {
        for header in [
            "{'descr': '<f4', 'shape': )(, 'fortran_order': False}",
            "{'descr': '<f4', 'shape': (1, 1), 'fortran_order': Maybe}",
            "{'descr': '<f4', 'shape': (1, 1)}",
            "{'descr': '<f4', 'descr': '<f4', 'shape': (1, 1), 'fortran_order': False}",
            "{'descr': '<f4', 'shape': (18446744073709551615, 8192), 'fortran_order': False}",
        ] {
            let mut bytes = b"\x93NUMPY\x01\x00".to_vec();
            bytes.extend_from_slice(&(header.len() as u16).to_le_bytes());
            bytes.extend_from_slice(header.as_bytes());
            bytes.extend([0; 4]);
            assert!(parse_npy_header(&mut std::io::Cursor::new(bytes)).is_err());
        }
    }
    #[test]
    fn candidates_and_nonfinite_queries_have_identical_layout_rules() {
        for name in ["vectors_dim8_c.npy", "vectors_dim8_fortran.npy"] {
            let mut file = File::open(Path::new("tests/fixtures/retrieval").join(name)).unwrap();
            let header = parse_npy_header(&mut file).unwrap();
            let query = vec![0.1; 8];
            let hits = scan_vector_rows(&mut file, &header, &query, &[2, 1, 2]).unwrap();
            assert_eq!(hits.iter().map(|v| v.1).collect::<Vec<_>>(), [1, 2]);
            for bad in [16, usize::MAX] {
                assert!(scan_vector_rows(&mut file, &header, &query, &[bad]).is_err());
            }
            assert!(scan_vector_rows(&mut file, &header, &[f32::NAN; 8], &[0]).is_err());
        }
    }
    #[test]
    fn top_k_is_bounded_and_ties_are_deterministic() {
        let mut top = VectorTopK::new(8);
        for row in (0..100_000).rev() {
            top.push(1.0, 1, row, row as i64);
            assert!(top.heap.len() <= 8);
        }
        let hits = top.finish();
        assert_eq!(
            hits.iter().map(|v| v.2).collect::<Vec<_>>(),
            (0..8).collect::<Vec<_>>()
        );
    }
    #[test]
    fn sparse_scan_does_not_read_a_full_tile() {
        struct Counted {
            inner: std::io::Cursor<Vec<u8>>,
            bytes: usize,
        }
        impl Read for Counted {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                let count = self.inner.read(out)?;
                self.bytes += count;
                Ok(count)
            }
        }
        impl Seek for Counted {
            fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
                self.inner.seek(pos)
            }
        }
        let header = NpyHeader {
            major: 1,
            minor: 0,
            descr: "<f4".into(),
            fortran_order: false,
            shape: vec![100_000, 8],
            data_offset: 0,
        };
        let mut reader = Counted {
            inner: std::io::Cursor::new(vec![0; 100_000 * 32]),
            bytes: 0,
        };
        assert_eq!(
            scan_vector_rows(&mut reader, &header, &[0.0; 8], &[50_000])
                .unwrap()
                .len(),
            1
        );
        assert_eq!(reader.bytes, 32);
    }
}
