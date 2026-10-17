use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use base64::Engine;
use dbx_plugin_sdk::{PluginEmitter, PluginError, PluginHandler, PluginMetadata, PluginServer, RequestContext};
use serde_json::{json, Value};

// ponytail: log keeps the last ~512 KiB; a verbose restore of a huge dump would otherwise grow without bound.
const MAX_LOG: usize = 512 * 1024;

#[derive(Default)]
struct Job {
    running: bool,
    log: String,
    /// Bytes dropped from the front of `log`, so UI offsets stay absolute.
    dropped: usize,
    exit_code: Option<i32>,
    child: Option<Child>,
}

impl Job {
    fn push(&mut self, line: &str) {
        self.log.push_str(line);
        self.log.push('\n');
        if self.log.len() > MAX_LOG {
            // Cut at a line boundary ('\n' is ASCII, so this is always a char boundary).
            let from = self.log.len() - MAX_LOG;
            let cut = self.log.as_bytes()[from..].iter().position(|&b| b == b'\n').map_or(self.log.len(), |i| from + i + 1);
            self.log.drain(..cut);
            self.dropped += cut;
        }
    }
}

#[derive(Default)]
struct Plugin {
    job: Arc<Mutex<Job>>,
    // ponytail: one upload at a time, matching the single restore job.
    upload: Mutex<Option<(PathBuf, File)>>,
}

/// Files picked in the UI are streamed here, restored, then deleted.
fn upload_dir() -> PathBuf {
    std::env::temp_dir().join("dbx-pg-restore")
}

#[derive(Debug, PartialEq)]
enum Format {
    Archive, // custom (PGDMP), tar, or directory dump -> pg_restore
    Plain,   // plain SQL -> psql
}

fn detect_format(path: &Path) -> std::io::Result<Format> {
    if path.is_dir() {
        return Ok(Format::Archive);
    }
    let mut head = Vec::with_capacity(512);
    File::open(path)?.take(512).read_to_end(&mut head)?;
    let custom = head.starts_with(b"PGDMP");
    let tar = head.len() >= 262 && &head[257..262] == b"ustar";
    Ok(if custom || tar { Format::Archive } else { Format::Plain })
}

fn str_param<'a>(params: &'a Value, key: &str) -> &'a str {
    params.get(key).and_then(Value::as_str).map(str::trim).unwrap_or("")
}

fn bool_param(params: &Value, key: &str) -> bool {
    params.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn lock(job: &Arc<Mutex<Job>>) -> MutexGuard<'_, Job> {
    job.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn tool(bin_dir: &str, name: &str) -> PathBuf {
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    if bin_dir.is_empty() { PathBuf::from(exe) } else { Path::new(bin_dir).join(exe) }
}

fn base_command(params: &Value, name: &str) -> Command {
    let mut cmd = Command::new(tool(str_param(params, "binDir"), name));
    cmd.arg("-h").arg(str_param(params, "host"))
        .arg("-p").arg(str_param(params, "port"))
        .arg("-U").arg(str_param(params, "user"))
        .arg("--no-password")
        // Password goes through the environment, never argv or the log.
        .env("PGPASSWORD", params.get("password").and_then(Value::as_str).unwrap_or(""))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn build_restore(params: &Value, format: &Format, file: &str) -> Command {
    let database = str_param(params, "database");
    match format {
        Format::Archive => {
            let mut cmd = base_command(params, "pg_restore");
            cmd.arg("-d").arg(database).arg("--verbose");
            if bool_param(params, "clean") {
                cmd.arg("--clean").arg("--if-exists");
            }
            if bool_param(params, "noOwner") {
                cmd.arg("--no-owner").arg("--no-privileges");
            }
            if let Some(jobs) = params.get("jobs").and_then(Value::as_u64).filter(|j| *j > 1) {
                cmd.arg("-j").arg(jobs.to_string());
            }
            cmd.arg(file);
            cmd
        }
        Format::Plain => {
            let mut cmd = base_command(params, "psql");
            cmd.arg("-d").arg(database).arg("-v").arg("ON_ERROR_STOP=0").arg("-f").arg(file);
            cmd
        }
    }
}

/// Drops every user schema (including `public`, cascading to all objects) and recreates an empty `public`.
const RESET_SQL: &str = "DO $$ DECLARE s text; BEGIN \
FOR s IN SELECT nspname FROM pg_namespace WHERE nspname <> 'information_schema' AND nspname NOT LIKE 'pg\\_%' \
LOOP EXECUTE format('DROP SCHEMA %I CASCADE', s); END LOOP; END $$; \
CREATE SCHEMA public; GRANT USAGE ON SCHEMA public TO PUBLIC;";

fn build_reset(params: &Value) -> Command {
    let mut cmd = base_command(params, "psql");
    cmd.arg("-d").arg(str_param(params, "database")).arg("-v").arg("ON_ERROR_STOP=1").arg("-c").arg(RESET_SQL);
    cmd
}

fn describe(cmd: &Command) -> String {
    let mut parts = vec![cmd.get_program().to_string_lossy().into_owned()];
    parts.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
    parts.join(" ")
}

fn pipe_lines(job: Arc<Mutex<Job>>, reader: Box<dyn Read + Send>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(reader).split(b'\n') {
            let Ok(line) = line else { break };
            lock(&job).push(String::from_utf8_lossy(&line).trim_end_matches('\r'));
        }
    })
}

