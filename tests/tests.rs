use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::thread::{self, sleep};
use std::time::{Duration, Instant, SystemTime};

use assert_cmd::Command;
use predicates::prelude::*;
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

use faceit_demo_extractor::config::{self, Config};
use faceit_demo_extractor::cs2;
use faceit_demo_extractor::extract::{self, ExtractResult};
use faceit_demo_extractor::queue;
use faceit_demo_extractor::registry;
use faceit_demo_extractor::validate::{is_valid_demo_filename, is_valid_filename, utf16_len};
use faceit_demo_extractor::{DEMO_MAGIC, HOME_ENV, MAX_FILENAME_LEN, Paths};

fn archive_bytes(payload: &[u8], checksum: bool) -> Vec<u8> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    encoder
        .set_pledged_src_size(Some(payload.len() as u64))
        .unwrap();
    encoder.include_checksum(checksum).unwrap();
    encoder.write_all(payload).unwrap();
    encoder.finish().unwrap()
}

fn write_archive(path: &Path, payload: &[u8], checksum: bool) {
    fs::write(path, archive_bytes(payload, checksum)).unwrap();
}

fn bzip2_archive_bytes(payload: &[u8]) -> Vec<u8> {
    let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
    encoder.write_all(payload).unwrap();
    encoder.finish().unwrap()
}

fn write_bzip2_archive(path: &Path, payload: &[u8]) {
    fs::write(path, bzip2_archive_bytes(payload)).unwrap();
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

fn cli(root: &Path) -> Command {
    let mut command = Command::cargo_bin("faceit-dem").unwrap();
    command.env(HOME_ENV, root).env("NO_COLOR", "1");
    command
}

fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(50));
    }
}

#[test]
fn filename_validation_rejects_invalid_windows_names() {
    assert!(is_valid_filename("1.dem"));
    assert!(is_valid_filename("demo_2024.dem"));
    assert!(is_valid_filename("a\u{0080}b.dem"));
    assert!(is_valid_filename(&"a".repeat(255)));

    assert!(!is_valid_filename(""));
    assert!(!is_valid_filename("."));
    assert!(!is_valid_filename(".."));
    assert!(is_valid_filename("COM10.dem"));
    assert!(!is_valid_filename("CON"));
    assert!(!is_valid_filename("con.dem"));
    assert!(!is_valid_filename("Lpt9.dem"));
    assert!(!is_valid_filename("CONIN$"));
    assert!(!is_valid_filename("conout$.dem"));
    assert!(!is_valid_filename("COM¹.dem"));
    assert!(!is_valid_filename("COM².dem"));
    assert!(!is_valid_filename("COM³.dem"));
    assert!(!is_valid_filename("LPT¹.dem"));
    assert!(!is_valid_filename("LPT².dem"));
    assert!(!is_valid_filename("LPT³.dem"));
    assert!(!is_valid_filename("foo/bar.dem"));
    assert!(!is_valid_filename(r"foo\bar.dem"));
    assert!(!is_valid_filename("bad<name"));
    assert!(!is_valid_filename("trailing."));
    assert!(!is_valid_filename("trailing "));
    assert!(!is_valid_filename(" leading"));
    assert!(!is_valid_filename(" tab\t"));
    assert!(!is_valid_filename("tab\tname"));
    assert!(!is_valid_filename("del\u{007f}"));
    assert!(!is_valid_filename("zero\u{200b}width"));
    assert!(!is_valid_filename(&"a".repeat(256)));
    assert!(!is_valid_filename(".dem"));
}

#[test]
fn demo_filename_validation_requires_dem_extension() {
    assert!(is_valid_demo_filename("1.dem"));
    assert!(is_valid_demo_filename("demo_2024.dem"));
    assert!(is_valid_demo_filename("DEMO.DEM"));
    assert!(is_valid_filename("1.txt"));
    assert!(!is_valid_demo_filename("1.txt"));
    assert!(!is_valid_demo_filename("1"));
    assert!(!is_valid_demo_filename("1.dem.txt"));
    assert!(!is_valid_demo_filename("con.dem"));
    assert!(!is_valid_demo_filename(" 1.dem"));
    assert!(!is_valid_demo_filename("1.dem "));
}

#[test]
fn split_ext_splits_extension() {
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
fn available_space_reads_the_target_drive() {
    let directory = tempfile::tempdir().unwrap();

    let available = faceit_demo_extractor::win32::available_space(directory.path()).unwrap();

    assert!(available > 0);
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
        r#"{"target_filename": 7, "keep_zst": "yes", "move_to_recycle_bin": true, "csgo_path": 7}"#,
    )
    .unwrap();

    let config = config::load_config(&path);
    assert_eq!(config.target_filename, "1.dem");
    assert!(!config.keep_zst);
    assert!(config.move_to_recycle_bin);
    assert!(config.overwrite_single_file_extraction);
    assert_eq!(config.csgo_path, None);
}

