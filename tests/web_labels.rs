//! The web page labels agents only with broker-resolved names (AGB-57/AGB-60). `tests/web/labels.js`
//! exercises the page's label helpers with payloads whose IDs must never appear as labels.
use std::process::Command;

#[test]
fn web_labels_never_show_an_id_where_a_name_exists() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/web/labels.js");
    let output = match Command::new("node").arg(script).output() {
        Ok(output) => output,
        // Rust-only checkouts need no Node.js (see .claude/build-test.md); CI runners provide it.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipped: node is not installed");
            return;
        }
        Err(error) => panic!("failed to run node: {error}"),
    };
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