/// Runs `cmd` to completion, streaming its output into the job log. Returns the exit code (-1 if killed).
fn run_step(job: &Arc<Mutex<Job>>, mut cmd: Command) -> i32 {
    lock(job).push(&format!("$ {}", describe(&cmd)));
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            lock(job).push(&format!("Failed to start {}: {err}", cmd.get_program().to_string_lossy()));
            return 127;
        }
    };
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(pipe_lines(job.clone(), Box::new(out)));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(pipe_lines(job.clone(), Box::new(err)));
    }
    lock(job).child = Some(child);
    let code = loop {
        {
            let mut guard = lock(job);
            let Some(child) = guard.child.as_mut() else { break -1 };
            match child.try_wait() {
                Ok(Some(status)) => {
                    guard.child = None;
                    break status.code().unwrap_or(-1);
                }
                Ok(None) => {}
                Err(err) => {
                    guard.child = None;
                    guard.push(&format!("Wait failed: {err}"));
                    break -1;
                }
            }
        }
        thread::sleep(Duration::from_millis(200));
    };
    for reader in readers {
        let _ = reader.join();
    }
    code
}

impl Plugin {
    fn start(&self, params: Value) -> Result<Value, PluginError> {
        for key in ["host", "port", "user", "database", "file"] {
            if str_param(&params, key).is_empty() {
                return Err(PluginError::new(-32602, format!("Field '{key}' is required")));
            }
        }
        let file = str_param(&params, "file").to_string();
        let format = detect_format(Path::new(&file))
            .map_err(|err| PluginError::new(-32602, format!("Cannot read {file}: {err}")))?;

        let mut job = lock(&self.job);
        if job.running {
            return Err(PluginError::new(-32000, "A restore is already running"));
        }
        let dropped = job.dropped + job.log.len(); // keep offsets monotonic across runs
        *job = Job { running: true, dropped, ..Job::default() };
        job.push(&format!(
            "Detected format: {}",
            if format == Format::Archive { "pg_dump archive -> pg_restore" } else { "plain SQL -> psql" }
        ));
        drop(job);

        let shared = self.job.clone();
        thread::spawn(move || {
            if bool_param(&params, "createDb") {
                let mut cmd = base_command(&params, "createdb");
                cmd.arg(str_param(&params, "database"));
                if run_step(&shared, cmd) != 0 {
                    lock(&shared).push("createdb failed (database may already exist), continuing...");
                }
            }
            let mut code = 0;
            if bool_param(&params, "resetDb") && lock(&shared).running {
                lock(&shared).push("Resetting database: dropping all schemas, recreating empty public...");
                code = run_step(&shared, build_reset(&params));
                if code != 0 {
                    lock(&shared).push("Reset failed, restore aborted.");
                }
            }
            if code == 0 {
                code = if lock(&shared).running { run_step(&shared, build_restore(&params, &format, &file)) } else { -1 };
            }
            if Path::new(&file).starts_with(upload_dir()) {
                let _ = std::fs::remove_file(&file);
            }
            let mut job = lock(&shared);
            job.push(&format!("Finished with exit code {code}"));
            job.exit_code = Some(code);
            job.running = false;
        });
        Ok(json!({ "started": true }))
    }

