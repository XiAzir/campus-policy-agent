use std::path::Path;

#[allow(dead_code)]
pub fn fixture_copy() -> tempfile::TempDir {
    fn copy(source: &Path, target: &Path) {
        std::fs::create_dir_all(target).unwrap();
        for entry in std::fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let dest = target.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &dest);
            } else {
                std::fs::copy(entry.path(), dest).unwrap();
            }
        }
    }
    let temp = tempfile::tempdir().unwrap();
    copy(Path::new("tests/fixtures/legacy_data"), temp.path());
    // Rebuild the ignored ZIP from tracked synthetic data, never from private fixtures.
    let package = small_package_bytes();
    use sha2::{Digest, Sha256};
    let hash = hex::encode(Sha256::digest(&package));
    std::fs::create_dir_all(temp.path().join("packages")).unwrap();
    std::fs::write(
        temp.path().join("packages").join(format!("{hash}.zip")),
        package,
    )
    .unwrap();
    let conn = rusqlite::Connection::open(temp.path().join("campus.db")).unwrap();
    conn.execute("UPDATE packages SET sha256=? WHERE id=1", [&hash])
        .unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(conn);
    temp
}

/// Deterministic, self-contained ZIP; cargo test needs neither Python nor untracked files.
#[allow(dead_code)]
pub fn small_package_bytes() -> Vec<u8> {
    use std::io::{Cursor, Write};
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(base.join("packages/manifest_snapshot.json")).unwrap(),
    )
    .unwrap();
    // The old synthetic four-line PDF claimed marks on lines 30 and 60.
    for document in manifest["documents"].as_array_mut().unwrap() {
        if let Some(marks) = document["page_map"].as_array_mut() {
            marks.truncate(1);
        }
    }
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for document in manifest["documents"].as_array().unwrap() {
        let hash = document["doc_hash"].as_str().unwrap();
        let ext = document["doc_type"].as_str().unwrap();
        for name in [format!("files/{hash}.{ext}"), format!("text/{hash}.txt")] {
            archive.start_file(&name, options).unwrap();
            archive
                .write_all(&std::fs::read(base.join("legacy_data").join(name)).unwrap())
                .unwrap();
        }
    }
    archive.start_file("vectors.npy", options).unwrap();
    archive
        .write_all(&std::fs::read(base.join("legacy_data/vectors/pkg-1.npy")).unwrap())
        .unwrap();
    archive.start_file("manifest.json", options).unwrap();
    archive
        .write_all(&serde_json::to_vec(&manifest).unwrap())
        .unwrap();
    archive.finish().unwrap().into_inner()
}

#[allow(dead_code)]
pub fn small_package() -> tempfile::NamedTempFile {
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&small_package_bytes()).unwrap();
    file
}
