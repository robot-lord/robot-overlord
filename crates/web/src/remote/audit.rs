//! The remote-access audit log (`<data dir>/remote.log`, 0600, rotated at
//! 1 MiB to `remote.log.1`): one line per pairing, confirmation,
//! revocation, link and remote op. Time, device id, what happened; never
//! an op's arguments or anything a device sent.

use std::io::Write;
use std::path::PathBuf;

use rlord_core::private;

const ROTATE_AT: u64 = 1 << 20;

pub struct Audit {
    path: PathBuf,
}

impl Audit {
    pub fn new(path: PathBuf) -> Audit {
        Audit { path }
    }

    /// One line: `<unix seconds> <device or -> <event>`. Failures to write
    /// are reported on stderr (the plugin's log) and otherwise ignored.
    pub fn log(&self, device: &str, event: &str) {
        if let Err(e) = self.write(device, event) {
            eprintln!("rlord-web: audit log {}: {e}", self.path.display());
        }
    }

    fn write(&self, device: &str, event: &str) -> std::io::Result<()> {
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() >= ROTATE_AT) {
            let mut old = self.path.clone().into_os_string();
            old.push(".1");
            std::fs::rename(&self.path, old)?;
        }
        if let Some(dir) = self.path.parent() {
            private::dir(dir)?;
        }
        let mut f = private::append(&self.path)?;
        let clean = |s: &str| s.replace(['\n', '\r'], " ");
        let device = if device.is_empty() { "-" } else { device };
        writeln!(f, "{} {} {}", super::now(), clean(device), clean(event))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_and_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("remote.log");
        let a = Audit::new(path.clone());
        a.log("abcd", "paired\nname");
        a.log("", "pairing opened");
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with(" abcd paired name"));
        assert!(lines[1].ends_with(" - pairing opened"));
        std::fs::write(&path, vec![b'x'; ROTATE_AT as usize]).unwrap();
        a.log("abcd", "op snapshot");
        assert!(tmp.path().join("remote.log.1").exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
    }
}