    /// `offset` is the absolute byte position the UI already has; only the new tail is returned.
    fn status(&self, params: Value) -> Value {
        let job = lock(&self.job);
        let end = job.dropped + job.log.len();
        let want = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let mut rel = want.saturating_sub(job.dropped).min(job.log.len());
        while !job.log.is_char_boundary(rel) {
            rel += 1;
        }
        json!({
            "running": job.running,
            "exitCode": job.exit_code,
            "log": &job.log[rel..],
            "offset": end,
        })
    }

    fn upload_begin(&self, params: Value) -> Result<Value, PluginError> {
        let io_err = |err: std::io::Error| PluginError::new(-32000, format!("Upload failed: {err}"));
        let name: String = str_param(&params, "name")
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || ".-_".contains(c) { c } else { '_' })
            .collect();
        let dir = upload_dir();
        std::fs::create_dir_all(&dir).map_err(io_err)?;
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
        let path = dir.join(format!("{stamp}-{name}"));
        let file = File::create(&path).map_err(io_err)?;
        let mut upload = self.upload.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((old, _)) = upload.take() {
            let _ = std::fs::remove_file(old);
        }
        *upload = Some((path.clone(), file));
        Ok(json!({ "path": path }))
    }

    fn upload_chunk(&self, params: Value) -> Result<Value, PluginError> {
        let data = base64::engine::general_purpose::STANDARD
            .decode(str_param(&params, "dataBase64"))
            .map_err(|err| PluginError::new(-32602, format!("Bad chunk: {err}")))?;
        let mut upload = self.upload.lock().unwrap_or_else(|p| p.into_inner());
        let (_, file) = upload.as_mut().ok_or_else(|| PluginError::new(-32000, "No upload in progress"))?;
        file.write_all(&data).map_err(|err| PluginError::new(-32000, format!("Upload failed: {err}")))?;
        Ok(json!({ "written": data.len() }))
    }

    fn upload_finish(&self) -> Result<Value, PluginError> {
        let (path, file) = self.upload.lock().unwrap_or_else(|p| p.into_inner()).take()
            .ok_or_else(|| PluginError::new(-32000, "No upload in progress"))?;
        file.sync_all().map_err(|err| PluginError::new(-32000, format!("Upload failed: {err}")))?;
        Ok(json!({ "path": path }))
    }

    fn list_databases(&self, params: Value) -> Result<Value, PluginError> {
        let mut cmd = base_command(&params, "psql");
        cmd.arg("-d").arg("postgres").arg("-Atc")
            .arg("select datname from pg_database where not datistemplate order by 1");
        let out = cmd.output().map_err(|err| PluginError::new(-32000, format!("Failed to start psql: {err}")))?;
        if !out.status.success() {
            return Err(PluginError::new(-32000, String::from_utf8_lossy(&out.stderr).trim().to_string()));
        }
        let names: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.is_empty()).map(String::from).collect();
        Ok(json!({ "databases": names }))
    }

    fn cancel(&self) -> Value {
        let mut job = lock(&self.job);
        let killed = job.child.as_mut().map(|c| c.kill().is_ok()).unwrap_or(false);
        if job.running {
            job.running = false;
            job.push("Cancelled by user");
        }
        json!({ "cancelled": killed })
    }
}

