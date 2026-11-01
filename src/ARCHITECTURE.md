# Sing-Along Frontend Architecture

## Architecture Overview

Vue 3 frontend in Tauri webview. Handles UI, playback, library browsing, lyric editing, and backend communication. Session state only; persistence in Rust/SQLite.

**Tech Stack**: Vue 3 (`<script setup>`), Vite, Tailwind CSS, Vue Final Modal, Floating Vue, Vue Toastification, TanStack Vue Virtual, CodeMirror, `unplugin-icons` + Iconify Material Design Icons (`@iconify-json/mdi`).

**Code Quality**: ESLint with Vue plugin + Prettier for formatting. Run `npm run lint` to check, `npm run format` to format.

**Core Patterns**:
| Pattern | Implementation |
|---------|---------------|
| Shell + modals | Main workspace + modal tasks (no router) |
| Composable state | Module-level refs, no store library |
| Backend-owned persistence | State from Rust commands, minimal client caching |
| Event-driven updates | Backend pushes scan/playback events |
| Virtualized lists | `@tanstack/vue-virtual` for large libraries |

## Project Structure

```
src/
├── App.vue                 # Root shell
├── main.js                 # Entry: plugins, Tailwind import
├── style.css               # Tailwind + custom classes
├── assets/                 # Static images
├── components/
│   ├── common/             # Reusable controls
│   ├── icons/              # Custom icons
│   ├── library/            # Library views + lyric edit/search
│   └── now-playing/        # Playback controls
├── composables/            # Shared state (player, downloader, search, edit)
├── i18n/                   # vue-i18n setup + locales/ (17 languages)
└── utils/                  # Helpers (lyrics, durations, linting)
```

## State Management

Module-level ref composables (singletons by design):

| Composable               | Purpose                                                                                                                                                                                                                                                                                                                                                                                                                      |
| ------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `useGlobalState()`       | `isHotkey`, `themeMode`, `lrclibInstance`                                                                                                                                                                                                                                                                                                                                                                                    |
| `usePlayer()`            | `playingTrack`, `status`, `duration`, `progress`, `volume`, plus queue state (`queueLength`, `queueIndex`, `shuffle`, `repeat`, `hasNext`, `hasPrevious`) and transport actions (`playNext`, `playPrevious`, `setShuffle`, `cycleRepeatMode`). Supports both library tracks (with `id`) and file-based tracks (with `file_path`). Listens to `player-state` events, which the backend emits only when state actually changes |
| `useDownloader()`        | Download queue, progress. Loop started by App.vue at boot                                                                                                                                                                                                                                                                                                                                                                    |
| `useExporter()`          | Mass export queue, progress; each queued track retains submitted formats. Used by ExportViewer modal                                                                                                                                                                                                                                                                                                                         |
| `useSearchLibrary()`     | Shared search text and track-centric filters; used by Tracks, Albums, and Artists tabs                                                                                                                                                                                                                                                                                                                                       |
| `useSearchLyrics()`      | Search modal state                                                                                                                                                                                                                                                                                                                                                                                                           |
| `useEditLyricsV2()`      | Edit lyrics modal state                                                                                                                                                                                                                                                                                                                                                                                                      |
| `useLibraryNavigation()` | Cross-tab navigation: clicking an album/artist name in `TrackItem.vue` or `NowPlaying.vue` switches to the Albums/Artists tab and opens the corresponding entity via `AlbumList`/`ArtistList` exposed methods                                                                                                                                                                                                                |

**Boot Flow**: `main.js` → Vue app init → `App.vue` checks `get_init()` → shows `ChooseDirectory.vue` (setup) or `Library.vue` (main). Loads config, applies theme, starts downloader loop, then subscribes to the `notification` event and drains the backend queue once.

**Notifications**: the backend pushes a `notification` event per toast. `App.vue` listens and also calls `drain_notifications()` exactly once at boot, to pick up messages produced before the listener existed (e.g. the audio-device failure path). There is deliberately no polling timer.

## UI Architecture

**Main Shells**:

- `ChooseDirectory.vue` - Setup: folder picker, persists via `set_directories`, emits to trigger library view
- `Library.vue` - Header + tabbed panes (Tracks/Albums/Artists/MyLrclib) + `NowPlaying.vue`. Manages scan lifecycle (`scan-progress`, `scan-complete`, `scan_library`)
- `NowPlaying.vue` - Persistent bottom panel. Track metadata, seek/play/volume/speed, lyrics. Keyboard shortcuts (space/enter/arrows) disabled when typing or via `isHotkey` state

**Modals**: `Config.vue`, `About.vue`, `DownloadViewer.vue`, `YoutubeDownload.vue`

