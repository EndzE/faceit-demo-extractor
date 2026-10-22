use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use assert_cmd::Command;
use predicates::prelude::*;
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

use faceit_demo_extractor::config::{self, Config};
use faceit_demo_extractor::extract::{self, ExtractResult};
use faceit_demo_extractor::queue;
use faceit_demo_extractor::validate::{is_valid_filename, utf16_len};
use faceit_demo_extractor::{DEMO_MAGIC, MAX_FILENAME_LEN, Paths};

fn write_archive(path: &Path, payload: &[u8], checksum: bool) {
    let file = File::create(path).unwrap();
    let mut encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    encoder.include_checksum(checksum).unwrap();
    encoder.write_all(payload).unwrap();
    encoder.finish().unwrap();
}

fn demo_payload(size: usize) -> Vec<u8> {
    let mut payload = DEMO_MAGIC.to_vec();
    payload.extend((0..size).map(|index| (index % 251) as u8));
    payload
}

fn no_partial_files(directory: &Path) -> bool {
    fs::read_dir(directory).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".faceit-")
    })
}

#[test]
fn filename_validation_matches_python_rules() {
    assert!(is_valid_filename("1.dem"));
    assert!(is_valid_filename("demo_2024.dem"));
    assert!(is_valid_filename("a\u{0080}b.dem"));
    assert!(is_valid_filename(&"a".repeat(255)));

    assert!(!is_valid_filename(""));
    assert!(!is_valid_filename("."));
    assert!(!is_valid_filename(".."));
    assert!(!is_valid_filename("CON"));
    assert!(!is_valid_filename("con.dem"));
    assert!(!is_valid_filename("Lpt9.dem"));
    assert!(!is_valid_filename("foo/bar.dem"));
    assert!(!is_valid_filename(r"foo\bar.dem"));
    assert!(!is_valid_filename("bad<name"));
    assert!(!is_valid_filename("trailing."));
    assert!(!is_valid_filename("trailing "));
    assert!(!is_valid_filename("tab\tname"));
    assert!(!is_valid_filename("del\u{007f}"));
    assert!(!is_valid_filename("zero\u{200b}width"));
    assert!(!is_valid_filename(&"a".repeat(256)));
    assert!(!is_valid_filename(".dem"));
}

#[test]
fn split_ext_matches_python_splitext() {
    assert_eq!(
        extract::split_ext("1.dem"),
        ("1".to_string(), ".dem".to_string())
    );
    assert_eq!(
        extract::split_ext("a.b.c"),
        ("a.b".to_string(), ".c".to_string())
    );
    assert_eq!(
        extract::split_ext("demo"),
        ("demo".to_string(), String::new())
    );
    assert_eq!(
        extract::split_ext(".dem"),
        (".dem".to_string(), String::new())
    );
    assert_eq!(extract::split_ext("1."), ("1".to_string(), ".".to_string()));
}

#[test]
fn next_free_name_finds_gaps_and_trims_long_stems() {
    let directory = tempfile::tempdir().unwrap();
    File::create(directory.path().join("1_1.dem")).unwrap();
    File::create(directory.path().join("1_3.dem")).unwrap();

    assert_eq!(
        extract::next_free_name(directory.path(), "1".to_string(), ".dem"),
        "1_2.dem"
    );

    let long_stem = "a".repeat(300);
    let name = extract::next_free_name(directory.path(), long_stem, ".dem");
    assert!(utf16_len(&name) <= MAX_FILENAME_LEN);
    assert!(name.ends_with("_1.dem"));
}

#[test]
fn config_preserves_unknown_keys_and_bom() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    let original = b"\xEF\xBB\xBF{\n  \"unknown\": 42,\n  \"target_filename\": \"demo.dem\"\n}\n";
    fs::write(&path, original).unwrap();

    config::save_config(&path, &[("keep_zst", serde_json::Value::Bool(true))]).unwrap();

    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["unknown"], serde_json::Value::from(42));
    assert_eq!(
        saved["target_filename"],
        serde_json::Value::from("demo.dem")
    );
    assert_eq!(saved["keep_zst"], serde_json::Value::Bool(true));
    assert!(!fs::read(&path).unwrap().starts_with(&[0xEF, 0xBB, 0xBF]));
}

#[test]
fn config_falls_back_on_invalid_values() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    fs::write(
        &path,
        r#"{"target_filename": 7, "keep_zst": "yes", "move_to_recycle_bin": true}"#,
    )
    .unwrap();

    let config = config::load_config(&path);
    assert_eq!(config.target_filename, "1.dem");
    assert!(!config.keep_zst);
    assert!(config.move_to_recycle_bin);
}