#[test]
fn config_rejects_target_filename_without_dem_extension() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    fs::write(&path, r#"{"target_filename": "1.txt"}"#).unwrap();

    assert_eq!(config::load_config(&path).target_filename, "1.dem");

    fs::write(&path, r#"{"target_filename": "DEMO.DEM"}"#).unwrap();
    assert_eq!(config::load_config(&path).target_filename, "DEMO.DEM");

    fs::write(&path, r#"{"target_filename": "  spaced.dem  "}"#).unwrap();
    assert_eq!(config::load_config(&path).target_filename, "spaced.dem");

    fs::write(&path, r#"{"target_filename": "   "}"#).unwrap();
    assert_eq!(config::load_config(&path).target_filename, "1.dem");
}

#[test]
fn config_reads_csgo_path() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    fs::write(&path, r#"{"csgo_path": " C:\\CS2\\game\\csgo "}"#).unwrap();

    let config = config::load_config(&path);
    assert_eq!(config.csgo_path.as_deref(), Some(r"C:\CS2\game\csgo"));
}

#[test]
fn csgo_path_override_resolves_install_and_csgo_folders() {
    let directory = tempfile::tempdir().unwrap();
    let csgo = directory.path().join("game").join("csgo");
    fs::create_dir_all(&csgo).unwrap();

    let (found, source) =
        cs2::find_cs2_csgo_path(Some(directory.path().to_str().unwrap()), directory.path());
    assert_eq!(found.as_deref(), Some(csgo.as_path()));
    assert!(source.contains("csgo_path"));

    let (found, _) = cs2::find_cs2_csgo_path(Some(csgo.to_str().unwrap()), directory.path());
    assert_eq!(found.as_deref(), Some(csgo.as_path()));
}

#[test]
fn csgo_path_override_resolves_root_folder_named_csgo() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("csgo");
    let csgo = root.join("game").join("csgo");
    fs::create_dir_all(&csgo).unwrap();

    let (found, _) = cs2::find_cs2_csgo_path(Some(root.to_str().unwrap()), directory.path());
    assert_eq!(found.as_deref(), Some(csgo.as_path()));
}

#[test]
fn relative_csgo_path_resolves_against_base_dir() {
    let directory = tempfile::tempdir().unwrap();
    let csgo = directory
        .path()
        .join("custom-cs2")
        .join("game")
        .join("csgo");
    fs::create_dir_all(&csgo).unwrap();

    let (found, source) = cs2::find_cs2_csgo_path(Some("custom-cs2"), directory.path());
    assert_eq!(found.as_deref(), Some(csgo.as_path()));
    assert!(source.contains("custom-cs2"));

    let (found, _) = cs2::find_cs2_csgo_path(Some(r"custom-cs2\game\csgo"), directory.path());
    assert_eq!(found.as_deref(), Some(csgo.as_path()));
}

#[test]
fn csgo_path_override_does_not_fall_back_to_registry_when_missing() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing");

    let (found, source) =
        cs2::find_cs2_csgo_path(Some(missing.to_str().unwrap()), directory.path());
    assert!(found.is_none());
    assert!(source.contains("csgo_path"));
    assert!(source.contains("missing"));
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
    assert_eq!(
        saved["overwrite_single_file_extraction"],
        serde_json::Value::Bool(true)
    );
    assert_eq!(saved["csgo_path"], serde_json::Value::Null);
}

#[test]
fn ensure_config_resets_invalid_json_and_non_object() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    let backup = directory.path().join("config.json.bak");

    fs::write(&path, "not json").unwrap();
    let (ok, error) = config::ensure_config_file(&path, false);
    assert!(ok, "{error:?}");
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        saved,
        serde_json::json!({
            "target_filename": "1.dem",
            "keep_zst": false,
            "move_to_recycle_bin": false,
            "overwrite_single_file_extraction": true,
            "csgo_path": null
        })
    );
    assert_eq!(fs::read_to_string(&backup).unwrap(), "not json");

    fs::write(&path, "[]").unwrap();
    let (ok, error) = config::ensure_config_file(&path, false);
    assert!(ok, "{error:?}");
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["target_filename"], serde_json::Value::from("1.dem"));
    assert_eq!(fs::read_to_string(&backup).unwrap(), "[]");
}

#[test]
fn ensure_config_refuses_to_reset_when_backup_fails() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    fs::write(&path, "not json").unwrap();
    fs::create_dir(directory.path().join("config.json.bak")).unwrap();

    let (ok, error) = config::ensure_config_file(&path, false);
    assert!(!ok);
    assert!(
        error.unwrap().contains("refusing to overwrite"),
        "unexpected error"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
}

#[test]
fn save_config_refuses_to_overwrite_invalid_json() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    fs::write(&path, "not json").unwrap();

    let error =
        config::save_config(&path, &[("keep_zst", serde_json::Value::Bool(true))]).unwrap_err();
    assert!(error.contains("not valid JSON"), "{error}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
    assert!(!directory.path().join("config.json.bak").exists());
}

#[test]
fn csgo_path_override_drives_extraction() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    let install = directory.path().join("custom-cs2");
    let csgo = install.join("game").join("csgo");
    fs::create_dir_all(&csgo).unwrap();
    fs::write(
        &paths.config,
        serde_json::json!({
            "target_filename": "1.dem",
            "keep_zst": true,
            "move_to_recycle_bin": false,
            "csgo_path": "custom-cs2"
        })
        .to_string(),
    )
    .unwrap();

    let input = directory.path().join("demo.dem.zst");
    let payload = demo_payload(4096);
    write_archive(&input, &payload, true);

    let code = extract::run_extract_mode(&paths, std::slice::from_ref(&input), false);
    assert_eq!(code, 0);
    assert_eq!(fs::read(csgo.join("1.dem")).unwrap(), payload);
    assert!(input.exists());
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
        encoder
            .set_pledged_src_size(Some(payload.len() as u64))
            .unwrap();
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
fn extract_accepts_a_skippable_frame_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("demo.dem.zst");

    let mut bytes = vec![0x50, 0x2A, 0x4D, 0x18];
    bytes.extend_from_slice(&3u32.to_le_bytes());
    bytes.extend_from_slice(b"abc");
    bytes.extend(archive_bytes(&demo_payload(512), true));
    fs::write(&input, bytes).unwrap();

    let code = extract::run_extract_batch(
        &[input],
        directory.path(),
        "test",
        &Config::defaults(),
        false,
    );

    assert_eq!(code, 0);
    assert!(directory.path().join("1.dem").exists());
}

