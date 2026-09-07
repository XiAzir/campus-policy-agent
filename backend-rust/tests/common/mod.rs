use std::path::Path;

pub fn fixture_copy() -> tempfile::TempDir {
    fn copy(source: &Path, target: &Path) {
        std::fs::create_dir_all(target).unwrap();
        for entry in std::fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let dest = target.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() { copy(&entry.path(), &dest); }
            else { std::fs::copy(entry.path(), dest).unwrap(); }
        }
    }
    let temp = tempfile::tempdir().unwrap();
    copy(Path::new("tests/fixtures/legacy_data"), temp.path());
    temp
}
