use nr_cli::launcher_process::{get_pid_file_path, read_pid_record, remove_pid_record_if_owner, write_pid_record, PidRecord};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_pid_record_round_trip() {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("launcher.pid");

    let record = PidRecord::new(1234, Some(5678), Some(20128), "9router".to_string());
    write_pid_record(&file_path, &record).unwrap();

    let read_back = read_pid_record(&file_path).unwrap();
    assert!(read_back.is_some());
    let recovered = read_back.unwrap();
    assert_eq!(recovered.cli_pid, 1234);
    assert_eq!(recovered.server_pid, Some(5678));
    assert_eq!(recovered.port, Some(20128));
    assert_eq!(recovered.exe, "9router");
}

#[test]
fn test_pid_record_nested_dir() {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("nested").join("dir").join("launcher.pid");

    let record = PidRecord::new(1234, Some(5678), Some(20128), "9router".to_string());
    write_pid_record(&file_path, &record).unwrap();

    assert!(file_path.exists());
    let read_back = read_pid_record(&file_path).unwrap();
    assert!(read_back.is_some());
}

#[test]
fn test_pid_record_returns_none_when_missing() {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("launcher.pid");

    let result = read_pid_record(&file_path).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_pid_record_returns_none_for_malformed_json() {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("launcher.pid");

    fs::write(&file_path, "{not valid json").unwrap();
    let result = read_pid_record(&file_path).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_pid_record_invalid_pids_filtered() {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("launcher.pid");

    // Write a record with invalid PIDs
    fs::write(&file_path, r#"{"cliPid": 0, "serverPid": -1, "port": 20128, "exe": "9router"}"#).unwrap();
    let result = read_pid_record(&file_path).unwrap();
    assert!(result.is_none()); // Invalid cliPid should reject whole record
}

#[test]
fn test_pid_record_missing_exe_filtered() {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("launcher.pid");

    fs::write(&file_path, r#"{"cliPid": 123, "serverPid": 456, "port": 20128, "exe": ""}"#).unwrap();
    let result = read_pid_record(&file_path).unwrap();
    assert!(result.is_none()); // Missing exe should reject
}

#[test]
fn test_remove_pid_record_only_by_owner() {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("launcher.pid");

    let record = PidRecord::new(1234, Some(5678), Some(20128), "9router".to_string());
    write_pid_record(&file_path, &record).unwrap();
    assert!(file_path.exists());

    // Try to remove with wrong owner
    remove_pid_record_if_owner(&file_path, 9999).unwrap();
    assert!(file_path.exists(), "should not remove when owner doesn't match");

    // Remove with correct owner
    remove_pid_record_if_owner(&file_path, 1234).unwrap();
    assert!(!file_path.exists(), "should remove when owner matches");
}

#[test]
fn test_remove_pid_record_nonexistent_file() {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("launcher.pid");

    // Should not error on nonexistent file
    remove_pid_record_if_owner(&file_path, 1234).unwrap();
}

#[test]
fn test_get_pid_file_path() {
    let temp_dir = TempDir::new().unwrap();
    let expected = temp_dir.path().join("launcher.pid");
    let actual = get_pid_file_path(temp_dir.path());
    assert_eq!(actual, expected);
}

#[test]
fn test_pid_record_atomic_write() {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("launcher.pid");

    // Write first record
    let record1 = PidRecord::new(1111, Some(2222), Some(20128), "9router".to_string());
    write_pid_record(&file_path, &record1).unwrap();

    // Write second record (should replace)
    let record2 = PidRecord::new(3333, Some(4444), Some(20129), "9router".to_string());
    write_pid_record(&file_path, &record2).unwrap();

    // Verify final state matches second record
    let final_record = read_pid_record(&file_path).unwrap().unwrap();
    assert_eq!(final_record.cli_pid, 3333);
    assert_eq!(final_record.port, Some(20129));
}