#[test]
fn extract_without_checksum_keeps_the_archive() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("demo.dem.zst");
    let output = directory.path().join("1.dem");
    let payload = demo_payload(1024);
    write_archive(&input, &payload, false);

    let result = extract::extract_one(&input, &output, false, false);

    assert_eq!(result, ExtractResult::Ok);
    assert_eq!(fs::read(&output).unwrap(), payload);
    assert!(input.exists());
}

#[test]
fn test_archive_verifies_checksum_without_writing() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("demo.dem.zst");
    write_archive(&input, &demo_payload(4096), true);

    let result = extract::test_archive(&input);

    assert_eq!(result, ExtractResult::Ok);
    assert!(input.exists());
    assert!(no_partial_files(directory.path()));
}

#[test]
fn inspect_zstd_header_handles_large_blocks_with_compact_buffer() {
    let path = std::path::Path::new(
        r"C:\Users\User\Downloads\1-0a0a90f7-0e49-4a87-8e94-8bbcc758f22d-1-1.dem.zst",
    );
    if !path.is_file() {
        return;
    }

    assert_eq!(extract::test_archive(path), ExtractResult::Ok);
}

#[test]
fn test_archive_rejects_checksum_mismatch() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("corrupt.dem.zst");
    write_archive(&input, &demo_payload(4096), true);

    let mut bytes = fs::read(&input).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    fs::write(&input, &bytes).unwrap();

    let result = extract::test_archive(&input);

    assert_eq!(result, ExtractResult::Failed);
    assert!(input.exists());
    assert!(no_partial_files(directory.path()));
}

#[test]
fn test_archive_rejects_archive_without_checksum() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("unchecked.dem.zst");
    write_archive(&input, &demo_payload(4096), false);

    let result = extract::test_archive(&input);

    assert_eq!(result, ExtractResult::Failed);
    assert!(input.exists());
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
fn extract_refuses_to_overwrite_the_archive_itself() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("1.dem");
    write_archive(&archive, &demo_payload(4096), true);
    let original = fs::read(&archive).unwrap();

    let result = extract::extract_one(&archive, &archive, false, false);
    assert_eq!(result, ExtractResult::Failed);
    assert_eq!(fs::read(&archive).unwrap(), original);

    let differently_cased = directory.path().join("1.DEM");
    let result = extract::extract_one(&archive, &differently_cased, false, false);
    assert_eq!(result, ExtractResult::Failed);
    assert_eq!(fs::read(&archive).unwrap(), original);

    let code = extract::run_extract_batch(
        std::slice::from_ref(&archive),
        directory.path(),
        "test",
        &Config::defaults(),
        false,
    );
    assert_eq!(code, 1);
    assert_eq!(fs::read(&archive).unwrap(), original);
    assert!(no_partial_files(directory.path()));
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
fn single_file_extraction_respects_overwrite_setting() {
    let directory = tempfile::tempdir().unwrap();
    let csgo = directory.path().join("csgo");
    fs::create_dir(&csgo).unwrap();

    let input1 = directory.path().join("demo1.zst");
    let input2 = directory.path().join("demo2.zst");
    let input3 = directory.path().join("demo3.zst");
    let payload1 = demo_payload(512);
    let payload2 = demo_payload(1024);
    let payload3 = demo_payload(2048);
    write_archive(&input1, &payload1, true);
    write_archive(&input2, &payload2, true);
    write_archive(&input3, &payload3, true);

    let mut config = Config::defaults();
    config.overwrite_single_file_extraction = false;

    // 1. When 1.dem does not exist, extracts as 1.dem
    let code = extract::run_extract_batch(&[input1], &csgo, "test", &config, false);
    assert_eq!(code, 0);
    assert_eq!(fs::read(csgo.join("1.dem")).unwrap(), payload1);

    // 2. When 1.dem exists and overwrite_single_file_extraction is false, saves as 1_1.dem
    let code = extract::run_extract_batch(&[input2], &csgo, "test", &config, false);
    assert_eq!(code, 0);
    assert_eq!(fs::read(csgo.join("1.dem")).unwrap(), payload1);
    assert_eq!(fs::read(csgo.join("1_1.dem")).unwrap(), payload2);

    // 3. When 1.dem and 1_1.dem exist, saves as 1_2.dem
    let code = extract::run_extract_batch(&[input3], &csgo, "test", &config, false);
    assert_eq!(code, 0);
    assert_eq!(fs::read(csgo.join("1.dem")).unwrap(), payload1);
    assert_eq!(fs::read(csgo.join("1_1.dem")).unwrap(), payload2);
    assert_eq!(fs::read(csgo.join("1_2.dem")).unwrap(), payload3);

    // 4. When overwrite_single_file_extraction is true, overwrites 1.dem
    let input4 = directory.path().join("demo4.zst");
    let payload4 = demo_payload(4096);
    write_archive(&input4, &payload4, true);

    config.overwrite_single_file_extraction = true;
    let code = extract::run_extract_batch(&[input4], &csgo, "test", &config, false);
    assert_eq!(code, 0);
    assert_eq!(fs::read(csgo.join("1.dem")).unwrap(), payload4);
}

