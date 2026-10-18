# PG Backup & Restore — DBX plugin

Restore PostgreSQL dumps from inside DBX. DBX's built-in "Run SQL file" only reads **plain SQL**; files produced by
`pg_dump -Fc` (custom format, starts with `PGDMP`) are binary even when named `.sql`, so DBX reports
*"Unsupported or invalid SQL file encoding"*. DBeaver works because it calls `pg_restore` — this plugin does the same.

| Detected dump | Tool used |
| --- | --- |
| custom (`PGDMP` header), tar, directory | `pg_restore` |
| anything else (plain SQL) | `psql -f` |

Requires PostgreSQL client tools on the machine (`pg_dump`, `pg_restore`, `psql`, `createdb`). Set
**PostgreSQL client tools → Bin directory** in the form if they are not on `PATH`.

> [!WARNING]
> - **Reset database** runs `DROP SCHEMA … CASCADE` on every schema of the target database. Only use it after you
>   have confirmed that a backup of that database exists and can be restored.
> - `pg_dump` / `pg_restore` must be compatible with the server: use the same major version as the server (and as
>   the dump being restored), or newer. Older tools fail with errors such as `unsupported version` or
>   `server version mismatch`.

## Use

Open **PG Backup & Restore** from the Plugin Center / sidebar, or right-click a connection → **Backup / restore PostgreSQL…**
(prefills the database name). Switch between the **Restore** and **Backup** tabs at the top.

- **Saved connection**: fill host/port/user/password once, **Save**, then pick it next time. DBX never shares its own
  connection credentials with plugins, so profiles live in the plugin's storage (password only if *Remember password* is ticked).
- **Database**: **↻** lists databases on the server.

A version check runs before every restore or backup: it compares `pg_dump` / `pg_restore` with the server (and with the
pg_dump version that made the dump) and stops with a clear message instead of failing halfway. Versions are shown under
the connection form (**PostgreSQL client tools → Check versions**).

### Restore

- **Dump file**: **Browse…** (native dialog) or drop a file onto the page; the file is streamed to a temp copy right away,
  then inspected. You can still type a path, which is the only option for directory-format dumps.
- **Contents** (custom / tar / directory dumps): untick schemas to skip them, or untick a table to skip only its data.
  Types and functions of skipped schemas are still restored, because other schemas often use them; foreign keys that point
  into a skipped schema fail and show as errors.
- **Create missing roles**: creates the owners named in the dump as `NOLOGIN` roles first (needs `CREATEROLE`), which
  avoids `role "…" does not exist` errors.
- **Reset database** (destructive): before restoring, drops every schema in the target database (including `public`,
  `CASCADE`), recreates an empty `public`, then restores. If the reset fails (e.g. the user doesn't own a schema) the
  restore is aborted. Restore asks for a second click to confirm.
- Progress is shown in percent for archive dumps.

### Backup

- **Content**: schema + data, schema only, data only, or **Roles** (`pg_dumpall --globals-only`, optionally without passwords).
- **Format**: custom (`.dump`, default), plain SQL, tar, or directory (parallel, `-j`).
- **Compression** (custom / directory): default, none, gzip 1 / 9, zstd, zstd 19, lz4 (zstd/lz4 need pg_dump 16+).
- **Contents**: untick schemas to leave them out (`-n`), or untick a table to keep its structure without data
  (`--exclude-table-data`); table sizes are shown to spot large log tables.
- **Save to**: clicking **Backup** opens the native save dialog first; `pg_dump` writes a temp file that is then streamed to
  the chosen location and deleted. Or type an absolute output path (required for directory format).
- **Recent backups** lists the last 20 backups; **Restore this** fills the Restore tab (or opens the file picker when the
  backup went through the save dialog).

The Output panel can **copy** the log or **save** it to a file. The UI is available in English, Bahasa Indonesia and 中文
(selector in the header; *Auto* follows DBX).

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
`upload/begin|chunk|finish`, `download/read|delete` (confined to the plugin temp dir), `db/list`, `db/objects`,
`dump/inspect`, `tools/check`.
