//! Bounded, best-effort diagnostic exports. Never include configuration files.
use hbb_common::{config::Config, regex::Regex, ResultType};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime},
};
use zip::{write::FileOptions, ZipWriter};

const FILE_LIMIT: u64 = 2 * 1024 * 1024;
const TOTAL_LIMIT: u64 = 32 * 1024 * 1024;
const MAX_FILES: usize = 64;
static ERROR_LOCK: Mutex<()> = Mutex::new(());

struct Redactor {
    secret: Regex,
    addresses: Regex,
    home: String,
}

impl Redactor {
    fn new() -> ResultType<Self> {
        Ok(Self {
            // Drop the whole line: credentials may be quoted, structured, or contain spaces.
            secret: Regex::new(
                r"(?i)password|passwd|pwd|token|secret|authorization|cookie|username|private[_ -]?key|credential|用户名|密码|口令|密钥",
            )?,
            addresses: Regex::new(
                r"(?i)\b(?:[0-9]{1,3}\.){3}[0-9]{1,3}\b|\b(?:[0-9a-f]{2}[:-]){5}[0-9a-f]{2}\b|(?:[0-9a-f]{0,4}:){2,}[0-9a-f:.%]*|(?i:[a-z]:[\\/]Users[\\/]|/Users/|/home/)[^/\\\s]+",
            )?,
            home: Config::get_home().to_string_lossy().into_owned(),
        })
    }

    fn text(&self, input: &str) -> String {
        input
            .lines()
            .map(|line| {
                if self.secret.is_match(line) {
                    return "[sensitive log line removed]\n".to_owned();
                }
                let line = if self.home.is_empty() {
                    line.to_owned()
                } else {
                    line.replace(&self.home, "<HOME>")
                };
                format!(
                    "{}\n",
                    self.addresses
                        .replace_all(&line, |caps: &hbb_common::regex::Captures<'_>| {
                            let value = &caps[0];
                            // IPv6 candidates also match clock times. Preserve those unless
                            // they parse as an actual address (or a MAC address).
                            if value.matches(':').count() > 1
                                && value.parse::<std::net::Ipv6Addr>().is_err()
                                && !(value.len() == 17 && value.matches(':').count() == 5)
                            {
                                value.to_owned()
                            } else {
                                "<REDACTED>".to_owned()
                            }
                        })
                )
            })
            .collect()
    }
}

fn collect(
    dir: &Path,
    depth: usize,
    files: &mut Vec<(SystemTime, PathBuf)>,
    warnings: &mut Vec<String>,
    budget: &mut usize,
) {
    if depth > 4 || *budget == 0 {
        warnings.push("Log discovery limit reached.".into());
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => {
            warnings.push(
                "A log directory is missing or unreadable (possibly another service account)."
                    .into(),
            );
            return;
        }
    };
    for entry in entries {
        if *budget == 0 {
            warnings.push("Log discovery limit reached.".into());
            break;
        }
        *budget -= 1;
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                warnings.push("A directory entry could not be read.".into());
                continue;
            }
        };
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                warnings.push("Log metadata could not be read.".into());
                continue;
            }
        };
        if metadata.is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect(&path, depth + 1, files, warnings, budget);
        } else if metadata.is_file() && path.extension().and_then(|s| s.to_str()) == Some("log") {
            match metadata.modified() {
                Ok(time)
                    if SystemTime::now().duration_since(time).unwrap_or_default()
                        <= Duration::from_secs(7 * 86400) =>
                {
                    files.push((time, path))
                }
                Ok(_) => (),
                Err(_) => warnings.push("A log with unknown modification time was skipped.".into()),
            }
        }
    }
}