#[test]
fn batch_mode_keeps_failed_archives() {
    let directory = tempfile::tempdir().unwrap();
    let good = directory.path().join("good.dem.zst");
    let bad = directory.path().join("bad.dem.zst");
    write_archive(&good, &demo_payload(512), true);
    fs::write(&bad, b"not a zstd archive").unwrap();

    let code = extract::run_extract_batch(
        &[good, bad.clone()],
        directory.path(),
        "test",
        &Config::defaults(),
        false,
    );

    assert_eq!(code, 1);
    assert!(directory.path().join("1_1.dem").exists());
    assert!(bad.exists());
}

#[test]
fn enqueue_writes_absolute_path_and_strips_quotes() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    let target = directory.path().join("demo.zst");
    fs::write(&target, b"x").unwrap();

    let files = vec![OsString::from(format!("  \"{}\"  ", target.display()))];
    let outcome = queue::enqueue_paths(&paths, &files).unwrap();
    assert_eq!(outcome.queued, 1);
    assert_eq!(outcome.duplicates, 0);
    assert!(outcome.failures.is_empty());

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
fn duplicate_enqueues_are_collapsed() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    let target = directory.path().join("demo.zst");
    fs::write(&target, b"x").unwrap();

    let first = queue::enqueue_paths(&paths, &[target.clone().into_os_string()]).unwrap();
    assert_eq!(first.queued, 1);

    let second = queue::enqueue_paths(
        &paths,
        &[
            target.clone().into_os_string(),
            target.clone().into_os_string(),
        ],
    )
    .unwrap();
    assert_eq!(second.queued, 0);
    assert_eq!(second.duplicates, 2);
    assert!(second.failures.is_empty());

    assert_eq!(queue::take_queue_files(&paths), vec![target]);
}

#[test]
fn duplicate_queue_entries_are_collapsed_on_take() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    fs::create_dir_all(&paths.queue_dir).unwrap();
    fs::write(paths.queue_dir.join("first.path"), r"C:\demo.zst").unwrap();
    fs::write(paths.queue_dir.join("second.path"), r"C:\demo.zst").unwrap();

    assert_eq!(
        queue::take_queue_files(&paths),
        vec![PathBuf::from(r"C:\demo.zst")]
    );
}

#[test]
fn duplicate_queue_entries_are_collapsed_across_poll_ticks() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    let target = directory.path().join("demo.zst");
    fs::write(&target, b"x").unwrap();

    let collector_paths = paths.clone();
    let handle = thread::spawn(move || {
        queue::collect_queued_files_with_timing(
            &collector_paths,
            Duration::from_millis(250),
            Duration::from_millis(500),
            Duration::from_secs(5),
        )
    });

    let first = queue::enqueue_paths(&paths, &[target.clone().into_os_string()]).unwrap();
    assert_eq!(first.queued, 1);

    // Wait long enough for collect_queued_files_with_timing to take the first batch
    // and remove the .path file from disk
    thread::sleep(Duration::from_millis(100));

    // Enqueue the exact same file in a subsequent tick while the idle window is active
    let second = queue::enqueue_paths(&paths, &[target.clone().into_os_string()]).unwrap();
    assert_eq!(second.queued, 1);

    let collected = handle.join().unwrap();
    assert_eq!(collected, vec![target]);
}

#[test]
fn enqueue_reports_invalid_paths() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());

    let outcome = queue::enqueue_paths(&paths, &[OsString::from("   ")]).unwrap();

    assert_eq!(outcome.queued, 0);
    assert_eq!(outcome.duplicates, 0);
    assert_eq!(outcome.failures.len(), 1);
    assert!(outcome.failures[0].contains("invalid path"));
}

#[test]
fn queue_preserves_unpaired_surrogates() {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    let units: Vec<u16> = r"C:\demo"
        .encode_utf16()
        .chain([0xD800])
        .chain(".zst".encode_utf16())
        .collect();
    let target = OsString::from_wide(&units);

    let outcome = queue::enqueue_paths(&paths, &[target]).unwrap();
    assert_eq!(outcome.queued, 1);

    let taken = queue::take_queue_files(&paths);
    assert_eq!(taken.len(), 1);
    assert_eq!(
        taken[0].as_os_str().encode_wide().collect::<Vec<_>>(),
        units
    );
}

#[test]
fn install_requires_both_binaries() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());

    assert!(registry::ensure_install_binaries(&paths).is_err());

    fs::write(paths.quiet_exe(), b"").unwrap();
    assert!(registry::ensure_install_binaries(&paths).is_err());

    fs::write(paths.console_exe(), b"").unwrap();
    assert!(registry::ensure_install_binaries(&paths).is_ok());
}