#[test]
fn ensure_config_adds_missing_keys_only() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    fs::write(&path, r#"{"target_filename": "demo.dem", "custom": true}"#).unwrap();

    let (ok, error) = config::ensure_config_file(&path, false);
    assert!(ok, "{error:?}");

    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        saved["target_filename"],
        serde_json::Value::from("demo.dem")
    );
    assert_eq!(saved["custom"], serde_json::Value::Bool(true));
    assert_eq!(saved["keep_zst"], serde_json::Value::Bool(false));
    assert_eq!(saved["move_to_recycle_bin"], serde_json::Value::Bool(false));
}

#[test]
fn ensure_config_resets_invalid_json_and_non_object() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");

    fs::write(&path, "not json").unwrap();
    let (ok, error) = config::ensure_config_file(&path, false);
    assert!(ok, "{error:?}");
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        saved,
        serde_json::json!({
            "target_filename": "1.dem",
            "keep_zst": false,
            "move_to_recycle_bin": false
        })
    );

    fs::write(&path, "[]").unwrap();
    let (ok, error) = config::ensure_config_file(&path, false);
    assert!(ok, "{error:?}");
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["target_filename"], serde_json::Value::from("1.dem"));
}

#[test]
fn extract_success_replaces_existing_target_and_keeps_archive() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("demo.dem.zst");
    let output = directory.path().join("1.dem");
    let payload = demo_payload(4096);
    write_archive(&input, &payload, true);
    fs::write(&output, b"old demo").unwrap();

    let result = extract::extract_one(&input, &output, true, false);

    assert_eq!(result, ExtractResult::Ok);
    assert_eq!(fs::read(&output).unwrap(), payload);
    assert!(input.exists());
    assert!(no_partial_files(directory.path()));
}

#[test]
fn extract_concatenated_frames_succeeds() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("multi.dem.zst");
    let output = directory.path().join("1.dem");

    let first = demo_payload(512);
    let second = demo_payload(1024);
    let mut file = File::create(&input).unwrap();
    for payload in [&first, &second] {
        let mut encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
        encoder.include_checksum(true).unwrap();
        encoder.write_all(payload).unwrap();
        file = encoder.finish().unwrap();
    }

    let result = extract::extract_one(&input, &output, false, false);

    assert_eq!(result, ExtractResult::Ok);
    assert_eq!(fs::read(&output).unwrap(), [first, second].concat());
    assert!(!input.exists());
    assert!(no_partial_files(directory.path()));
}

#[test]
fn extract_empty_archive_fails_cleanly() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("empty.dem.zst");
    let output = directory.path().join("1.dem");
    fs::write(&input, b"").unwrap();
    fs::write(&output, b"existing demo").unwrap();

    let result = extract::extract_one(&input, &output, false, false);

    assert_eq!(result, ExtractResult::Failed);
    assert_eq!(fs::read(&output).unwrap(), b"existing demo");
    assert!(input.exists());
    assert!(no_partial_files(directory.path()));
}

#[test]
fn extract_without_checksum_succeeds() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("demo.dem.zst");
    let output = directory.path().join("1.dem");
    let payload = demo_payload(1024);
    write_archive(&input, &payload, false);

    let result = extract::extract_one(&input, &output, true, false);

    assert_eq!(result, ExtractResult::Ok);
    assert_eq!(fs::read(&output).unwrap(), payload);
}

#[test]
fn extract_wrong_magic_keeps_target_and_archive() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("wrong.dem.zst");
    let output = directory.path().join("1.dem");
    write_archive(&input, b"NOTADEMO and more bytes", true);
    fs::write(&output, b"existing demo").unwrap();

    let result = extract::extract_one(&input, &output, false, false);

    assert_eq!(result, ExtractResult::Failed);
    assert_eq!(fs::read(&output).unwrap(), b"existing demo");
    assert!(input.exists());
    assert!(no_partial_files(directory.path()));
}

#[test]
fn extract_truncated_archive_fails_cleanly() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("truncated.dem.zst");
    let output = directory.path().join("1.dem");
    write_archive(&input, &demo_payload(65536), true);

    let bytes = fs::read(&input).unwrap();
    fs::write(&input, &bytes[..bytes.len() / 2]).unwrap();
    fs::write(&output, b"existing demo").unwrap();

    let result = extract::extract_one(&input, &output, false, false);

    assert_eq!(result, ExtractResult::Failed);
    assert_eq!(fs::read(&output).unwrap(), b"existing demo");
    assert!(input.exists());
    assert!(no_partial_files(directory.path()));
}

