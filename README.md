# PG Backup & Restore — DBX plugin

Restore PostgreSQL dumps from inside DBX. DBX's built-in "Run SQL file" only reads **plain SQL**; files produced by
`pg_dump -Fc` (custom format, starts with `PGDMP`) are binary even when named `.sql`, so DBX reports
*"Unsupported or invalid SQL file encoding"*. DBeaver works because it calls `pg_restore` — this plugin does the same.

| Detected dump | Tool used |
| --- | --- |
| custom (`PGDMP` header), tar, directory | `pg_restore` |
| anything else (plain SQL) | `psql -f` |

Requires PostgreSQL client tools on the machine (`pg_restore`, `psql`, `createdb`), same major version as the dump or newer.
Set **PostgreSQL bin directory** in the form if they are not on `PATH`.

## Use

Open **PG Backup & Restore** from the Plugin Center / sidebar, or right-click a connection → **Backup / restore PostgreSQL…**
(prefills the database name). Switch between the **Restore** and **Backup** tabs at the top.

- **Saved connection**: fill host/port/user/password once, **Save**, then pick it next time. DBX never shares its own
  connection credentials with plugins, so profiles live in the plugin's storage (password only if *Remember password* is ticked).
- **Database**: **↻** lists databases on the server.

### Restore

- **Dump file**: **Browse…** (native dialog) or drop a file onto the page; the file is streamed to a temp copy, restored,
  then deleted. You can still type a path, which is the only option for directory-format dumps.
- **Reset database** (destructive): before restoring, drops every schema in the target database (including `public`,
  `CASCADE`), recreates an empty `public`, then restores. If the reset fails (e.g. the user doesn't own a schema) the
  restore is aborted. Restore asks for a second click to confirm.

### Backup

- **Format**: custom (`.dump`, default), plain SQL, tar, or directory (parallel, `-j`).
- **Content**: schema + data, schema only, or data only. Optional `--no-owner --no-privileges` and `--clean --if-exists`.
- **Save to**: clicking **Backup** opens the native save dialog first; `pg_dump` writes a temp file that is then streamed to
  the chosen location and deleted. Or type an absolute output path (required for directory format).

The log streams live; **Cancel** kills the running process. The password is passed via `PGPASSWORD`, never on the command line or in the log.

## Build & install

```bash
npx --yes @dbx-app/plugin-cli@0.1.9 package .
```

Produces `dist/alvinades98.pg-backup-restore-<version>-<target>.dbxp`. In DBX: **Plugin Center → Settings**, enable unsigned
development packages, then install the local `.dbxp`.

Run backend tests (the SDK isn't on crates.io; the CLI ships it):

```bash
SDK=$(ls -d ~/.npm/_npx/*/node_modules/@dbx-app/plugin-cli/sdk-root/plugins/sdk/rust/dbx-plugin-sdk | head -1)
cd backend && cargo test --config "patch.crates-io.dbx-plugin-sdk.path=\"$SDK\""
```

Backend RPC: `restore/start`, `backup/start`, `restore/status { offset }` (shared job log), `restore/cancel`,
`upload/begin|chunk|finish`, `download/read|delete` (confined to the plugin temp dir), `db/list`.
