use std::sync::atomic::Ordering;

use faceit_demo_extractor::config::Config;
use faceit_demo_extractor::{CANCEL, extract};

struct CancelReset;

impl Drop for CancelReset {
    fn drop(&mut self) {
        CANCEL.store(false, Ordering::SeqCst);
    }
}

#[test]
fn cancelled_batch_exits_130() {
    let directory = tempfile::tempdir().unwrap();
    let _reset = CancelReset;
    let input = directory.path().join("demo.dem.zst");

    CANCEL.store(true, Ordering::SeqCst);

    let code = extract::run_extract_batch(
        &[input],
        directory.path(),
        "test",
        &Config::defaults(),
        false,
    );

    assert_eq!(code, 130);
}
