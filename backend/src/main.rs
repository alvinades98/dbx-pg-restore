mod toc;

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

/// Files picked in the UI are streamed here, restored, then deleted; backups are staged here before download.
fn upload_dir() -> PathBuf {
    std::env::temp_dir().join("dbx-pg-restore")
}

/// A fresh file path inside `upload_dir()`; `name` is reduced to a safe single path segment.
fn temp_path(name: &str) -> Result<PathBuf, PluginError> {
    let name: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || ".-_".contains(c) { c } else { '_' }).collect();
    let dir = upload_dir();
    std::fs::create_dir_all(&dir).map_err(|err| PluginError::new(-32000, format!("Cannot create temp dir: {err}")))?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    Ok(dir.join(format!("{stamp}-{name}")))
}

/// Resolves `params.path`, refusing anything outside `upload_dir()`.
fn temp_file(params: &Value) -> Result<PathBuf, PluginError> {
    let denied = || PluginError::new(-32602, "Path is not a plugin temp file");
    let path = std::fs::canonicalize(str_param(params, "path")).map_err(|_| denied())?;
    let dir = std::fs::canonicalize(upload_dir()).map_err(|_| denied())?;
    if path.starts_with(&dir) && path.is_file() { Ok(path) } else { Err(denied()) }
}

