# Sing-Along Tauri Architecture

## Architecture Overview

**Stack:** Tauri v2 (Rust) + SQLite + Kira audio + LRCLIB API

**Patterns:**
| Pattern | Implementation |
|---------|---------------|
| State | Global `AppState` with `Mutex<Connection>` and `Mutex<Player>` |
| DB Access | `ServiceAccess` trait on `AppHandle` for read/write operations |
| Events | Async backend→frontend via `app.emit()` |
| Commands | All FFI in `main.rs`, organized by domain |
| Scanning | Single-pass streaming with batch processing (100 files) |
| Export | Optional TXT/LRC/embedded export after library/album/artist bulk Download; shared manual writers |

**Key Dependencies:** `tauri`, `rusqlite`+`rusqlite_migration`, `lofty`, `kira`, `reqwest`, `rayon`, `xxhash-rust`, `regex` (for LRC parsing), `charabia` (international word segmentation)

**Word Segmentation Command:** `segment_words(text)` in `main.rs` uses Charabia's segmenter (`Segment::segment_str`) and then applies language-agnostic post-processing: segments containing at least one letter/number are kept as tokens, while separator-only segments (spaces/punctuation/symbols) are merged into adjacent tokens.

## Project Structure

```
src-tauri/
├── src/
│   ├── main.rs              # Entry point, Tauri commands, WebDriver plugin wiring
│   ├── state.rs             # AppState, ServiceAccess trait
│   ├── db.rs                # SQLite operations, migrations
│   ├── library.rs           # High-level library API
│   ├── scanner/             # Incremental file scanning (NEW)
│   │   ├── scan.rs          # Single-pass streaming scanner
│   │   ├── hasher.rs        # xxhash3 content hashing
│   │   ├── metadata.rs      # Audio metadata extraction
│   │   └── models.rs        # ScanResult, ScanProgress
│   ├── parser/              # File format parsers
│   │   └── lrc.rs           # LRC lyrics parser (replaces lrc crate)
│   ├── word_segmentation.rs # Charabia-based tokenization + separator-merging logic/tests
│   ├── export.rs            # Manual sidecar/embed export helpers
│   ├── export_template.rs   # Filename templates for sidecar exports
│   ├── lyricsfile.rs        # YAML lyricsfile helpers
│   ├── player.rs            # Kira audio playback
│   ├── persistent_entities.rs # Track/Album/Artist structs
│   ├── utils.rs             # Text normalization
│   ├── amll/                # AMLL word-level lyrics database client
│   │   ├── mod.rs           # TTML fetch + mirror fallback
│   │   └── ttml.rs          # TTML parser (line + word timings)
│   ├── cookies.rs           # Browser cookie export (JSON) → Netscape text
│   ├── ytdlp.rs             # Drives an external yt-dlp for downloads
│   ├── youtube.rs           # Release database lookup: album + cover
│   ├── youtube_gui.rs       # The Tauri commands for the above
│   └── lrclib/              # LRCLIB API client
│       ├── search.rs        # GET /api/search
│       ├── get.rs           # GET /api/get
│       ├── get_by_id.rs     # GET /api/get/{id}
│       ├── publish.rs       # POST /api/publish (with PoW) — currently unwired
│       └── challenge_solver.rs # SHA256 proof-of-work
├── migrations/              # SQL files (v1-v25), rusqlite_migration
└── Cargo.toml / tauri.conf.json
```

## Core Components

### State Management (`state.rs`)

```rust
struct AppState {
    db: Mutex<Option<Connection>>,           // SQLite
    player: Mutex<Option<Player>>,           // Kira audio
    queued_notifications: Mutex<Vec<Notify>>,
}

trait ServiceAccess {
    fn db<F, T>(&self, f: F) -> T;      // Read-only
    fn db_mut<F, T>(&self, f: F) -> T;  // Mutable
}
```

### Database Schema (v10)

**Tables:**
| Table | Purpose |
|-------|---------|
| `directories` | Watched music paths |
| `library_data` | Init flag (single row) |
| `config_data` | Settings (embed, skip flags, theme, LRCLIB instance) |
| `artists` | name, name_lower (search) |
| `albums` | name, album_artist_name, image_path |
| `tracks` | file_path, title, duration, lrc_lyrics, txt_lyrics |
| `lyricsfiles` | Persisted YAML lyrics (decoupled from tracks). Contains track metadata (title, album, artist, duration), presence fields (`has_plain_lyrics`, `has_synced_lyrics`, `has_word_synced_lyrics`, `instrumental`), and optional LRCLIB source fields (`lrclib_instance`, `lrclib_id`). `track_id` is NULL for standalone LRCLIB lyrics without local track association. |

**Migration v8 (scanning):** Added `file_size`, `modified_time`, `content_hash`, `scan_status`

**Migration v9:** Added `lyricsfiles` table with denormalized track metadata

**Migration v10:** Added indexed lyrics-presence booleans on `tracks` (`has_plain_lyrics`, `has_synced_lyrics`, `has_word_synced_lyrics`) used for filtering

**Migration v11:** Added `volume` column to `config_data` for persisting player volume level

**Migration v12:** Added `lrclib_instance` and `lrclib_id` columns to `lyricsfiles` table for tracking remote LRCLIB lyrics sources. Enables editing lyrics from LRCLIB without requiring a local track association.