#[test]
fn extract_checksum_mismatch_fails_cleanly() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("corrupt.dem.zst");
    let output = directory.path().join("1.dem");
    write_archive(&input, &demo_payload(65536), true);

    let mut bytes = fs::read(&input).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    fs::write(&input, &bytes).unwrap();
    fs::write(&output, b"existing demo").unwrap();

    let result = extract::extract_one(&input, &output, false, false);

    assert_eq!(result, ExtractResult::Failed);
    assert_eq!(fs::read(&output).unwrap(), b"existing demo");
    assert!(input.exists());
    assert!(no_partial_files(directory.path()));
}

#[test]
fn extract_not_zstd_fails_before_writing() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("plain.zst");
    let output = directory.path().join("1.dem");
    fs::write(&input, b"not a zstd archive").unwrap();
    fs::write(&output, b"existing demo").unwrap();

    let result = extract::extract_one(&input, &output, false, false);

    assert_eq!(result, ExtractResult::Failed);
    assert_eq!(fs::read(&output).unwrap(), b"existing demo");
    assert!(input.exists());
}

#[test]
fn extract_deletes_original_when_configured() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("demo.dem.zst");
    let output = directory.path().join("1.dem");
    write_archive(&input, &demo_payload(512), true);

    let result = extract::extract_one(&input, &output, false, false);

    assert_eq!(result, ExtractResult::Ok);
    assert!(!input.exists());
    assert!(output.exists());
}

#[test]
fn batch_mode_writes_first_free_slots() {
    let directory = tempfile::tempdir().unwrap();
    let inputs_dir = directory.path().join("in");
    fs::create_dir(&inputs_dir).unwrap();

    let mut inputs = Vec::new();
    for index in 0..2 {
        let input = inputs_dir.join(format!("demo{index}.zst"));
        write_archive(&input, &demo_payload(256 + index), true);
        inputs.push(input);
    }

    File::create(directory.path().join("1_2.dem")).unwrap();
    let code = extract::run_extract_batch(
        &inputs,
        directory.path(),
        "test",
        &Config::defaults(),
        false,
    );

    assert_eq!(code, 0);
    assert!(directory.path().join("1_1.dem").exists());
    assert!(directory.path().join("1_3.dem").exists());
    assert!(!inputs_dir.join("demo0.zst").exists());
    assert!(!inputs_dir.join("demo1.zst").exists());
}

#[test]
fn enqueue_writes_absolute_path_and_strips_quotes() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    let target = directory.path().join("demo.zst");
    fs::write(&target, b"x").unwrap();

    let files = vec![OsString::from(format!("  \"{}\"  ", target.display()))];
    let queued = queue::enqueue_paths(&paths, &files).unwrap();
    assert_eq!(queued, 1);

    let entries = fs::read_dir(&paths.queue_dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "path"))
        .count();
    assert_eq!(entries, 1);

    let taken = queue::take_queue_files(&paths);
    assert_eq!(taken, vec![target]);
}

#[test]
fn queue_tmp_entries_are_ignored() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    let target = directory.path().join("demo.zst");
    fs::write(&target, b"x").unwrap();

    queue::enqueue_paths(&paths, &[target.into_os_string()]).unwrap();
    let incomplete = paths.queue_dir.join("incomplete.tmp");
    fs::write(&incomplete, r"C:\should\not\be\read.zst").unwrap();

    let taken = queue::take_queue_files(&paths);
    assert_eq!(taken.len(), 1);
    assert!(incomplete.exists());
}

#[test]
fn queue_entries_sort_by_mtime() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    fs::create_dir_all(&paths.queue_dir).unwrap();

    let first = paths.queue_dir.join("first.path");
    let second = paths.queue_dir.join("second.path");
    fs::write(&first, r"C:\first.zst").unwrap();
    fs::write(&second, r"C:\second.zst").unwrap();

    let now = SystemTime::now();
    File::options()
        .write(true)
        .open(&first)
        .unwrap()
        .set_modified(now)
        .unwrap();
    File::options()
        .write(true)
        .open(&second)
        .unwrap()
        .set_modified(now - Duration::from_secs(10))
        .unwrap();

    let taken = queue::take_queue_files(&paths);
    assert_eq!(
        taken,
        vec![
            PathBuf::from(r"C:\second.zst"),
            PathBuf::from(r"C:\first.zst")
        ]
    );
}

#[test]
fn collect_waits_for_idle_window_and_initial_timeout() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    let target = directory.path().join("demo.zst");
    fs::write(&target, b"x").unwrap();
    queue::enqueue_paths(&paths, &[target.into_os_string()]).unwrap();

    let start = Instant::now();
    let collected = queue::collect_queued_files_with_timing(
        &paths,
        Duration::from_millis(120),
        Duration::from_secs(1),
        Duration::from_secs(3),
    );
    let elapsed = start.elapsed();

    assert_eq!(collected.len(), 1);
    assert!(elapsed >= Duration::from_millis(120));
    assert!(elapsed < Duration::from_secs(3));

    let start = Instant::now();
    let empty = queue::collect_queued_files_with_timing(
        &paths,
        Duration::from_millis(120),
        Duration::from_millis(120),
        Duration::from_secs(1),
    );
    assert!(empty.is_empty());
    assert!(start.elapsed() >= Duration::from_millis(120));
}

