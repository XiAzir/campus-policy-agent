use std::process::Command;

#[test]
fn incomplete_restore_never_creates_database() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    std::fs::write(temp.path().join("data.restore-in-progress"), "incomplete").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_campus_policy_backend"))
        .env("DATA_DIR", &data).output().unwrap();
    assert!(!output.status.success());
    assert!(!data.join("campus.db").exists());
}

#[test]
fn initialization_never_interpolates_password_into_audit() {
    let source = include_str!("../src/main.rs");
    assert!(!source.contains("format!(\"初始管理员密码"));
    assert!(source.contains("初始管理员密码已设置"));
}