**Migration v13:** Added `has_plain_lyrics`, `has_synced_lyrics`, `has_word_synced_lyrics`, `instrumental` columns to `lyricsfiles` table. This moves the source of truth for lyrics presence to the `lyricsfiles` table, enabling filtering based on lyricsfile content for both library tracks and standalone LRCLIB lyrics.

**Migration v14:** Recreates the `tracks` table without deprecated lyrics columns (`txt_lyrics`, `lrc_lyrics`, `instrumental`, `has_plain_lyrics`, `has_synced_lyrics`, `has_word_synced_lyrics`). All lyrics data is now stored exclusively in the `lyricsfiles` table. Forces a library reset.

**Migration v15:** Added indexes on `tracks.album_id` and `tracks.artist_id`.

**Migration v17:** Added `auto_export_enabled` (false), `export_lrc` (true), `export_txt` (false), and `export_embedded` (false) to `config_data`. Shared Download/Export defaults persist through a scoped update, without resetting library data. Ordinary `set_config` leaves these fields untouched.

**Migration v16:** Added SQLite FTS5 virtual tables for full-text search:
- `tracks_fts` — indexed `title`, `artist_name`, `album_name` (with `track_id` as UNINDEXED key)
- `albums_fts` — indexed `album_name`, `album_artist_name` (with `album_id` as UNINDEXED key)
- `artists_fts` — indexed `artist_name` (with `artist_id` as UNINDEXED key)
- Backfilled from existing normalized `*_lower` columns.

**Indexes:** All `*_lower` columns + `content_hash`, `scan_status`, `modified_time+file_size` (fingerprint) + lyrics-presence indexes + LRCLIB composite index (`lrclib_instance`, `lrclib_id`)

### File Scanning (`scanner/`)

**ScanResult:** `{ total_files, added, modified, deleted, moved, unchanged, is_initial_scan, duration_ms }`

**ScanProgress:** `{ phase: "discovering"|"updating", progress: f64, files_processed, files_total, message }`

**Process:**
1. Mark existing tracks as "pending" (scan_status=0)
2. Single-pass streaming: discover + process simultaneously
3. Batch size: 100 files
4. Detection modes:
   - **Hash** (default): xxhash3 of first 64KB - detects moves, 100% accurate
   - **Metadata**: mtime+size only - faster, may duplicate on metadata changes
5. Delete remaining "pending" tracks

**Performance:** 100K files HDD ~30-90s faster, SSD ~5-10s faster; Memory ~10MB vs ~200MB old

### FTS5 Search

Library search (tracks, albums, artists) uses SQLite FTS5 virtual tables for tokenized, prefix-matching full-text search. A runtime `fts5_enabled()` check verifies the virtual tables exist; if missing, queries transparently fall back to `LIKE` on `*_lower` columns.

**Virtual Tables:**
| Table | Indexed Columns | Key |
|-------|----------------|-----|
| `tracks_fts` | `title`, `artist_name`, `album_name` | `track_id` (UNINDEXED) |
| `albums_fts` | `album_name`, `album_artist_name` | `album_id` (UNINDEXED) |
| `artists_fts` | `artist_name` | `artist_id` (UNINDEXED) |

**Query Building:** `utils::build_fts_query()` normalizes input (diacritics stripped, punctuation removed, lowercased) and converts each token into a prefix query. Example: `Love Way` → `love* way*`.

**Ordering:** Results are ordered by `rank` (FTS5 relevance) when FTS5 is available; otherwise alphabetical by `*_lower`.

**Sync:** The FTS indexes are kept in sync with the source tables at mutation points:
- **Insert:** `insert_new_track()` inserts into `tracks_fts`; `add_artist_tx()` inserts into `artists_fts`; `add_album_tx()` inserts into `albums_fts`.
- **Delete:** `delete_unprocessed_tracks()` removes pending track IDs from `tracks_fts` and orphaned album/artist IDs from `albums_fts`/`artists_fts`; `clean_library()` clears all three virtual tables.

### Orphaned Lyricsfile Reattachment

When tracks are deleted from the library, their associated `lyricsfiles` records are retained (via `ON DELETE SET NULL`). When the same track is re-added during a library scan, the system automatically reattaches these orphaned lyricsfiles instead of requiring re-import from embedded metadata or sidecar files.

**Matching Criteria:**
- Exact match on normalized `track_artist_name_lower` + `track_title_lower` + `track_album_name_lower`
- Duration must be within ±2 seconds
- Only considers lyricsfiles where `track_id IS NULL` (orphaned)

**Process during `insert_new_track()`:**
1. Extract metadata from audio file
2. Query for matching orphaned lyricsfile via `db::find_orphaned_lyricsfile_tx()`
3. If found: reattach via `db::reattach_lyricsfile_to_track_tx()`, skip embedded lyrics import
4. If not found: proceed with normal embedded/sidecar lyrics import

**Benefits:**
- Preserves lyrics edits when tracks are temporarily removed and re-added
- Avoids redundant re-import from sidecar files
- Works for both previously-deleted tracks and standalone LRCLIB lyrics

### Audio Player (`player.rs`)

```rust
struct Player {
    manager: Option<AudioManager>,          // Kira — created lazily on first play
    sound_handle: Option<StreamingSoundHandle>,
    track: Option<PlayableTrack>,           // Unified type for DB and file-based tracks
    status: PlayerStatus,                   // Playing/Paused/Stopped
    progress: f64, duration: f64, volume: f64, playback_speed: f64,
}

struct PlayerSnapshot {                     // What crosses IPC
    status, track_id, progress, duration, volume, playback_speed
}
```