#[test]
fn worker_spawn_failure_is_reported() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    let target = directory.path().join("demo.zst");
    fs::write(&target, b"x").unwrap();

    match queue::handle_context_menu(&paths, &[target.into_os_string()]) {
        Err(queue::ContextMenuError::Failure(message)) => {
            assert!(message.contains("could not start the worker"), "{message}");
            assert!(
                message.contains("faceit-dem.exe was not found"),
                "{message}"
            );
        }
        _ => panic!("expected a worker spawn failure"),
    }
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

    let idle = Duration::from_millis(250);
    let start = Instant::now();
    let collected = queue::collect_queued_files_with_timing(
        &paths,
        idle,
        Duration::from_secs(1),
        Duration::from_secs(5),
    );
    let elapsed = start.elapsed();

    assert_eq!(collected.len(), 1);
    assert!(elapsed >= idle);

    let start = Instant::now();
    let empty = queue::collect_queued_files_with_timing(&paths, idle, idle, Duration::from_secs(5));
    assert!(empty.is_empty());
    assert!(start.elapsed() >= idle);
}

#[test]
fn worker_lock_is_exclusive_and_released_on_drop() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());

    let lock = queue::try_acquire_worker_lock(&paths)
        .unwrap()
        .expect("first lock");
    assert!(!queue::worker_lock_is_free(&paths).unwrap());
    assert!(queue::try_acquire_worker_lock(&paths).unwrap().is_none());

    drop(lock);
    assert!(queue::worker_lock_is_free(&paths).unwrap());
}

#[test]
fn spawn_lock_allows_only_one_spawner() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());

    let gate = queue::try_acquire_spawn_lock(&paths)
        .unwrap()
        .expect("spawn gate");
    assert!(queue::try_acquire_spawn_lock(&paths).unwrap().is_none());

    drop(gate);
    let gate = queue::try_acquire_spawn_lock(&paths).unwrap();
    assert!(gate.is_some());
}

#[test]
fn retirement_rescan_keeps_late_files() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    let target = directory.path().join("late.zst");
    fs::write(&target, b"x").unwrap();

    let lock = queue::try_acquire_worker_lock(&paths)
        .unwrap()
        .expect("worker lock");
    queue::enqueue_paths(&paths, &[target.into_os_string()]).unwrap();
    drop(lock);

    assert!(queue::has_queued_files(&paths));
    let replacement = queue::try_acquire_worker_lock(&paths).unwrap();
    assert!(replacement.is_some());
}

#[test]
fn worker_breaks_cleanly_when_handoff_lock_is_lost() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    let csgo = directory.path().join("csgo");
    fs::create_dir_all(&csgo).unwrap();
    fs::write(
        &paths.config,
        serde_json::json!({
            "target_filename": "1.dem",
            "keep_zst": true,
            "move_to_recycle_bin": false,
            "csgo_path": "csgo"
        })
        .to_string(),
    )
    .unwrap();

    let input = directory.path().join("demo.dem.zst");
    let payload = demo_payload(1024);
    write_archive(&input, &payload, true);
    queue::enqueue_paths(&paths, &[input.into_os_string()]).unwrap();

    let worker_paths = paths.clone();
    let worker = thread::spawn(move || {
        queue::run_worker(&worker_paths)
    });

    let output = csgo.join("1.dem");
    assert!(
        wait_until(|| output.is_file(), Duration::from_secs(5)),
        "the demo was not extracted"
    );

    assert!(
        wait_until(
            || queue::worker_lock_is_free(&paths).unwrap_or(false),
            Duration::from_secs(5)
        ),
        "the worker lock did not become free"
    );

    let late = directory.path().join("late.dem.zst");
    fs::write(&late, b"x").unwrap();
    queue::enqueue_paths(&paths, &[late.into_os_string()]).unwrap();

    let rival_lock = queue::try_acquire_worker_lock(&paths)
        .unwrap()
        .expect("rival lock acquisition");

    let exit_code = worker.join().unwrap();
    assert_eq!(exit_code, 0);

    drop(rival_lock);
}

#[test]
fn worker_pid_round_trips() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    fs::create_dir_all(&paths.queue_dir).unwrap();

    queue::write_worker_pid(&paths);
    assert_eq!(queue::read_worker_pid(&paths), Some(std::process::id()));

    queue::write_worker_pid(&paths);
    assert_eq!(queue::read_worker_pid(&paths), Some(std::process::id()));

    queue::cleanup_worker_pid(&paths);
    assert_eq!(queue::read_worker_pid(&paths), None);
}

#[test]
fn worker_state_reports_held_lock_and_pid() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());

    let lock = queue::try_acquire_worker_lock(&paths).unwrap().unwrap();
    queue::write_worker_pid(&paths);

    assert_eq!(
        queue::worker_state(&paths).unwrap(),
        (true, Some(std::process::id()))
    );

    drop(lock);
    assert_eq!(queue::worker_state(&paths).unwrap(), (false, None));
    queue::cleanup_worker_pid(&paths);
}

#[test]
fn worker_lock_errors_are_reported() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    fs::create_dir_all(paths.queue_dir.join("worker.lock")).unwrap();

    assert!(queue::try_acquire_worker_lock(&paths).is_err());
    assert!(queue::worker_lock_is_free(&paths).is_err());
    assert!(queue::worker_state(&paths).is_err());

    let target = directory.path().join("demo.zst");
    fs::write(&target, b"x").unwrap();

    match queue::handle_context_menu(&paths, &[target.into_os_string()]) {
        Err(queue::ContextMenuError::Failure(message)) => {
            assert!(message.contains("worker lock"), "{message}");
        }
        _ => panic!("expected a worker lock failure"),
    }
}

