use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::windows::fs::OpenOptionsExt,
    path::PathBuf,
};

use serde_json::{Value, json};
use tempfile::{Builder, TempDir};
use tokio_util::sync::CancellationToken;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

use super::*;
use crate::settings::ReaderSettings;

fn fixture() -> (TempDir, Spool, PathBuf) {
    let home = Builder::new().prefix("mcp-vm-spool-").tempdir().unwrap();
    let shared = home.path().join("shared");
    fs::create_dir(&shared).unwrap();
    let root = Root::new(&shared, ReaderSettings::default()).unwrap();
    let spool = Spool::create(&root).unwrap();
    let directory = shared.join(spool.name());
    assert!(directory.is_dir());
    assert!(spool.name().starts_with("mcp-vm-session-"));
    (home, spool, directory)
}

fn budget() -> Budget {
    Budget::new(CancellationToken::new())
}

fn read_packet(directory: &std::path::Path, name: &str) -> Value {
    serde_json::from_slice(&fs::read(directory.join(name)).unwrap()).unwrap()
}

#[test]
fn readiness_revalidates_mappings_before_publishing_a_single_request() {
    let (home, mut spool, directory) = fixture();
    let shared = directory.parent().unwrap();
    let root = Root::new(shared, ReaderSettings::default()).unwrap();
    let mut budget = budget();
    let plan = Plan::fixture("main", &root, home.path(), &mut budget).unwrap();
    assert_eq!(
        spool.guest_path("main", &plan).unwrap(),
        format!(r"C:\MCP\mapping-0\{}", spool.name())
    );
    assert!(spool.guest_path("other", &plan).is_err());
    let other = home.path().join("other");
    fs::create_dir(&other).unwrap();
    let other = Root::new(&other, ReaderSettings::default()).unwrap();
    let other_plan = Plan::fixture("main", &other, home.path(), &mut budget).unwrap();
    assert!(
        spool
            .guest_path("main", &other_plan)
            .unwrap_err()
            .to_string()
            .contains("VM_SPOOL_SCOPE")
    );

    let command = "合成請求";
    assert!(
        !spool
            .request_if_ready(command, "main", &plan, &mut budget)
            .unwrap()
    );
    assert!(!directory.join("request.json").exists());
    fs::write(directory.join("ready.json"), br#"{"ready":true,"extra":1}"#).unwrap();
    assert!(
        spool
            .request_if_ready(command, "main", &plan, &mut budget)
            .is_err()
    );
    fs::write(directory.join("ready.json"), br#"{"ready":true}"#).unwrap();
    let external = home.path().join("outside.txt");
    fs::write(&external, "synthetic outside").unwrap();
    let alias = shared.join("foreign-alias.txt");
    fs::hard_link(&external, &alias).unwrap();
    let error = spool
        .request_if_ready(command, "main", &plan, &mut budget)
        .unwrap_err();
    assert!(error.to_string().contains("VM_MAPPING_ALIAS"), "{error}");
    assert!(!directory.join("request.json").exists());
    fs::remove_file(alias).unwrap();
    assert!(
        spool
            .request_if_ready(command, "main", &plan, &mut budget)
            .unwrap()
    );
    assert_eq!(
        read_packet(&directory, "request.json"),
        json!({"command":command})
    );
    assert!(
        spool
            .request_if_ready(command, "main", &plan, &mut budget)
            .is_err()
    );
    spool.close_after_stop(&mut budget, || Ok(())).unwrap();
}

#[test]
fn stdin_keeps_sequence_and_does_not_replace_existing_packets() {
    let (_home, mut spool, directory) = fixture();
    let mut budget = budget();
    let data = "輸入".as_bytes();
    assert_eq!(
        spool.write_stdin(data, false, &mut budget).unwrap(),
        data.len()
    );
    assert_eq!(
        read_packet(&directory, "stdin-000000.json"),
        json!({"data":STANDARD.encode(data)})
    );
    let existing = directory.join("stdin-000001.json");
    fs::write(&existing, b"synthetic collision").unwrap();
    assert!(spool.write_stdin(b"second", true, &mut budget).is_err());
    assert_eq!(fs::read(&existing).unwrap(), b"synthetic collision");
    assert!(!fs::read_dir(&directory).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("pending-")
    }));
    fs::remove_file(existing).unwrap();
    assert!(
        spool
            .write_stdin(&[0; MAX_STDIN + 1], false, &mut budget)
            .unwrap_err()
            .to_string()
            .contains("STDIN_LIMIT")
    );
    assert_eq!(spool.write_stdin(b"second", true, &mut budget).unwrap(), 6);
    assert_eq!(
        read_packet(&directory, "stdin-000001.json"),
        json!({"data":STANDARD.encode(b"second"),"eof":true})
    );
    assert!(!directory.join("stdin-000002.json").exists());
    assert!(spool.write_stdin(b"late", false, &mut budget).is_err());
    spool.close_after_stop(&mut budget, || Ok(())).unwrap();
}

