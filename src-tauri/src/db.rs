use crate::lyricsfile::{lyrics_presence_from_lyricsfile, LyricsPresence};
use crate::persistent_entities::{
    PersistentAlbum, PersistentArtist, PersistentConfig, PersistentTrack,
};
use crate::scanner::models::DbTrack;
use crate::utils::prepare_input;
use anyhow::Result;
use include_dir::{include_dir, Dir};
use indoc::indoc;
use rusqlite::{named_params, params, Connection, OptionalExtension};
use rusqlite_migration::Migrations;
use std::fs;
use tauri::{AppHandle, Manager};

static MIGRATIONS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/migrations");

/// The identifier the app used before it was renamed to `com.freelamp.SingAlong`.
///
/// It was inherited from LRCGET, which names its data directory after a reverse
/// DNS name it never owned. Flathub requires that the identifier correspond to a
/// domain the project controls, so it moved — and the data directory moved with
/// it, because Tauri derives the directory from the identifier.
const LEGACY_IDENTIFIER: &str = "app.singalong.desktop";

/// Move the data directory from the old identifier to the new one, once.
///
/// Runs before the database is opened: afterwards the new path exists and the
/// condition below can never be true again, so a fresh install does nothing and a
/// failure can be retried on the next launch rather than leaving a half-moved
/// directory that the app then refuses to look at.
fn migrate_legacy_data_dir(new_dir: &std::path::Path) {
    // Test for the database, not the directory.
    //
    // Tauri creates the app data directory while the app is being set up — before
    // any command runs, because the webview's own storage lives there too. So by
    // the time this is called the directory exists on every launch, and asking
    // `new_dir.exists()` would skip the migration forever. The database file is
    // what `initialize_database` is about to create, and the one thing whose
    // absence actually means "this is a first run".
    if new_dir.join("db.sqlite3").exists() {
        return;
    }
    let Some(parent) = new_dir.parent() else {
        return;
    };
    let legacy = parent.join(LEGACY_IDENTIFIER);
    if !legacy.join("db.sqlite3").is_file() {
        return;
    }

    // A per-entry move into the existing directory, rather than renaming it:
    // the destination already exists, and `rename` fails when it does.
    match move_entries(&legacy, new_dir) {
        Ok(()) => {
            println!(
                "[DB Init] moved data from {} to {}",
                legacy.display(),
                new_dir.display()
            );
            // Left behind deliberately: an empty directory is the signal that the
            // move finished, and removing it is not worth a failure path.
        }
        Err(err) => {
            eprintln!(
                "[DB Init] could not move data from {}: {err}",
                legacy.display()
            );
            // Undo, so the next launch retries rather than finding a new
            // database that looks legitimate but was never populated.
            let _ = std::fs::remove_file(new_dir.join("db.sqlite3"));
        }
    }
}

/// Move every entry of `from` into `to`, overwriting nothing that is already there.
fn move_entries(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if target.exists() {
            // The webview's own files are recreated on demand; the database and
            // the cover cache are what must not be overwritten with an older copy.
            continue;
        }
        match std::fs::rename(entry.path(), &target) {
            Ok(()) => {}
            Err(_) => copy_path(&entry.path(), &target)?,
        }
    }
    Ok(())
}