#[test]
fn help_prints_usage() {
    let directory = tempfile::tempdir().unwrap();

    cli(directory.path())
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage"))
        .stdout(predicate::str::contains("faceit-dem --status"))
        .stdout(predicate::str::contains("faceit-dem --test <archive>"))
        .stdout(predicate::str::contains("\u{1b}[").not());
}

#[test]
fn test_command_verifies_checksum() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("demo.dem.zst");
    write_archive(&input, &demo_payload(4096), true);

    cli(directory.path())
        .arg("--test")
        .arg(&input)
        .assert()
        .success()
        .stdout(predicate::str::contains("XXH64 verified"))
        .stdout(predicate::str::contains("passed"))
        .stdout(predicate::str::contains("No files were written"));

    assert!(input.exists());
}

#[test]
fn test_command_rejects_corrupted_archive() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("corrupt.dem.zst");
    write_archive(&input, &demo_payload(4096), true);

    let mut bytes = fs::read(&input).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    fs::write(&input, &bytes).unwrap();

    cli(directory.path())
        .arg("--test")
        .arg(&input)
        .assert()
        .code(1)
        .stdout(predicate::str::contains("Result"))
        .stdout(predicate::str::contains("failed"))
        .stderr(predicate::str::contains("checksum"));

    assert!(input.exists());
}

#[test]
fn test_command_requires_one_archive() {
    let directory = tempfile::tempdir().unwrap();

    cli(directory.path())
        .arg("--test")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--test expects one archive path"));
}

#[test]
fn unknown_option_exits_two() {
    let directory = tempfile::tempdir().unwrap();

    cli(directory.path())
        .arg("--definitely-not-an-option")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unknown option"));
}

#[test]
fn flag_commands_reject_trailing_arguments() {
    let directory = tempfile::tempdir().unwrap();

    for flag in [
        "--status",
        "--install",
        "--remove",
        "--update-label",
        "--queue-worker",
    ] {
        cli(directory.path())
            .arg(flag)
            .arg("unexpected_arg")
            .assert()
            .code(2)
            .stderr(predicate::str::contains(format!(
                "{flag} does not take additional arguments"
            )));
    }
}

#[test]
fn dash_dash_without_files_exits_two() {
    let directory = tempfile::tempdir().unwrap();

    cli(directory.path())
        .arg("--")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no input files after --"));
}

#[test]
fn status_prints_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    fs::create_dir_all(directory.path().join("csgo")).unwrap();
    fs::write(
        &paths.config,
        serde_json::json!({
            "target_filename": "1.dem",
            "keep_zst": true,
            "move_to_recycle_bin": false,
            "csgo_path": "csgo"
        })
        .to_string(),
    )
    .unwrap();

    cli(directory.path())
        .arg("--status")
        .assert()
        .success()
        .stdout(predicate::str::contains("FACEIT Demo Extractor"))
        .stdout(predicate::str::contains("Runtime"))
        .stdout(predicate::str::contains("Target demo"))
        .stdout(predicate::str::contains("Right-click menu"))
        .stdout(predicate::str::contains("\u{1b}[").not());
}

#[test]
fn context_menu_quiet_worker_end_to_end() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    let csgo = directory.path().join("csgo");
    fs::create_dir_all(&csgo).unwrap();
    fs::write(
        &paths.config,
        serde_json::json!({
            "target_filename": "1.dem",
            "keep_zst": true,
            "move_to_recycle_bin": false,
            "csgo_path": "csgo"
        })
        .to_string(),
    )
    .unwrap();
    fs::copy(env!("CARGO_BIN_EXE_faceit-dem"), paths.console_exe()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_faceit-dem-quiet"), paths.quiet_exe()).unwrap();

    let input = directory.path().join("demo.dem.zst");
    let payload = demo_payload(4096);
    write_archive(&input, &payload, true);

    let mut child = ProcessCommand::new(paths.quiet_exe())
        .env(HOME_ENV, directory.path())
        .env("FACEIT_DEMO_EXTRACTOR_TEST_NO_PAUSE", "1")
        .arg("--context-menu")
        .arg(&input)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the quiet app did not finish");
        }
        sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "the quiet app exited with {status}");

    let output = csgo.join("1.dem");
    assert!(
        wait_until(|| output.is_file(), Duration::from_secs(10)),
        "the queued demo was not extracted"
    );
    assert_eq!(fs::read(&output).unwrap(), payload);
    assert!(input.exists());
    assert!(
        wait_until(
            || queue::worker_lock_is_free(&paths).unwrap_or(false),
            Duration::from_secs(10)
        ),
        "the worker did not finish"
    );
}

#[test]
fn menu_renders_and_exits_cleanly() {
    let directory = tempfile::tempdir().unwrap();

    cli(directory.path())
        .write_stdin("0\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Change demo filename"))
        .stdout(predicate::str::contains("Keep original archives"))
        .stdout(predicate::str::contains("Use the Recycle Bin"))
        .stdout(predicate::str::contains("Select an option [0-5]"))
        .stdout(predicate::str::contains("Goodbye."))
        .stdout(predicate::str::contains("\u{1b}[").not());
}

