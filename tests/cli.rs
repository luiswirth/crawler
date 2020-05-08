use assert_cmd::prelude::*;
use predicates::prelude::*;
use std::process::Command;

use std::io::Write;
use tempfile::NamedTempFile;

use lw::DynResult;
use lwirth_rs as lw;

#[test]
fn invalid_url() -> DynResult<()> {
    let mut cmd = Command::cargo_bin("crawler")?;
    cmd.arg("not_a_url.123").arg("another.invalid/url");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("Invalid URL"));

    Ok(())
}

fn create_temp_file() -> DynResult<()> {
    let mut file = NamedTempFile::new()?;
    writeln!(file, "some content")?;
    Ok(())
}