**Common Components**:

- `BaseModal.vue` — Reusable modal wrapper using Vue Final Modal with consistent styling
- `ConfirmModal.vue` — Simple confirmation modal for user actions (e.g., confirming close on unsaved changes)

## Internationalisation

`src/i18n/index.js` exports the configured vue-i18n instance plus
`AVAILABLE_LOCALES`, `resolveLocale`, `initLocale` and `setLocale`.

**Never read `localStorage` at module import time.** The locale file is imported
by Node test runs where `localStorage` does not exist; `initLocale()` is called
from `main.js` after the DOM is ready for exactly that reason. A module-level
`readLocalePreference()` here breaks every test that transitively imports it.

**Locale resolution** walks `navigator.languages` and takes the first tag that
matches a shipped locale, so `['fr-CA', 'en']` yields French rather than falling
to English. Matching is tried in order: the full tag (`pt-BR`), then the primary
subtag (`fr-CA` → `fr`), then a shipped locale sharing the primary subtag
(`pt-PT` → `pt-BR`, the only Portuguese that ships). Chinese is special-cased on
the script/region subtag first: `Hant`/`TW`/`HK`/`MO` give `zh-Hant`, everything
else under `zh` gives `zh-Hans`.

**Adding a language** is three steps: copy `locales/en.js` to
`locales/<tag>.js`, translate the values (never the keys), then add the module to
the imports, the `messages` map, and `AVAILABLE_LOCALES` in `index.js`.
`i18n.test.js` enforces the rest — every locale must have exactly the English key
set, no empty strings, and the same `{placeholder}` set per key as English, which
is what catches a dropped `{count}` before it renders literally on screen.

**Window title** is applied through `applyWindowTitle()`, which writes both
`document.title` and the OS window title via the Tauri window API. That API is a
static import on purpose: a `await import('@tauri-apps/api/window')` fails to
resolve under Vite, and the resulting rejection was once swallowed by a `catch`,
leaving the title stuck on whatever `tauri.conf.json` said.

## Library & Lyrics System

**Library Browsing**:

- Search via `useSearchLibrary()`; shared search text across Tracks/Albums/Artists tabs
- `MiniSearch.vue` with context-aware placeholder; filter dropdown hidden for Albums/Artists
- All tabs use virtualized lists (`@tanstack/vue-virtual`) with IDs only
- Clickable album/artist names in track rows and the NowPlaying panel navigate to the respective tab and open the entity detail view (`AlbumTrackList`/`ArtistTrackList`)

**Lyrics Workflows**:

| Aspect                 | Details                                                                                                                                               |
| ---------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Display**            | `LyricsViewer.vue` (synced) / `PlainLyricsViewer.vue`. Click to `seek()`                                                                              |
| **Search**             | `SearchLyrics.vue` + `Preview.vue` for LRCLIB lookup; word-level highlight when available                                                             |
| **Normalization**      | `normalizeLrclibLyrics()` derives plain/synced/instrumental from `lyricsfile` when LRCLIB omits direct fields                                         |
| **Edit/Publish**       | `EditLyricsV2.vue` + `useEditLyricsV2Publish.js` + `useEditLyricsV2Export.js`. For detailed editor behavior, see **Edit/Publish Details** below.      |
| **Keyboard Shortcuts** | `KeyboardShortcutsModal.vue` + shared registry in `composables/edit-lyrics-v2/shortcutRegistry.js`. See **Keyboard Shortcuts Details** below.         |
| **Mass Export**        | `LibraryHeader.vue` → `ExportViewer.vue` → `useExporter()` queue → `export_track_lyrics` per track                                                    |
| **My LRCLIB**          | User workflows (preview, edit, publish, flag) in `my-lrclib/`                                                                                         |
| **Track Association**  | My LRCLIB edit flow: `prepare_lrclib_lyricsfile()` → `AssociateTrackModal.vue` → `EditLyricsV2.vue` with `trackId: null` (temporary association only) |

### Download and Export preferences

`Library.vue` mounts one `DownloadOptionsPopup.vue` for the library-wide button and the album/artist row and detail Download actions. `useDownloadOptions()` owns the shared draft, hydration, submission, and queue preparation. `useDownloadTrigger()` captures an immutable target ID/name and the clicked anchor; row triggers hold no separate popup or preference state. Scrolling outside the popup, recycling/unmounting its trigger, Escape, or dismissal clears the draft. Late hydration or hide events from an older target cannot change the new session. Only confirmation saves preferences and queues tracks for the captured target with the confirmed filters. The manual Export popup stays separate. `useExportPreferences()` (`composables/export-preferences.js`) provides one reactive source backed by `get_config` / scoped `set_export_preferences` commands. Both popups load drafts on opening and save only on submission; dismissing discards drafts. No browser storage is used. Popup styling is scoped to `DownloadOptionsPopup.vue` and manual Export in `LibraryHeader.vue`: theme-aware error text, readable disabled actions, darker accent action backgrounds for text contrast, and viewport-bounded scrolling.