impl PluginHandler for Plugin {
    fn handle(&self, _context: RequestContext, method: &str, params: Value, _emitter: &PluginEmitter) -> Result<Value, PluginError> {
        match method {
            "restore/start" => self.start(params),
            "restore/status" => Ok(self.status(params)),
            "restore/cancel" => Ok(self.cancel()),
            "upload/begin" => self.upload_begin(params),
            "upload/chunk" => self.upload_chunk(params),
            "upload/finish" => self.upload_finish(),
            "db/list" => self.list_databases(params),
            _ => Err(PluginError::method_not_found(method)),
        }
    }
}

fn main() -> std::io::Result<()> {
    let metadata = PluginMetadata::new("marco.pg-restore", env!("CARGO_PKG_VERSION"));
    PluginServer::new(metadata, Plugin::default()).serve()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_formats() {
        let dir = std::env::temp_dir().join(format!("pgr-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let custom = dir.join("c.sql");
        std::fs::write(&custom, b"PGDMP\x01\x10\x00binary").unwrap();
        let plain = dir.join("p.sql");
        std::fs::write(&plain, b"-- PostgreSQL database dump\nSET x = 1;\n").unwrap();
        let mut tar = vec![0u8; 512];
        tar[257..262].copy_from_slice(b"ustar");
        let tar_file = dir.join("t.tar");
        std::fs::write(&tar_file, tar).unwrap();

        assert_eq!(detect_format(&custom).unwrap(), Format::Archive);
        assert_eq!(detect_format(&plain).unwrap(), Format::Plain);
        assert_eq!(detect_format(&tar_file).unwrap(), Format::Archive);
        assert_eq!(detect_format(&dir).unwrap(), Format::Archive);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn password_not_in_command_line() {
        let params = json!({ "host": "h", "port": "5432", "user": "u", "password": "s3cret", "database": "d", "clean": true });
        let line = describe(&build_restore(&params, &Format::Archive, "/x.sql"));
        assert!(!line.contains("s3cret"));
        assert!(line.contains("--clean --if-exists"));
    }

    #[test]
    fn reset_targets_database_and_stops_on_error() {
        let params = json!({ "host": "h", "port": "5432", "user": "u", "password": "p", "database": "target" });
        let line = describe(&build_reset(&params));
        assert!(line.contains("-d target -v ON_ERROR_STOP=1 -c DO $$"));
        assert!(RESET_SQL.contains("NOT LIKE 'pg\\_%'"));
        assert!(RESET_SQL.ends_with("CREATE SCHEMA public; GRANT USAGE ON SCHEMA public TO PUBLIC;"));
    }

    #[test]
    fn upload_roundtrip() {
        let plugin = Plugin::default();
        let path = plugin.upload_begin(json!({ "name": "../evil name.sql" })).unwrap()["path"].as_str().unwrap().to_string();
        assert!(Path::new(&path).starts_with(upload_dir()));
        assert!(path.ends_with(".._evil_name.sql"));
        plugin.upload_chunk(json!({ "dataBase64": "UEdETVA=" })).unwrap(); // "PGDMP"
        plugin.upload_chunk(json!({ "dataBase64": "AQ==" })).unwrap();
        plugin.upload_finish().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"PGDMP\x01");
        assert_eq!(detect_format(Path::new(&path)).unwrap(), Format::Archive);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn log_offsets_survive_truncation() {
        let mut job = Job::default();
        for _ in 0..20_000 {
            job.push("0123456789012345678901234567890123456789");
        }
        assert!(job.log.len() <= MAX_LOG);
        assert_eq!(job.dropped + job.log.len(), 20_000 * 41);
        assert!(job.log.starts_with("0123"));
        for _ in 0..200_000 {
            job.push("é—ü"); // multibyte lines must not panic on truncation
        }
        assert!(job.log.starts_with("é—ü\n"));
    }
}