fn finish_job(job: &Arc<Mutex<Job>>, code: i32) {
    let mut job = lock(job);
    job.push(&format!("Finished with exit code {code}"));
    job.exit_code = Some(code);
    job.running = false;
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

/// Leading major version of "18.6", "17.2 (Debian ...)" or "9.6.24".
fn major(version: &str) -> Option<u32> {
    version.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// Version reported by `<tool> --version`, e.g. "18.6" from "pg_dump (PostgreSQL) 18.6".
fn tool_version(bin_dir: &str, name: &str) -> Option<String> {
    let out = Command::new(tool(bin_dir, name)).arg("--version").stdin(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    out.status.success().then(|| text.split_whitespace().find(|w| w.starts_with(|c: char| c.is_ascii_digit())).map(String::from))?
}

/// pg_dump pattern matching exactly `name` (quoted, so case and wildcards are taken literally).
fn quote_pattern(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `-Z` values pg_dump accepts: a level 0-9, or gzip/lz4/zstd with an optional ":level".
fn valid_compress(value: &str) -> bool {
    let (method, level) = match value.split_once(':') {
        Some((m, l)) => (m, Some(l)),
        None => (value, None),
    };
    let level_ok = level.map_or(true, |l| (1..=2).contains(&l.len()) && l.chars().all(|c| c.is_ascii_digit()));
    match method {
        "gzip" | "lz4" | "zstd" => level_ok,
        m => level.is_none() && m.len() == 1 && m.chars().all(|c| c.is_ascii_digit()),
    }
}

fn string_list(params: &Value, key: &str) -> Vec<String> {
    params.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(String::from).collect()
}

/// `[[schema, table], ...]` pairs.
fn pair_list(params: &Value, key: &str) -> Vec<(String, String)> {
    params.get(key).and_then(Value::as_array).into_iter().flatten()
        .filter_map(|p| Some((p.get(0)?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string())))
        .collect()
}

/// Creates each missing role as NOLOGIN so owner/ACL statements in the dump find it.
fn roles_sql(roles: &[String]) -> Option<String> {
    let literals: Vec<String> = roles.iter()
        .filter(|r| !r.is_empty() && !r.contains('$') && !r.contains('\0'))
        .map(|r| format!("'{}'", r.replace('\'', "''")))
        .collect();
    (!literals.is_empty()).then(|| format!(
        "DO $pgr$ DECLARE r text; BEGIN FOREACH r IN ARRAY ARRAY[{}]::text[] LOOP \
         IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = r) THEN \
         EXECUTE format('CREATE ROLE %I NOLOGIN', r); RAISE NOTICE 'created role %', r; END IF; END LOOP; END $pgr$;",
        literals.join(", ")
    ))
}

fn read_toc(params: &Value, file: &str) -> Result<toc::Toc, PluginError> {
    let out = Command::new(tool(str_param(params, "binDir"), "pg_restore"))
        .arg("-l").arg(file).stdin(Stdio::null()).output()
        .map_err(|err| PluginError::new(-32000, format!("Failed to start pg_restore: {err}")))?;
    if !out.status.success() {
        return Err(PluginError::new(-32000, String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(toc::parse(&String::from_utf8_lossy(&out.stdout)))
}

/// Header lines of a plain SQL dump: "-- Dumped from database version 14.2" / "-- Dumped by pg_dump version 18.6".
fn plain_header(path: &Path) -> (Option<String>, Option<String>) {
    let mut head = Vec::new();
    if let Ok(file) = File::open(path) {
        let _ = file.take(64 * 1024).read_to_end(&mut head);
    }
    let text = String::from_utf8_lossy(&head);
    let find = |prefix: &str| text.lines().find_map(|l| l.strip_prefix(prefix)).map(|v| v.trim().to_string());
    (find("-- Dumped by pg_dump version "), find("-- Dumped from database version "))
}

fn query_lines(params: &Value, database: &str, sql: &str) -> Result<Vec<String>, PluginError> {
    let mut cmd = base_command(params, "psql");
    cmd.arg("-d").arg(database).arg("-AtX").arg("-F").arg("\t").arg("-c").arg(sql);
    let out = cmd.output().map_err(|err| PluginError::new(-32000, format!("Failed to start psql: {err}")))?;
    if !out.status.success() {
        return Err(PluginError::new(-32000, String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.is_empty()).map(String::from).collect())
}

/// Removes temp files a crashed or closed session left behind.
fn cleanup_temp_dir() {
    let day = Duration::from_secs(24 * 3600);
    for entry in std::fs::read_dir(upload_dir()).into_iter().flatten().flatten() {
        let old = entry.metadata().and_then(|m| m.modified()).map(|t| t.elapsed().unwrap_or_default() > day).unwrap_or(false);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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

fn build_restore(params: &Value, format: &Format, file: &str, list: Option<&Path>) -> Command {
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
            if let Some(list) = list {
                cmd.arg("-L").arg(list);
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

fn build_backup(params: &Value, output: &Path) -> Command {
    if str_param(params, "content") == "roles" {
        let mut cmd = base_command(params, "pg_dumpall");
        cmd.arg("-l").arg(str_param(params, "database")).arg("--globals-only").arg("-f").arg(output).arg("--verbose");
        if bool_param(params, "noRolePasswords") {
            cmd.arg("--no-role-passwords");
        }
        return cmd;
    }
    let format = str_param(params, "format");
    let mut cmd = base_command(params, "pg_dump");
    cmd.arg("-d").arg(str_param(params, "database")).arg("-F").arg(format).arg("-f").arg(output).arg("--verbose");
    match str_param(params, "content") {
        "schema" => { cmd.arg("--schema-only"); }
        "data" => { cmd.arg("--data-only"); }
        _ => {}
    }
    if bool_param(params, "noOwner") {
        cmd.arg("--no-owner").arg("--no-privileges");
    }
    if bool_param(params, "clean") {
        cmd.arg("--clean").arg("--if-exists");
    }
    if let Some(jobs) = params.get("jobs").and_then(Value::as_u64).filter(|j| *j > 1 && format == "d") {
        cmd.arg("-j").arg(jobs.to_string());
    }
    for schema in string_list(params, "schemas") {
        cmd.arg("-n").arg(quote_pattern(&schema));
    }
    for (schema, table) in pair_list(params, "excludeData") {
        cmd.arg("--exclude-table-data").arg(format!("{}.{}", quote_pattern(&schema), quote_pattern(&table)));
    }
    let compress = str_param(params, "compress");
    if !compress.is_empty() && matches!(format, "c" | "d") && valid_compress(compress) {
        cmd.arg("-Z").arg(compress);
    }
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

        // Archives: read the TOC up front, so a pg_restore that cannot read the dump fails before anything runs,
        // the UI gets an item count for progress, and a selection can be written as a `-L` list.
        let mut items = None;
        let mut list = None;
        let mut roles = Vec::new();
        if format == Format::Archive {
            let toc = read_toc(&params, &file)?;
            let schemas = string_list(&params, "schemas");
            let selection = toc::Selection {
                schemas: params.get("schemas").filter(|v| v.is_array()).map(|_| schemas.into_iter().collect()),
                skip_data: pair_list(&params, "skipData").into_iter().collect(),
            };
            let kept = selection.select(&toc);
            items = Some(kept.len());
            if bool_param(&params, "createRoles") {
                roles = toc::owners(kept.iter().copied()).into_iter().collect();
            }
            if !selection.is_everything() {
                let path = temp_path("selection.list")?;
                let body: String = kept.iter().map(|e| format!("{}\n", e.line)).collect();
                std::fs::write(&path, body).map_err(|err| PluginError::new(-32000, format!("Cannot write list file: {err}")))?;
                list = Some(path);
            }
        }

        self.begin_job(&format!(
            "Detected format: {}{}",
            if format == Format::Archive { "pg_dump archive -> pg_restore" } else { "plain SQL -> psql" },
            match (&items, &list) {
                (Some(n), Some(_)) => format!(" ({n} selected items)"),
                (Some(n), None) => format!(" ({n} items)"),
                _ => String::new(),
            }
        ))?;

        let shared = self.job.clone();
        thread::spawn(move || {
            if bool_param(&params, "createDb") {
                let mut cmd = base_command(&params, "createdb");
                cmd.arg(str_param(&params, "database"));
                if run_step(&shared, cmd) != 0 {
                    lock(&shared).push("createdb failed (database may already exist), continuing...");
                }
            }
            if let Some(sql) = roles_sql(&roles).filter(|_| lock(&shared).running) {
                lock(&shared).push(&format!("Creating missing roles as NOLOGIN: {}", roles.join(", ")));
                let mut cmd = base_command(&params, "psql");
                cmd.arg("-d").arg(str_param(&params, "database")).arg("-v").arg("ON_ERROR_STOP=1").arg("-c").arg(sql);
                if run_step(&shared, cmd) != 0 {
                    lock(&shared).push("warning: could not create roles (needs CREATEROLE), continuing...");
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
                code = if lock(&shared).running {
                    run_step(&shared, build_restore(&params, &format, &file, list.as_deref()))
                } else {
                    -1
                };
            }
            if let Some(list) = list {
                let _ = std::fs::remove_file(list);
            }
            if Path::new(&file).starts_with(upload_dir()) {
                let _ = std::fs::remove_file(&file);
            }
            finish_job(&shared, code);
        });
        Ok(json!({ "started": true, "items": items }))
    }

    /// Resets the shared job for a new run; only one restore or backup runs at a time.
    fn begin_job(&self, intro: &str) -> Result<(), PluginError> {
        let mut job = lock(&self.job);
        if job.running {
            return Err(PluginError::new(-32000, "Another restore or backup is already running"));
        }
        let dropped = job.dropped + job.log.len(); // keep offsets monotonic across runs
        *job = Job { running: true, dropped, ..Job::default() };
        job.push(intro);
        Ok(())
    }

    fn backup_start(&self, params: Value) -> Result<Value, PluginError> {
        for key in ["host", "port", "user", "database"] {
            if str_param(&params, key).is_empty() {
                return Err(PluginError::new(-32602, format!("Field '{key}' is required")));
            }
        }
        let roles = str_param(&params, "content") == "roles";
        let format = if roles { "p" } else { str_param(&params, "format") };
        if !["c", "p", "t", "d"].contains(&format) {
            return Err(PluginError::new(-32602, "Format must be one of c, p, t, d"));
        }
        if !valid_compress(str_param(&params, "compress")) && !str_param(&params, "compress").is_empty() {
            return Err(PluginError::new(-32602, "Invalid compression"));
        }
        // Explicit path: pg_dump writes there directly. Otherwise a temp file the UI streams to the user's save target.
        let output = match str_param(&params, "output") {
            "" if format == "d" => return Err(PluginError::new(-32602, "Directory format needs an output path")),
            "" => temp_path(str_param(&params, "name"))?,
            path => PathBuf::from(path),
        };
        self.begin_job(&if roles {
            format!("Backing up roles and tablespaces (pg_dumpall --globals-only) to {}", output.display())
        } else {
            format!("Backing up \"{}\" to {}", str_param(&params, "database"), output.display())
        })?;
        let shared = self.job.clone();
        let out = output.clone();
        thread::spawn(move || {
            let code = run_step(&shared, build_backup(&params, &out));
            if code != 0 && out.starts_with(upload_dir()) {
                let _ = std::fs::remove_file(&out);
            }
            finish_job(&shared, code);
        });
        Ok(json!({ "path": output }))
    }

    /// Reads a chunk of a finished backup from the plugin's temp dir (never anywhere else).
    fn download_read(&self, params: Value) -> Result<Value, PluginError> {
        use std::io::{Seek, SeekFrom};
        let path = temp_file(&params)?;
        let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let length = params.get("length").and_then(Value::as_u64).unwrap_or(1 << 20).min(4 << 20);
        let io_err = |err: std::io::Error| PluginError::new(-32000, format!("Read failed: {err}"));
        let mut file = File::open(&path).map_err(io_err)?;
        let size = file.metadata().map_err(io_err)?.len();
        file.seek(SeekFrom::Start(offset)).map_err(io_err)?;
        let mut buf = Vec::with_capacity(length as usize);
        file.take(length).read_to_end(&mut buf).map_err(io_err)?;
        Ok(json!({
            "dataBase64": base64::engine::general_purpose::STANDARD.encode(&buf),
            "length": buf.len(),
            "eof": offset + buf.len() as u64 >= size,
            "size": size,
        }))
    }

    fn download_delete(&self, params: Value) -> Result<Value, PluginError> {
        let path = temp_file(&params)?;
        Ok(json!({ "deleted": std::fs::remove_file(path).is_ok() }))
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
        let path = temp_path(str_param(&params, "name"))?;
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

    /// Server and client tool versions, so the UI can stop an incompatible run before it starts.
    fn check_tools(&self, params: Value) -> Value {
        let bin_dir = str_param(&params, "binDir");
        let tools: serde_json::Map<String, Value> = ["pg_dump", "pg_restore", "psql", "pg_dumpall", "createdb"]
            .iter()
            .map(|name| {
                let version = tool_version(bin_dir, name);
                let major = version.as_deref().and_then(major);
                (name.to_string(), json!({ "version": version, "major": major }))
            })
            .collect();
        let database = Some(str_param(&params, "database")).filter(|d| !d.is_empty()).unwrap_or("postgres");
        let server = query_lines(&params, database, "show server_version").map(|lines| lines.into_iter().next().unwrap_or_default());
        match server {
            Ok(version) => json!({ "tools": tools, "server": { "version": version, "major": major(&version) } }),
            Err(err) => json!({ "tools": tools, "server": null, "serverError": err.message }),
        }
    }

    /// Format, pg_dump/server versions and (for archives) the schema/table tree and owners of a dump file.
    fn inspect_dump(&self, params: Value) -> Result<Value, PluginError> {
        let file = str_param(&params, "path");
        let path = Path::new(file);
        let format = detect_format(path).map_err(|err| PluginError::new(-32602, format!("Cannot read {file}: {err}")))?;
        if format == Format::Plain {
            let (dumped_by, dumped_from) = plain_header(path);
            return Ok(json!({ "format": "plain", "dumpedBy": dumped_by, "dumpedFrom": dumped_from }));
        }
        let toc = read_toc(&params, file)?;
        let schemas: Vec<Value> = toc::schemas(&toc).into_iter().map(|(name, tables)| json!({ "name": name, "tables": tables })).collect();
        Ok(json!({
            "format": "archive",
            "dumpedBy": toc.dumped_by,
            "dumpedFrom": toc.dumped_from,
            "items": toc.entries.len(),
            "schemas": schemas,
            "owners": toc::owners(toc.entries.iter()),
        }))
    }

    /// User schemas with their tables and total sizes, for choosing what a backup includes.
    fn list_objects(&self, params: Value) -> Result<Value, PluginError> {
        let sql = "SELECT n.nspname, coalesce(c.relname, ''), coalesce(pg_total_relation_size(c.oid), 0) \
                   FROM pg_namespace n LEFT JOIN pg_class c ON c.relnamespace = n.oid AND c.relkind IN ('r', 'p') \
                   WHERE n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\\_%' ORDER BY 1, 2";
        let mut schemas: Vec<Value> = Vec::new();
        for line in query_lines(&params, str_param(&params, "database"), sql)? {
            let mut cols = line.split('\t');
            let (Some(schema), Some(table), Some(size)) = (cols.next(), cols.next(), cols.next()) else { continue };
            if schemas.last().and_then(|s| s["name"].as_str()) != Some(schema) {
                schemas.push(json!({ "name": schema, "tables": [] }));
            }
            if !table.is_empty() {
                let size: u64 = size.parse().unwrap_or(0);
                schemas.last_mut().unwrap()["tables"].as_array_mut().unwrap().push(json!({ "name": table, "size": size }));
            }
        }
        Ok(json!({ "schemas": schemas }))
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
            "backup/start" => self.backup_start(params),
            "download/read" => self.download_read(params),
            "download/delete" => self.download_delete(params),
            "tools/check" => Ok(self.check_tools(params)),
            "dump/inspect" => self.inspect_dump(params),
            "db/objects" => self.list_objects(params),
            _ => Err(PluginError::method_not_found(method)),
        }
    }
}

fn main() -> std::io::Result<()> {
    cleanup_temp_dir();
    let metadata = PluginMetadata::new("alvinades98.pg-backup-restore", env!("CARGO_PKG_VERSION"));
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
        let line = describe(&build_restore(&params, &Format::Archive, "/x.sql", Some(Path::new("/l.list"))));
        assert!(line.contains("-L /l.list /x.sql"));
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
    fn backup_command_flags() {
        let params = json!({ "host": "h", "port": "5432", "user": "u", "password": "p", "database": "src",
            "format": "d", "content": "schema", "noOwner": true, "jobs": 4 });
        let line = describe(&build_backup(&params, Path::new("/out")));
        assert!(line.contains("pg_dump -h h -p 5432 -U u --no-password -d src -F d -f /out --verbose --schema-only --no-owner --no-privileges -j 4"));
        let plain = describe(&build_backup(&json!({ "format": "p", "jobs": 4 }), Path::new("/o")));
        assert!(!plain.contains("-j"), "jobs only apply to directory format");
    }

    #[test]
    fn download_is_confined_to_temp_dir() {
        let plugin = Plugin::default();
        assert!(plugin.download_read(json!({ "path": "/etc/passwd" })).is_err());
        let path = temp_path("x.dump").unwrap();
        std::fs::write(&path, b"hello world").unwrap();
        let first = plugin.download_read(json!({ "path": path, "offset": 0, "length": 5 })).unwrap();
        assert_eq!(first["dataBase64"], "aGVsbG8=");
        assert_eq!(first["eof"], false);
        let rest = plugin.download_read(json!({ "path": path, "offset": 5, "length": 100 })).unwrap();
        assert_eq!(rest["length"], 6);
        assert_eq!(rest["eof"], true);
        let sneaky = format!("{}/../../etc/passwd", upload_dir().display());
        assert!(plugin.download_read(json!({ "path": sneaky })).is_err());
        assert_eq!(plugin.download_delete(json!({ "path": path })).unwrap()["deleted"], true);
    }

    #[test]
    fn versions_and_patterns() {
        assert_eq!(major("18.6"), Some(18));
        assert_eq!(major("17.2 (Debian 17.2-1)"), Some(17));
        assert_eq!(major("9.6.24"), Some(9));
        assert_eq!(major("devel"), None);
        assert_eq!(quote_pattern("My\"Schema"), "\"My\"\"Schema\"");
        for ok in ["0", "9", "gzip", "gzip:9", "zstd", "zstd:19", "lz4:3"] {
            assert!(valid_compress(ok), "{ok}");
        }
        for bad in ["", "10", "zstd:", "zstd:100", "brotli", "1; rm -rf /", "gzip:a"] {
            assert!(!valid_compress(bad), "{bad}");
        }
    }

    #[test]
    fn roles_sql_quotes_and_skips_unsafe_names() {
        let sql = roles_sql(&["user_remote".into(), "o'brien".into(), "bad$name".into()]).unwrap();
        assert!(sql.contains("ARRAY['user_remote', 'o''brien']::text[]"));
        assert!(!sql.contains("bad$name"));
        assert!(roles_sql(&["$x".into()]).is_none());
    }

    #[test]
    fn backup_selection_roles_and_compression_flags() {
        let params = json!({ "host": "h", "port": "1", "user": "u", "database": "src", "format": "c",
            "schemas": ["akademik", "Big\"One"], "excludeData": [["akademik", "logs"]], "compress": "zstd:9" });
        let line = describe(&build_backup(&params, Path::new("/o")));
        assert!(line.contains("-n \"akademik\" -n \"Big\"\"One\""));
        assert!(line.contains("--exclude-table-data \"akademik\".\"logs\""));
        assert!(line.ends_with("-Z zstd:9"));
        let tar = describe(&build_backup(&json!({ "format": "t", "compress": "9" }), Path::new("/o")));
        assert!(!tar.contains("-Z"), "tar archives are not compressed by pg_dump");
        let roles = describe(&build_backup(&json!({ "content": "roles", "database": "postgres", "noRolePasswords": true }), Path::new("/r.sql")));
        assert!(roles.starts_with("pg_dumpall ") && roles.contains("-l postgres --globals-only -f /r.sql --verbose --no-role-passwords"));
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