The Download popup also drafts the three existing Configuration “Download lyrics for” choices above a separator and the auto-export controls. `utils/download-filter.js` matches Configuration’s skip-flag mapping (plain-flag precedence), and submission saves those existing flags with export preferences in one scoped update. Queue filters use the captured submission, not returned/re-read config. Configuration remains available and refreshes from `get_config` before opening. The popup contains controls and actionable errors without explanatory export copy.

Shared fields are `export_txt`, `export_lrc`, and `export_embedded`. `auto_export_enabled` belongs only to Download. Download updates applicable format choices when enabled; turning it off preserves all formats. Manual Export does not update the toggle. Both popups show the embedded choice under the experimental `try_embed_lyrics` gate and preserve its remembered value when disabled. Defaults: auto-export off, LRC selected, TXT/embedded off. Automatic export accepts embed-only batches when the gate is enabled; at least one effective format is required.

The manual Export popup also carries two fields of its own:

- **File name** — the `{variable}` template a sidecar is written with (see *Export filename templates* below). It is saved with the other export preferences, but on its own submit, so a template the backend rejects leaves the popup open on its own error and restores the stored text rather than starting the batch.
- **Preview only** — resolves the export for one track through `plan_export_lyrics` and reports what it would do, without writing or deleting anything. A sidecar write removes the opposite extension first, so this is where a template pointing at the wrong folder is caught before a batch runs.

`downloader.js` queues `{ trackId, autoExport }` entries; `autoExport` is an immutable TXT/LRC/embedded snapshot or null. The album/artist row actions (`AlbumItem.vue`, `ArtistItem.vue`) and detail actions (`AlbumTrackList.vue`, `ArtistTrackList.vue`) use the same popup and snapshots as Download All. Scope queries use `get_album_track_ids` / `get_artist_track_ids` with the submitted skip flags; library-wide queries retain their existing presence filters. Single-track Download, Apply, and editor saves are unchanged. `export.js` queues `{ trackId, formats }` with immutable ordered TXT/LRC/embedded selections. Subsequent submissions cannot mutate queued batches.

`download_lyrics` still returns a string. Post-save export failures are appended to that string and stay on the existing green success/FOUND log path in `DownloadViewer.vue`. No reporting UI or retry flow is added. The shared writers still overwrite targets and delete the opposite sidecar; format-operation success does not imply both files remain.

### Export filename templates

Where an exported sidecar lands used to be hardcoded to the audio file's stem: `01. Song.mp3` always produced `01. Song.lrc`. The manual Export popup now carries a **File name** field holding a relative path with `{variable}` placeholders, resolved against the audio file's own directory:

```
{filename}.{ext}                                         # the default, = the old behaviour
{artist}/{album}/{track_number_padded} - {title}.lrc
lyrics/{title_sanitized}.{ext}
```

**Variables:** `title`, `artist`, `album`, `album_artist`, `track_number`, `track_number_padded`, `filename` (the audio file's stem) and `ext` (the format's own extension). Append `_sanitized` to any of them to replace filesystem-unsafe characters. Directories are made with `/`.

**Validation is the backend's.** `plan_export_lyrics` and the writers both resolve the template server-side, and an unknown variable is refused by `set_export_preferences` before it is stored. `LibraryHeader.vue` submits the template separately from the format checkboxes so a rejected one shows its own error and restores the stored text, instead of failing the whole submission. `export-workflows.test.js` covers both paths.

**Preview only.** `plan_export_lyrics(trackId, formats)` returns what each format would do — the resolved path, or the reason it skips — and writes nothing. The popup shows the first entry, which is enough to answer \"is this template pointing where I think?\"; the point is to check before a batch, not to enumerate it.

### Downloading from YouTube

A track can be fetched from YouTube through the user's own `yt-dlp`. The
frontend does three things, in this order:

1. **Look up the track.** `probe_youtube_track` asks `yt-dlp` what the track is
   and returns the title, the artist, the album YouTube reports and the album
   the release database has. The user sees both before anything is
   downloaded, so a wrong URL is caught before a large file is fetched.