The player uses `PlayableTrack` to support both library tracks (from database) and arbitrary tracks (from file picker). This enables playback of tracks not present in the library database.

**Audio device lifecycle:** `AudioManager` is created on the first `play()`, not at startup, and `stop_track` calls `release_audio_device()` which drops it again. `stop()` alone keeps the manager alive because starting a new track stops the previous one first, and reopening the device per track change would add latency. Rationale: the CPAL/ALSA threads (`data-loop.0`, `cpal_alsa_out`) keep waking up on an idle output stream, costing ~7% of a core indefinitely. A session that never plays audio now opens no device at all, and a stopped player returns to that state.

**State events:** the background loop in `main.rs` ticks at 100ms while playing and 500ms otherwise, and **only emits `player-state` when the snapshot differs from the last one it sent** (one payload is forced at startup so the frontend receives the persisted volume). It emits a `PlayerSnapshot`, not the serialized `Player`. Measured: ~10 events/s while playing, **0 events/s while paused or stopped** (previously 25/s unconditionally).

The comparison is against the last *emitted* snapshot rather than a before/after pair taken inside `renew_state`: commands such as `set_shuffle`, `set_repeat_mode` and `play_track` mutate the player directly, so by the next tick the "before" value would already be the new one and the change would never be emitted.

`PlayerSnapshot` is `#[serde(rename_all = "camelCase")]` and the frontend reads camelCase (`playbackSpeed`, `queueLength`, `hasNext`, ...). Changing one side without the other silently falls back to defaults.

Volume persistence:
- Player initializes with volume from `config_data` on startup
- `set_volume()` command updates both the player and persists to config
- Frontend receives volume updates via `player-state` events

Playback speed:
- Player tracks current `playback_speed` and reapplies it when a new track starts
- `set_playback_speed()` command updates the active sound handle rate (clamped 0.5x–2.0x)
- Frontend receives playback speed via `player-state` events

### Play Queue (`playlist.rs`)

```rust
enum RepeatMode { Off, One, All }

struct Playlist {
    ids: Vec<i64>,              // play order (shuffled order when shuffle is on)
    cursor: Option<usize>,
    original: Vec<i64>,         // restored when shuffle is turned off
    shuffle: bool,
    repeat: RepeatMode,
    playing_original_index: Option<usize>,
}
```

Holds ids only: resolving an id to playable metadata is the caller's job, which keeps the queue cloneable into a snapshot and free of file-path concerns.

**Semantics:**
- `advance` / `rewind` move the cursor; repeat-all wraps, repeat-off stops at the ends. An explicit "next" always advances — repeat-one is implemented in `on_track_ended`, not in `advance`, so a deliberate skip is never swallowed.
- `on_track_ended` returns `Play` / `Replay` / `Stop`, and is where repeat-one replays the current track.
- Shuffle reorders a copy of `original`, keeps the playing track first, and re-seats the cursor. The shuffled order is **stored**, so "previous" returns to a track the user actually heard instead of re-randomising on every advance. Turning shuffle off restores `original` and re-selects the playing track.
- Shuffling uses Fisher-Yates seeded from `RandomState` (SipHash); `rand` is not a dependency of this crate and is not worth adding for one shuffle.

**End-of-track detection** lives in `Player::renew_state`: the backend reported `Playing` last tick and reports `Stopped` now. It cannot misfire on `stop()` (which clears the sound handle) or on seek. It cannot start the next track either — resolving an id needs the database — so the id is pushed onto `Player::finished` and the state loop drains it *after* releasing the player lock. That avoids holding the player lock across file I/O and avoids a player/db lock-order hazard.

Queue transitions go through `start_queued_track`, which uses `play()` and **never** `release_audio_device()`, so the output device is not reopened between tracks.

`play_track` takes an optional `queue` + `index`; a bare track or a file-picker track collapses to a one-track queue so transport controls cannot report a stale queue from a previous selection.

### Export Module (`export.rs`) and filename templates (`export_template.rs`)

Shared lyrics export to sidecar files and manual embedded metadata export. Automatic Download export calls the same writers after successful internal save.

```rust
pub enum ExportFormat {
    Txt,        // Plain text sidecar file (.txt)
    Lrc,        // Synced LRC sidecar file (.lrc), line-level
    LrcA2,      // Enhanced LRC / A2 with inline word tags (.alrc) — Android
    Ttml,       // TTML with one <span> per word (.ttml) — iOS
    Embedded,   // Embedded in audio metadata (MP3/FLAC)
}

pub struct ExportResult {
    pub format: ExportFormat,
    pub path: Option<PathBuf>,
    pub status: ExportStatus,   // Success | Skipped(reason) | Error(reason)
}
```

**Key Functions:**
- `export_track()` / `export_track_with_template()` - Export a track to multiple formats
- `export_track_with_embed_gate()` - Same, consulting `try_embed_lyrics` before each embedded write
- `export_track_format_with_template()` - Export to a specific format
- `plan_export()` - What the export *would* do, writing nothing (the dry run)
- `embed_lyrics()` - Embed lyrics into MP3 (ID3v2 USLT/SYLT) or FLAC (Vorbis comments)