#[test]
fn missing_file_exits_one() {
    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    fs::create_dir_all(directory.path().join("csgo")).unwrap();
    fs::write(
        &paths.config,
        serde_json::json!({
            "target_filename": "1.dem",
            "keep_zst": true,
            "move_to_recycle_bin": false,
            "csgo_path": "csgo"
        })
        .to_string(),
    )
    .unwrap();
    let missing = directory.path().join("definitely-missing-demo.zst");

    cli(directory.path())
        .arg(&missing)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("input file not found"))
        .stderr(predicate::str::contains("definitely-missing-demo.zst"))
        .stderr(predicate::str::contains("\u{1b}[").not());
}

const REGISTRY_TEST_KEY: &str = r"Software\FaceitDemoExtractorTestsContextMenu";
const REGISTRY_LABEL_TEST_KEY: &str = r"Software\FaceitDemoExtractorTestsUpdateLabel";
const REGISTRY_STATE_TEST_KEY: &str = r"Software\FaceitDemoExtractorTestsState";
const REGISTRY_STATE_COMMAND_TEST_KEY: &str = r"Software\FaceitDemoExtractorTestsState\command";
const REGISTRY_PARTIAL_ZST_KEY: &str = r"Software\FaceitDemoExtractorTestsPartialZst";
const REGISTRY_PARTIAL_BZ2_KEY: &str = r"Software\FaceitDemoExtractorTestsPartialBz2";

struct RegistryTestKey(&'static str);

impl Drop for RegistryTestKey {
    fn drop(&mut self) {
        let _ = registry::remove_context_menu_at(self.0);
    }
}

#[test]
fn registry_install_and_remove_test_subkey() {
    let _guard = RegistryTestKey(REGISTRY_TEST_KEY);
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let _ = hkcu.delete_subkey_all(REGISTRY_TEST_KEY);

    registry::install_context_menu_at(
        REGISTRY_TEST_KEY,
        "Test label",
        r#""C:\test\faceit-dem-quiet.exe" --context-menu "%1""#,
    )
    .unwrap();

    let key = hkcu.open_subkey(REGISTRY_TEST_KEY).unwrap();
    let label: String = key.get_value("").unwrap();
    assert_eq!(label, "Test label");

    let command_key = hkcu
        .open_subkey(format!("{REGISTRY_TEST_KEY}\\command"))
        .unwrap();
    let command: String = command_key.get_value("").unwrap();
    assert!(command.contains("--context-menu"));

    registry::remove_context_menu_at(REGISTRY_TEST_KEY).unwrap();
    assert!(hkcu.open_subkey(REGISTRY_TEST_KEY).is_err());
}

#[test]
fn update_label_uses_the_current_target_filename() {
    let _guard = RegistryTestKey(REGISTRY_LABEL_TEST_KEY);
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let _ = hkcu.delete_subkey_all(REGISTRY_LABEL_TEST_KEY);

    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    fs::write(
        &paths.config,
        serde_json::json!({
            "target_filename": "demo.dem",
            "keep_zst": true,
            "move_to_recycle_bin": false,
            "csgo_path": null
        })
        .to_string(),
    )
    .unwrap();

    registry::install_context_menu_at(REGISTRY_LABEL_TEST_KEY, "Old label", "old command").unwrap();
    registry::perform_update_label_at(&paths, REGISTRY_LABEL_TEST_KEY).unwrap();

    let key = hkcu.open_subkey(REGISTRY_LABEL_TEST_KEY).unwrap();
    let label: String = key.get_value("").unwrap();
    assert_eq!(label, registry::menu_label_for("demo.dem"));

    registry::remove_context_menu_at(REGISTRY_LABEL_TEST_KEY).unwrap();
    assert!(registry::perform_update_label_at(&paths, REGISTRY_LABEL_TEST_KEY).is_err());
}

#[test]
fn context_menu_state_detects_moved_executable_and_missing_label() {
    let _guard = RegistryTestKey(REGISTRY_STATE_TEST_KEY);
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let _ = hkcu.delete_subkey_all(REGISTRY_STATE_TEST_KEY);

    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());
    let test_menus = &[registry::ContextMenu {
        extension: ".test",
        key: REGISTRY_STATE_TEST_KEY,
        command_key: REGISTRY_STATE_COMMAND_TEST_KEY,
    }];

    assert_eq!(registry::context_menu_state_for(test_menus, &paths), None);

    let expected = registry::build_context_menu_command(&paths);
    registry::install_context_menu_at(REGISTRY_STATE_TEST_KEY, "Test Label", &expected).unwrap();
    assert_eq!(
        registry::context_menu_state_for(test_menus, &paths),
        Some(true)
    );

    let moved_paths = Paths::for_tests(&directory.path().join("moved"));
    assert_eq!(
        registry::context_menu_state_for(test_menus, &moved_paths),
        Some(false)
    );

    let (key, _) = hkcu.create_subkey(REGISTRY_STATE_TEST_KEY).unwrap();
    let _ = key.delete_value("");
    assert_eq!(
        registry::context_menu_state_for(test_menus, &paths),
        Some(false)
    );
}

