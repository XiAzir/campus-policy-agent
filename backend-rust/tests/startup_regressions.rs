use std::process::Command;

#[test]
fn incomplete_restore_never_creates_database() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    std::fs::write(temp.path().join("data.restore-in-progress"), "incomplete").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_campus_policy_backend"))
        .env("DATA_DIR", &data)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!data.join("campus.db").exists());
}

#[test]
fn initialization_never_interpolates_password_into_audit() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_campus_policy_backend"))
        .env("DATA_DIR", &data)
        .env("HOST", "127.0.0.1")
        .env("PORT", occupied.local_addr().unwrap().port().to_string())
        .env("INITIAL_ADMIN_PASSWORD", "test-private-password-unique")
        .env("GEMINI_BASE_URL", "http://127.0.0.1:1")
        .env("GEMINI_MODEL", "test")
        .env("GEMINI_API_KEY", "fake")
        .env("SILICONFLOW_API_KEY", "fake")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let db = rusqlite::Connection::open(data.join("campus.db")).unwrap();
    let detail: String = db
        .query_row(
            "SELECT detail FROM audit_log WHERE action='init_admin_password'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(detail, "初始管理员密码已设置");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("test-private-password-unique"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("test-private-password-unique"));
}