fn build_bundle(root: &Path, output: File, system: &str) -> ResultType<()> {
    let redactor = Redactor::new()?;
    let mut warnings = vec!["Only logs accessible to the current account are included; services under other accounts may be absent.".to_owned()];
    let mut files = Vec::new();
    collect(root, 0, &mut files, &mut warnings, &mut 4096);
    files.sort_by(|a, b| b.0.cmp(&a.0));
    if files.len() > MAX_FILES {
        warnings.push("Only the newest 64 log files were included.".into());
    }
    let mut zip = ZipWriter::new(output);
    let options = FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o600);
    let mut remaining = TOTAL_LIMIT;
    let mut manifest = Vec::new();
    for (_, path) in files.into_iter().take(MAX_FILES) {
        if remaining == 0 {
            warnings.push("Total log size limit reached.".into());
            break;
        }
        let read = (|| -> std::io::Result<(Vec<u8>, bool, u64)> {
            let mut file = File::open(&path)?;
            let len = file.metadata()?.len();
            let take = len.min(FILE_LIMIT).min(remaining);
            file.seek(SeekFrom::Start(len - take))?;
            let mut bytes = Vec::new();
            file.take(take).read_to_end(&mut bytes)?;
            let consumed = bytes.len() as u64;
            // Discard a partial first line so a split credential cannot escape redaction.
            if len > take {
                if let Some(end) = bytes.iter().position(|b| *b == b'\n') {
                    bytes.drain(..=end);
                } else {
                    bytes.clear();
                }
            }
            Ok((bytes, len > take, consumed))
        })();
        let (bytes, truncated, consumed) = match read {
            Ok(value) => value,
            Err(_) => {
                warnings.push("A log file could not be read.".into());
                continue;
            }
        };
        remaining = remaining.saturating_sub(consumed);
        let name = format!("logs/{:03}.log", manifest.len() + 1);
        let content = redactor.text(&String::from_utf8_lossy(&bytes));
        zip.start_file(&name, options)?;
        zip.write_all(content.as_bytes())?;
        let source = path.strip_prefix(root).unwrap_or(&path).to_string_lossy();
        manifest.push(serde_json::json!({"file": name, "source": redactor.text(&source).trim(), "truncated": truncated}));
    }
    if manifest.is_empty() {
        warnings.push("No recent logs found. Debug builds normally log to the console.".into());
    }
    zip.start_file("diagnostics.json", options)?;
    zip.write_all(&serde_json::to_vec_pretty(&serde_json::json!({
        "version": crate::VERSION, "build_date": crate::BUILD_DATE,
        "exported_at": chrono::Utc::now().to_rfc3339(),
        "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
        "system_version": redactor.text(system).trim(),
        "debug_build": cfg!(debug_assertions), "logs": manifest, "warnings": warnings,
    }))?)?;
    zip.start_file("README.txt", options)?;
    zip.write_all(b"SubnetDesk diagnostic bundle\nAttach this ZIP with reproduction steps and the time of the problem.\nContains recent logs (7 days, newest 64 files, up to 2 MiB per file / 32 MiB input).\nConfiguration files are excluded. Common credentials, addresses and home paths are redacted on a best-effort basis. Review before sharing publicly; unstructured logs may still contain personal information.\nSee diagnostics.json for missing or truncated logs.\n")?;
    zip.finish()?.sync_all()?;
    Ok(())
}

pub fn export(request: &str) -> ResultType<()> {
    #[derive(serde::Deserialize)]
    struct Request {
        path: PathBuf,
        system: String,
    }
    let request: Request = serde_json::from_str(request)?;
    let parent = request.path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid destination")
    })?;
    let temporary = parent.join(format!(
        ".subnetdesk-diagnostics-{}.tmp",
        uuid::Uuid::new_v4()
    ));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = build_bundle(&Config::log_path(), file, &request.system).and_then(|()| {
        fs::rename(&temporary, &request.path)?;
        Ok(())
    });
    if result.is_err() {
        if let Err(error) = fs::remove_file(&temporary) {
            hbb_common::log::warn!("Failed to remove partial diagnostic bundle: {error}");
        }
    }
    result
}