**Note:** Sidecar exports retain their existing semantics: overwrite the selected target and attempt to delete the opposite extension before writing available content. Unavailable formats skip before deletion. Bulk export runs TXT, LRC, then embedded, so both successful sidecar operations normally leave only LRC. Failures do not roll back filesystem changes — which is what `plan_export` exists to mitigate, by showing the deletions before they happen. Embedded exports use the existing `lofty` MP3/FLAC writers. The shared format boundary reports unsupported extensions as `Skipped` before touching the file. Both manual commands and automatic export use `export_track_with_embed_gate`, reading current `try_embed_lyrics` immediately before each embedded operation (read failures deny embedding). A disabled gate produces a format-level skip, including embed-only batches whose gate was disabled after submission; successful internal downloads retain their success string. Preference submission rejects zero effective formats and ignores embedded-choice updates while the gate is disabled. Supported-writer edge cases remain unchanged: MP3 requires an existing primary tag; FLAC without Vorbis comments currently performs no tag update.

### Filename templates (`export_template.rs`)

Where a sidecar is written used to be hardcoded to the audio file's stem: `01. Song.mp3` always produced `01. Song.lrc`. A template is a relative path with `{variable}` placeholders, resolved against the audio file's own directory, so a template can only ever place a sidecar beside the track or in a subfolder of it:

```
{filename}.{ext}                                         # the default, = the old behaviour
{artist}/{album}/{track_number_padded} - {title}.lrc
lyrics/{title_sanitized}.{ext}
```

**Variables:** `title`, `artist`, `album`, `album_artist`, `track_number`, `track_number_padded`, `filename` (the audio file's stem) and `ext` (the format's own extension).

**The `_sanitized` suffix.** Sanitising is opt-in per variable, not always-on: `{title_sanitized}` replaces the characters a filesystem refuses (backslash, `/`, `:`, `*`, `?`, quotes, `<>`, `|` and control characters) with `_`, while `{title}` keeps them. A template that builds a directory must not be sanitised — a sanitised `lyrics/` stops being a path — and literal text in a template is never touched, for the same reason.

**Validation.** `ExportTemplate::parse()` rejects an unknown variable, an unclosed brace, an absolute path (POSIX `/` and Windows `C:`) and `..`. It runs when the template is saved (`db::set_export_preferences`), so a typo is caught at the point of entry rather than on the fiftieth track of a batch. `ensure_extension()` appends the extension when the template omits it and is idempotent, so an explicit `.lrc` is not doubled.

**Path resolution is pure.** `build_sidecar_path_for_track()` creates nothing; the writer (`write_sidecar`) creates the parent directories a template asks for. That split is what lets `plan_export` name a path without touching the filesystem.

**CLI:** `sing-along export <path> --template '<pattern>'`. The GUI saves the template with the other export preferences and validates it on submit, restoring the stored text when the backend refuses it.

### Migration v26

`export_template TEXT NOT NULL DEFAULT ''` on `config_data`. Empty means the default `{filename}.{ext}`, which is the behaviour that was hardcoded before the column existed.

### LRC Parser (`parser/lrc.rs`)

Lightweight LRC (LyRiCs) format parser that replaces the external `lrc` crate. Supports timestamp tags with 1-3 digit precision for milliseconds.

**Problem with `lrc` crate:** The original crate only supported 1-2 digit fractional seconds (centiseconds) and silently ignored lines with 3-digit milliseconds, causing data loss when parsing LRC files like `[01:35.492] Line text`.

**Solution:** Custom parser with flexible timestamp parsing:
- `[mm:ss.x]` - 1 digit = deciseconds (×100ms)
- `[mm:ss.xx]` - 2 digits = centiseconds (×10ms)
- `[mm:ss.xxx]` - 3 digits = milliseconds (×1ms)

```rust
pub struct TimedLine {
    pub timestamp_ms: i64,
    pub text: String,
}

pub struct ParsedLrc {
    pub timed_lines: Vec<TimedLine>,
    pub id_tags: Vec<(String, String)>,
}

pub fn parse_lrc(input: &str) -> ParsedLrc;
pub fn is_instrumental_lrc(input: &str) -> bool;
pub fn format_timestamp(timestamp_ms: i64) -> String;
```

**Features:**
- Multiple timestamps per line: `[00:01.00][00:03.00] Repeated text`
- ID tags: `[ti:Title]`, `[ar:Artist]`, `[au:instrumental]`
- Automatic sorting by timestamp
- Instrumental detection via `[au:instrumental]` marker

**Used by:**
- `lyricsfile.rs` - Parsing embedded LRC lyrics during import
- `export.rs` - Converting LRC to SYLT format for MP3 embedding

### LRCLIB API (`lrclib/`)

**Endpoints:** search, get, get_by_id, publish, flag, request_challenge

**Challenge-Response (publish/flag):**
1. Request challenge → prefix + target hash
2. Solve PoW (SHA256): find nonce where hash(prefix+nonce) < target
3. Submit with token

### Data Entities (`persistent_entities.rs`)

**PersistentTrack:** id, file_path, file_name, title, album_name, artist_name, album_id, artist_id, image_path, track_number, txt_lyrics, lrc_lyrics, lyricsfile, duration, instrumental

**PlayableTrack:** A unified type for playback that works with both database tracks and arbitrary file-based tracks. Used by the `Player` to support:
- Library tracks (from database): `id` is `Some(track_id)`
- File picker tracks: `id` is `None`, metadata provided directly

Implements `From<PersistentTrack>` for seamless conversion from database entities.

**PersistentAlbum:** id, name, artist_name, tracks_count

**PersistentArtist:** id, name, tracks_count

