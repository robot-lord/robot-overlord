//! Running a command for what it prints, with a time limit.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// Run `cmd` to its end, at most `limit`: its exit status and what it
/// printed. `stdin`: text written to it (from its own thread, so a command
/// that stalls can't hold the caller past `limit`). The caller sets up
/// everything else (arguments, folder, environment, stderr).
///
/// Its output goes to a file in `dir`, not a pipe: a command that starts a
/// background process (OpenCode's server) would hand it the pipe, and
/// reading to its end would then wait on that process. The file is removed
/// afterwards. Past `limit` the command is killed and this is an error.
pub fn run_to_file(
    mut cmd: Command,
    dir: &Path,
    limit: Duration,
    stdin: Option<&str>,
) -> Result<(ExitStatus, String)> {
    std::fs::create_dir_all(dir)?;
    let printed = unique_file(dir, "out");
    let out = std::fs::File::create(&printed).context("a file for the command's output")?;
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(out);
    let waited = cmd
        .spawn()
        .map_err(anyhow::Error::from)
        .and_then(|mut child| {
            if let (Some(text), Some(mut w)) = (stdin, child.stdin.take()) {
                let text = text.to_string();
                std::thread::spawn(move || {
                    let _ = w.write_all(text.as_bytes());
                });
            }
            let start = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(st)) => break Ok(st),
                    Ok(None) if start.elapsed() < limit => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        bail!("took too long");
                    }
                }
            }
        });
    let text = std::fs::read(&printed)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let _ = std::fs::remove_file(&printed);
    Ok((waited?, text))
}

/// A file name in `dir` no other run uses: `<prefix>-<pid>-<uuid>.txt`.
pub fn unique_file(dir: &Path, prefix: &str) -> PathBuf {
    dir.join(format!(
        "{prefix}-{}-{}.txt",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ))
}

/// Remove files `unique_file` named in `dir` (`out-…`, `answer-…`) that a
/// process that ended mid-run left behind. Only for a folder nothing is
/// running in yet.
pub fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if (name.starts_with("out-") || name.starts_with("answer-")) && name.ends_with(".txt") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_comes_from_a_file_and_a_slow_command_is_cut_off() {
        let dir = tempfile::tempdir().unwrap();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "cat; echo done"]);
        let (st, out) = run_to_file(cmd, dir.path(), Duration::from_secs(5), Some("in ")).unwrap();
        assert!(st.success());
        assert_eq!(out, "in done\n");
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 5"]);
        let e = run_to_file(cmd, dir.path(), Duration::from_millis(200), None).unwrap_err();
        assert!(e.to_string().contains("too long"), "{e}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        // Left behind by a run that never finished: swept.
        std::fs::write(dir.path().join("out-1-x.txt"), "").unwrap();
        std::fs::write(dir.path().join("keep.txt"), "").unwrap();
        sweep(dir.path());
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, ["keep.txt"]);
    }
}
