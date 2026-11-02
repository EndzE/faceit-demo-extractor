use std::sync::atomic::Ordering;

use faceit_demo_extractor::config::Config;
use faceit_demo_extractor::{CANCEL, extract};

struct CancelReset;

impl Drop for CancelReset {
    fn drop(&mut self) {
        CANCEL.store(false, Ordering::Relaxed);
    }
}

#[test]
fn cancelled_batch_exits_130() {
    let directory = tempfile::tempdir().unwrap();
    let _reset = CancelReset;
    let input = directory.path().join("demo.dem.zst");

    CANCEL.store(true, Ordering::Relaxed);

    let code = extract::run_extract_batch(
        &[input],
        directory.path(),
        "test",
        &Config::defaults(),
        false,
    );

    assert_eq!(code, 130);
}

#[test]
fn cleanup_required_is_false_after_batch() {
    use faceit_demo_extractor::CLEANUP_REQUIRED;

    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("demo.dem.zst");

    assert!(!CLEANUP_REQUIRED.load(Ordering::Relaxed));
    let _outcome = extract::run_extract_batch_outcome(
        &[input],
        directory.path(),
        "test",
        &Config::defaults(),
        false,
    );
    assert!(!CLEANUP_REQUIRED.load(Ordering::Relaxed));
}