#[test]
fn worker_lock_is_exclusive_and_released_on_drop() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());

    let lock = queue::try_acquire_worker_lock(&paths).expect("first lock");
    assert!(!queue::worker_lock_is_free(&paths));
    assert!(queue::try_acquire_worker_lock(&paths).is_none());

    drop(lock);
    assert!(queue::worker_lock_is_free(&paths));
}

#[test]
fn spawn_lock_allows_only_one_spawner() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());

    let gate = queue::try_acquire_spawn_lock(&paths).expect("spawn gate");
    assert!(queue::try_acquire_spawn_lock(&paths).is_none());

    drop(gate);
    let gate = queue::try_acquire_spawn_lock(&paths);
    assert!(gate.is_some());
}

#[test]
fn retirement_rescan_keeps_late_files() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    let target = directory.path().join("late.zst");
    fs::write(&target, b"x").unwrap();

    let lock = queue::try_acquire_worker_lock(&paths).expect("worker lock");
    queue::enqueue_paths(&paths, &[target.into_os_string()]).unwrap();
    drop(lock);

    assert!(queue::has_queued_files(&paths));
    let replacement = queue::try_acquire_worker_lock(&paths);
    assert!(replacement.is_some());
}

#[test]
fn worker_pid_round_trips() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    fs::create_dir_all(&paths.queue_dir).unwrap();

    queue::write_worker_pid(&paths);
    assert_eq!(queue::read_worker_pid(&paths), Some(std::process::id()));

    queue::cleanup_worker_pid(&paths);
    assert_eq!(queue::read_worker_pid(&paths), None);
}

#[test]
fn worker_state_reports_held_lock_and_pid() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());

    let lock = queue::try_acquire_worker_lock(&paths).unwrap();
    queue::write_worker_pid(&paths);

    assert_eq!(
        queue::worker_state(&paths),
        (true, Some(std::process::id()))
    );

    drop(lock);
    assert_eq!(queue::worker_state(&paths), (false, None));
    queue::cleanup_worker_pid(&paths);
}

#[test]
fn help_prints_usage() {
    Command::cargo_bin("faceit-dem")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage:"))
        .stdout(predicate::str::contains("faceit-dem --status"));
}

#[test]
fn unknown_option_exits_two() {
    Command::cargo_bin("faceit-dem")
        .unwrap()
        .arg("--definitely-not-an-option")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unknown option"));
}

#[test]
fn dash_dash_without_files_exits_two() {
    Command::cargo_bin("faceit-dem")
        .unwrap()
        .arg("--")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no input files after --"));
}

#[test]
fn status_prints_configuration() {
    Command::cargo_bin("faceit-dem")
        .unwrap()
        .arg("--status")
        .assert()
        .success()
        .stdout(predicate::str::contains("FACEIT Demo Extractor"))
        .stdout(predicate::str::contains("zstd          : bundled"))
        .stdout(predicate::str::contains("target_filename"));
}

#[test]
fn missing_file_exits_one() {
    Command::cargo_bin("faceit-dem")
        .unwrap()
        .arg("definitely-missing-demo.zst")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("ERROR:"));
}

#[test]
#[ignore = "touches HKCU test subkey"]
fn registry_install_and_remove_test_subkey() {
    const TEST_KEY: &str = r"Software\FaceitDemoExtractorTests\ContextMenu";
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let _ = hkcu.delete_subkey_all(TEST_KEY);

    faceit_demo_extractor::registry::install_context_menu_at(
        TEST_KEY,
        "Test label",
        r#""C:\test\faceit-dem-quiet.exe" --context-menu "%1""#,
    )
    .unwrap();

    let key = hkcu.open_subkey(TEST_KEY).unwrap();
    let label: String = key.get_value("").unwrap();
    assert_eq!(label, "Test label");

    let command_key = hkcu.open_subkey(format!("{TEST_KEY}\\command")).unwrap();
    let command: String = command_key.get_value("").unwrap();
    assert!(command.contains("--context-menu"));

    faceit_demo_extractor::registry::remove_context_menu_at(TEST_KEY).unwrap();
    assert!(hkcu.open_subkey(TEST_KEY).is_err());
}

#[test]
#[ignore = "moves a temporary file to the Recycle Bin"]
fn recycle_bin_moves_temp_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("recycle-me.txt");
    fs::write(&path, b"x").unwrap();

    extract::delete_original(&path, true).unwrap();
    assert!(!path.exists());
}