#[test]
fn status_lines_shows_partially_uninstalled_extensions() {
    let _guard_zst = RegistryTestKey(REGISTRY_PARTIAL_ZST_KEY);
    let _guard_bz2 = RegistryTestKey(REGISTRY_PARTIAL_BZ2_KEY);
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let _ = hkcu.delete_subkey_all(REGISTRY_PARTIAL_ZST_KEY);
    let _ = hkcu.delete_subkey_all(REGISTRY_PARTIAL_BZ2_KEY);

    let directory = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(directory.path());

    let test_menus = &[
        registry::ContextMenu {
            extension: ".zst",
            key: REGISTRY_PARTIAL_ZST_KEY,
            command_key: r"Software\FaceitDemoExtractorTestsPartialZst\command",
        },
        registry::ContextMenu {
            extension: ".bz2",
            key: REGISTRY_PARTIAL_BZ2_KEY,
            command_key: r"Software\FaceitDemoExtractorTestsPartialBz2\command",
        },
    ];

    let expected = registry::build_context_menu_command(&paths);
    registry::install_context_menu_at(REGISTRY_PARTIAL_ZST_KEY, "Extract .zst", &expected).unwrap();

    let output = faceit_demo_extractor::cli::status_lines_for(&paths, test_menus).join("\n");
    assert!(output.contains("needs repair"), "{output}");
    assert!(output.contains("Menu .zst"), "{output}");
    assert!(output.contains("Extract .zst"), "{output}");
    assert!(output.contains("Menu .bz2"), "{output}");
    assert!(output.contains("not installed"), "{output}");
}

#[test]
fn recycle_bin_moves_temp_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("recycle-me.txt");
    fs::write(&path, b"x").unwrap();

    extract::delete_original(&path, true).unwrap();
    assert!(!path.exists());
}

#[test]
fn recycle_bin_nonexistent_file_succeeds() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("does-not-exist.txt");
    assert!(extract::delete_original(&path, true).is_ok());
}

#[test]
fn extract_bz2_success_and_deletes_original() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("demo.dem.bz2");
    let output = directory.path().join("1.dem");
    let payload = demo_payload(65536);
    write_bzip2_archive(&input, &payload);

    let result = extract::extract_one(&input, &output, false, false);

    assert_eq!(result, ExtractResult::Ok);
    assert_eq!(fs::read(&output).unwrap(), payload);
    assert!(!input.exists());
    assert!(no_partial_files(directory.path()));
}

#[test]
fn extract_bz2_truncated_archive_fails_cleanly() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("truncated.dem.bz2");
    let output = directory.path().join("1.dem");
    write_bzip2_archive(&input, &demo_payload(65536));

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
fn extract_bz2_checksum_mismatch_fails_cleanly() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("corrupt.dem.bz2");
    let output = directory.path().join("1.dem");
    write_bzip2_archive(&input, &demo_payload(65536));

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
fn extract_bz2_with_wrong_payload_keeps_target_and_archive() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("wrong.dem.bz2");
    let output = directory.path().join("1.dem");
    write_bzip2_archive(&input, b"NOTADEMO and more bytes");
    fs::write(&output, b"existing demo").unwrap();

    let result = extract::extract_one(&input, &output, false, false);

    assert_eq!(result, ExtractResult::Failed);
    assert_eq!(fs::read(&output).unwrap(), b"existing demo");
    assert!(input.exists());
    assert!(no_partial_files(directory.path()));
}

#[test]
fn extract_bz2_multistream_decodes_all_streams() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("multi.dem.bz2");
    let output = directory.path().join("1.dem");

    let first = demo_payload(2048);
    let second = demo_payload(4096);
    let mut bytes = bzip2_archive_bytes(&first);
    bytes.extend(bzip2_archive_bytes(&second));
    fs::write(&input, bytes).unwrap();

    let result = extract::extract_one(&input, &output, false, false);

    assert_eq!(result, ExtractResult::Ok);
    assert_eq!(fs::read(&output).unwrap(), [first, second].concat());
    assert!(!input.exists());
    assert!(no_partial_files(directory.path()));
}

#[test]
fn extract_rejects_a_fake_bz2_before_writing() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("plain.bz2");
    let output = directory.path().join("1.dem");
    fs::write(&input, b"BZh9not-a-real-bzip2-stream").unwrap();
    fs::write(&output, b"existing demo").unwrap();

    let result = extract::extract_one(&input, &output, false, false);

    assert_eq!(result, ExtractResult::Failed);
    assert_eq!(fs::read(&output).unwrap(), b"existing demo");
    assert!(input.exists());
}

#[test]
fn batch_mode_extracts_mixed_formats() {
    let directory = tempfile::tempdir().unwrap();
    let inputs_dir = directory.path().join("in");
    fs::create_dir(&inputs_dir).unwrap();

    let zst = inputs_dir.join("demo0.zst");
    let bz2 = inputs_dir.join("demo1.bz2");
    write_archive(&zst, &demo_payload(256), true);
    write_bzip2_archive(&bz2, &demo_payload(512));

    let code = extract::run_extract_batch(
        &[zst, bz2],
        directory.path(),
        "test",
        &Config::defaults(),
        false,
    );

    assert_eq!(code, 0);
    assert!(directory.path().join("1_1.dem").exists());
    assert!(directory.path().join("1_2.dem").exists());
    assert!(!inputs_dir.join("demo0.zst").exists());
    assert!(!inputs_dir.join("demo1.bz2").exists());
}

#[test]
fn test_command_verifies_bz2() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("demo.dem.bz2");
    write_bzip2_archive(&input, &demo_payload(4096));

    cli(directory.path())
        .arg("--test")
        .arg(&input)
        .assert()
        .success()
        .stdout(predicate::str::contains("CRC32 verified"))
        .stdout(predicate::str::contains("passed"))
        .stdout(predicate::str::contains("No files were written"));

    assert!(input.exists());
}
