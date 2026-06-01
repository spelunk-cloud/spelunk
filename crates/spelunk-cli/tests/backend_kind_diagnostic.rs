//! Integration tests for issue #308: `memory_backend` field in
//! `spelunk status --format json` and `spelunk check --format json`.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

fn init_git_repo(dir: &std::path::Path) {
    std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(dir)
        .output()
        .unwrap();
    // Commit something so HEAD exists (required for git-meta backend).
    let f = dir.join("readme.txt");
    fs::write(&f, "hi").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(dir)
        .output()
        .unwrap();
}

fn write_config(dir: &std::path::Path) -> std::path::PathBuf {
    let db_path = dir.join("index.db");
    let config_path = dir.join("config.toml");
    fs::write(
        &config_path,
        format!("db_path = {:?}\n", db_path.display().to_string()),
    )
    .unwrap();
    config_path
}

/// `spelunk status --format json` must include a `memory_backend` field with
/// a known value (issue #308, stable schema field).
#[test]
fn status_json_includes_memory_backend_field() {
    let temp = tempdir().unwrap();
    init_git_repo(temp.path());
    let config_path = write_config(temp.path());

    // Index the repo so status can return JSON without the "no index" bail-out.
    let mut index_cmd = Command::cargo_bin("spelunk").unwrap();
    index_cmd
        .current_dir(temp.path())
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(".")
        .assert()
        .success();

    let output = Command::cargo_bin("spelunk")
        .unwrap()
        .current_dir(temp.path())
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value =
        serde_json::from_slice(&output).expect("status --format json must produce valid JSON");
    let backend = json
        .get("memory_backend")
        .and_then(|v| v.as_str())
        .expect("memory_backend field must be present and a string");

    assert!(
        matches!(backend, "sqlite" | "git-meta" | "git-notes" | "remote"),
        "memory_backend must be one of the known values, got: {backend}"
    );
}

/// `spelunk check --format json` must include a `memory_backend` field (issue #308).
#[test]
fn check_json_includes_memory_backend_field() {
    let temp = tempdir().unwrap();
    init_git_repo(temp.path());
    let config_path = write_config(temp.path());

    // Index so check can open the DB.
    let mut index_cmd = Command::cargo_bin("spelunk").unwrap();
    index_cmd
        .current_dir(temp.path())
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(".")
        .assert()
        .success();

    let output = Command::cargo_bin("spelunk")
        .unwrap()
        .current_dir(temp.path())
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(&config_path)
        .arg("check")
        .arg("--format")
        .arg("json")
        .output()
        .expect("check --format json must run");

    // check exits 1 when stale files exist, which is fine for this test.
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("check --format json must produce valid JSON on stdout");
    let backend = json
        .get("memory_backend")
        .and_then(|v| v.as_str())
        .expect("memory_backend field must be present and a string");

    assert!(
        matches!(backend, "sqlite" | "git-meta" | "git-notes" | "remote"),
        "memory_backend must be one of the known values, got: {backend}"
    );
}

/// `spelunk status` text output must mention the memory backend (issue #308).
#[test]
fn status_text_mentions_memory_backend() {
    let temp = tempdir().unwrap();
    init_git_repo(temp.path());
    let config_path = write_config(temp.path());

    // Index first.
    Command::cargo_bin("spelunk")
        .unwrap()
        .current_dir(temp.path())
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(".")
        .assert()
        .success();

    Command::cargo_bin("spelunk")
        .unwrap()
        .current_dir(temp.path())
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Memory backend:"));
}