2. **Download it.** `download_youtube_track` runs the download, corrects the
   metadata, adds the file to the library, fetches its lyrics, and returns
   where the file landed. The `duplicateChoice` says what to do about a
   track the library already holds: replace it, keep both, or cancel.
3. **Report the outcome.** The toast says what was downloaded, with the
   album name that ended up in the file, and whether lyrics were found.

The dialog has two switches: one shows what the downloader is doing, line by
line, with the percentage and the speed; the other plays the track as soon as
it is downloaded. The progress arrives as a `youtube-download-progress`
event, which the backend emits while the download runs.

The YouTube dialog searches the YouTube Music catalogue rather than taking a
pasted URL: the point is to pick songs, and a pasted URL is a link to one of
them. The catalogue that is searched is the one for the interface language, and
an empty search shows the songs that are popular there. Several songs can be
picked at once; the download command takes a list and fetches them in turn.

The metadata repair is a third dialog, also reached from the menu. It finds
the album names that came from a folder — a name under more than one artist,
because a real album belongs to one — looks the tracks up in the release
database, and shows what each would become before anything is written. It
also writes the text in UTF-8: the text frames of an ID3v2 tag say what
encoding they are in, and a library collected over years holds files in
several.

The library check is a separate dialog, reached from the menu. It reports the
entries whose file is not on the disk and the pairs that look like the same
recording, grouped by how sure the library is. Nothing is removed without the
user's agreement.

The settings panel has the two settings the download needs: where the
`yt-dlp` is, and which cookie export to use. The cookie export is picked with
the file dialog and converted immediately, so the user only ever handles the
JSON that the browser extension wrote. The panel also reports how many
cookies there are, how many have expired and whether they carry a signed-in
session — a browser rotates its session cookies without warning, and the
failure that follows is a message blaming the user for something they did
not do.

`Config.vue` holds the three fields the backend stores: `ytdlp_binary`,
`ytdlp_cookies_json` and `ytdlp_output_dir`. They are written with
`set_config` alongside everything else, and the downloader reads them back
with `get_ytdlp_status`, which is also what the panel calls to show the
state.

### Edit/Publish Details

`EditLyricsV2.vue` combines CodeMirror plain editing and synced editing with a word timing lane.

- Props/context: `audioSource` (playback source), `lyricsfile` (editing target), `trackId` (save behavior)
- Instrumental mode: toggle via `PlainLyricsEmptyState.vue` / `SyncedLyricsEmptyState.vue`
- Publish/export: handled by `useEditLyricsV2Publish.js` and `useEditLyricsV2Export.js`
- Synced lines: multi-line selection via drag and Ctrl/Cmd+click, with floating bulk rewind/forward/delete toolbar
- Synced line nudge shortcuts: `Left`/`Right` adjust selected line start by `-/+100ms`; `Shift+Left`/`Shift+Right` adjust selected line end by `-/+100ms`
- End timestamp visibility: in synced rows, the end timestamp pill stays visible even without hover when it differs from the next line's start timestamp (helps surface gaps/overlaps), and color-codes direction (`before` = gap, `after` = overlap)
- Player bar: playback speed control (`0.5x`-`2.0x`)
- Line status: each synced line row shows a tiny word-sync status dot
- Word timing: multi-separator selection (Ctrl/Cmd+click + Shift+click), merge separators (`Delete`/`Backspace`), hover split preview snapped to grapheme boundaries, double-click split at cursor, and `Z` syncs selected separator then advances (last-word sync advances to next line)
- Narrow segment hint: when a segment is too narrow to show text, a visible hint is rendered beneath it with the next word text
- Boundary sync: can cascade adjacent boundaries so sync is not blocked by intervening separators, while staying within line bounds
- Reset behavior: clears persisted word timings and reloads default (non-persisted) segmentation
- Line-start sync behavior: syncing line start shifts existing word boundaries by the same offset
- Selection behavior: selecting a synced line starts at the second boundary by default

### Keyboard Shortcuts Details

Shortcut behavior and shortcut-menu content share one canonical registry:

- Definitions live in `composables/edit-lyrics-v2/shortcutRegistry.js`
- Runtime handlers consume that registry: `useEditLyricsV2Hotkeys.js`, `useEditLyricsV2SyncedHotkeys.js`, `useEditLyricsV2WordTimingHotkeys.js`
- Menu data (`keyboardShortcuts.js`) is derived from the same registry
- Result: shortcut behavior and displayed shortcut menu stay in sync
- Shortcut-aware button tooltips are also derived from the same registry (e.g., `Sync line to current playback (Space)`)
- Shortcuts are configurable via registry override APIs (`setShortcutOverride`, `resetShortcutOverride`, `resetAllShortcutOverrides`) and persisted in browser `localStorage`
- `KeyboardShortcutsModal.vue` includes a Configure mode window to remap shortcuts by key capture and reset per-shortcut or all shortcuts
- Configure mode detects duplicate shortcut assignments and shows warnings both globally and per conflicting shortcut
- Configure mode shows capture/result feedback through button color states (listening, assigned, canceled, reset)
- Modified shortcuts (customized from defaults) are highlighted via key chip and reset-button colors
- Access: header keyboard icon and `Ctrl+/` open `KeyboardShortcutsModal.vue`