**PersistentConfig:** skip_synced, skip_plain, show_line_count, try_embed, theme_mode, lrclib_instance, volume, auto_export_enabled, export_lrc, export_txt, export_embedded

## Build Features (dev vs release)

Development-only capabilities are opt-in cargo features, never enabled by default:

| Feature | Enables | Turned on by |
|---------|---------|--------------|
| `devtools` | `tauri/devtools` (devtools API) | `npm run tauri:dev` |
| `webdriver` | `tauri-plugin-webdriver` | — |
| `dev` | `webdriver` | `tauri dev` automatically |

None of them is in `default`, so `cargo build --release` and `npm run tauri build` produce a binary with no devtools API and no WebDriver server. Verify with `cargo tree -i tauri-plugin-webdriver`, which fails to resolve the package at all in the default feature set, or `strings … | grep -c tauri_plugin_webdriver` (0 for release, tens of thousands for debug).

This replaced `[target.'cfg(debug_assertions)'.dependencies]` for the WebDriver plugin, which Cargo silently ignores for dependency selection — it only prints a warning, so the plugin *was* compiled into release builds. `tauri`'s `devtools` feature had the same problem: it was requested unconditionally.

`open_devtools` stays registered in both builds so the command list does not change shape; without the feature it is a no-op.

Build note: charabia's lindera dictionaries are downloaded by build scripts. If that fails (network/TLS), pre-populate a cache directory and point `LINDERA_CACHE` at it — it contains one subdirectory per lindera version, e.g. `LINDERA_CACHE=/path/to/cache` with `0.43.3/lindera-unidic` and `0.43.3/lindera-ko-dic` inside. The script's fast path returns immediately when the built dictionary directory exists.

## WebDriver Dev Bridge (`tauri-plugin-webdriver`)