#[test]
fn snapshots_are_bounded_and_preserve_prior_output_bytes() {
    let (_home, mut spool, directory) = fixture();
    let mut budget = budget();
    spool.refresh_output(&mut budget).unwrap();
    assert!(spool.output().is_empty());
    let path = directory.join("output.txt");
    fs::write(&path, "前段").unwrap();
    spool.refresh_output(&mut budget).unwrap();
    assert_eq!(spool.output(), "前段".as_bytes());
    let mut writer = OpenOptions::new()
        .append(true)
        .share_mode(FILE_SHARE_READ)
        .open(&path)
        .unwrap();
    writer.write_all("後段".as_bytes()).unwrap();
    writer.flush().unwrap();
    spool.refresh_output(&mut budget).unwrap();
    assert_eq!(spool.output(), "前段後段".as_bytes());
    drop(writer);
    fs::write(&path, "改寫後段").unwrap();
    assert!(
        spool
            .refresh_output(&mut budget)
            .unwrap_err()
            .to_string()
            .contains("VM_OUTPUT_CHANGED")
    );
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len((MAX_OUTPUT + 1) as u64)
        .unwrap();
    assert!(
        spool
            .refresh_output(&mut budget)
            .unwrap_err()
            .to_string()
            .contains("VM_OUTPUT_LIMIT")
    );
    spool.close_after_stop(&mut budget, || Ok(())).unwrap();
}

#[test]
fn results_have_typed_codes_and_cannot_report_early_success() {
    let (_home, mut spool, directory) = fixture();
    let mut budget = budget();
    let result = directory.join("result.json");
    assert_eq!(spool.result(&mut budget).unwrap(), None);
    fs::write(&result, br#"{"exit_code":0}"#).unwrap();
    assert!(spool.result(&mut budget).is_err());
    fs::write(&result, br#"{"exit_code":true}"#).unwrap();
    assert!(spool.result(&mut budget).is_err());
    fs::write(&result, br#"{"exit_code":1,"error":"GUEST_IO_FAILED"}"#).unwrap();
    assert_eq!(
        spool.result(&mut budget).unwrap(),
        Some(GuestResult {
            exit_code: 1,
            error: Some(GuestError::GuestIoFailed)
        })
    );
    assert!(spool.write_stdin(b"late", false, &mut budget).is_err());
    spool.close_after_stop(&mut budget, || Ok(())).unwrap();
}

#[test]
fn cleanup_requires_stop_proof_and_handles_owned_nested_readonly_files() {
    let (_home, mut spool, directory) = fixture();
    let closed_file = directory.join("nested/temporary.txt");
    fs::create_dir(closed_file.parent().unwrap()).unwrap();
    fs::write(&closed_file, "synthetic").unwrap();
    let mut attributes = fs::metadata(&closed_file).unwrap().permissions();
    attributes.set_readonly(true);
    fs::set_permissions(&closed_file, attributes).unwrap();
    let mut budget = budget();
    assert!(
        spool
            .close_after_stop(&mut budget, || anyhow::bail!(
                "synthetic missing stop proof"
            ))
            .is_err()
    );
    assert_eq!(fs::read(&closed_file).unwrap(), b"synthetic");
    assert!(spool.write_stdin(b"closing", false, &mut budget).is_err());
    spool.close_after_stop(&mut budget, || Ok(())).unwrap();
    assert!(!directory.exists());
    spool
        .close_after_stop(&mut budget, || panic!("already closed"))
        .unwrap();
}

#[test]
fn hardlinks_are_rejected_as_output_and_readonly_aliases_keep_their_attributes() {
    let (home, mut spool, directory) = fixture();
    let exterior = home.path().join("outside.txt");
    fs::write(&exterior, b"must remain outside").unwrap();
    fs::hard_link(&exterior, directory.join("output.txt")).unwrap();
    let mut budget = budget();
    assert!(spool.refresh_output(&mut budget).is_err());
    let mut attributes = fs::metadata(&exterior).unwrap().permissions();
    attributes.set_readonly(true);
    fs::set_permissions(&exterior, attributes.clone()).unwrap();
    let result = spool.close_after_stop(&mut budget, || Ok(()));
    let preserved = fs::metadata(&exterior).unwrap().permissions().readonly();
    attributes.set_readonly(false);
    fs::set_permissions(&exterior, attributes).unwrap();
    assert!(result.is_err() && preserved);
    spool.close_after_stop(&mut budget, || Ok(())).unwrap();
    assert_eq!(fs::read(&exterior).unwrap(), b"must remain outside");
}

#[test]
fn cleanup_retry_refuses_a_directory_that_replaced_the_owned_spool() {
    let (_home, mut spool, directory) = fixture();
    // A second no-delete handle prevents the last delete after children are gone.
    let blocking = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(&directory)
        .unwrap();
    let mut budget = budget();
    assert!(spool.close_after_stop(&mut budget, || Ok(())).is_err());
    drop(blocking);
    fs::rename(&directory, directory.with_file_name("renamed-original")).unwrap();
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join("outside.txt"), b"new owner").unwrap();
    assert!(
        spool
            .close_after_stop(&mut budget, || Ok(()))
            .unwrap_err()
            .to_string()
            .contains("VM_SPOOL_CHANGED")
    );
    assert_eq!(
        fs::read(directory.join("outside.txt")).unwrap(),
        b"new owner"
    );
}

#[test]
#[ignore = "requires Windows Developer Mode or symlink privilege"]
fn cleanup_unlinks_a_guest_directory_symlink_without_traversal() {
    let (home, mut spool, directory) = fixture();
    let external = home.path().join("external");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("untouched.txt"), b"outside").unwrap();
    std::os::windows::fs::symlink_dir(&external, directory.join("external-link")).unwrap();
    spool.close_after_stop(&mut budget(), || Ok(())).unwrap();
    assert!(!directory.exists());
    assert_eq!(
        fs::read(external.join("untouched.txt")).unwrap(),
        b"outside"
    );
}