Utils: `src/utils/` (parsing, linting), Composables: `composables/edit-lyrics-v2/`, `composables/export.js`. Default word timing tokenization uses backend `segment_words` (Charabia), with frontend tokenizer fallback.

## Technical Details

**Styling**: Tailwind CSS + custom classes in `style.css`. Primary accent palette (`hoa`) from `tailwind.config.cjs`. Structural colors (backgrounds, text, borders) use Tailwind's default `neutral` scale. Dark mode via `html.dark`. Semantic classes: `.button`, `.input`, `.select`, `.modal-content`, `.link`.

**Icons**: Icons are imported directly per-file from `~icons/mdi/*` (powered by `unplugin-icons` in `vite.config.js`). Avoid adding `mdue`; use MDI icon imports instead.

**Utilities**: `src/utils/` — duration formatting, line counts, lyric parsing/linting, Lyricsfile YAML helpers (including LRCLIB payload normalization for lyricsfile-first responses), and `lyrics-timeline.js`.

### Lyrics timeline lookups (`utils/lyrics-timeline.js`)

`LyricsViewer.vue` re-evaluates on every playback tick, so locating the playing line must not be a linear scan. `findLineIndexAt` is a binary search over the sorted `start_ms` values, `lineEndMs` resolves a line's end (explicit `end_ms` → next line's start → `Infinity`), and `findPrimaryLineIndex` returns the most recently started still-playing line (resolving overlaps toward the later line, and falling back to the most recent past line during gaps). Covered by `lyrics-timeline.test.js`, including an exhaustive comparison against a linear scan.

The expanded lyrics overlay's scroll offset is measured only while the overlay is open — it walks rendered DOM elements, so computing it on every tick while collapsed was pure waste.

### Playback System

The player supports two types of tracks via a unified `PlayableTrack` type:

| Track Source       | Required Fields | Backend Command                               |
| ------------------ | --------------- | --------------------------------------------- |
| Library (Database) | `id`            | `play_track({ trackId: id, ...metadata })`    |
| File Picker        | `file_path`     | `play_track({ filePath: path, ...metadata })` |

**`usePlayer().playTrack(track, queue?, index?)`** automatically detects the track source:

- If `track.id` is present → Database track, fetches full data from SQLite
- If only `track.file_path` is present → File-based track, metadata extracted from file or provided directly

**Queue wiring.** A virtualised `TrackItem` only knows its own id, so it cannot build a queue; it emits `playWithQueue` and the owning list attaches its full `trackIds` plus the row index (`TrackList`, `AlbumTrackList`, `ArtistTrackList`), forwarded through `AlbumList`/`ArtistList` to `Library.vue`, which calls `playTrack(track, queue, index)`. Without a queue the backend falls back to a one-track queue, so a bare call or a file-picker track behaves predictably.

**Automatic advance.** When the backend moves to another track on its own, it is the only side that knows what is now playing. `usePlayer` watches `trackId` in `player-state` and refetches the track when it differs from the one it last saw, rather than guessing client-side.

**Transport UI** (`NowPlaying.vue`): previous/next rows appear only when `queueLength > 1`; shuffle and repeat buttons live in the right-hand group next to speed and volume, and repeat cycles `off → all → one` via `cycleRepeatMode()`.

This enables the V2 lyrics editor to support playback for:

- Scanned library tracks (full features)
- Arbitrary files from file picker (full features)
- Tracks without audio (disabled playback, manual timestamp editing only)

## Testing

**Framework**: Vitest. Run `npm test` (once) or `npm run test:watch` (watch mode).

Tests live next to the source files they exercise (e.g. `word-tokenizer.test.js` for `word-tokenizer.js`). Add tests for any utilities that involve non-trivial branching logic (e.g. parsing, tokenization, transformations). `export-workflows.test.js` verifies shared preferences and batch snapshots with mocked commands. `download-options.test.js` uses a minimal Vue renderer to exercise all four album/artist trigger setups, recycling/unmount lifecycle, target isolation, and scope queries without a DOM or real downloads. DOM component tests are not yet set up.
