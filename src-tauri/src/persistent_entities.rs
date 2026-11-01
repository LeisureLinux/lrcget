use serde::Serialize;

/// A track that can be played - either from database or from a file path.
/// This is used by the player and can represent both scanned library tracks
/// and arbitrary tracks selected via file picker.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayableTrack {
    pub id: Option<i64>, // None for file-based tracks not in database
    pub file_path: String,
    pub file_name: String,
    pub title: String,
    pub album_name: String,
    pub artist_name: String,
    pub album_artist_name: Option<String>,
    pub image_path: Option<String>,
    pub track_number: Option<i64>,
    pub duration: f64,
    pub instrumental: bool,
    pub lyricsfile: Option<String>,
    pub lyricsfile_id: Option<i64>, // ID from lyricsfiles table
}

impl From<PersistentTrack> for PlayableTrack {
    fn from(track: PersistentTrack) -> Self {
        Self {
            id: Some(track.id),
            file_path: track.file_path,
            file_name: track.file_name,
            title: track.title,
            album_name: track.album_name,
            artist_name: track.artist_name,
            album_artist_name: track.album_artist_name,
            image_path: track.image_path,
            track_number: track.track_number,
            duration: track.duration,
            instrumental: track.instrumental,
            lyricsfile: track.lyricsfile,
            lyricsfile_id: track.lyricsfile_id,
        }
    }
}

#[derive(Clone, Serialize)]
pub struct PersistentTrack {
    pub id: i64,
    pub file_path: String,
    pub file_name: String,
    pub title: String,
    pub album_name: String,
    pub album_artist_name: Option<String>,
    pub album_id: i64,
    pub artist_name: String,
    pub artist_id: i64,
    pub image_path: Option<String>,
    pub track_number: Option<i64>,
    pub txt_lyrics: Option<String>,
    pub lrc_lyrics: Option<String>,
    pub lyricsfile: Option<String>,
    pub lyricsfile_id: Option<i64>, // ID from lyricsfiles table (null if no lyricsfile exists)
    pub duration: f64,
    pub instrumental: bool,
}

#[derive(Serialize)]
pub struct PersistentAlbum {
    pub id: i64,
    pub name: String,
    pub image_path: Option<String>,
    pub artist_name: String,
    pub album_artist_name: Option<String>,
    pub tracks_count: i64,
}

#[derive(Serialize)]
pub struct PersistentArtist {
    pub id: i64,
    pub name: String,
    // pub albums_count: i64,
    pub tracks_count: i64,
}

#[derive(Serialize)]
pub struct PersistentConfig {
    pub auto_export_enabled: bool,
    pub export_lrc: bool,
    pub export_txt: bool,
    pub export_embedded: bool,
    pub skip_tracks_with_synced_lyrics: bool,
    pub skip_tracks_with_plain_lyrics: bool,
    pub show_line_count: bool,
    pub try_embed_lyrics: bool,
    pub theme_mode: String,
    pub lrclib_instance: String,
    pub volume: f64,
    /// 'ask' | 'off' | 'simplified' | 'traditional'
    pub lyric_script: String,
    /// Whether LRCLIB misses fall back to NetEase (unofficial API).
    pub netease_fallback_enabled: bool,
    pub sync_cover: bool,
    /// Empty means direct connection. See migration 22.
    pub proxy_url: String,
    /// Whether to look for a newer release on startup.
    pub update_check_enabled: bool,
    /// Last version the user was told about, so it is only announced once.
    pub last_announced_version: String,
    /// Where the AMLL word-level lyrics database is served from.
    ///
    /// Empty means the built-in default; a mirror serves the same paths, and the
    /// database is one volunteer-maintained repository.
    pub amll_db_url: String,
    /// The `yt-dlp` to run for downloads from YouTube. Empty means the one on
    /// PATH.
    pub ytdlp_binary: String,
    /// The browser cookie export the user points at.
    ///
    /// The JSON the browser extension writes. The text format `yt-dlp` reads is
    /// derived from it.
    pub ytdlp_cookies_json: String,
    /// Where the downloads land. Empty means the first watched directory.
    pub ytdlp_output_dir: String,
    /// The filename template sidecar exports are written with.
    ///
    /// A relative path with `{variable}` placeholders, resolved against the
    /// audio file's own directory. Empty means the default `{filename}.{ext}`,
    /// which is the historical behaviour.
    pub export_template: String,
}