Agent-driven inspection (screenshots, DOM queries, JS eval, interaction, logs) via a [W3C WebDriver](https://www.w3.org/TR/webdriver/) server, provided by [tauri-plugin-webdriver](https://github.com/Choochmeque/tauri-plugin-webdriver).

- **Debug builds only**: the dependency lives under `[target.'cfg(debug_assertions)'.dependencies]` in `Cargo.toml`, and the plugin is registered with `#[cfg(debug_assertions)] let builder = builder.plugin(tauri_plugin_webdriver::init());` in `main.rs`. Release builds have no server and no plugin code.
- **Transport**: HTTP server on `127.0.0.1:4445` (override with `TAURI_WEBDRIVER_PORT` env var or `init_with_port`). Drive it with any W3C WebDriver client (Selenium, WebdriverIO, `fantoccini`).
- **Linux backend**: WebKitGTK native APIs (`webkit_web_view_get_snapshot` for screenshots, `evaluate_javascript_future` for scripts) — compositor-independent, works on X11 and any Wayland compositor. Element screenshots are a full-webview snapshot after `scrollIntoView` (no pixel crop on Linux).
- **No custom IPC commands**: unlike the previous tauri-agent-tools bridge, nothing is added to the `invoke_handler` list, so there is no debug/release duplication to keep in sync.
- **How to run and use it** (launch recipe, session flow, caveats): see *"Inspecting a running dev build"* in the root `AGENTS.md`.

## Commands

### Library
| Command | Description |
|---------|-------------|
| `scan_library(use_hash?)` | Incremental scan. Emits `scan-progress`, `scan-complete` |
| `uninitialize_library()` | Clear all library data |

### Data Queries
| Command | Returns |
|---------|---------|
| `get_track(s/ids)(search_query?)` | `PersistentTrack` (includes joined `lyricsfile`) |
| `get_album(s/ids)(search_query?)` | `PersistentAlbum` |
| `get_artist(s/ids)(search_query?)` | `PersistentArtist` |
| `get_album_tracks/ids()` | Filtered tracks |
| `get_artist_tracks/ids()` | Filtered tracks |

Search across all three entity types uses SQLite FTS5 (via `tracks_fts`, `albums_fts`, `artists_fts`). Results are ordered by FTS5 `rank` (relevance). A runtime `fts5_enabled()` check falls back to `LIKE` queries on `*_lower` columns if the virtual tables are unavailable.

### Lyrics
| Command | Purpose |
|---------|---------|
| `download_lyrics(track_id, auto_export?)` | Download from LRCLIB and save internally. Optional explicit `{ plainText, syncedLrc, embedIntoTrack }` batch snapshot enables TXT/LRC/embedded export after save; empty selections are rejected before downloading. Returns the existing string, appending ordered export outcomes/errors after save. Omitted options preserve internal-only behavior. |
| `retrieve_lyrics/by_id()` | Get raw LRCLIB response |
| `search_lyrics()` | Search LRCLIB database |
| `apply_lyrics()` | Save a selected LRCLIB result into database-backed lyrics storage |
| `prepare_lrclib_lyricsfile(lrclib_id)` | Get or create lyricsfile from LRCLIB. Checks local cache first, fetches from API if needed. Saves to `lyricsfiles` table with `lrclib_instance` + `lrclib_id`. Returns `lyricsfile_id` + content + `exists_in_db` flag. |
| `refresh_lrclib_lyricsfile(lrclib_id)` | Force re-download lyrics from LRCLIB API. Updates existing record in `lyricsfiles` table. Returns refreshed `lyricsfile_id` + content. |
| `save_lyrics(track_id?, lyricsfile_id?, plain?, synced?, lyricsfile?)` | Save lyrics edits. For library tracks: provide `track_id`. For standalone LRCLIB lyrics: provide `lyricsfile_id`. Prefers `lyricsfile` format. |
| `publish_lyrics(title, album, artist, duration, plain?, synced?, lyricsfile?)` | Upload to LRCLIB (with PoW; accepts Lyricsfile-only payloads) |
| `export_lyrics(track_id, formats, lyricsfile?)` | Manual export to `.txt`, `.lrc`, or embedded tags |
| `export_track_lyrics(track_id, formats)` | Export single track, returns summary for mass export |
| `get_track_ids_with_lyrics()` | Get all track IDs that have lyrics for mass export |
| `flag_lyrics()` | Report to LRCLIB (with PoW) |
| `find_matching_tracks(title, album, artist, duration?)` | Find local tracks matching LRCLIB metadata (for My LRCLIB edit flow) |
| `get_audio_metadata(filePath)` | Extract metadata from audio file (for file picker) |
| `prepare_search_query(title)` | Prepare search query by removing brackets and normalizing |

### Playback & Config
- `play_track(track_id?, file_path?, title?, album_name?, artist_name?, album_artist_name?, duration?)` - Unified playback for both library tracks (via `track_id`) and file-based tracks (via `file_path` with metadata)
- `pause/resume_track()`, `seek_track()`, `set_volume()` (persists volume to config), `set_playback_speed()`
- `stop_track()` - Stops playback **and releases the audio device** (see Audio Player lifecycle). The queue survives, so play resumes at the same position
- `play_next()` / `play_previous()` - Skip within the queue. Always advances (repeat-one does not swallow an explicit skip)
- `set_shuffle(shuffle)` / `set_repeat_mode(repeat)` - Queue modes; `repeat` is `off`/`all`/`one`
- `get/set_directories()`, `get/set_config()`, `get_init()`
- `set_export_preferences(auto_export_enabled?, export_lrc?, export_txt?, export_embedded?, skip_tracks_with_synced_lyrics?, skip_tracks_with_plain_lyrics?)` updates only supplied fields in `config_data` and returns config. Download atomically supplies its existing skip flags, toggle, and effective format selections; manual Export supplies shared formats only. Hidden/omitted fields and unrelated settings survive.
- Volume is loaded from config on startup and auto-saved when changed via `set_volume()`
- `open_devtools()`, `drain_notifications()` (startup backstop; notifications are pushed via the `notification` event)

### Runtime configuration

`#[tokio::main]` is configured with `worker_threads = 4`. The workload is I/O bound (LRCLIB requests, scanning) with a handful of concurrent commands, so the default one-thread-per-core pool only added idle threads; scans still use `block_in_place`.

### Memory: virtual vs resident

A running LRCGET shows ~74 GB of VSZ on Linux. That is **not a leak or an allocation** — it is WebKit's JavaScriptCore gigacage, a fixed 64 GB `PROT_NONE` reservation (`[anon:JSGigacage]` in `smaps`, RSS 0) plus a 4 GB JS structure heap and 1 GB JIT region. It is identical for any WebKitGTK host and independent of library size. Judge the process by RSS/PSS (~315 MB / ~140 MB), never by VSZ, and never cap VSZ with `ulimit -v` or `cgroup memory.max` — WebKit aborts instead of suspending the gigacage.

## Events (Backend → Frontend)

| Event | Payload | Purpose |
|-------|---------|---------|
| `scan-progress` | `{ phase, progress, filesProcessed, filesTotal, message }` | Real-time scan updates |
| `scan-complete` | `{ totalFiles, added, modified, deleted, moved, unchanged, isInitialScan, durationMs }` | Scan finished |
| `player-state` | `{ status, trackId, progress, duration, volume, playbackSpeed, queueLength, queueIndex, shuffle, repeat, hasNext, hasPrevious }` | Playback updates — emitted **only on change**, at 100ms while playing / 500ms otherwise |
| `notification` | `{ message, type }` | Backend notifications, pushed instead of polled (queue drained once at boot as a backstop) |
| `reload-track-id` | track_id | Request refresh |
| `publish-lyrics-progress` | Status | Publishing updates |
| `flag-lyrics-progress` | Status | Flagging updates |


## Configuration

**tauri.conf.json:** Window 1024x768 min, CSP for asset protocol + media

**Cargo.toml:** Rust 2021, Tauri v2 (dialog, shell, global-shortcut, os plugins)

## Track Matching (`find_matching_tracks`)

Backend command for finding local tracks that match LRCLIB metadata. Uses `prepare_input()` normalization for case-insensitive matching.

**Algorithm:**
1. **Strong Match**: Title + Artist + Album match (via `prepare_input()` normalization) AND duration within ±2 seconds (if provided)
2. **Partial Match**: Title matches, returned if no strong matches found

**Types:**
```rust
enum MatchQuality { Strong, Partial }
struct MatchingTrack {
    #[serde(flatten)]
    track: PersistentTrack,
    match_quality: MatchQuality,
}
```

**DB Function:** `db::find_tracks_by_metadata()` searches using normalized `*_lower` columns for case-insensitive matching.

**Note:** The frontend currently uses a simpler approach with `get_tracks` + client-side filtering in `AssociateTrackModal.vue`.

## Audio Metadata Extraction (`get_audio_metadata`)

Extracts metadata from an audio file path using the existing scanner logic. Used by the file picker in the track association flow.

**Returns:**
```rust
struct AudioMetadataResponse {
    file_path: String,
    file_name: String,
    title: String,
    album: String,
    artist: String,
    album_artist: String,
    duration: f64,
    track_number: Option<u32>,
}
```

**Implementation:** Reuses `scanner::metadata::TrackMetadata::from_path()` from the scanning module.

## Search Query Preparation (`prepare_search_query`)

Prepares a search query from track title by:
1. Removing content inside `()` and `[]` brackets (including the brackets)
2. Applying `prepare_input()` normalization (lowercase, secular normalization, special char removal, whitespace collapsing)

**Example:**
- Input: `title="Love The Way You Lie (Remix) [Explicit]"`
- Output: `"love the way you lie"`

Used by `AssociateTrackModal.vue` to prefill the search input when associating LRCLIB tracks with local tracks.

## Notes

- **Lyrics Storage:** `lyricsfiles` table remains the source of truth. The shared confirmed library/album/artist Download popup can opt into TXT/LRC/embedded export after save; all other save paths remain manual export. Automatic export parses the just-saved snapshot without holding the DB lock during file I/O, and failures remain text in the successful download response. The `tracks` table no longer contains `txt_lyrics` or `lrc_lyrics` columns as of migration v14.
- **Filtering Source of Truth:** Lyrics filters use `lyricsfiles.has_*_lyrics` booleans (via LEFT JOIN with COALESCE)
- **Instrumental:** `[au: instrumental]` marker in lrc_lyrics, stored as `instrumental = true` in lyricsfiles table
- **Security:** PoW for LRCLIB writes, user-agent in requests, DB in app_data_dir

## Deprecated Code Cleanup

The following deprecated functions were removed from `db.rs` (no longer used):
- `set_track_lyrics_presence()` / `set_track_lyrics_presence_tx()` - No-ops after presence fields moved to `lyricsfiles` table
- `update_track_synced_lyrics()` - Replaced by `upsert_lyricsfile_for_track()`
- `update_track_plain_lyrics()` - Replaced by `upsert_lyricsfile_for_track()`
- `update_track_null_lyrics()` - Replaced by `delete_lyricsfile_by_track_id()`
- `update_track_instrumental()` - Replaced by `upsert_lyricsfile_for_track()` with instrumental lyricsfile content
- `derive_lyrics_presence_from_legacy()` - No longer needed; presence is calculated from lyricsfile content via `lyrics_presence_from_lyricsfile()`

**Scanning Changes:**
- `insert_track_from_metadata_tx()` no longer stores `txt_lyrics`, `lrc_lyrics`, `instrumental`, or presence fields in the `tracks` table
- Embedded lyrics are now exclusively stored in the `lyricsfiles` table via `upsert_lyricsfile_for_track_tx()`
- The tracks table schema for new databases (post-reset) will not include these deprecated columns

**Migration v14:** Uses `ALTER TABLE DROP COLUMN` (SQLite 3.35.0+) to remove deprecated columns: `txt_lyrics`, `lrc_lyrics`, `instrumental`, `has_plain_lyrics`, `has_synced_lyrics`, `has_word_synced_lyrics`. Drops related indexes and forces a library reset.

## Downloading from YouTube (`ytdlp.rs`, `youtube.rs`, `cookies.rs`)

A track can be fetched from YouTube through the user's own `yt-dlp`. The
downloader is not reimplemented: `yt-dlp` is the only thing that knows how to
talk to YouTube, and it changes every week, so a copy of it in this project
would be a copy that stops working.

### The pieces

| Module | What it does |
| ------ | ------------ |
| `cookies.rs` | Turns the browser's cookie export (JSON) into the Netscape text format `yt-dlp` reads, and reports whether the cookies are still usable |
| `ytdlp.rs` | Runs `yt-dlp`: a probe for the metadata, then the download |
| `youtube.rs` | Looks the track up in the release database, for the real album name, the year and the artwork |
| `youtube_gui.rs` | The Tauri commands, and the order the steps happen in |

### Why the cookies are a problem

YouTube does not serve a residential address anonymously, reliably. The
browser's cookie database on Linux is encrypted with a key in the login
keyring, which a desktop session that is not the browser's own cannot read —
so `--cookies-from-browser` decrypts only a fraction of the cookies and the
rest is silently useless. The cookies have to be exported.

The export is a JSON file that a browser extension writes. `cookies.rs`
converts it, and the GUI does the conversion as soon as the user picks the
file, so the user never has to run a second program.

### Why the expiry is checked

A browser rotates the session cookies whenever it decides to, and a rotated
cookie is not refused — it is answered with "Sign in to confirm you're not a
bot", which blames the user for something they did not do. So the cookies are
checked for expiry when they are read, and the settings panel says so plainly.

### Why the album name is looked up

`yt-dlp` writes what YouTube tells it, and for a song on YouTube Music the
album comes back as a compilation ("2021熱歌合集" — a "2021 hits" collection),
the cover is a square video thumbnail, and the Chinese text is in whatever
script the uploader used.

The release database knows the real release. The lookup is by title and
artist, and the release is chosen by how well it fits: the artist's own
release beats one they merely appear on, an official release beats a bootleg,
an album beats a compilation, and an earlier release beats a later one. The
artwork comes from the cover art archive, which serves the picture for a
release that the database knows.

### The duplicate

A download can be about a track the library already holds, and the file
the download would write may be the file the library entry points at. Three
things are done about it:

- The probe reports the existing track, so the dialog can ask. The answer is
  the user's: replace the file, keep both, or do not download.
- The comparison is made in one script. YouTube publishes in whatever script
  the uploader chose, and the library holds the title as it was written into
  the file, so a library entry in Simplified and a download in Traditional
  are the same recording. The database's search is exact on the normalised
  title and the normalisation does not know about the scripts, so the
  candidates are looked at again with the script ignored.
- `yt-dlp` does not replace a file that is already there: it reports "this
  has already been downloaded" and exits successfully, which leaves the
  caller believing it succeeded. `--force-overwrites` is therefore passed
  when the user chose to replace.

### The library

The downloaded file is added to the library with `db::add_single_track`,
which adds one track and touches nothing else. A scan cannot be used for
this: a scan marks every track as pending and then deletes the ones it did
not see, so scanning the download folder would empty the rest of the
library.

The lyrics are then fetched for the new track, through the same pipeline as
any other track — LRCLIB, then NetEase as a fallback, then the AMLL word
timings. A song with no lyrics anywhere is an ordinary outcome, so it is
reported rather than raised.

### The metadata repair (`repair.rs`, `repair_gui.rs`)

A library built by copying files into folders inherits the folders' names: a
track whose tag had no album gets the name of the directory it was dropped
into, and a library of Chinese songs ends up with an album called "KXT"
holding a hundred songs by seventy-seven artists.

**Which names are worth repairing.** A name under more than one artist. A
real album belongs to one artist, so a name that several artists share is a
name that came from a folder. The unit of the answer is the *name*, not the
album row: the row is per artist and per name, so "KXT" is one row for 齐秦,
one for 梁静茹, and one for every other artist whose files were dropped into
a folder called KXT. Looking at the rows would find nothing, because no one
of them has more than one artist.

**What the repair writes.** The album name, the year and the artwork come
from the release database, and the text is converted to the requested script
and written in UTF-8. The file is rewritten, not just the library row: a
row that disagrees with the file it points at is a library that changes on
the next scan, and a player that reads the file shows the old values.

**The encoding.** The text frames of an ID3v2 tag say what encoding they
are in, and a library collected over years holds files in several: UTF-16
from one ripper, UTF-8 from another. The same title is then spelled
differently depending on the file it is in. The repaired file is written in
UTF-8, which is what the format's version 4 uses.

**Nothing is guessed.** A track the release database does not have keeps
what it has, and the report says so. The user is shown what the database
has for each track before anything is written, and only the tracks that were
found are offered.

### The library check

`get_library_problems` reports two things, and neither is a verdict.

**The entries whose file is not on the disk.** A file can go missing without
the library being told: a drive that is not mounted, a folder that was moved,
a file that was deleted behind the application's back. The check is on the
file, not on the scan, because a scan would remove the entries — and a scan
of a library whose drive is not mounted would empty the whole library.

**The pairs that look like the same recording.** Three rules, in the order
they are applied:

- **The same content hash** means the same audio. Certain.
- **The same title and artist, and the same length**, means the same
  recording from two sources. Very likely.
- **The same title and artist, but a different length**, means the same song
  in different versions — a studio take and a live one. These are *not*
  duplicates, and the third group is reported but never offered for removal.

A title on its own is never evidence: two different songs can share a name,
and a library that removed them would lose music. The artist comparison is
looser than the title comparison, because the names are written by hand:
"温拿" and "温拿乐队" are the same band, and "陈奕迅 + 王菲" and
"王菲 & 陈奕迅" are the same two people in a different order.

The comparison is made in one Chinese script, so a track in Traditional and
one in Simplified are recognised as the same song.

### The catalogue search (`ytmusic.rs`)

`music.youtube.com/youtubei/v1/search` is the internal API the web player
searches through, and it answers with the songs rather than with the videos.

The filter in the request decides what comes back. The one for songs is
`EgWKAQIIAWoMEA4QChADEAQQCRAF`; without it the answer is albums and
artists, which carry no video id and so cannot be downloaded.

**The locale.** The country decides which catalogue is searched and the
language decides how the results are spelled. Both come from the interface
language, because that is what the user chose. A Chinese reader in Taiwan
gets the Taiwanese catalogue.

**The popular songs.** The charts themselves are not available without a
paid subscription, so the songs a region is listening to are found by
searching for the words that region uses for them — "熱門歌曲" in Taiwan,
"人気曲" in Japan. The search is answered from that region's catalogue, so
the answer is the songs the region is listening to.

**The collections.** The answer holds the songs *and* the collections that
were put together around them: a "20 songs of X" album, a DJ mix, a chart.
Those are not songs, and they are dropped — a collection has a long title and
a count in it, and a song has a name and an artist.

### The order of the steps

1. The cookie export is converted and checked. This is first because it is
   the step that fails most often, and the failure it prevents is the worst
   one to debug.
2. `yt-dlp` is asked what the track is. This is cheap, and it gives the user
   the title, the artist and the album before anything is downloaded, so a
   wrong URL is caught before a large file is fetched.
3. The download runs.
4. The metadata is corrected: the album, the year and the cover are replaced,
   and the text is converted to the script the user asked for. The conversion
   comes last, so it also converts the album name that was just written.

A failure in step 4 does not fail the download: the file is already there,
and a wrong album name is a smaller problem than a lost download.

### The rate limit

The release database asks for no more than one request a second and answers
anything faster with "503 Service Unavailable". A download triggers a lookup,
so a batch would otherwise trip the limit — and the failure would look like
the service being down rather than like being too fast. The requests are
therefore spaced out by a process-wide lock.
