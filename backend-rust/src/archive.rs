//! Reject excessive central directories *before* zip::ZipArchive allocates entries.
use std::io::{self, Read, Seek, SeekFrom};
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn u16_at(data: &[u8], offset: usize) -> u64 {
    u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as u64
}
fn u32_at(data: &[u8], offset: usize) -> u64 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as u64
}
fn u64_at(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

pub fn preflight<R: Read + Seek>(
    reader: &mut R,
    max_entries: u64,
    max_directory: u64,
) -> io::Result<()> {
    let length = reader.seek(SeekFrom::End(0))?;
    let tail_len = length.min(65_557) as usize;
    if tail_len < 22 {
        return Err(invalid("ZIP 缺少结束记录"));
    }
    reader.seek(SeekFrom::End(-(tail_len as i64)))?;
    let mut tail = vec![0; tail_len];
    reader.read_exact(&mut tail)?;
    let end = (0..=tail_len - 22)
        .rev()
        .find(|&index| {
            tail[index..index + 4] == *b"PK\x05\x06"
                && index + 22 + u16_at(&tail, index + 20) as usize == tail_len
        })
        .ok_or_else(|| invalid("ZIP 结束记录无效或包含尾随数据"))?;
    let eocd = &tail[end..end + 22];
    if u16_at(eocd, 4) != 0 || u16_at(eocd, 6) != 0 || u16_at(eocd, 8) != u16_at(eocd, 10) {
        return Err(invalid("不支持分卷 ZIP"));
    }
    let end_position = length - tail_len as u64 + end as u64;
    let mut entries = u16_at(eocd, 10);
    let mut directory_size = u32_at(eocd, 12);
    let mut directory_offset = u32_at(eocd, 16);
    let mut directory_end = end_position;
    let mut has_zip64 = false;
    if end_position >= 20 {
        reader.seek(SeekFrom::Start(end_position - 20))?;
        let mut locator = [0u8; 20];
        reader.read_exact(&mut locator)?;
        if locator[..4] == *b"PK\x06\x07" {
            has_zip64 = true;
            if u32_at(&locator, 4) != 0 || u32_at(&locator, 16) != 1 {
                return Err(invalid("不支持分卷 ZIP64"));
            }
            let position = u64_at(&locator, 8);
            if position
                .checked_add(56)
                .is_none_or(|end| end > end_position - 20)
            {
                return Err(invalid("ZIP64 记录越界"));
            }
            reader.seek(SeekFrom::Start(position))?;
            let mut record = [0u8; 56];
            reader.read_exact(&mut record)?;
            if record[..4] != *b"PK\x06\x06"
                || !(44..=44 + 65_536).contains(&u64_at(&record, 4))
                || position
                    .checked_add(12)
                    .and_then(|v| v.checked_add(u64_at(&record, 4)))
                    != Some(end_position - 20)
                || u32_at(&record, 16) != 0
                || u32_at(&record, 20) != 0
                || u64_at(&record, 24) != u64_at(&record, 32)
            {
                return Err(invalid("ZIP64 结束记录无效"));
            }
            entries = u64_at(&record, 32);
            directory_size = u64_at(&record, 40);
            directory_offset = u64_at(&record, 48);
            directory_end = position;
        }
    }
    if !has_zip64
        && (entries == 65_535
            || directory_size == u32::MAX as u64
            || directory_offset == u32::MAX as u64)
    {
        return Err(invalid("ZIP64 定位记录缺失"));
    }
    if entries > max_entries || directory_size > max_directory {
        return Err(invalid("ZIP 条目数或中央目录大小超过上限"));
    }
    if directory_offset.checked_add(directory_size) != Some(directory_end) {
        return Err(invalid("ZIP 中央目录越界或与结束记录不连续"));
    }
    // Check physical record lengths, not just the untrusted EOCD size. Otherwise
    // large names/extra fields could make the library read beyond that budget.
    let mut position = directory_offset;
    for _ in 0..entries {
        if position
            .checked_add(46)
            .is_none_or(|end| end > directory_end)
        {
            return Err(invalid("ZIP 中央目录条目数与实际长度不一致"));
        }
        reader.seek(SeekFrom::Start(position))?;
        let mut header = [0u8; 46];
        reader.read_exact(&mut header)?;
        if header[..4] != *b"PK\x01\x02" {
            return Err(invalid("ZIP 中央目录条目无效"));
        }
        let variable = u16_at(&header, 28) + u16_at(&header, 30) + u16_at(&header, 32);
        position = position
            .checked_add(46 + variable)
            .filter(|&end| end <= directory_end)
            .ok_or_else(|| invalid("ZIP 中央目录可变字段越界"))?;
    }
    if position != directory_end {
        return Err(invalid("ZIP 中央目录实际条目数与声明不一致"));
    }
    reader.seek(SeekFrom::Start(0))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    fn archive() -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("manifest.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"{}").unwrap();
        zip.finish().unwrap().into_inner()
    }
    #[test]
    fn validates_before_constructing_archive() {
        let bytes = archive();
        assert!(preflight(&mut Cursor::new(&bytes), 100, 4096).is_ok());
        assert!(preflight(&mut Cursor::new(&bytes), 0, 4096).is_err());
        assert!(preflight(&mut Cursor::new(&bytes), 100, 1).is_err());
        let mut bad = bytes.clone();
        let end = bad.len() - 22;
        bad[end + 8..end + 12].fill(0xff);
        assert!(preflight(&mut Cursor::new(bad), 100, 4096).is_err());
        let mut wrong_count = bytes.clone();
        wrong_count[end + 8..end + 12].fill(0);
        assert!(preflight(&mut Cursor::new(wrong_count), 100, 4096).is_err());
        let mut oversized_name = bytes.clone();
        let cd = u32_at(&oversized_name, end + 16) as usize;
        oversized_name[cd + 28..cd + 30].fill(255);
        assert!(preflight(&mut Cursor::new(oversized_name), 100, 4096).is_err());
        let mut trailing = bytes;
        trailing.push(0);
        assert!(preflight(&mut Cursor::new(trailing), 100, 4096).is_err());
    }
    #[test]
    fn zip64_preflight_accepts_valid_layout_and_rejects_overflow() {
        let mut bytes = archive();
        let eocd_at = bytes.len() - 22;
        let mut eocd = bytes.split_off(eocd_at);
        let count = u16_at(&eocd, 10);
        let size = u32_at(&eocd, 12);
        let offset = u32_at(&eocd, 16);
        bytes.extend(b"PK\x06\x06");
        bytes.extend(44u64.to_le_bytes());
        bytes.extend(45u16.to_le_bytes());
        bytes.extend(45u16.to_le_bytes());
        bytes.extend([0; 8]);
        bytes.extend(count.to_le_bytes());
        bytes.extend(count.to_le_bytes());
        bytes.extend(size.to_le_bytes());
        bytes.extend(offset.to_le_bytes());
        bytes.extend(b"PK\x06\x07");
        bytes.extend(0u32.to_le_bytes());
        bytes.extend((eocd_at as u64).to_le_bytes());
        bytes.extend(1u32.to_le_bytes());
        eocd[8..20].fill(255);
        bytes.extend(eocd);
        assert!(preflight(&mut Cursor::new(&bytes), 100, 4096).is_ok());
        assert!(zip::ZipArchive::new(Cursor::new(&bytes)).is_ok());
        bytes[eocd_at + 24..eocd_at + 40].fill(255);
        assert!(preflight(&mut Cursor::new(&bytes), 100, 4096).is_err());
    }
}