pub fn record_flutter_error(message: &str) -> ResultType<()> {
    let _guard = ERROR_LOCK.lock().unwrap();
    let dir = Config::log_path().join("flutter-errors");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("errors-{}.log", std::process::id()));
    if path.exists() && fs::metadata(&path)?.len() > FILE_LIMIT {
        let old = dir.join(format!("errors-{}-previous.log", std::process::id()));
        if old.exists() {
            fs::remove_file(&old)?;
        }
        fs::rename(&path, old)?;
    }
    let bounded: String = message.chars().take(32 * 1024).collect();
    let text = Redactor::new()?.text(&bounded);
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(
        file,
        "{} Flutter error\n{}",
        chrono::Utc::now().to_rfc3339(),
        text
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "subnetdesk-diagnostics-test-{}",
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn bundle(&self, root: &Path) -> zip::ZipArchive<File> {
            let path = self.0.join("bundle.zip");
            build_bundle(root, File::create(&path).unwrap(), "test OS").unwrap();
            zip::ZipArchive::new(File::open(path).unwrap()).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn read(zip: &mut zip::ZipArchive<File>, name: &str) -> String {
        let mut text = String::new();
        zip.by_name(name)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        text
    }

    #[test]
    fn redacts_credentials_addresses_and_paths_but_preserves_time() {
        let redactor = Redactor::new().unwrap();
        let text = redactor.text("2026-09-07 12:34:56 connected 192.168.1.2 ::1 2001:db8::1 aa:bb:cc:dd:ee:ff /Users/alice/file\npassword=very secret\nAuthorization: Bearer abc\nfailed: timeout\n");
        for secret in [
            "192.168",
            "::1",
            "db8",
            "aa:bb",
            "alice",
            "very secret",
            "Bearer abc",
        ] {
            assert!(!text.contains(secret), "{text}");
        }
        assert!(text.contains("12:34:56"));
        assert!(text.contains("failed: timeout"));
    }

    #[test]
    fn includes_nested_logs_but_excludes_config_and_old_files() {
        let fixture = Fixture::new();
        let root = fixture.0.join("logs");
        fs::create_dir_all(root.join("service")).unwrap();
        fs::write(
            root.join("service/current.log"),
            "12:34:56 failure 10.0.0.1\n",
        )
        .unwrap();
        fs::write(root.join("config.toml"), "sensitive config").unwrap();
        let old = File::create(root.join("old.log")).unwrap();
        old.set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::now() - Duration::from_secs(8 * 86400)),
        )
        .unwrap();
        let mut zip = fixture.bundle(&root);
        assert_eq!(zip.len(), 3);
        assert!(read(&mut zip, "logs/001.log").contains("failure <REDACTED>"));
        let metadata = read(&mut zip, "diagnostics.json");
        assert!(metadata.contains("service/current.log"));
        assert!(!metadata.contains("old.log"));
    }

    #[test]
    fn missing_logs_still_produce_useful_metadata() {
        let fixture = Fixture::new();
        let mut zip = fixture.bundle(&fixture.0.join("missing"));
        assert_eq!(zip.len(), 2);
        assert!(read(&mut zip, "diagnostics.json").contains("No recent logs found"));
    }

    #[test]
    fn oversized_log_exports_tail_and_reports_truncation() {
        let fixture = Fixture::new();
        let root = fixture.0.join("logs");
        fs::create_dir(&root).unwrap();
        let mut data = vec![b'x'; FILE_LIMIT as usize + 100];
        data.extend_from_slice(b"\nlast failure\n");
        fs::write(root.join("large.log"), data).unwrap();
        let mut zip = fixture.bundle(&root);
        assert_eq!(read(&mut zip, "logs/001.log"), "last failure\n");
        assert!(read(&mut zip, "diagnostics.json").contains("\"truncated\": true"));
    }

    #[test]
    fn caps_file_count_and_explains_omissions() {
        let fixture = Fixture::new();
        let root = fixture.0.join("logs");
        fs::create_dir(&root).unwrap();
        for index in 0..MAX_FILES + 1 {
            fs::write(root.join(format!("{index}.log")), "failure\n").unwrap();
        }
        let mut zip = fixture.bundle(&root);
        assert_eq!(zip.len(), MAX_FILES + 2);
        assert!(read(&mut zip, "diagnostics.json").contains("newest 64"));
    }

    #[test]
    fn counts_discarded_partial_lines_toward_total_input_limit() {
        let fixture = Fixture::new();
        let root = fixture.0.join("logs");
        fs::create_dir(&root).unwrap();
        let data = vec![b'x'; FILE_LIMIT as usize + 1];
        for index in 0..(TOTAL_LIMIT / FILE_LIMIT) + 1 {
            fs::write(root.join(format!("{index}.log")), &data).unwrap();
        }
        let mut zip = fixture.bundle(&root);
        assert_eq!(zip.len(), (TOTAL_LIMIT / FILE_LIMIT) as usize + 2);
        assert!(read(&mut zip, "diagnostics.json").contains("Total log size limit reached"));
    }

    #[cfg(unix)]
    #[test]
    fn ignores_symlinked_files_and_directories() {
        let fixture = Fixture::new();
        let root = fixture.0.join("logs");
        fs::create_dir(&root).unwrap();
        let outside = fixture.0.join("private.log");
        fs::write(&outside, "private data").unwrap();
        std::os::unix::fs::symlink(outside, root.join("linked.log")).unwrap();
        std::os::unix::fs::symlink(&fixture.0, root.join("linked-dir")).unwrap();
        let mut zip = fixture.bundle(&root);
        assert_eq!(zip.len(), 2);
        assert!(read(&mut zip, "diagnostics.json").contains("No recent logs found"));
    }
}