fn copy_path(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    if from.is_dir() {
        copy_dir_all(from, to)
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

fn copy_dir_all(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Initializes the database connection, creating the .sqlite file if needed, and upgrading the
/// database if it's out of date.
pub fn initialize_database(app_handle: &AppHandle) -> Result<Connection, rusqlite::Error> {
    let app_dir = app_handle
        .path()
        .app_data_dir()
        .expect("The app data directory should exist.");
    migrate_legacy_data_dir(&app_dir);
    fs::create_dir_all(&app_dir).expect("The app data directory should be created.");
    let sqlite_path = app_dir.join("db.sqlite3");

    println!("Database file path: {}", sqlite_path.display());

    let mut db = Connection::open(sqlite_path)?;

    // Exclusive access for the lifetime of this connection.
    //
    // Two processes sharing this file is not survivable: SQLite serialises
    // writes but each process holds its own cache, so one can migrate the schema
    // or delete rows the other still believes exist. The single-instance plugin
    // already stops a second GUI from starting, but it keys on the process, not
    // the data — a stale lock from a crash, or a copy in another path, would slip
    // past it. Taking an exclusive lock means the second process fails here, at
    // the only place that actually matters.
    match db.pragma_update(None, "locking_mode", "EXCLUSIVE") {
        Ok(()) => {}
        Err(err) => {
            eprintln!("[DB Init] could not set exclusive locking: {err}");
        }
    }

    // Prove the lock is ours. `BEGIN EXCLUSIVE` takes a write lock immediately
    // and fails with SQLITE_BUSY when another process holds the database; a read
    // like `quick_check` would instead block until the busy timeout expired and
    // then report success.
    db.busy_timeout(std::time::Duration::from_millis(500))?;
    if let Err(err) = db.execute_batch("BEGIN EXCLUSIVE; COMMIT;") {
        eprintln!("[DB Init] database is locked by another process: {err}");
        return Err(err);
    }

    db.pragma_update(None, "journal_mode", "WAL")?;

    let current_version: i64 = db
        .query_row("PRAGMA user_version;", [], |row| row.get(0))
        .unwrap_or(-1);
    println!("[DB Init] Current user_version: {}", current_version);

    let migrations = Migrations::from_directory(&MIGRATIONS_DIR)
        .expect("Failed to load migrations from directory");

    match migrations.to_latest(&mut db) {
        Ok(()) => {
            let new_version: i64 = db
                .query_row("PRAGMA user_version;", [], |row| row.get(0))
                .unwrap_or(-1);
            println!("[DB Init] Migrations applied successfully. New user_version: {}", new_version);
        }
        Err(e) => {
            eprintln!("[DB Init] Failed to run database migrations: {:?}", e);
            panic!("Failed to run database migrations: {}", e);
        }
    }

    if let Err(error) = backfill_track_lyrics_presence(&db) {
        eprintln!("Failed to backfill track lyrics presence flags: {}", error);
    }

    Ok(db)
}

/// Check whether FTS5 virtual tables were created successfully.
/// This is a safety net in case SQLite was compiled without FTS5 support.
pub fn fts5_enabled(db: &Connection) -> bool {
    db.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'tracks_fts'",
        [],
        |_| Ok(()),
    )
    .is_ok()
}

// Backfill lyrics presence data from lyricsfile content
// This updates lyricsfiles records that may not have presence fields set
pub fn backfill_track_lyrics_presence(db: &Connection) -> Result<()> {
    let mut select_statement = db.prepare(indoc! {"
      SELECT
        lyricsfiles.id,
        lyricsfiles.lyricsfile
      FROM lyricsfiles
      WHERE lyricsfiles.has_plain_lyrics = 0
         AND lyricsfiles.has_synced_lyrics = 0
         AND lyricsfiles.has_word_synced_lyrics = 0
    "})?;

    let mut rows = select_statement.query([])?;
    let mut updates: Vec<(i64, LyricsPresence)> = Vec::new();

    while let Some(row) = rows.next()? {
        let lyricsfile_id: i64 = row.get("id")?;
        let lyricsfile_content: Option<String> = row.get("lyricsfile")?;

        let presence = match lyricsfile_content
            .as_deref()
            .map(str::trim)
            .filter(|content| !content.is_empty())
        {
            Some(content) => lyrics_presence_from_lyricsfile(content).unwrap_or_default(),
            None => LyricsPresence::default(),
        };

        updates.push((lyricsfile_id, presence));
    }

    // Update lyricsfiles records with calculated presence
    for (lyricsfile_id, presence) in updates {
        db.execute(
            "UPDATE lyricsfiles SET has_plain_lyrics = ?, has_synced_lyrics = ?, has_word_synced_lyrics = ?, instrumental = ? WHERE id = ?",
            (
                presence.has_plain_lyrics,
                presence.has_synced_lyrics,
                presence.has_word_synced_lyrics,
                presence.is_instrumental,
                lyricsfile_id,
            ),
        )?;
    }

    Ok(())
}

pub fn get_directories(db: &Connection) -> Result<Vec<String>> {
    let mut statement = db.prepare("SELECT * FROM directories")?;
    let mut rows = statement.query([])?;
    let mut directories: Vec<String> = Vec::new();
    while let Some(row) = rows.next()? {
        let path: String = row.get("path")?;

        directories.push(path);
    }

    Ok(directories)
}

pub fn set_directories(directories: Vec<String>, db: &Connection) -> Result<()> {
    db.execute("DELETE FROM directories WHERE 1", ())?;
    let mut statement = db.prepare("INSERT INTO directories (path) VALUES (@path)")?;
    for directory in directories.iter() {
        statement.execute(named_params! { "@path": directory })?;
    }

    Ok(())
}

/// Add a directory to the watched list, if it is not there already.
///
/// Returns whether it was added. The directories are a set rather than a
/// list in the code that uses them, and adding one twice would make the
/// scanner walk it twice.
pub fn add_directory(path: &str, db: &Connection) -> Result<bool> {
    let path = path.trim();
    if path.is_empty() {
        return Ok(false);
    }

    if get_directories(db)?.iter().any(|existing| existing == path) {
        return Ok(false);
    }

    db.execute("INSERT INTO directories (path) VALUES (?1)", [path])?;
    Ok(true)
}

pub fn get_init(db: &Connection) -> Result<bool> {
    let mut statement = db.prepare("SELECT init FROM library_data LIMIT 1")?;
    let init: bool = statement.query_row([], |r| r.get(0))?;
    Ok(init)
}

pub fn set_init(init: bool, db: &Connection) -> Result<()> {
    let mut statement = db.prepare("UPDATE library_data SET init = ? WHERE 1")?;
    statement.execute([init])?;
    Ok(())
}

pub fn get_config(db: &Connection) -> Result<PersistentConfig> {
    let mut statement = db.prepare(indoc! {"
      SELECT
        skip_tracks_with_synced_lyrics,
        skip_tracks_with_plain_lyrics,
        show_line_count,
        try_embed_lyrics,
        theme_mode,
        lrclib_instance,
        volume,
        auto_export_enabled,
        export_lrc,
        export_txt,
        export_embedded,
        lyric_script,
        netease_fallback_enabled,
        sync_cover,
        proxy_url,
        update_check_enabled,
        last_announced_version,
        amll_db_url,
        ytdlp_binary,
        ytdlp_cookies_json,
        ytdlp_output_dir,
        export_template
      FROM config_data
      LIMIT 1
    "})?;
    let row = statement.query_row([], |r| {
        Ok(PersistentConfig {
            auto_export_enabled: r.get("auto_export_enabled")?,
            export_lrc: r.get("export_lrc")?,
            export_txt: r.get("export_txt")?,
            export_embedded: r.get("export_embedded")?,
            lyric_script: r.get("lyric_script")?,
            netease_fallback_enabled: r.get("netease_fallback_enabled")?,
            sync_cover: r.get("sync_cover")?,
            proxy_url: r.get("proxy_url")?,
            update_check_enabled: r.get("update_check_enabled")?,
            last_announced_version: r.get("last_announced_version")?,
            amll_db_url: r.get("amll_db_url")?,
            ytdlp_binary: r.get("ytdlp_binary")?,
            ytdlp_cookies_json: r.get("ytdlp_cookies_json")?,
            ytdlp_output_dir: r.get("ytdlp_output_dir")?,
            export_template: r.get("export_template")?,
            skip_tracks_with_synced_lyrics: r.get("skip_tracks_with_synced_lyrics")?,
            skip_tracks_with_plain_lyrics: r.get("skip_tracks_with_plain_lyrics")?,
            show_line_count: r.get("show_line_count")?,
            try_embed_lyrics: r.get("try_embed_lyrics")?,
            theme_mode: r.get("theme_mode")?,
            lrclib_instance: r.get("lrclib_instance")?,
            volume: r.get("volume")?,
        })
    })?;
    Ok(row)
}

pub fn set_config(
    skip_tracks_with_synced_lyrics: bool,
    skip_tracks_with_plain_lyrics: bool,
    show_line_count: bool,
    try_embed_lyrics: bool,
    theme_mode: &str,
    lrclib_instance: &str,
    volume: f64,
    sync_cover: bool,
    proxy_url: &str,
    update_check_enabled: bool,
    amll_db_url: &str,
    ytdlp_binary: &str,
    ytdlp_cookies_json: &str,
    ytdlp_output_dir: &str,
    export_template: &str,
    db: &Connection,
) -> Result<()> {
    let mut statement = db.prepare(indoc! {"
      UPDATE config_data
      SET
        skip_tracks_with_synced_lyrics = ?,
        skip_tracks_with_plain_lyrics = ?,
        show_line_count = ?,
        try_embed_lyrics = ?,
        theme_mode = ?,
        lrclib_instance = ?,
        volume = ?,
        sync_cover = ?,
        proxy_url = ?,
        update_check_enabled = ?,
        amll_db_url = ?,
        ytdlp_binary = ?,
        ytdlp_cookies_json = ?,
        ytdlp_output_dir = ?,
        export_template = ?
      WHERE 1
    "})?;
    statement.execute((
        skip_tracks_with_synced_lyrics,
        skip_tracks_with_plain_lyrics,
        show_line_count,
        try_embed_lyrics,
        theme_mode,
        lrclib_instance,
        volume,
        sync_cover,
        proxy_url,
        update_check_enabled,
        amll_db_url,
        ytdlp_binary,
        ytdlp_cookies_json,
        ytdlp_output_dir,
        export_template,
    ))?;
    Ok(())
}

// Omitted fields preserve hidden choices and preferences owned by the other popup.
pub fn set_export_preferences(
    auto_export_enabled: Option<bool>,
    export_lrc: Option<bool>,
    export_txt: Option<bool>,
    export_embedded: Option<bool>,
    skip_tracks_with_synced_lyrics: Option<bool>,
    skip_tracks_with_plain_lyrics: Option<bool>,
    export_template: Option<&str>,
    db: &Connection,
) -> Result<()> {
    let config = get_config(db)?;
    // A disabled experimental option cannot overwrite its remembered selection.
    let export_embedded = export_embedded.filter(|_| config.try_embed_lyrics);
    let has_sidecar =
        export_lrc.unwrap_or(config.export_lrc) || export_txt.unwrap_or(config.export_txt);
    if auto_export_enabled != Some(false)
        && !has_sidecar
        && !(export_embedded.unwrap_or(config.export_embedded) && config.try_embed_lyrics)
    {
        anyhow::bail!("Select at least one enabled export format");
    }

    // The template is checked here so a bad one is refused at the point of
    // entry. `set_config` writes it too, but the settings panel is where a user
    // types it, and that is where the mistake is worth catching.
    if let Some(raw) = export_template {
        if !raw.trim().is_empty() {
            crate::export_template::ExportTemplate::parse(raw)
                .map_err(|err| anyhow::anyhow!("invalid export template: {err}"))?;
        }
    }

    db.execute(
        "UPDATE config_data SET
            auto_export_enabled = COALESCE(?1, auto_export_enabled),
            export_lrc = COALESCE(?2, export_lrc),
            export_txt = COALESCE(?3, export_txt),
            export_embedded = COALESCE(?4, export_embedded),
            skip_tracks_with_synced_lyrics = COALESCE(?5, skip_tracks_with_synced_lyrics),
            skip_tracks_with_plain_lyrics = COALESCE(?6, skip_tracks_with_plain_lyrics),
            export_template = COALESCE(?7, export_template)",
        (
            auto_export_enabled,
            export_lrc,
            export_txt,
            export_embedded,
            skip_tracks_with_synced_lyrics,
            skip_tracks_with_plain_lyrics,
            export_template,
        ),
    )?;
    Ok(())
}

/// Persist the external downloader's settings on their own.
///
/// A scoped update, like the export preferences: the downloader panel can be
/// opened and closed without the rest of the settings being touched, and
/// saving one thing must not reset another.
pub fn set_ytdlp_settings(
    ytdlp_binary: &str,
    ytdlp_cookies_json: &str,
    ytdlp_output_dir: &str,
    db: &Connection,
) -> Result<()> {
    db.execute(
        "UPDATE config_data SET
            ytdlp_binary = ?1,
            ytdlp_cookies_json = ?2,
            ytdlp_output_dir = ?3",
        (ytdlp_binary, ytdlp_cookies_json, ytdlp_output_dir),
    )?;
    Ok(())
}

/// Persist the downloaded-lyrics script preference.
///
/// Values: `ask` (prompt on the download popup), `off` (keep as published),
/// `simplified`, `traditional`.
pub fn set_lyric_script(script: &str, db: &Connection) -> Result<()> {
    db.execute(
        "UPDATE config_data SET lyric_script = ?1",
        rusqlite::params![script],
    )?;
    Ok(())
}

pub fn set_volume_config(volume: f64, db: &Connection) -> Result<()> {
    let mut statement = db.prepare("UPDATE config_data SET volume = ? WHERE 1")?;
    statement.execute([volume])?;
    Ok(())
}

pub fn find_artist(name: &str, db: &Connection) -> Result<i64> {
    let mut statement = db.prepare("SELECT id FROM artists WHERE name = ?")?;
    let id: i64 = statement.query_row([name], |r| r.get(0))?;
    Ok(id)
}

pub fn add_artist(name: &str, db: &Connection) -> Result<i64> {
    let mut statement = db.prepare("INSERT INTO artists (name, name_lower) VALUES (?, ?)")?;
    let row_id = statement.insert((name, prepare_input(name)))?;
    Ok(row_id)
}

pub fn find_album(name: &str, album_artist_name: &str, db: &Connection) -> Result<i64> {
    let mut statement =
        db.prepare("SELECT id FROM albums WHERE name = ? AND album_artist_name = ?")?;
    let id: i64 = statement.query_row((name, album_artist_name), |r| r.get(0))?;
    Ok(id)
}

pub fn add_album(name: &str, album_artist_name: &str, db: &Connection) -> Result<i64> {
    let mut statement = db.prepare("INSERT INTO albums (name, name_lower, album_artist_name, album_artist_name_lower) VALUES (?, ?, ?, ?)")?;
    let row_id = statement.insert((
        name,
        prepare_input(name),
        album_artist_name,
        prepare_input(album_artist_name),
    ))?;
    Ok(row_id)
}

pub fn get_track_by_id(id: i64, db: &Connection) -> Result<PersistentTrack> {
    let query = indoc! {"
    SELECT
      tracks.id,
      tracks.file_path,
      tracks.file_name,
      tracks.title,
      artists.name AS artist_name,
      tracks.artist_id,
      albums.name AS album_name,
      albums.album_artist_name,
      tracks.album_id,
      tracks.duration,
      tracks.track_number,
      albums.image_path,
      lyricsfiles.id AS lyricsfile_id,
      lyricsfiles.lyricsfile,
      COALESCE(lyricsfiles.instrumental, 0) AS instrumental
    FROM tracks
    JOIN albums ON tracks.album_id = albums.id
    JOIN artists ON tracks.artist_id = artists.id
    LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
    WHERE tracks.id = ?
    LIMIT 1
  "};

    let mut statement = db.prepare(query)?;
    let row = statement.query_row([id], |row| {
        let is_instrumental: bool = row.get("instrumental")?;

        Ok(PersistentTrack {
            id: row.get("id")?,
            file_path: row.get("file_path")?,
            file_name: row.get("file_name")?,
            title: row.get("title")?,
            artist_name: row.get("artist_name")?,
            artist_id: row.get("artist_id")?,
            album_name: row.get("album_name")?,
            album_artist_name: row.get("album_artist_name")?,
            album_id: row.get("album_id")?,
            duration: row.get("duration")?,
            track_number: row.get("track_number")?,
            txt_lyrics: None,
            lrc_lyrics: None,
            lyricsfile: row.get("lyricsfile")?,
            lyricsfile_id: row.get("lyricsfile_id")?,
            image_path: row.get("image_path")?,
            instrumental: is_instrumental,
        })
    })?;
    Ok(row)
}

pub fn upsert_lyricsfile_for_track(
    track_id: i64,
    track_title: &str,
    track_album_name: &str,
    track_artist_name: &str,
    track_duration: f64,
    lyricsfile: &str,
    db: &Connection,
) -> Result<()> {
    let presence = lyrics_presence_from_lyricsfile(lyricsfile)?;

    db.execute(
        indoc! {"
        INSERT INTO lyricsfiles (
            track_id,
            track_title,
            track_title_lower,
            track_album_name,
            track_album_name_lower,
            track_artist_name,
            track_artist_name_lower,
            track_duration,
            lyricsfile,
            has_plain_lyrics,
            has_synced_lyrics,
            has_word_synced_lyrics,
            instrumental,
            created_at,
            updated_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
        ON CONFLICT(track_id) DO UPDATE SET
            track_title = excluded.track_title,
            track_title_lower = excluded.track_title_lower,
            track_album_name = excluded.track_album_name,
            track_album_name_lower = excluded.track_album_name_lower,
            track_artist_name = excluded.track_artist_name,
            track_artist_name_lower = excluded.track_artist_name_lower,
            track_duration = excluded.track_duration,
            lyricsfile = excluded.lyricsfile,
            has_plain_lyrics = excluded.has_plain_lyrics,
            has_synced_lyrics = excluded.has_synced_lyrics,
            has_word_synced_lyrics = excluded.has_word_synced_lyrics,
            instrumental = excluded.instrumental,
            updated_at = CURRENT_TIMESTAMP
    "},
        (
            track_id,
            track_title,
            prepare_input(track_title),
            track_album_name,
            prepare_input(track_album_name),
            track_artist_name,
            prepare_input(track_artist_name),
            track_duration,
            lyricsfile,
            presence.has_plain_lyrics,
            presence.has_synced_lyrics,
            presence.has_word_synced_lyrics,
            presence.is_instrumental,
        ),
    )?;

    Ok(())
}

pub fn upsert_lyricsfile_for_track_tx(
    track_id: i64,
    track_title: &str,
    track_album_name: &str,
    track_artist_name: &str,
    track_duration: f64,
    lyricsfile: &str,
    tx: &rusqlite::Transaction,
) -> Result<()> {
    let presence = lyrics_presence_from_lyricsfile(lyricsfile)?;

    tx.execute(
        indoc! {"
        INSERT INTO lyricsfiles (
            track_id,
            track_title,
            track_title_lower,
            track_album_name,
            track_album_name_lower,
            track_artist_name,
            track_artist_name_lower,
            track_duration,
            lyricsfile,
            has_plain_lyrics,
            has_synced_lyrics,
            has_word_synced_lyrics,
            instrumental,
            created_at,
            updated_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
        ON CONFLICT(track_id) DO UPDATE SET
            track_title = excluded.track_title,
            track_title_lower = excluded.track_title_lower,
            track_album_name = excluded.track_album_name,
            track_album_name_lower = excluded.track_album_name_lower,
            track_artist_name = excluded.track_artist_name,
            track_artist_name_lower = excluded.track_artist_name_lower,
            track_duration = excluded.track_duration,
            lyricsfile = excluded.lyricsfile,
            has_plain_lyrics = excluded.has_plain_lyrics,
            has_synced_lyrics = excluded.has_synced_lyrics,
            has_word_synced_lyrics = excluded.has_word_synced_lyrics,
            instrumental = excluded.instrumental,
            updated_at = CURRENT_TIMESTAMP
    "},
        (
            track_id,
            track_title,
            prepare_input(track_title),
            track_album_name,
            prepare_input(track_album_name),
            track_artist_name,
            prepare_input(track_artist_name),
            track_duration,
            lyricsfile,
            presence.has_plain_lyrics,
            presence.has_synced_lyrics,
            presence.has_word_synced_lyrics,
            presence.is_instrumental,
        ),
    )?;

    Ok(())
}

pub fn delete_lyricsfile_by_track_id(track_id: i64, db: &Connection) -> Result<()> {
    db.execute("DELETE FROM lyricsfiles WHERE track_id = ?", [track_id])?;
    Ok(())
}

/// Get lyricsfile by LRCLIB instance and ID
/// Returns (lyricsfile_id, lyricsfile_content) if found
pub fn get_lyricsfile_by_lrclib(
    lrclib_instance: &str,
    lrclib_id: i64,
    db: &Connection,
) -> Result<Option<(i64, String)>> {
    let result = db
        .query_row(
            "SELECT id, lyricsfile FROM lyricsfiles WHERE lrclib_instance = ? AND lrclib_id = ?",
            [lrclib_instance, &lrclib_id.to_string()],
            |row| {
                let id: i64 = row.get(0)?;
                let lyricsfile: String = row.get(1)?;
                Ok((id, lyricsfile))
            },
        )
        .optional()?;
    Ok(result)
}

/// Upsert lyricsfile for LRCLIB track (standalone, no track association)
/// Returns the lyricsfile_id
pub fn upsert_lyricsfile_for_lrclib(
    lrclib_instance: &str,
    lrclib_id: i64,
    track_title: &str,
    track_album_name: &str,
    track_artist_name: &str,
    track_duration: f64,
    lyricsfile: &str,
    db: &Connection,
) -> Result<i64> {
    // Calculate presence fields from lyricsfile content
    let presence = lyrics_presence_from_lyricsfile(lyricsfile)?;

    db.execute(
        indoc! {"
        INSERT INTO lyricsfiles (
            lrclib_instance,
            lrclib_id,
            track_title,
            track_title_lower,
            track_album_name,
            track_album_name_lower,
            track_artist_name,
            track_artist_name_lower,
            track_duration,
            lyricsfile,
            has_plain_lyrics,
            has_synced_lyrics,
            has_word_synced_lyrics,
            instrumental,
            created_at,
            updated_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
        ON CONFLICT(lrclib_instance, lrclib_id) DO UPDATE SET
            track_title = excluded.track_title,
            track_title_lower = excluded.track_title_lower,
            track_album_name = excluded.track_album_name,
            track_album_name_lower = excluded.track_album_name_lower,
            track_artist_name = excluded.track_artist_name,
            track_artist_name_lower = excluded.track_artist_name_lower,
            track_duration = excluded.track_duration,
            lyricsfile = excluded.lyricsfile,
            has_plain_lyrics = excluded.has_plain_lyrics,
            has_synced_lyrics = excluded.has_synced_lyrics,
            has_word_synced_lyrics = excluded.has_word_synced_lyrics,
            instrumental = excluded.instrumental,
            updated_at = CURRENT_TIMESTAMP
    "},
        (
            lrclib_instance,
            lrclib_id,
            track_title,
            prepare_input(track_title),
            track_album_name,
            prepare_input(track_album_name),
            track_artist_name,
            prepare_input(track_artist_name),
            track_duration,
            lyricsfile,
            presence.has_plain_lyrics,
            presence.has_synced_lyrics,
            presence.has_word_synced_lyrics,
            presence.is_instrumental,
        ),
    )?;

    // Get the inserted/updated row ID
    let lyricsfile_id = db.query_row(
        "SELECT id FROM lyricsfiles WHERE lrclib_instance = ? AND lrclib_id = ?",
        [lrclib_instance, &lrclib_id.to_string()],
        |row| row.get(0),
    )?;

    Ok(lyricsfile_id)
}

/// Update lyricsfile content by ID (for standalone lyricsfiles without track association)
pub fn update_lyricsfile_by_id(
    lyricsfile_id: i64,
    lyricsfile: &str,
    db: &Connection,
) -> Result<()> {
    // Calculate presence fields from lyricsfile content
    let presence = lyrics_presence_from_lyricsfile(lyricsfile)?;

    db.execute(
        "UPDATE lyricsfiles SET lyricsfile = ?, has_plain_lyrics = ?, has_synced_lyrics = ?, has_word_synced_lyrics = ?, instrumental = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        (
            lyricsfile,
            presence.has_plain_lyrics,
            presence.has_synced_lyrics,
            presence.has_word_synced_lyrics,
            presence.is_instrumental,
            lyricsfile_id,
        ),
    )?;
    Ok(())
}

/// Get lyricsfile by ID
/// Returns (lyricsfile_id, track_id, lyricsfile_content) if found
/// Add a single file to the library.
///
/// The scan does this for every file in the library, but a scan cannot be
/// aimed at one directory: it marks every track as pending and then deletes
/// the ones it did not see, so scanning one folder would remove the rest of
/// the library. This adds one file and touches nothing else, which is what a
/// download needs.
pub fn add_single_track(
    path: &std::path::Path,
    conn: &mut Connection,
    cover_cache_dir: &std::path::Path,
) -> Result<i64> {
    let (metadata, lyrics) = crate::scanner::metadata::extract_track_info(path)?;

    // A file that is already in the library is updated rather than added
    // again. This is the "replace" case: the download overwrote the file, so
    // the entry that points at it should point at the new content rather
    // than be a second entry for the same path.
    let path_str = path.to_string_lossy().to_string();
    if let Some(existing) = find_track_by_path(&path_str, conn).ok() {
        let file_size = std::fs::metadata(path)?.len() as i64;
        let modified_time = std::fs::metadata(path)?
            .modified()?
            .duration_since(std::time::SystemTime::UNIX_EPOCH)?
            .as_secs() as i64;
        let content_hash = crate::scanner::hasher::compute_quick_hash(path)?;

        let tx = conn.transaction()?;

        let artist_id = match find_artist_tx(&metadata.artist, &tx) {
            Ok(id) => id,
            Err(_) => add_artist_tx(&metadata.artist, &tx)?,
        };
        let album_id = match find_album_tx(&metadata.album, &metadata.album_artist, &tx) {
            Ok(id) => id,
            Err(_) => add_album_tx(&metadata.album, &metadata.album_artist, &tx)?,
        };

        update_track_path_and_fingerprint_tx(
            existing.id,
            &path_str,
            file_size,
            modified_time,
            &content_hash,
            &tx,
        )?;
        update_track_metadata_tx(existing.id, &metadata, artist_id, album_id, &tx)?;

        insert_track_fts_tx(
            existing.id,
            &crate::utils::prepare_input(&metadata.title),
            &crate::utils::prepare_input(&metadata.artist),
            &crate::utils::prepare_input(&metadata.album),
            &tx,
        )?;

        tx.commit()?;
        return Ok(existing.id);
    }

    let file_size = std::fs::metadata(path)?.len() as i64;
    let modified_time = std::fs::metadata(path)?
        .modified()?
        .duration_since(std::time::SystemTime::UNIX_EPOCH)?
        .as_secs() as i64;
    let content_hash = crate::scanner::hasher::compute_quick_hash(path)?;

    let tx = conn.transaction()?;

    let artist_id = match find_artist_tx(&metadata.artist, &tx) {
        Ok(id) => id,
        Err(_) => add_artist_tx(&metadata.artist, &tx)?,
    };

    let (album_id, album_was_created) =
        match find_album_tx(&metadata.album, &metadata.album_artist, &tx) {
            Ok(id) => (id, false),
            Err(_) => (add_album_tx(&metadata.album, &metadata.album_artist, &tx)?, true),
        };

    // The album gets a cover from the file, the same way the scan does it.
    if album_was_created {
        if let Some(bytes) = crate::cover_cache::cover_for_track(path) {
            if let Some(file_name) = crate::cover_cache::store_cover(cover_cache_dir, &format!("album-{album_id}"), &bytes) {
                set_album_image_tx(album_id, &file_name, &tx)?;
            }
        }
    }

    let track_id = insert_track_from_metadata_tx(
        &metadata,
        &lyrics,
        file_size,
        modified_time,
        &content_hash,
        artist_id,
        album_id,
        &tx,
    )?;

    insert_track_fts_tx(
        track_id,
        &crate::utils::prepare_input(&metadata.title),
        &crate::utils::prepare_input(&metadata.artist),
        &crate::utils::prepare_input(&metadata.album),
        &tx,
    )?;

    tx.commit()?;
    Ok(track_id)
}

/// How sure the library is that two tracks are the same recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DuplicateCertainty {
    /// The two files have the same content hash: they are the same audio.
    Identical,
    /// The two have the same title and artist and the same length: they are
    /// the same recording, from different sources.
    SameRecording,
    /// The two have the same title and artist but different lengths: they
    /// are the same song, but different takes or different editions.
    DifferentRecording,
}

/// A pair of library tracks that might be the same recording.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicatePair {
    pub first_id: i64,
    pub first_path: String,
    pub first_title: String,
    pub first_artist: String,
    pub first_album: String,
    pub first_duration: f64,
    pub second_id: i64,
    pub second_path: String,
    pub second_title: String,
    pub second_artist: String,
    pub second_album: String,
    pub second_duration: f64,
    /// How sure the library is.
    pub certainty: DuplicateCertainty,
    /// Whether both files are on the disk.
    pub both_present: bool,
}

/// The library tracks that look like the same recording.
///
/// The rules, in the order they are applied:
///
/// * **The same content hash** means the same audio. Two files with the
///   same hash are the same recording, whatever their names say. This is
///   certain.
/// * **The same title and artist, and the same length**, means the same
///   recording from two sources: a rip and a download, or a copy in two
///   folders. This is very likely, but not certain — a song can be
///   re-recorded, and the two versions can be the same length.
/// * **The same title and artist, but different lengths**, means the same
///   song in different versions: a studio take and a live one, or an album
///   version and a single. These are *not* duplicates. A library that
///   keeps both is right, and a library that throws one away loses a track.
///
/// The last rule is the one that matters: a title alone is not evidence.
/// Two different songs can share a name, and a library that removed them
/// would lose music.
///
/// The comparison is made in one Chinese script, so a track in Traditional
/// and one in Simplified are recognised as the same song.
pub fn find_duplicate_tracks(db: &Connection) -> Result<Vec<DuplicatePair>> {
    let tracks = get_tracks(db)?;

    // The content hashes come separately: the track the library shows does
    // not carry one, and the hash is what tells two files apart.
    let mut hashes: std::collections::HashMap<i64, String> =
        std::collections::HashMap::new();
    for track in get_tracks_with_fingerprints(db)? {
        if let Some(hash) = track.content_hash {
            hashes.insert(track.id, hash);
        }
    }

    let mut pairs = Vec::new();

    for (index, first) in tracks.iter().enumerate() {
        for second in tracks.iter().skip(index + 1) {
            // The same file, listed twice, is a different kind of problem.
            if first.file_path == second.file_path {
                continue;
            }

            if normalise_for_comparison(&first.title) != normalise_for_comparison(&second.title) {
                continue;
            }
            if !artists_match(&first.artist_name, &second.artist_name) {
                continue;
            }

            let first_hash = hashes.get(&first.id);
            let same_hash = first_hash.is_some()
                && first_hash == hashes.get(&second.id);

            let length_difference = (first.duration - second.duration).abs();
            let same_length = length_difference <= 3.0;

            let certainty = if same_hash {
                DuplicateCertainty::Identical
            } else if same_length {
                DuplicateCertainty::SameRecording
            } else {
                DuplicateCertainty::DifferentRecording
            };

            let both_present = std::path::Path::new(&first.file_path).exists()
                && std::path::Path::new(&second.file_path).exists();

            // A pair where the two recordings are of a different length is
            // reported but not offered for removal: they are different
            // versions of the same song, and a library that threw one away
            // would lose a track.
            //
            // The pair is only reported when both files are there, because
            // otherwise the pair is really one missing file and a duplicate
            // of it — and the missing file is reported on its own.
            if certainty == DuplicateCertainty::DifferentRecording && !both_present {
                continue;
            }

            pairs.push(DuplicatePair {
                first_id: first.id,
                first_path: first.file_path.clone(),
                first_title: first.title.clone(),
                first_artist: first.artist_name.clone(),
                first_album: first.album_name.clone(),
                first_duration: first.duration,
                second_id: second.id,
                second_path: second.file_path.clone(),
                second_title: second.title.clone(),
                second_artist: second.artist_name.clone(),
                second_album: second.album_name.clone(),
                second_duration: second.duration,
                certainty,
                both_present,
            });
        }
    }

    Ok(pairs)
}

/// A title reduced to a comparable form.
///
/// The Chinese is put into one script, so that a track in Traditional and
/// one in Simplified are recognised as the same song.
fn normalise_for_comparison(text: &str) -> String {
    crate::script_convert::convert(text, crate::script_convert::ScriptTarget::Simplified)
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Whether two artist names name the same artist.
///
/// The names are written by hand and by different hands, so the same artist
/// appears as "温拿" and "温拿乐队", and two artists on the same song appear
/// as "陈奕迅 + 王菲" and "王菲 & 陈奕迅" — the same two people in a
/// different order.
///
/// So the names are compared three ways: as they are, as a set of the names
/// they are made of, and by one containing the other. The last is the loosest
/// and the most likely to be wrong, so it is the last one tried.
fn artists_match(left: &str, right: &str) -> bool {
    // The text is split before it is normalised, because the normalisation
    // drops the separators: after it, "陈奕迅 + 王菲" and "王菲 & 陈奕迅" are
    // two strings with no difference in kind, only in order.
    let left_parts = split_artists(left);
    let right_parts = split_artists(right);

    // The same people in a different order.
    if left_parts == right_parts {
        return true;
    }

    let left = left_parts.join("");
    let right = right_parts.join("");

    // One name is a longer form of the other.
    left.contains(&right) || right.contains(&left)
}

/// The names an artist credit is made of.
///
/// Split on the separators a credit uses, and on the conjunction words in the
/// languages this project ships.
fn split_artists(name: &str) -> Vec<String> {
    const SEPARATORS: [&str; 10] = ["&", "+", ",", "、", "，", " feat. ", " ft. ", " x ", "／", "/"];

    // The text is split before it is normalised, so the separators are still
    // there to be split on.
    let mut parts = vec![name.to_owned()];
    for separator in SEPARATORS {
        parts = parts
            .iter()
            .flat_map(|part| part.split(separator).map(|piece| piece.trim().to_owned()))
            .collect();
    }

    // The words that join two names without being one.
    const CONJUNCTIONS: [&str; 6] = ["and", "with", "et", "und", "与", "和"];
    let mut names: Vec<String> = parts
        .iter()
        .flat_map(|part| {
            part.split_whitespace()
                .filter(|word| !CONJUNCTIONS.contains(word))
                .map(normalise_for_comparison)
                .collect::<Vec<_>>()
        })
        .filter(|part| !part.is_empty())
        .collect();

    // Sorted, because the order of a credit is not the identity of the
    // artists: "陈奕迅 + 王菲" and "王菲 & 陈奕迅" are the same two people.
    names.sort();
    names
}

/// The library tracks whose file is not on the disk.
///
/// A file can go missing without the library being told: an external drive
/// that is not mounted, a folder that was moved, a file that was deleted
/// behind the application's back. The library still has the entry, and every
/// attempt to play it fails.
///
/// The check is on the file, not on the scan: a scan would remove the
/// entries, but the user may want to know what is missing before it is
/// removed, and a scan of a library whose drive is not mounted would empty
/// the whole library.
pub fn tracks_with_missing_files(db: &Connection) -> Result<Vec<PersistentTrack>> {
    Ok(get_tracks(db)?
        .into_iter()
        .filter(|track| !std::path::Path::new(&track.file_path).exists())
        .collect())
}

/// Remove library entries whose file is not on the disk.
///
/// The entries and their lyrics go; the albums and the artists stay, because
/// they may hold other tracks. Returns how many were removed.
pub fn remove_tracks_with_missing_files(db: &mut Connection) -> Result<usize> {
    let missing: Vec<i64> = get_tracks(db)?
        .into_iter()
        .filter(|track| !std::path::Path::new(&track.file_path).exists())
        .map(|track| track.id)
        .collect();

    if missing.is_empty() {
        return Ok(0);
    }

    let tx = db.transaction()?;
    for id in &missing {
        delete_tracks_fts_by_ids_tx(&[*id], &tx)?;
        // The lyrics of a file that is gone are gone too, but a standalone
        // lyricsfile that was never attached to a track is left alone.
        tx.execute("DELETE FROM lyricsfiles WHERE track_id = ?", [id])?;
        tx.execute("DELETE FROM tracks WHERE id = ?", [id])?;
    }
    // An album with no tracks left is not an album any more.
    tx.execute(
        "DELETE FROM albums WHERE id NOT IN (SELECT DISTINCT album_id FROM tracks)",
        [],
    )?;
    tx.commit()?;

    Ok(missing.len())
}

/// The track at a file path, if there is one.
///
/// Used after a scan, to find the entry the scan has just created.
pub fn find_track_by_path(file_path: &str, db: &Connection) -> Result<PersistentTrack> {
    let mut statement = db.prepare(indoc! {"
      SELECT
        tracks.id,
        tracks.file_path,
        tracks.file_name,
        tracks.title,
        tracks.album_id,
        tracks.artist_id,
        tracks.duration,
        tracks.track_number,
        tracks.instrumental,
        tracks.lyricsfile_id,
        albums.name AS album_name,
        albums.artist_name AS album_artist_name,
        artists.name AS artist_name
      FROM tracks
      JOIN albums ON tracks.album_id = albums.id
      JOIN artists ON tracks.artist_id = artists.id
      WHERE tracks.file_path = ?
      LIMIT 1
    "})?;

    let mut rows = statement.query([file_path])?;
    let row = rows.next()?.ok_or_else(|| {
        anyhow::anyhow!("no track at {file_path}")
    })?;

    Ok(PersistentTrack {
        id: row.get("id")?,
        file_path: row.get("file_path")?,
        file_name: row.get("file_name")?,
        title: row.get("title")?,
        album_name: row.get("album_name")?,
        album_artist_name: row.get("album_artist_name")?,
        album_id: row.get("album_id")?,
        artist_name: row.get("artist_name")?,
        artist_id: row.get("artist_id")?,
        image_path: None,
        track_number: row.get("track_number")?,
        txt_lyrics: None,
        lrc_lyrics: None,
        lyricsfile: None,
        lyricsfile_id: row.get("lyricsfile_id")?,
        duration: row.get("duration")?,
        instrumental: row.get("instrumental")?,
    })
}

/// The lyricsfile of a track, if it has one.
///
/// A narrow question, asked when a download is about to duplicate a track:
/// the user is told whether the library entry already has lyrics, because
/// that changes what they want to do.
pub fn get_track_lyricsfile(track_id: i64, db: &Connection) -> Result<Option<String>> {
    let mut statement = db.prepare("SELECT lyricsfile FROM lyricsfiles WHERE track_id = ? LIMIT 1")?;
    let mut rows = statement.query([track_id])?;
    if let Some(row) = rows.next()? {
        let lyricsfile: Option<String> = row.get(0)?;
        return Ok(lyricsfile);
    }
    Ok(None)
}

pub fn get_lyricsfile_by_id(
    lyricsfile_id: i64,
    db: &Connection,
) -> Result<Option<(i64, Option<i64>, String)>> {
    let result = db
        .query_row(
            "SELECT id, track_id, lyricsfile FROM lyricsfiles WHERE id = ?",
            [lyricsfile_id],
            |row| {
                let id: i64 = row.get(0)?;
                let track_id: Option<i64> = row.get(1)?;
                let lyricsfile: String = row.get(2)?;
                Ok((id, track_id, lyricsfile))
            },
        )
        .optional()?;
    Ok(result)
}

pub fn get_tracks(db: &Connection) -> Result<Vec<PersistentTrack>> {
    let query = indoc! {"
      SELECT
          tracks.id, tracks.file_path, tracks.file_name, tracks.title,
          artists.name AS artist_name, tracks.artist_id,
          albums.name AS album_name, albums.album_artist_name, tracks.album_id, tracks.duration, tracks.track_number,
          albums.image_path, lyricsfiles.id AS lyricsfile_id, lyricsfiles.lyricsfile, COALESCE(lyricsfiles.instrumental, 0) AS instrumental
      FROM tracks
      JOIN albums ON tracks.album_id = albums.id
      JOIN artists ON tracks.artist_id = artists.id
      LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
      ORDER BY tracks.title_lower ASC
  "};
    let mut statement = db.prepare(query)?;
    let mut rows = statement.query([])?;
    let mut tracks: Vec<PersistentTrack> = Vec::new();

    while let Some(row) = rows.next()? {
        let is_instrumental: bool = row.get("instrumental")?;

        let track = PersistentTrack {
            id: row.get("id")?,
            file_path: row.get("file_path")?,
            file_name: row.get("file_name")?,
            title: row.get("title")?,
            artist_name: row.get("artist_name")?,
            artist_id: row.get("artist_id")?,
            album_name: row.get("album_name")?,
            album_artist_name: row.get("album_artist_name")?,
            album_id: row.get("album_id")?,
            duration: row.get("duration")?,
            track_number: row.get("track_number")?,
            txt_lyrics: None,
            lrc_lyrics: None,
            lyricsfile: row.get("lyricsfile")?,
            lyricsfile_id: row.get("lyricsfile_id")?,
            image_path: row.get("image_path")?,
            instrumental: is_instrumental,
        };

        tracks.push(track);
    }

    Ok(tracks)
}

pub fn get_track_ids(
    synced_lyrics: bool,
    plain_lyrics: bool,
    instrumental: bool,
    no_lyrics: bool,
    db: &Connection,
) -> Result<Vec<i64>> {
    // Join with lyricsfiles table and use COALESCE to handle tracks without lyricsfiles
    let base_query = indoc! {"
        SELECT tracks.id 
        FROM tracks 
        LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
    "};

    let mut included_categories: Vec<String> = Vec::new();
    if synced_lyrics {
        included_categories.push(
            "(COALESCE(lyricsfiles.has_synced_lyrics, 0) = 1 AND COALESCE(lyricsfiles.instrumental, 0) = 0)".to_string(),
        );
    }
    if plain_lyrics {
        included_categories.push(
            "(COALESCE(lyricsfiles.has_plain_lyrics, 0) = 1 AND COALESCE(lyricsfiles.has_synced_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0)".to_string(),
        );
    }
    if instrumental {
        included_categories.push("COALESCE(lyricsfiles.instrumental, 0) = 1".to_string());
    }
    if no_lyrics {
        included_categories.push(
            "(COALESCE(lyricsfiles.has_plain_lyrics, 0) = 0 AND COALESCE(lyricsfiles.has_synced_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0)".to_string(),
        );
    }

    let where_clause = if included_categories.len() == 4 {
        String::new()
    } else if included_categories.is_empty() {
        " WHERE 0".to_string()
    } else {
        format!(" WHERE ({})", included_categories.join(" OR "))
    };

    let full_query = format!(
        "{}{} ORDER BY tracks.title_lower ASC",
        base_query, where_clause
    );

    let mut statement = db.prepare(&full_query)?;
    let mut rows = statement.query([])?;
    let mut track_ids: Vec<i64> = Vec::new();

    while let Some(row) = rows.next()? {
        track_ids.push(row.get("id")?);
    }

    Ok(track_ids)
}

pub fn get_search_track_ids(
    query_str: &String,
    synced_lyrics: bool,
    plain_lyrics: bool,
    instrumental: bool,
    no_lyrics: bool,
    db: &Connection,
) -> Result<Vec<i64>> {
    let use_fts = fts5_enabled(db);

    // A short query cannot use trigram, so it falls back to LIKE even when the
    // index exists. The base query has to follow the *effective* path, not just
    // `fts5_enabled`: the FTS form takes one bound parameter and the LIKE form
    // three, so picking the wrong one fails at query time with a parameter-count
    // error.
    let fts_query = if use_fts {
        crate::utils::build_fts_query(query_str)
    } else {
        None
    };
    let use_fts_path = fts_query.is_some();

    let base_query = if use_fts_path {
        indoc::indoc! {"
          SELECT tracks.id
          FROM tracks_fts
          JOIN tracks ON tracks_fts.track_id = tracks.id
          LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
          WHERE tracks_fts MATCH ?
        "}
    } else {
        indoc::indoc! {"
          SELECT tracks.id
          FROM tracks
          JOIN artists ON tracks.artist_id = artists.id
          JOIN albums ON tracks.album_id = albums.id
          LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
          WHERE (artists.name_lower LIKE ?
          OR albums.name_lower LIKE ?
          OR tracks.title_lower LIKE ?)
        "}
    };

    let mut included_categories: Vec<String> = Vec::new();
    if synced_lyrics {
        included_categories.push(
            "(COALESCE(lyricsfiles.has_synced_lyrics, 0) = 1 AND COALESCE(lyricsfiles.instrumental, 0) = 0)".to_string(),
        );
    }
    if plain_lyrics {
        included_categories.push(
            "(COALESCE(lyricsfiles.has_plain_lyrics, 0) = 1 AND COALESCE(lyricsfiles.has_synced_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0)".to_string(),
        );
    }
    if instrumental {
        included_categories.push("COALESCE(lyricsfiles.instrumental, 0) = 1".to_string());
    }
    if no_lyrics {
        included_categories.push(
            "(COALESCE(lyricsfiles.has_plain_lyrics, 0) = 0 AND COALESCE(lyricsfiles.has_synced_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0)".to_string(),
        );
    }

    let where_clause = if included_categories.len() == 4 {
        String::new()
    } else if included_categories.is_empty() {
        if use_fts {
            " AND 0".to_string()
        } else {
            " AND 0".to_string()
        }
    } else {
        format!(" AND ({})", included_categories.join(" OR "))
    };

    let full_query = if use_fts_path {
        format!("{}{} ORDER BY tracks_fts.rank", base_query, where_clause)
    } else {
        format!("{}{} ORDER BY tracks.title_lower ASC", base_query, where_clause)
    };

    let mut statement = db.prepare(&full_query)?;

    let mut rows = if let Some(fts_query) = fts_query {
        statement.query(params![fts_query])?
    } else {
        let formatted_query_str = format!("%{}%", prepare_input(query_str));
        statement.query(params![
            formatted_query_str,
            formatted_query_str,
            formatted_query_str
        ])?
    };

    let mut track_ids: Vec<i64> = Vec::new();

    while let Some(row) = rows.next()? {
        track_ids.push(row.get("id")?);
    }

    Ok(track_ids)
}

pub fn get_albums(db: &Connection) -> Result<Vec<PersistentAlbum>> {
    let mut statement = db.prepare(indoc! {"
      SELECT albums.id, albums.name, albums.album_artist_name AS album_artist_name, albums.album_artist_name,
          COUNT(tracks.id) AS tracks_count
      FROM albums
      JOIN tracks ON tracks.album_id = albums.id
      GROUP BY albums.id, albums.name, albums.album_artist_name
      ORDER BY albums.name_lower ASC
  "})?;
    let mut rows = statement.query([])?;
    let mut albums: Vec<PersistentAlbum> = Vec::new();

    while let Some(row) = rows.next()? {
        let album = PersistentAlbum {
            id: row.get("id")?,
            name: row.get("name")?,
            image_path: row.get("image_path")?,
            artist_name: row.get("album_artist_name")?,
            album_artist_name: row.get("album_artist_name")?,
            tracks_count: row.get("tracks_count")?,
        };

        albums.push(album);
    }

    Ok(albums)
}

pub fn get_album_by_id(id: i64, db: &Connection) -> Result<PersistentAlbum> {
    let mut statement = db.prepare(indoc! {"
    SELECT
      albums.id,
      albums.name,
      albums.album_artist_name,
      albums.image_path,
      COUNT(tracks.id) AS tracks_count
    FROM albums
    JOIN tracks ON tracks.album_id = albums.id
    WHERE albums.id = ?
    GROUP BY
      albums.id,
      albums.name,
      albums.album_artist_name,
      albums.image_path
    LIMIT 1
  "})?;
    let row = statement.query_row([id], |row| {
        Ok(PersistentAlbum {
            id: row.get("id")?,
            name: row.get("name")?,
            image_path: row.get("image_path")?,
            artist_name: row.get("album_artist_name")?,
            album_artist_name: row.get("album_artist_name")?,
            tracks_count: row.get("tracks_count")?,
        })
    })?;
    Ok(row)
}

pub fn get_album_ids(db: &Connection) -> Result<Vec<i64>> {
    let mut statement = db.prepare("SELECT id FROM albums ORDER BY name_lower ASC")?;
    let mut rows = statement.query([])?;
    let mut album_ids: Vec<i64> = Vec::new();

    while let Some(row) = rows.next()? {
        album_ids.push(row.get("id")?);
    }

    Ok(album_ids)
}

pub fn get_search_album_ids(query_str: &String, db: &Connection) -> Result<Vec<i64>> {
    let use_fts = fts5_enabled(db);

    let fts_query = if use_fts {
        crate::utils::build_fts_query(query_str)
    } else {
        None
    };

    let mut statement = if fts_query.is_some() {
        db.prepare(indoc! {"
          SELECT albums.id
          FROM albums_fts
          JOIN albums ON albums_fts.album_id = albums.id
          WHERE albums_fts MATCH ?
          ORDER BY albums_fts.rank
        "})?
    } else {
        db.prepare("SELECT id FROM albums WHERE name_lower LIKE ? ORDER BY name_lower ASC")?
    };

    let mut rows = if let Some(fts_query) = fts_query {
        statement.query([fts_query])?
    } else {
        let formatted = format!("%{}%", prepare_input(query_str));
        statement.query([formatted])?
    };

    let mut album_ids: Vec<i64> = Vec::new();
    while let Some(row) = rows.next()? {
        album_ids.push(row.get("id")?);
    }

    Ok(album_ids)
}

pub fn get_artists(db: &Connection) -> Result<Vec<PersistentArtist>> {
    let mut statement = db.prepare(indoc! {"
    SELECT artists.id, artists.name AS name, COUNT(tracks.id) AS tracks_count
    FROM artists
    JOIN tracks ON tracks.artist_id = artists.id
    GROUP BY artists.id, artists.name
    ORDER BY artists.name_lower ASC
  "})?;
    let mut rows = statement.query([])?;
    let mut artists: Vec<PersistentArtist> = Vec::new();

    while let Some(row) = rows.next()? {
        let artist = PersistentArtist {
            id: row.get("id")?,
            name: row.get("name")?,
            // albums_count: row.get("albums_count")?,
            tracks_count: row.get("tracks_count")?,
        };

        artists.push(artist);
    }

    Ok(artists)
}

pub fn get_artist_by_id(id: i64, db: &Connection) -> Result<PersistentArtist> {
    let mut statement = db.prepare(indoc! {"
    SELECT artists.id,
      artists.name AS name,
      COUNT(tracks.id) AS tracks_count
    FROM artists
    JOIN tracks ON tracks.artist_id = artists.id
    WHERE artists.id = ?
    GROUP BY artists.id, artists.name
    LIMIT 1
  "})?;
    let row = statement.query_row([id], |row| {
        Ok(PersistentArtist {
            id: row.get("id")?,
            name: row.get("name")?,
            // albums_count: row.get("albums_count")?,
            tracks_count: row.get("tracks_count")?,
        })
    })?;
    Ok(row)
}

pub fn get_artist_ids(db: &Connection) -> Result<Vec<i64>> {
    let mut statement = db.prepare("SELECT id FROM artists ORDER BY name_lower ASC")?;
    let mut rows = statement.query([])?;
    let mut artist_ids: Vec<i64> = Vec::new();

    while let Some(row) = rows.next()? {
        artist_ids.push(row.get("id")?);
    }

    Ok(artist_ids)
}

pub fn get_search_artist_ids(query_str: &String, db: &Connection) -> Result<Vec<i64>> {
    let use_fts = fts5_enabled(db);

    let fts_query = if use_fts {
        crate::utils::build_fts_query(query_str)
    } else {
        None
    };

    let mut statement = if fts_query.is_some() {
        db.prepare(indoc! {"
          SELECT artists.id
          FROM artists_fts
          JOIN artists ON artists_fts.artist_id = artists.id
          WHERE artists_fts MATCH ?
          ORDER BY artists_fts.rank
        "})?
    } else {
        db.prepare("SELECT id FROM artists WHERE name_lower LIKE ? ORDER BY name_lower ASC")?
    };

    let mut rows = if let Some(fts_query) = fts_query {
        statement.query([fts_query])?
    } else {
        let formatted = format!("%{}%", prepare_input(query_str));
        statement.query([formatted])?
    };

    let mut artist_ids: Vec<i64> = Vec::new();
    while let Some(row) = rows.next()? {
        artist_ids.push(row.get("id")?);
    }

    Ok(artist_ids)
}

pub fn get_album_tracks(album_id: i64, db: &Connection) -> Result<Vec<PersistentTrack>> {
    let mut statement = db.prepare(indoc! {"
    SELECT
      tracks.id,
      tracks.file_path,
      tracks.file_name,
      tracks.title,
      artists.name AS artist_name,
      tracks.artist_id,
      albums.name AS album_name,
      albums.album_artist_name,
      tracks.album_id,
      tracks.duration,
      tracks.track_number,
      albums.image_path,
      lyricsfiles.id AS lyricsfile_id,
      lyricsfiles.lyricsfile,
      COALESCE(lyricsfiles.instrumental, 0) AS instrumental
    FROM tracks
    JOIN albums ON tracks.album_id = albums.id
    JOIN artists ON tracks.artist_id = artists.id
    LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
    WHERE tracks.album_id = ?
    ORDER BY tracks.track_number ASC
  "})?;
    let mut rows = statement.query([album_id])?;
    let mut tracks: Vec<PersistentTrack> = Vec::new();

    while let Some(row) = rows.next()? {
        let is_instrumental: bool = row.get("instrumental")?;

        let track = PersistentTrack {
            id: row.get("id")?,
            file_path: row.get("file_path")?,
            file_name: row.get("file_name")?,
            title: row.get("title")?,
            artist_name: row.get("artist_name")?,
            album_artist_name: row.get("album_artist_name")?,
            album_name: row.get("album_name")?,
            album_id: row.get("album_id")?,
            artist_id: row.get("artist_id")?,
            duration: row.get("duration")?,
            track_number: row.get("track_number")?,
            txt_lyrics: None,
            lrc_lyrics: None,
            lyricsfile: row.get("lyricsfile")?,
            lyricsfile_id: row.get("lyricsfile_id")?,
            image_path: row.get("image_path")?,
            instrumental: is_instrumental,
        };

        tracks.push(track);
    }

    Ok(tracks)
}

pub fn get_album_track_ids(
    album_id: i64,
    without_plain_lyrics: bool,
    without_synced_lyrics: bool,
    db: &Connection,
) -> Result<Vec<i64>> {
    let base_query = indoc! {"
      SELECT tracks.id
      FROM tracks
      JOIN albums ON tracks.album_id = albums.id
      LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
      WHERE tracks.album_id = ?"};

    let lyrics_conditions = match (without_plain_lyrics, without_synced_lyrics) {
        (true, true) => {
            " AND COALESCE(lyricsfiles.has_plain_lyrics, 0) = 0 AND COALESCE(lyricsfiles.has_synced_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0"
        }
        (true, false) => " AND COALESCE(lyricsfiles.has_plain_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0",
        (false, true) => " AND COALESCE(lyricsfiles.has_synced_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0",
        (false, false) => "",
    };

    let full_query = format!(
        "{}{} ORDER BY tracks.track_number ASC",
        base_query, lyrics_conditions
    );

    let mut statement = db.prepare(&full_query)?;
    let mut rows = statement.query([album_id])?;
    let mut tracks: Vec<i64> = Vec::new();

    while let Some(row) = rows.next()? {
        tracks.push(row.get("id")?);
    }

    Ok(tracks)
}

pub fn get_artist_tracks(artist_id: i64, db: &Connection) -> Result<Vec<PersistentTrack>> {
    let mut statement = db.prepare(indoc! {"
      SELECT tracks.id, tracks.file_path, tracks.file_name, tracks.title, artists.name AS artist_name,
        tracks.artist_id, albums.name AS album_name, albums.album_artist_name, tracks.album_id, tracks.duration, tracks.track_number,
        albums.image_path, lyricsfiles.id AS lyricsfile_id, lyricsfiles.lyricsfile, COALESCE(lyricsfiles.instrumental, 0) AS instrumental
      FROM tracks
      JOIN albums ON tracks.album_id = albums.id
      JOIN artists ON tracks.artist_id = artists.id
      LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
      WHERE tracks.artist_id = ?
      ORDER BY albums.name_lower ASC, tracks.track_number ASC
  "})?;
    let mut rows = statement.query([artist_id])?;
    let mut tracks: Vec<PersistentTrack> = Vec::new();

    while let Some(row) = rows.next()? {
        let is_instrumental: bool = row.get("instrumental")?;

        let track = PersistentTrack {
            id: row.get("id")?,
            file_path: row.get("file_path")?,
            file_name: row.get("file_name")?,
            title: row.get("title")?,
            artist_name: row.get("artist_name")?,
            artist_id: row.get("artist_id")?,
            album_name: row.get("album_name")?,
            album_artist_name: row.get("album_artist_name")?,
            album_id: row.get("album_id")?,
            duration: row.get("duration")?,
            track_number: row.get("track_number")?,
            txt_lyrics: None,
            lrc_lyrics: None,
            lyricsfile: row.get("lyricsfile")?,
            lyricsfile_id: row.get("lyricsfile_id")?,
            image_path: row.get("image_path")?,
            instrumental: is_instrumental,
        };

        tracks.push(track);
    }

    Ok(tracks)
}

pub fn get_artist_track_ids(
    artist_id: i64,
    without_plain_lyrics: bool,
    without_synced_lyrics: bool,
    db: &Connection,
) -> Result<Vec<i64>> {
    let base_query = indoc! {"
      SELECT tracks.id
      FROM tracks
      JOIN albums ON tracks.album_id = albums.id
      JOIN artists ON tracks.artist_id = artists.id
      LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
      WHERE tracks.artist_id = ?"};

    let lyrics_conditions = match (without_plain_lyrics, without_synced_lyrics) {
        (true, true) => {
            " AND COALESCE(lyricsfiles.has_plain_lyrics, 0) = 0 AND COALESCE(lyricsfiles.has_synced_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0"
        }
        (true, false) => " AND COALESCE(lyricsfiles.has_plain_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0",
        (false, true) => " AND COALESCE(lyricsfiles.has_synced_lyrics, 0) = 0 AND COALESCE(lyricsfiles.instrumental, 0) = 0",
        (false, false) => "",
    };

    let full_query = format!(
        "{}{} ORDER BY albums.name_lower ASC, tracks.track_number ASC",
        base_query, lyrics_conditions
    );

    let mut statement = db.prepare(&full_query)?;
    let mut rows = statement.query([artist_id])?;
    let mut tracks: Vec<i64> = Vec::new();

    while let Some(row) = rows.next()? {
        tracks.push(row.get("id")?);
    }

    Ok(tracks)
}

pub fn clean_library(db: &Connection) -> Result<()> {
    db.execute("DELETE FROM tracks WHERE 1", ())?;
    db.execute("DELETE FROM albums WHERE 1", ())?;
    db.execute("DELETE FROM artists WHERE 1", ())?;
    db.execute("DELETE FROM tracks_fts", ())?;
    db.execute("DELETE FROM albums_fts", ())?;
    db.execute("DELETE FROM artists_fts", ())?;
    Ok(())
}

// ============================================================================
// FTS5 sync helpers
// ============================================================================

pub fn insert_track_fts_tx(
    track_id: i64,
    title: &str,
    artist_name: &str,
    album_name: &str,
    tx: &rusqlite::Transaction,
) -> Result<()> {
    tx.execute(
        "INSERT INTO tracks_fts (track_id, title, artist_name, album_name) VALUES (?, ?, ?, ?)",
        (track_id, title, artist_name, album_name),
    )?;
    Ok(())
}

pub fn delete_tracks_fts_by_ids_tx(ids: &[i64], tx: &rusqlite::Transaction) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders: Vec<String> = (0..ids.len()).map(|i| format!("?{}", i + 1)).collect();
    let query = format!(
        "DELETE FROM tracks_fts WHERE track_id IN ({})",
        placeholders.join(", ")
    );
    let mut statement = tx.prepare(&query)?;
    let params: Vec<rusqlite::types::Value> = ids.iter().map(|&id| id.into()).collect();
    statement.execute(rusqlite::params_from_iter(params.iter()))?;
    Ok(())
}

pub fn insert_album_fts_tx(
    album_id: i64,
    album_name: &str,
    album_artist_name: &str,
    tx: &rusqlite::Transaction,
) -> Result<()> {
    tx.execute(
        "INSERT INTO albums_fts (album_id, album_name, album_artist_name) VALUES (?, ?, ?)",
        (album_id, album_name, album_artist_name),
    )?;
    Ok(())
}

pub fn delete_albums_fts_by_ids_tx(ids: &[i64], tx: &rusqlite::Transaction) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders: Vec<String> = (0..ids.len()).map(|i| format!("?{}", i + 1)).collect();
    let query = format!(
        "DELETE FROM albums_fts WHERE album_id IN ({})",
        placeholders.join(", ")
    );
    let mut statement = tx.prepare(&query)?;
    let params: Vec<rusqlite::types::Value> = ids.iter().map(|&id| id.into()).collect();
    statement.execute(rusqlite::params_from_iter(params.iter()))?;
    Ok(())
}

pub fn insert_artist_fts_tx(
    artist_id: i64,
    artist_name: &str,
    tx: &rusqlite::Transaction,
) -> Result<()> {
    tx.execute(
        "INSERT INTO artists_fts (artist_id, artist_name) VALUES (?, ?)",
        (artist_id, artist_name),
    )?;
    Ok(())
}

pub fn delete_artists_fts_by_ids_tx(ids: &[i64], tx: &rusqlite::Transaction) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders: Vec<String> = (0..ids.len()).map(|i| format!("?{}", i + 1)).collect();
    let query = format!(
        "DELETE FROM artists_fts WHERE artist_id IN ({})",
        placeholders.join(", ")
    );
    let mut statement = tx.prepare(&query)?;
    let params: Vec<rusqlite::types::Value> = ids.iter().map(|&id| id.into()).collect();
    statement.execute(rusqlite::params_from_iter(params.iter()))?;
    Ok(())
}

/// Get all tracks with their fingerprint data for comparison
pub fn get_tracks_with_fingerprints(db: &Connection) -> Result<Vec<DbTrack>> {
    let mut statement =
        db.prepare("SELECT id, file_path, file_size, modified_time, content_hash FROM tracks")?;
    let mut rows = statement.query([])?;
    let mut tracks = Vec::new();

    while let Some(row) = rows.next()? {
        tracks.push(DbTrack {
            id: row.get("id")?,
            file_path: row.get("file_path")?,
            file_size: row.get("file_size")?,
            modified_time: row.get("modified_time")?,
            content_hash: row.get("content_hash")?,
        });
    }

    Ok(tracks)
}

/// Delete tracks by their IDs (batch operation)
pub fn delete_tracks_by_ids(ids: &[i64], conn: &Connection) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }

    // Delete from FTS index first
    {
        let placeholders: Vec<String> = (0..ids.len()).map(|i| format!("?{}", i + 1)).collect();
        let query = format!(
            "DELETE FROM tracks_fts WHERE track_id IN ({})",
            placeholders.join(", ")
        );
        let mut statement = conn.prepare(&query)?;
        let params: Vec<rusqlite::types::Value> = ids.iter().map(|&id| id.into()).collect();
        statement.execute(rusqlite::params_from_iter(params.iter()))?;
    }

    // Delete tracks
    {
        let placeholders: Vec<String> = (0..ids.len()).map(|i| format!("?{}", i + 1)).collect();
        let query = format!(
            "DELETE FROM tracks WHERE id IN ({})",
            placeholders.join(", ")
        );
        let mut statement = conn.prepare(&query)?;
        let params: Vec<rusqlite::types::Value> = ids.iter().map(|&id| id.into()).collect();
        statement.execute(rusqlite::params_from_iter(params.iter()))?;
    }

    Ok(())
}

/// Update a track's file path (for move/rename detection)
pub fn update_track_path(
    track_id: i64,
    new_path: &std::path::Path,
    conn: &Connection,
) -> Result<()> {
    let new_path_str = new_path.to_string_lossy().to_string();
    let file_name = new_path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();

    conn.execute(
        "UPDATE tracks SET file_path = ?, file_name = ? WHERE id = ?",
        (new_path_str, file_name, track_id),
    )?;

    Ok(())
}

// ============================================================================
// Scan-related database operations (transaction-based)
// ============================================================================

const SCAN_STATUS_PENDING: i32 = 0;
const SCAN_STATUS_PROCESSED: i32 = 1;

/// Track info for scan operations
#[derive(Debug)]
pub struct ScanTrackInfo {
    pub id: i64,
    pub file_path: String,
}

/// Find track by fingerprint (mtime + size) - for scan operations
pub fn find_track_by_fingerprint_tx(
    modified_time: i64,
    file_size: i64,
    tx: &rusqlite::Transaction,
) -> Result<Option<ScanTrackInfo>> {
    let mut stmt = tx.prepare(
        "SELECT id, file_path FROM tracks WHERE modified_time = ? AND file_size = ? LIMIT 1",
    )?;

    let result = stmt
        .query_row([modified_time, file_size], |row| {
            Ok(ScanTrackInfo {
                id: row.get(0)?,
                file_path: row.get(1)?,
            })
        })
        .optional()?;

    Ok(result)
}

/// Find track by content hash - for scan operations
pub fn find_track_by_hash_tx(
    hash: &str,
    tx: &rusqlite::Transaction,
) -> Result<Option<ScanTrackInfo>> {
    let mut stmt = tx.prepare("SELECT id, file_path FROM tracks WHERE content_hash = ? LIMIT 1")?;

    let result = stmt
        .query_row([hash], |row| {
            Ok(ScanTrackInfo {
                id: row.get(0)?,
                file_path: row.get(1)?,
            })
        })
        .optional()?;

    Ok(result)
}

/// Mark track as processed during scan
pub fn mark_track_processed_tx(track_id: i64, tx: &rusqlite::Transaction) -> Result<()> {
    tx.execute(
        "UPDATE tracks SET scan_status = ? WHERE id = ?",
        [SCAN_STATUS_PROCESSED, track_id as i32],
    )?;
    Ok(())
}

/// Update track path after move (fingerprint already matches)
pub fn update_track_path_tx(
    track_id: i64,
    new_path: &str,
    tx: &rusqlite::Transaction,
) -> Result<()> {
    let file_name = std::path::Path::new(new_path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();

    tx.execute(
        "UPDATE tracks SET file_path = ?, file_name = ?, scan_status = ? WHERE id = ?",
        (new_path, file_name, SCAN_STATUS_PROCESSED, track_id),
    )?;

    Ok(())
}

/// Update track path and fingerprint after move
/// Update the metadata of a track that is already in the library.
///
/// Used when a download replaces a file: the path is the same, so the entry
/// stays, but the title, the artist and the album may have changed.
pub fn update_track_metadata_tx(
    track_id: i64,
    metadata: &crate::scanner::metadata::TrackMetadata,
    artist_id: i64,
    album_id: i64,
    tx: &rusqlite::Transaction,
) -> Result<()> {
    use crate::utils::prepare_input;

    tx.execute(
        "UPDATE tracks SET title = ?, title_lower = ?, album_id = ?, artist_id = ?, duration = ?, track_number = ? WHERE id = ?",
        params![
            &metadata.title,
            prepare_input(&metadata.title),
            album_id,
            artist_id,
            metadata.duration,
            metadata.track_number,
            track_id,
        ],
    )?;

    Ok(())
}

pub fn update_track_path_and_fingerprint_tx(
    track_id: i64,
    new_path: &str,
    file_size: i64,
    modified_time: i64,
    content_hash: &str,
    tx: &rusqlite::Transaction,
) -> Result<()> {
    let file_name = std::path::Path::new(new_path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();

    tx.execute(
        "UPDATE tracks SET file_path = ?, file_name = ?, file_size = ?, modified_time = ?, content_hash = ?, scan_status = ? WHERE id = ?",
        (new_path, file_name, file_size, modified_time, content_hash, SCAN_STATUS_PROCESSED, track_id),
    )?;

    Ok(())
}

/// Insert a new track from metadata during scan
/// Note: Lyrics are stored separately in the lyricsfiles table via upsert_lyricsfile_for_track_tx
pub fn insert_track_from_metadata_tx(
    metadata: &crate::scanner::metadata::TrackMetadata,
    _lyrics: &crate::scanner::metadata::LyricsInfo,
    file_size: i64,
    modified_time: i64,
    content_hash: &str,
    artist_id: i64,
    album_id: i64,
    tx: &rusqlite::Transaction,
) -> Result<i64> {
    use crate::utils::prepare_input;

    tx.execute(
        "INSERT INTO tracks (file_path, file_name, title, title_lower, album_id, artist_id, duration, track_number, file_size, modified_time, content_hash, scan_status) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            &metadata.file_path,
            &metadata.file_name,
            &metadata.title,
            prepare_input(&metadata.title),
            album_id,
            artist_id,
            metadata.duration,
            metadata.track_number,
            file_size,
            modified_time,
            content_hash,
            SCAN_STATUS_PROCESSED,
        ],
    )?;

    Ok(tx.last_insert_rowid())
}

/// Delete tracks that weren't processed during scan and clean up orphaned albums/artists
pub fn delete_unprocessed_tracks(conn: &mut Connection) -> Result<usize> {
    let tx = conn.transaction()?;

    // Collect IDs of tracks that will be deleted for FTS cleanup
    let pending_track_ids: Vec<i64> = {
        let mut stmt = tx.prepare("SELECT id FROM tracks WHERE scan_status = ?")?;
        let rows = stmt.query_map([SCAN_STATUS_PENDING], |r| r.get(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };

    // Delete from tracks_fts first
    if !pending_track_ids.is_empty() {
        delete_tracks_fts_by_ids_tx(&pending_track_ids, &tx)?;
    }

    // Delete tracks that weren't seen during this scan
    let deleted_count = tx.execute(
        "DELETE FROM tracks WHERE scan_status = ?",
        [SCAN_STATUS_PENDING],
    )?;

    // Collect orphaned album/artist IDs before deleting them
    let orphaned_album_ids: Vec<i64> = {
        let mut stmt = tx.prepare("SELECT id FROM albums WHERE id NOT IN (SELECT DISTINCT album_id FROM tracks)")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };

    let orphaned_artist_ids: Vec<i64> = {
        let mut stmt = tx.prepare("SELECT id FROM artists WHERE id NOT IN (SELECT DISTINCT artist_id FROM tracks)")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };

    // Clean up orphaned albums and artists
    tx.execute(
        "DELETE FROM albums WHERE id NOT IN (SELECT DISTINCT album_id FROM tracks)",
        [],
    )?;

    tx.execute(
        "DELETE FROM artists WHERE id NOT IN (SELECT DISTINCT artist_id FROM tracks)",
        [],
    )?;

    // Sync FTS indexes
    if !orphaned_album_ids.is_empty() {
        delete_albums_fts_by_ids_tx(&orphaned_album_ids, &tx)?;
    }
    if !orphaned_artist_ids.is_empty() {
        delete_artists_fts_by_ids_tx(&orphaned_artist_ids, &tx)?;
    }

    tx.commit()?;

    Ok(deleted_count)
}

/// Mark all tracks as pending before scan
pub fn mark_all_tracks_pending(conn: &mut Connection) -> Result<()> {
    conn.execute("UPDATE tracks SET scan_status = ?", [SCAN_STATUS_PENDING])?;
    Ok(())
}

// Transaction-based versions of artist/album functions for scan operations

/// Find artist by name (transaction version)
pub fn find_artist_tx(name: &str, tx: &rusqlite::Transaction) -> Result<i64> {
    let mut statement = tx.prepare("SELECT id FROM artists WHERE name = ?")?;
    let id: i64 = statement.query_row([name], |r| r.get(0))?;
    Ok(id)
}

/// Add new artist (transaction version)
pub fn add_artist_tx(name: &str, tx: &rusqlite::Transaction) -> Result<i64> {
    let mut statement = tx.prepare("INSERT INTO artists (name, name_lower) VALUES (?, ?)")?;
    let row_id = statement.insert((name, prepare_input(name)))?;
    insert_artist_fts_tx(row_id, prepare_input(name).as_str(), tx)?;
    Ok(row_id)
}

/// Find album by name and artist (transaction version)
pub fn find_album_tx(
    name: &str,
    album_artist_name: &str,
    tx: &rusqlite::Transaction,
) -> Result<i64> {
    let mut statement =
        tx.prepare("SELECT id FROM albums WHERE name = ? AND album_artist_name = ?")?;
    let id: i64 = statement.query_row((name, album_artist_name), |r| r.get(0))?;
    Ok(id)
}

/// Add new album (transaction version)
/// Albums that do not have a cover yet, with a representative track file.
///
/// Returns `(album_id, any_track_path)` so the caller can extract art from one
/// of the album's files. Used to backfill libraries scanned before covers were
/// extracted; a normal rescan only fills in albums it creates.
pub fn albums_without_cover(db: &Connection) -> Result<Vec<(i64, String)>> {
    let mut statement = db.prepare(
        "SELECT albums.id, tracks.file_path
         FROM albums
         JOIN tracks ON tracks.album_id = albums.id
         WHERE albums.image_path IS NULL OR TRIM(albums.image_path) = ''
         GROUP BY albums.id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// True when the album has no cover recorded yet.
///
/// Used by the scanner to avoid re-extracting embedded art for every track of a
/// known album: once a cover is stored, later tracks skip the work.
pub fn album_missing_cover_tx(album_id: i64, tx: &rusqlite::Transaction) -> Result<bool> {
    let mut statement = tx.prepare("SELECT image_path FROM albums WHERE id = ?")?;
    let image: Option<Option<String>> = statement
        .query_row([album_id], |r| r.get::<_, Option<String>>(0))
        .optional()?;
    Ok(match image {
        Some(Some(path)) => path.trim().is_empty(),
        Some(None) => true,
        None => true,
    })
}

/// Record the cached cover file name for an album.
pub fn set_album_image_tx(
    album_id: i64,
    image_name: &str,
    tx: &rusqlite::Transaction,
) -> Result<()> {
    tx.execute(
        "UPDATE albums SET image_path = ?1 WHERE id = ?2",
        rusqlite::params![image_name, album_id],
    )?;
    Ok(())
}

/// Set the cover for an album by id, outside a scan transaction.
pub fn set_album_image(album_id: i64, image_name: &str, db: &Connection) -> Result<()> {
    db.execute(
        "UPDATE albums SET image_path = ?1 WHERE id = ?2",
        rusqlite::params![image_name, album_id],
    )?;
    Ok(())
}

pub fn add_album_tx(
    name: &str,
    album_artist_name: &str,
    tx: &rusqlite::Transaction,
) -> Result<i64> {
    let mut statement = tx.prepare("INSERT INTO albums (name, name_lower, album_artist_name, album_artist_name_lower) VALUES (?, ?, ?, ?)")?;
    let row_id = statement.insert((
        name,
        prepare_input(name),
        album_artist_name,
        prepare_input(album_artist_name),
    ))?;
    insert_album_fts_tx(row_id, prepare_input(name).as_str(), prepare_input(album_artist_name).as_str(), tx)?;
    Ok(row_id)
}

/// Get track IDs that have lyrics (for mass export)
pub fn get_track_ids_with_lyrics(db: &Connection) -> Result<Vec<i64>> {
    let mut statement = db.prepare(indoc! {"
      SELECT tracks.id
      FROM tracks
      JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
      WHERE lyricsfiles.has_plain_lyrics = 1
         OR lyricsfiles.has_synced_lyrics = 1
      ORDER BY tracks.artist_id ASC, tracks.album_id ASC, tracks.track_number ASC NULLS LAST
    "})?;

    let mut rows = statement.query([])?;
    let mut track_ids: Vec<i64> = Vec::new();

    while let Some(row) = rows.next()? {
        track_ids.push(row.get("id")?);
    }

    Ok(track_ids)
}

/// Find orphaned lyricsfiles by track metadata (for reattachment during scan)
/// Returns the lyricsfile_id if found, None otherwise
/// Matches on normalized title, artist, album, and duration within ±2 seconds
pub fn find_orphaned_lyricsfile_tx(
    title: &str,
    artist_name: &str,
    album_name: &str,
    duration: f64,
    tx: &rusqlite::Transaction,
) -> Result<Option<i64>> {
    let result = tx
        .query_row(
            indoc! {"
                SELECT id FROM lyricsfiles
                WHERE track_id IS NULL
                  AND track_title_lower = ?
                  AND track_artist_name_lower = ?
                  AND track_album_name_lower = ?
                  AND ABS(track_duration - ?) <= 2.0
                LIMIT 1
            "},
            [
                prepare_input(title),
                prepare_input(artist_name),
                prepare_input(album_name),
                duration.to_string(),
            ],
            |row| {
                let id: i64 = row.get(0)?;
                Ok(id)
            },
        )
        .optional()?;
    Ok(result)
}

/// Reattach an orphaned lyricsfile to a track
pub fn reattach_lyricsfile_to_track_tx(
    lyricsfile_id: i64,
    track_id: i64,
    tx: &rusqlite::Transaction,
) -> Result<()> {
    tx.execute(
        "UPDATE lyricsfiles SET track_id = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        (track_id, lyricsfile_id),
    )?;
    Ok(())
}

/// Find tracks by metadata for matching against LRCLIB tracks
pub fn find_tracks_by_metadata(
    title: &str,
    artist_name: Option<&str>,
    album_name: Option<&str>,
    duration: Option<f64>,
    db: &Connection,
) -> Result<Vec<PersistentTrack>> {
    let normalized_title = prepare_input(title);
    let normalized_artist = artist_name.map(prepare_input);
    let normalized_album = album_name.map(prepare_input);

    // Build the query based on available metadata
    // We search using normalized (name_lower) fields for case-insensitive matching
    let mut conditions = vec!["tracks.title_lower = ?"];
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(normalized_title.clone())];

    if let Some(ref artist) = normalized_artist {
        conditions.push("artists.name_lower = ?");
        params.push(Box::new(artist.clone()));
    }

    if let Some(ref album) = normalized_album {
        conditions.push("albums.name_lower = ?");
        params.push(Box::new(album.clone()));
    }

    if let Some(dur) = duration {
        conditions.push("ABS(tracks.duration - ?) <= 2.0");
        params.push(Box::new(dur));
    }

    let where_clause = conditions.join(" AND ");

    let query = format!(
        indoc! {r#"
        SELECT
            tracks.id,
            tracks.file_path,
            tracks.file_name,
            tracks.title,
            artists.name AS artist_name,
            tracks.artist_id,
            albums.name AS album_name,
            albums.album_artist_name,
            tracks.album_id,
            tracks.duration,
            tracks.track_number,
            albums.image_path,
            lyricsfiles.id AS lyricsfile_id,
            lyricsfiles.lyricsfile,
            COALESCE(lyricsfiles.instrumental, 0) AS instrumental
        FROM tracks
        JOIN albums ON tracks.album_id = albums.id
        JOIN artists ON tracks.artist_id = artists.id
        LEFT JOIN lyricsfiles ON lyricsfiles.track_id = tracks.id
        WHERE {}
        ORDER BY tracks.title_lower ASC
    "#},
        where_clause
    );

    let mut statement = db.prepare(&query)?;

    // Convert params to rusqlite params
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let mut rows = statement.query(param_refs.as_slice())?;
    let mut tracks: Vec<PersistentTrack> = Vec::new();

    while let Some(row) = rows.next()? {
        let is_instrumental: bool = row.get("instrumental")?;

        let track = PersistentTrack {
            id: row.get("id")?,
            file_path: row.get("file_path")?,
            file_name: row.get("file_name")?,
            title: row.get("title")?,
            artist_name: row.get("artist_name")?,
            artist_id: row.get("artist_id")?,
            album_name: row.get("album_name")?,
            album_artist_name: row.get("album_artist_name")?,
            album_id: row.get("album_id")?,
            duration: row.get("duration")?,
            track_number: row.get("track_number")?,
            txt_lyrics: None,
            lrc_lyrics: None,
            lyricsfile: row.get("lyricsfile")?,
            lyricsfile_id: row.get("lyricsfile_id")?,
            image_path: row.get("image_path")?,
            instrumental: is_instrumental,
        };

        tracks.push(track);
    }

    Ok(tracks)
}

#[cfg(test)]
mod export_preferences_tests {
    use super::*;

    #[test]
    fn embed_only_preferences_require_gate_and_preserve_disabled_choice() {
        let mut conn = Connection::open_in_memory().unwrap();
        Migrations::from_directory(&MIGRATIONS_DIR)
            .unwrap()
            .to_latest(&mut conn)
            .unwrap();
        conn.execute("UPDATE config_data SET try_embed_lyrics = TRUE", [])
            .unwrap();
        set_export_preferences(
            Some(true),
            Some(false),
            Some(false),
            Some(true),
            None,
            None,
            None, &conn,
        )
        .unwrap();
        let config = get_config(&conn).unwrap();
        assert!(config.auto_export_enabled && config.export_embedded);
        assert!(!config.export_lrc && !config.export_txt);

        conn.execute("UPDATE config_data SET try_embed_lyrics = FALSE", [])
            .unwrap();
        // Stale/bypassed embed-only submissions are rejected before any fields change.
        for auto in [Some(true), None] {
            assert!(set_export_preferences(
                auto,
                Some(false),
                Some(false),
                Some(true),
                Some(false),
                Some(false),
                None, &conn
            )
            .is_err());
        }
        // An effective sidecar remains valid, but the disabled embedded choice is retained.
        set_export_preferences(
            Some(true),
            Some(true),
            Some(false),
            Some(false),
            None,
            None,
            None, &conn,
        )
        .unwrap();
        assert!(get_config(&conn).unwrap().export_embedded);
        set_export_preferences(Some(false), None, None, Some(false), None, None, None, &conn).unwrap();
        assert!(get_config(&conn).unwrap().export_embedded);
    }

    #[test]
    fn download_filter_writes_preserve_unrelated_settings_and_hidden_formats() {
        let mut conn = Connection::open_in_memory().unwrap();
        Migrations::from_directory(&MIGRATIONS_DIR)
            .unwrap()
            .to_latest(&mut conn)
            .unwrap();
        set_config(
            true,
            false,
            true,
            true,
            "dark",
            "https://example.test",
            0.4,
            false,
            "",
            true,
            "",
            "",
            "",
            "",
            "", &conn,
        )
        .unwrap();
        set_export_preferences(None, Some(true), Some(true), Some(true), None, None, None, &conn)
            .unwrap();

        for (skip_synced, skip_plain) in [(false, false), (true, false), (true, true)] {
            // Download off preserves formats while atomically saving the submitted filter.
            set_export_preferences(
                Some(false), None, None, None, Some(skip_synced), Some(skip_plain), None, &conn,
            )
            .unwrap();
            let config = get_config(&conn).unwrap();
            assert_eq!(config.skip_tracks_with_synced_lyrics, skip_synced);
            assert_eq!(config.skip_tracks_with_plain_lyrics, skip_plain);
            assert!(!config.auto_export_enabled);
            assert!(config.export_lrc && config.export_txt && config.export_embedded);
            assert!(config.show_line_count && config.try_embed_lyrics);
            assert_eq!(config.theme_mode, "dark");
            assert_eq!(config.lrclib_instance, "https://example.test");
            assert_eq!(config.volume, 0.4);

            // Manual Export omits both flags and must leave the Download filter alone.
            set_export_preferences(None, Some(true), None, None, None, None, None, &conn).unwrap();
            let config = get_config(&conn).unwrap();
            assert_eq!(config.skip_tracks_with_synced_lyrics, skip_synced);
            assert_eq!(config.skip_tracks_with_plain_lyrics, skip_plain);
        }
    }

    #[test]
    fn migration_and_scoped_writes_preserve_existing_settings() {
        let mut conn = Connection::open_in_memory().unwrap();
        let migrations = Migrations::from_directory(&MIGRATIONS_DIR).unwrap();
        migrations.to_version(&mut conn, 16).unwrap();
        // Write the pre-21 settings with raw SQL: set_config also writes
        // sync_cover, which does not exist at schema version 16.
        conn.execute(
            "UPDATE config_data SET skip_tracks_with_synced_lyrics = 1, \
             skip_tracks_with_plain_lyrics = 0, show_line_count = 1, \
             try_embed_lyrics = 1, theme_mode = 'dark', \
             lrclib_instance = 'https://example.test', volume = 0.4",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO lyricsfiles (lyricsfile) VALUES ('plain: saved')",
            [],
        )
        .unwrap();
        migrations.to_latest(&mut conn).unwrap();
        let config = get_config(&conn).unwrap();
        assert!(!config.auto_export_enabled);
        assert!(config.export_lrc);
        assert!(!config.export_txt && !config.export_embedded);
        assert_eq!(config.theme_mode, "dark");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM lyricsfiles", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // Manual Export shares formats but does not touch the Download toggle.
        set_export_preferences(Some(true), None, None, None, None, None, None, &conn).unwrap();
        set_export_preferences(None, Some(false), Some(true), Some(true), None, None, None, &conn).unwrap();
        assert!(get_config(&conn).unwrap().auto_export_enabled);
        // Turning auto-export off preserves formats, including hidden embedded selection.
        set_export_preferences(Some(false), None, None, None, None, None, None, &conn).unwrap();
        // Ordinary Settings writes leave all export preferences alone.
        set_config(
            false,
            true,
            false,
            false,
            "light",
            "https://other.test",
            0.6,
            false,
            "",
            true,
            "",
            "",
            "",
            "",
            "", &conn,
        )
        .unwrap();
        let config = get_config(&conn).unwrap();
        assert!(!config.auto_export_enabled && !config.export_lrc);
        assert!(config.export_txt && config.export_embedded);
        assert_eq!(config.lrclib_instance, "https://other.test");
        assert_eq!(config.volume, 0.6);
    }
}


#[cfg(test)]
mod migration_tests {
    use super::*;

    /// A directory that looks like the pre-rename data directory.
    fn fake_dir(root: &std::path::Path, name: &str, with_db: bool) -> std::path::PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        if with_db {
            std::fs::write(dir.join("db.sqlite3"), b"database").unwrap();
        }
        dir
    }

    #[test]
    fn moves_the_database_into_an_existing_empty_dir() {
        let root = std::env::temp_dir().join("sa-migrate-1");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let legacy = fake_dir(&root, LEGACY_IDENTIFIER, true);
        // Tauri has already made the new directory; that is the situation this
        // has to cope with, not the tidy one where it does not exist yet.
        let new = fake_dir(&root, "com.freelamp.SingAlong", false);

        migrate_legacy_data_dir(&new);

        assert_eq!(std::fs::read(new.join("db.sqlite3")).unwrap(), b"database");
        assert!(!legacy.join("db.sqlite3").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn does_nothing_when_the_new_directory_already_has_a_database() {
        let root = std::env::temp_dir().join("sa-migrate-2");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        fake_dir(&root, LEGACY_IDENTIFIER, true);
        let new = fake_dir(&root, "com.freelamp.SingAlong", true);
        std::fs::write(new.join("db.sqlite3"), b"current").unwrap();

        migrate_legacy_data_dir(&new);

        // The existing database wins: a second launch must not resurrect old data.
        assert_eq!(std::fs::read(new.join("db.sqlite3")).unwrap(), b"current");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn does_nothing_on_a_fresh_install() {
        let root = std::env::temp_dir().join("sa-migrate-3");
        let _ = std::fs::remove_dir_all(&root);
        let new = fake_dir(&root, "com.freelamp.SingAlong", false);

        migrate_legacy_data_dir(&new);

        assert!(!new.join("db.sqlite3").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn moves_the_cover_cache_too() {
        let root = std::env::temp_dir().join("sa-migrate-4");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let legacy = fake_dir(&root, LEGACY_IDENTIFIER, true);
        std::fs::create_dir_all(legacy.join("covers")).unwrap();
        std::fs::write(legacy.join("covers").join("a.jpg"), b"jpeg").unwrap();
        let new = fake_dir(&root, "com.freelamp.SingAlong", false);

        migrate_legacy_data_dir(&new);

        assert_eq!(
            std::fs::read(new.join("covers").join("a.jpg")).unwrap(),
            b"jpeg"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod duplicate_tests {
    use super::*;

    #[test]
    fn the_same_artist_written_differently_matches() {
        // The same band, with and without the word for "band".
        assert!(artists_match("温拿", "温拿乐队"));
        // The same two people, in a different order.
        assert!(artists_match("陈奕迅 + 王菲", "王菲 & 陈奕迅"));
        // The same, in English.
        assert!(artists_match("Romeo & Juliet", "Juliet & Romeo"));
    }

    #[test]
    fn different_artists_do_not_match() {
        // Two different songs that happen to share a name.
        assert!(!artists_match("戴佩妮", "赵咏华"));
        assert!(!artists_match("姜育恒", "苏芮"));
    }

    #[test]
    fn a_title_is_compared_without_its_script() {
        assert_eq!(
            normalise_for_comparison("城裡的月光"),
            normalise_for_comparison("城里的月光")
        );
        assert_eq!(
            normalise_for_comparison("Rolling In The Deep"),
            normalise_for_comparison("Rolling in the Deep")
        );
        assert_ne!(
            normalise_for_comparison("城裡的月光"),
            normalise_for_comparison("城裡的太陽")
        );
    }

    #[test]
    fn the_export_template_is_stored_and_validated() {
        let mut conn = Connection::open_in_memory().unwrap();
        Migrations::from_directory(&MIGRATIONS_DIR)
            .unwrap()
            .to_latest(&mut conn)
            .unwrap();

        // The default is empty, which means the historical `{stem}.{ext}`.
        assert_eq!(get_config(&conn).unwrap().export_template, "");

        // A valid template is stored.
        set_export_preferences(None, None, None, None, None, None, Some("{artist}/{title}.{ext}"), &conn)
            .unwrap();
        assert_eq!(
            get_config(&conn).unwrap().export_template,
            "{artist}/{title}.{ext}"
        );

        // An unknown variable is refused, so a typo is caught at the point of
        // entry rather than on the fiftieth track of a batch.
        assert!(set_export_preferences(
            None,
            None,
            None,
            None,
            None,
            None,
            Some("{titel}.{ext}"),
            &conn
        )
        .is_err());
        // ...and the stored value survives the rejected write.
        assert_eq!(
            get_config(&conn).unwrap().export_template,
            "{artist}/{title}.{ext}"
        );

        // An empty template means "use the default", and is accepted.
        set_export_preferences(None, None, None, None, None, None, Some(""), &conn).unwrap();
        assert_eq!(get_config(&conn).unwrap().export_template, "");

        // `set_config` writes it alongside everything else.
        set_config(
            false, false, false, false, "dark", "", 0.5, false, "", false, "", "", "", "",
            "{filename}.{ext}", &conn,
        )
        .unwrap();
        assert_eq!(
            get_config(&conn).unwrap().export_template,
            "{filename}.{ext}"
        );
    }
}
