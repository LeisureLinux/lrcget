use crate::export_template::{ensure_extension, ExportTemplate};
use crate::lyricsfile::ParsedLyricsfile;
use crate::parser::lrc::parse_lrc;
use crate::persistent_entities::PersistentTrack;
use anyhow::{Context, Result};
use lofty::config::WriteOptions;
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::id3::v2::{
    BinaryFrame, CommentFrame, Frame, FrameId, Id3v2Tag, SyncTextContentType,
    SynchronizedTextFrame, TimestampFormat, UnsynchronizedTextFrame,
};
use lofty::TextEncoding;
use serde::Serialize;
use std::fs::{remove_file, write};
use std::io::Seek;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors that can occur during export operations
#[derive(Error, Debug)]
pub enum ExportError {
    #[error("Failed to build export path: {0}")]
    PathBuildError(String),

    #[error("Failed to write file: {0}")]
    WriteError(String),

    #[error("Failed to embed lyrics: {0}")]
    EmbedError(String),

    #[error("Invalid lyrics data: {0}")]
    InvalidData(String),
}

/// Export format types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    /// Plain text format (.txt)
    Txt,
    /// Standard LRC format (.lrc), line-level
    Lrc,
    /// Enhanced LRC (A2) with inline `<mm:ss.xx>` word tags (.alrc)
    ///
    /// Read by Gramophone, Fluxio and other A2-aware Android players. Kept
    /// distinct from `Lrc` because sending word tags to a player whose LRC
    /// reader is line-level shows the timestamps as lyric text.
    LrcA2,
    /// TTML with one `<span>` per word (.ttml)
    ///
    /// The only word-level format iOS players read; Steve's LRC reader stops at
    /// line level.
    Ttml,
    /// Embedded in audio file metadata
    Embedded,
}

/// Status of an export operation
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "type", content = "message")]
pub enum ExportStatus {
    /// Export was successful
    Success,
    /// Export was skipped (e.g., no lyrics available for this format)
    Skipped(String),
    /// Export failed with an error
    Error(String),
}

/// Result of an export operation
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    pub format: ExportFormat,
    pub path: Option<PathBuf>,
    pub status: ExportStatus,
}

/// Build the file path for a lyrics sidecar file.
///
/// `template` decides the name. The default `{filename}.{ext}` reproduces the
/// historical `{stem}.{ext}` behaviour, which is why this still exists as a
/// separate function: the CLI calls it with a path only.
pub fn build_sidecar_path(track_path: &str, extension: &str) -> Result<PathBuf, ExportError> {
    build_sidecar_path_for_track(&path_only_track(track_path), extension, &ExportTemplate::default())
}

/// Resolve a sidecar path for a real library track through a template.
///
/// The template is relative and the result is resolved against the audio file's
/// own directory, so a template can only ever place the sidecar beside the
/// track (or in a subfolder of it) — never somewhere else in the filesystem.
pub fn build_sidecar_path_for_track(
    track: &PersistentTrack,
    extension: &str,
    template: &ExportTemplate,
) -> Result<PathBuf, ExportError> {
    let vars = crate::export_template::TemplateVars::from_track(track);
    let relative = template
        .resolve(&vars, extension)
        .map_err(|err| ExportError::PathBuildError(err.to_string()))?;

    let parent = Path::new(&track.file_path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            ExportError::PathBuildError("Track has no parent directory".to_string())
        })?;

    Ok(ensure_extension(parent.join(relative), extension))
}

/// A `PersistentTrack` carrying only a path, for the path-only helpers.
///
/// The template reads metadata off a track, but `build_sidecar_path` is called
/// with a path and an extension only — from the CLI, and from the tests. Every
/// field the template could use is empty here, so a template that needs real
/// metadata fails with an empty-name error instead of inventing values.
fn path_only_track(track_path: &str) -> PersistentTrack {
    PersistentTrack {
        id: 0,
        file_path: track_path.to_owned(),
        file_name: Path::new(track_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned(),
        title: String::new(),
        album_name: String::new(),
        album_artist_name: None,
        album_id: 0,
        artist_name: String::new(),
        artist_id: 0,
        image_path: None,
        track_number: None,
        txt_lyrics: None,
        lrc_lyrics: None,
        lyricsfile: None,
        lyricsfile_id: None,
        duration: 0.0,
        instrumental: false,
    }
}

/// A resolved sidecar path plus what the export would do to it.
///
/// Returned by [`plan_export`] and never written to. This is the dry-run: the
/// same decisions the real export makes, without the writes.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportPlan {
    pub format: ExportFormat,
    /// Where the file would be written. `None` when the format is skipped.
    pub path: Option<PathBuf>,
    /// What would happen, in words a user can act on.
    pub action: String,
}

/// What an export intends to do, without doing it.
///
/// The real export path is destructive in a way no rollback can undo: a sidecar
/// is written *and the opposite extension is deleted first*, so a failure
/// halfway through a batch leaves the user with fewer files than they started
/// with. Running the plan first turns that from a surprise into a decision.
pub fn plan_export(
    track: &PersistentTrack,
    parsed: &ParsedLyricsfile,
    formats: &[ExportFormat],
    template: &ExportTemplate,
) -> Vec<ExportPlan> {
    let mut plans = Vec::with_capacity(formats.len());

    for format in formats {
        match content_for_format(track, parsed, *format) {
            Some((_, extension)) => match *format {
                // There is no sidecar for embedding: the audio file itself is
                // what gets rewritten, so that is what the plan points at.
                ExportFormat::Embedded => plans.push(ExportPlan {
                    format: *format,
                    path: Some(PathBuf::from(&track.file_path)),
                    action: "embed into the audio file".to_owned(),
                }),
                _ => {
                    let sidecar = planned_sidecar(track, template, extension);
                    let action = match conflicting_sidecar(&sidecar, extension) {
                        Some(other) => format!(
                            "write {}, replacing {}",
                            sidecar.display(),
                            other.display()
                        ),
                        None => format!("write {}", sidecar.display()),
                    };
                    plans.push(ExportPlan {
                        format: *format,
                        path: Some(sidecar),
                        action,
                    });
                }
            },
            None => plans.push(ExportPlan {
                format: *format,
                path: None,
                action: skip_reason(parsed, *format),
            }),
        }
    }

    plans
}

/// Where a sidecar would land, for the plan.
///
/// A template that cannot be resolved is not fatal to the plan — the real
/// export fails the same way, and the plan is where the user should learn about
/// it. Falling back to the audio file keeps the entry present.
fn planned_sidecar(track: &PersistentTrack, template: &ExportTemplate, extension: &str) -> PathBuf {
    let path = build_sidecar_path_for_track(track, extension, template)
        .unwrap_or_else(|_| PathBuf::from(&track.file_path));
    ensure_extension(path, extension)
}

/// The other sidecar that gets removed when a sidecar is written.
///
/// This mirrors real behaviour, which is deliberate: writing `.lrc` and leaving
/// `.txt` behind means two files disagreeing about the same lyrics, and most
/// players read whichever they find first.
fn conflicting_sidecar(sidecar: &Path, extension: &str) -> Option<PathBuf> {
    let other = match extension {
        "txt" => "lrc",
        "lrc" => "txt",
        _ => return None,
    };
    let candidate = sidecar.with_extension(other);
    candidate.exists().then_some(candidate)
}

/// Generate plain text lyrics content from parsed lyricsfile
pub fn generate_txt_content(parsed: &ParsedLyricsfile) -> Option<String> {
    if parsed.is_instrumental {
        return None;
    }

    parsed.plain_lyrics.clone().filter(|s| !s.trim().is_empty())
}

/// Generate standard LRC format content from parsed lyricsfile
pub fn generate_lrc_content(parsed: &ParsedLyricsfile) -> Option<String> {
    if parsed.is_instrumental {
        return Some(crate::lyricsfile::INSTRUMENTAL_LRC.to_string());
    }

    parsed
        .synced_lyrics
        .clone()
        .filter(|s| !s.trim().is_empty())
}

/// Export lyrics for a single track in the specified format
pub fn export_track_format(
    track: &PersistentTrack,
    parsed: &ParsedLyricsfile,
    format: ExportFormat,
) -> Result<ExportResult, ExportError> {
    export_track_format_with_template(track, parsed, format, &ExportTemplate::default())
}

/// The content a format would write, and the extension it writes it with.
///
/// Shared by the plan and the real export so the two cannot disagree about
/// whether a format has anything to write.
fn content_for_format(
    track: &PersistentTrack,
    parsed: &ParsedLyricsfile,
    format: ExportFormat,
) -> Option<(String, &'static str)> {
    match format {
        ExportFormat::Txt => generate_txt_content(parsed).map(|c| (c, "txt")),
        ExportFormat::Lrc => generate_lrc_content(parsed).map(|c| (c, "lrc")),
        ExportFormat::Embedded => Some((String::new(), "")),
        ExportFormat::LrcA2 => word_synced_lines(parsed)
            .map(|lines| (crate::word_sync::to_enhanced_lrc(&lines), "alrc")),
        ExportFormat::Ttml => word_synced_lines(parsed).map(|lines| {
            (
                crate::word_sync::to_ttml(
                    &lines,
                    &track.title,
                    &track.artist_name,
                    &track.album_name,
                ),
                "ttml",
            )
        }),
    }
}

/// Why a format has nothing to write, in words.
fn skip_reason(parsed: &ParsedLyricsfile, format: ExportFormat) -> String {
    if parsed.is_instrumental {
        return "instrumental track".to_owned();
    }
    match format {
        ExportFormat::Txt => "no plain lyrics available".to_owned(),
        ExportFormat::Embedded => "unsupported audio format (MP3/FLAC only)".to_owned(),
        _ => "no timed lyrics to export".to_owned(),
    }
}

/// The word-timing lines, or `None` when there is nothing to time.
fn word_synced_lines(parsed: &ParsedLyricsfile) -> Option<Vec<crate::word_sync::WordSyncLine>> {
    if parsed.is_instrumental {
        return None;
    }
    let synced = parsed.synced_lyrics.clone().filter(|s| !s.trim().is_empty())?;
    crate::word_sync::build_word_sync(&synced)
}

/// Export lyrics for a single track in the specified format, through a template.
pub fn export_track_format_with_template(
    track: &PersistentTrack,
    parsed: &ParsedLyricsfile,
    format: ExportFormat,
    template: &ExportTemplate,
) -> Result<ExportResult, ExportError> {
    match format {
        ExportFormat::Txt => export_txt(track, parsed, template),
        ExportFormat::Lrc => export_lrc(track, parsed, template),
        ExportFormat::LrcA2 => export_word_synced(track, parsed, true, template),
        ExportFormat::Ttml => export_word_synced(track, parsed, false, template),
        ExportFormat::Embedded => export_embedded(track, parsed),
    }
}

/// Write a sidecar, creating the directories the template asked for.
///
/// A template may place lyrics in a subfolder of the track's directory
/// (`{artist}/{title}.lrc`), and that folder is not guaranteed to exist yet.
/// Creating it here rather than in `build_sidecar_path_for_track` keeps path
/// resolution pure — the plan must be able to name a path without creating
/// anything on disk.
fn write_sidecar(path: &Path, content: &str) -> Result<(), ExportError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ExportError::WriteError(format!("creating {}: {e}", parent.display())))?;
        }
    }
    write(path, content).map_err(|e| ExportError::WriteError(e.to_string()))
}

/// Export plain text lyrics to .txt file
fn export_txt(
    track: &PersistentTrack,
    parsed: &ParsedLyricsfile,
    template: &ExportTemplate,
) -> Result<ExportResult, ExportError> {
    let content = match generate_txt_content(parsed) {
        Some(content) => content,
        None => {
            // Not an error - just no plain lyrics available
            return Ok(ExportResult {
                format: ExportFormat::Txt,
                path: None,
                status: ExportStatus::Skipped("no plain lyrics available".to_string()),
            });
        }
    };

    let txt_path = build_sidecar_path_for_track(track, "txt", template)?;

    // Remove conflicting .lrc file if it exists
    let lrc_path = build_sidecar_path_for_track(track, "lrc", template).ok();
    if let Some(ref lrc_path) = lrc_path {
        let _ = remove_file(lrc_path);
    }

    write_sidecar(&txt_path, &content)?;

    Ok(ExportResult {
        format: ExportFormat::Txt,
        path: Some(txt_path),
        status: ExportStatus::Success,
    })
}

/// Export synced lyrics to .lrc file
fn export_lrc(
    track: &PersistentTrack,
    parsed: &ParsedLyricsfile,
    template: &ExportTemplate,
) -> Result<ExportResult, ExportError> {
    let content = match generate_lrc_content(parsed) {
        Some(content) => content,
        None => {
            // Not an error - just no synced lyrics available
            return Ok(ExportResult {
                format: ExportFormat::Lrc,
                path: None,
                status: ExportStatus::Skipped("no timed lyrics to export".to_string()),
            });
        }
    };

    let lrc_path = build_sidecar_path_for_track(track, "lrc", template)?;

    // Remove conflicting .txt file if it exists
    let txt_path = build_sidecar_path_for_track(track, "txt", template).ok();
    if let Some(ref txt_path) = txt_path {
        let _ = remove_file(txt_path);
    }

    write_sidecar(&lrc_path, &content)?;

    Ok(ExportResult {
        format: ExportFormat::Lrc,
        path: Some(lrc_path),
        status: ExportStatus::Success,
    })
}

/// Export word-level lyrics as either enhanced LRC (A2) or TTML.
///
/// Both come from the same word-timing pass, so the two files agree with each
/// other. Word timings are derived from the line-level synced lyrics that are
/// already stored; a track with no synced lyrics is skipped rather than having
/// timings invented for it.
fn export_word_synced(
    track: &PersistentTrack,
    parsed: &ParsedLyricsfile,
    enhanced_lrc: bool,
    template: &ExportTemplate,
) -> Result<ExportResult, ExportError> {
    let format = if enhanced_lrc {
        ExportFormat::LrcA2
    } else {
        ExportFormat::Ttml
    };

    if parsed.is_instrumental {
        return Ok(ExportResult {
            format,
            path: None,
            status: ExportStatus::Skipped("instrumental track".to_string()),
        });
    }

    let synced = match parsed.synced_lyrics.clone().filter(|s| !s.trim().is_empty()) {
        Some(synced) => synced,
        None => {
            return Ok(ExportResult {
                format,
                path: None,
                status: ExportStatus::Skipped("no timed lyrics to export".to_string()),
            })
        }
    };

    let lines = match crate::word_sync::build_word_sync(&synced) {
        Some(lines) => lines,
        None => {
            return Ok(ExportResult {
                format,
                path: None,
                status: ExportStatus::Skipped("no timed lines to convert".to_string()),
            })
        }
    };

    let (content, extension) = if enhanced_lrc {
        (crate::word_sync::to_enhanced_lrc(&lines), "alrc")
    } else {
        (
            crate::word_sync::to_ttml(
                &lines,
                &track.title,
                &track.artist_name,
                &track.album_name,
            ),
            "ttml",
        )
    };

    let path = build_sidecar_path_for_track(track, extension, template)?;
    write_sidecar(&path, &content)?;;

    Ok(ExportResult {
        format,
        path: Some(path),
        status: ExportStatus::Success,
    })
}

/// Export lyrics by embedding into audio file metadata
fn export_embedded(
    track: &PersistentTrack,
    parsed: &ParsedLyricsfile,
) -> Result<ExportResult, ExportError> {
    let path_lower = track.file_path.to_lowercase();
    if !path_lower.ends_with(".mp3") && !path_lower.ends_with(".flac") {
        return Ok(ExportResult {
            format: ExportFormat::Embedded,
            path: None,
            status: ExportStatus::Skipped("unsupported audio format (MP3/FLAC only)".to_owned()),
        });
    }
    let plain_lyrics = parsed.plain_lyrics.clone().unwrap_or_default();
    let synced_lyrics = if parsed.is_instrumental {
        crate::lyricsfile::INSTRUMENTAL_LRC.to_string()
    } else {
        parsed.synced_lyrics.clone().unwrap_or_default()
    };

    embed_lyrics(&track.file_path, &plain_lyrics, &synced_lyrics)
        .map_err(|e| ExportError::EmbedError(e.to_string()))?;

    Ok(ExportResult {
        format: ExportFormat::Embedded,
        path: Some(PathBuf::from(&track.file_path)),
        status: ExportStatus::Success,
    })
}

/// Export lyrics for a track in multiple formats
pub fn export_track(
    track: &PersistentTrack,
    parsed: &ParsedLyricsfile,
    formats: &[ExportFormat],
) -> Vec<ExportResult> {
    export_track_with_template(track, parsed, formats, &ExportTemplate::default())
}

/// Export lyrics for a track in multiple formats, through a template.
pub fn export_track_with_template(
    track: &PersistentTrack,
    parsed: &ParsedLyricsfile,
    formats: &[ExportFormat],
    template: &ExportTemplate,
) -> Vec<ExportResult> {
    export_track_with_embed_gate(track, parsed, formats, template, || true)
}

/// Consult the current experimental setting at each embedded operation, not at batch creation.
pub fn export_track_with_embed_gate(
    track: &PersistentTrack,
    parsed: &ParsedLyricsfile,
    formats: &[ExportFormat],
    template: &ExportTemplate,
    mut embed_allowed: impl FnMut() -> bool,
) -> Vec<ExportResult> {
    let mut results = Vec::with_capacity(formats.len());

    for format in formats {
        if *format == ExportFormat::Embedded && !embed_allowed() {
            results.push(ExportResult {
                format: *format,
                path: None,
                status: ExportStatus::Skipped(
                    "experimental embedding disabled or unavailable".to_owned(),
                ),
            });
            continue;
        }
        match export_track_format_with_template(track, parsed, *format, template) {
            Ok(result) => results.push(result),
            Err(e) => results.push(ExportResult {
                format: *format,
                path: None,
                status: ExportStatus::Error(e.to_string()),
            }),
        }
    }

    results
}

/// Embed lyrics into audio file metadata (MP3/FLAC)
pub fn embed_lyrics(track_path: &str, plain_lyrics: &str, synced_lyrics: &str) -> Result<()> {
    let path_lower = track_path.to_lowercase();

    if path_lower.ends_with(".mp3") {
        embed_lyrics_mp3(track_path, plain_lyrics, synced_lyrics)
    } else if path_lower.ends_with(".flac") {
        embed_lyrics_flac(track_path, plain_lyrics, synced_lyrics)
    } else {
        // Not an error - just not supported for this format
        Ok(())
    }
}

/// Embed lyrics into FLAC file using Vorbis comments
fn embed_lyrics_flac(track_path: &str, plain_lyrics: &str, synced_lyrics: &str) -> Result<()> {
    use lofty::config::ParseOptions;
    use std::fs::OpenOptions;

    let mut file_content = OpenOptions::new()
        .read(true)
        .write(true)
        .open(track_path)
        .context("Failed to open FLAC file")?;

    let mut flac_file = FlacFile::read_from(&mut file_content, ParseOptions::new())
        .context("Failed to parse FLAC file")?;

    if let Some(vorbis_comments) = flac_file.vorbis_comments_mut() {
        // Handle unsynced lyrics (USLT equivalent in FLAC)
        if !plain_lyrics.is_empty() {
            vorbis_comments.insert("UNSYNCEDLYRICS".to_string(), plain_lyrics.to_string());
        } else {
            let _ = vorbis_comments.remove("UNSYNCEDLYRICS");
        }

        // Handle synced lyrics (SYLT equivalent in FLAC)
        if !synced_lyrics.is_empty() {
            vorbis_comments.insert("LYRICS".to_string(), synced_lyrics.to_string());
        } else {
            let _ = vorbis_comments.remove("LYRICS");
        }

        file_content
            .seek(std::io::SeekFrom::Start(0))
            .context("Failed to seek in FLAC file")?;
        flac_file
            .save_to(&mut file_content, WriteOptions::default())
            .context("Failed to save FLAC file")?;
    }

    Ok(())
}

/// Embed lyrics into MP3 file using ID3v2 tags
fn embed_lyrics_mp3(track_path: &str, plain_lyrics: &str, synced_lyrics: &str) -> Result<()> {
    use lofty::file::TaggedFileExt;
    use lofty::probe::Probe;
    use lofty::id3::v2::Id3v2Tag;

    let file_probe = Probe::open(track_path).context("Failed to open MP3 file")?;
    let mut file = file_probe
        .guess_file_type()
        .context("Failed to guess file type")?
        .read()
        .context("Failed to read MP3 file")?;
    let mut primary_tag = file
        .remove(file.primary_tag_type())
        .context("Failed to find ID3v2 tag")?;
    let mut id3v2: Id3v2Tag = primary_tag.into();

    // Fix malformed COMMENT frames: re-insert with valid language code
    let removed_comments: Vec<_> = id3v2.remove(&FrameId::new("COMM")?).collect();
    for frame in removed_comments {
        if let Frame::Comment(comment) = frame {
            id3v2.insert(Frame::Comment(CommentFrame::new(
                comment.encoding,
                [b'X', b'X', b'X'],
                comment.description,
                comment.content,
            )));
        }
    }

    // Insert unsynchronized lyrics (USLT)
    insert_uslt_frame(&mut id3v2, plain_lyrics).context("Failed to insert USLT frame")?;
    // Insert synchronized lyrics (SYLT)
    insert_sylt_frame(&mut id3v2, synced_lyrics).context("Failed to insert SYLT frame")?;

    primary_tag = id3v2.into();
    file.insert_tag(primary_tag);
    file.save_to_path(track_path, WriteOptions::default())
        .context("Failed to save MP3 file")?;

    Ok(())
}

/// Insert USLT (unsynchronized lyrics) frame into ID3v2 tag
fn insert_uslt_frame(id3v2: &mut Id3v2Tag, plain_lyrics: &str) -> Result<()> {
    if !plain_lyrics.is_empty() {
        let uslt_frame = UnsynchronizedTextFrame::new(
            TextEncoding::UTF8,
            [b'X', b'X', b'X'],
            "".to_string(),
            plain_lyrics.to_string(),
        );
        id3v2.insert(Frame::UnsynchronizedText(uslt_frame));
    } else {
        let _ = id3v2.remove(&FrameId::new("USLT")?);
    }

    Ok(())
}

/// Insert SYLT (synchronized lyrics) frame into ID3v2 tag
fn insert_sylt_frame(id3v2: &mut Id3v2Tag, synced_lyrics: &str) -> Result<()> {
    if !synced_lyrics.is_empty() {
        let synced_lyrics_vec = synced_lyrics_to_sylt_vec(synced_lyrics)?;

        let sylt_frame = SynchronizedTextFrame::new(
            TextEncoding::UTF8,
            [b'X', b'X', b'X'],
            TimestampFormat::MS,
            SyncTextContentType::Lyrics,
            None,
            synced_lyrics_vec,
        );

        let sylt_frame_byte = sylt_frame.as_bytes(WriteOptions::default())?;
        let sylt_frame_id = FrameId::new("SYLT")?;
        id3v2.insert(Frame::Binary(BinaryFrame::new(
            sylt_frame_id,
            sylt_frame_byte,
        )));
    } else {
        let _ = id3v2.remove(&FrameId::new("SYLT")?);
    }

    Ok(())
}

/// Convert synced LRC lyrics to SYLT vector format
fn synced_lyrics_to_sylt_vec(synced_lyrics: &str) -> Result<Vec<(u32, String)>> {
    let parsed = parse_lrc(synced_lyrics);

    let converted_lyrics: Vec<(u32, String)> = parsed
        .timed_lines
        .iter()
        .map(|timed_line| (timed_line.timestamp_ms as u32, timed_line.text.clone()))
        .collect();

    Ok(converted_lyrics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_sidecar_path() {
        // Compare against paths built with `Path` rather than against string
        // literals: Windows joins with a backslash, so a hardcoded
        // "/music/.../song.txt" only holds on Unix.
        let track_path = Path::new("/music/artist/album/song.mp3");
        let parent = track_path.parent().unwrap();

        let txt_path = build_sidecar_path(track_path.to_str().unwrap(), "txt").unwrap();
        assert_eq!(txt_path, parent.join("song.txt"));

        let lrc_path = build_sidecar_path(track_path.to_str().unwrap(), "lrc").unwrap();
        assert_eq!(lrc_path, parent.join("song.lrc"));

        // The extension must replace the audio one, not append to the stem.
        assert!(!txt_path.to_string_lossy().contains("song.mp3"));
    }

    #[test]
    fn sidecar_path_handles_a_stem_containing_dots() {
        // "01. Song.mp3" must become "01. Song.lrc", not "01.lrc".
        let txt = build_sidecar_path("/music/01. Song.mp3", "txt").unwrap();
        assert_eq!(
            txt.file_name().unwrap().to_str().unwrap(),
            "01. Song.txt"
        );
    }

    fn library_track(path: &str) -> PersistentTrack {
        PersistentTrack {
            id: 7,
            file_path: path.to_owned(),
            file_name: "Song.mp3".to_owned(),
            title: "Song".to_owned(),
            album_name: "Album".to_owned(),
            album_artist_name: None,
            album_id: 1,
            artist_name: "Artist".to_owned(),
            artist_id: 1,
            image_path: None,
            track_number: Some(3),
            txt_lyrics: None,
            lrc_lyrics: None,
            lyricsfile: None,
            lyricsfile_id: None,
            duration: 200.0,
            instrumental: false,
        }
    }

    #[test]
    fn a_track_sidecar_follows_the_template() {
        let template = ExportTemplate::parse("{artist}/{title}.{ext}").unwrap();
        let path =
            build_sidecar_path_for_track(&library_track("/music/Artist/Album/Song.mp3"), "lrc", &template)
                .unwrap();
        assert_eq!(path, Path::new("/music/Artist/Album/Artist/Song.lrc"));
    }

    #[test]
    fn a_template_that_makes_a_subfolder_writes_into_it() {
        // The path resolver must not create anything: it is used by the plan,
        // which has to be side-effect free. The writer creates the directory.
        let dir = std::env::temp_dir().join("sa-template-subfolder");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let track_path = dir.join("Song.mp3");
        std::fs::write(&track_path, b"not really audio").unwrap();

        let template = ExportTemplate::parse("lyrics/{title}.{ext}").unwrap();
        let resolved =
            build_sidecar_path_for_track(&library_track(track_path.to_str().unwrap()), "lrc", &template)
                .unwrap();
        assert_eq!(resolved, dir.join("lyrics/Song.lrc"));
        assert!(!dir.join("lyrics").exists(), "resolving must not create anything");

        let parsed = ParsedLyricsfile {
            plain_lyrics: Some("words".to_owned()),
            synced_lyrics: Some("[00:01.00]words".to_owned()),
            is_instrumental: false,
        };
        let results = export_track_with_template(
            &library_track(track_path.to_str().unwrap()),
            &parsed,
            &[ExportFormat::Lrc],
            &template,
        );
        assert!(matches!(results[0].status, ExportStatus::Success), "{results:?}");
        assert_eq!(std::fs::read_to_string(&resolved).unwrap(), "[00:01.00]words");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_template_cannot_write_outside_the_tracks_directory() {
        // The path is joined onto the audio file's parent, so a `..` in the
        // template must be refused rather than resolving somewhere else.
        let template = ExportTemplate::parse("../{title}").unwrap();
        let err = build_sidecar_path_for_track(
            &library_track("/music/Artist/Album/Song.mp3"),
            "lrc",
            &template,
        )
        .unwrap_err();
        assert!(matches!(err, ExportError::PathBuildError(_)));
    }

    #[test]
    fn the_plan_names_the_file_the_export_would_write() {
        let template = ExportTemplate::parse("{artist}/{title}.{ext}").unwrap();
        let parsed = ParsedLyricsfile {
            plain_lyrics: Some("words".to_owned()),
            synced_lyrics: Some("[00:01.00]words".to_owned()),
            is_instrumental: false,
        };
        let plans = plan_export(
            &library_track("/music/Artist/Album/Song.mp3"),
            &parsed,
            &[ExportFormat::Lrc],
            &template,
        );
        assert_eq!(plans.len(), 1);
        assert_eq!(
            plans[0].path.as_deref(),
            Some(Path::new("/music/Artist/Album/Artist/Song.lrc"))
        );
        // Nothing was written by planning.
        assert!(!plans[0].path.as_deref().unwrap().exists());
    }

    #[test]
    fn the_plan_reports_a_skip_instead_of_a_path() {
        let parsed = ParsedLyricsfile {
            plain_lyrics: None,
            synced_lyrics: None,
            is_instrumental: true,
        };
        let plans = plan_export(
            &library_track("/music/Artist/Album/Song.mp3"),
            &parsed,
            &[ExportFormat::Lrc],
            &ExportTemplate::default(),
        );
        assert_eq!(plans.len(), 1);
        // An instrumental track still writes an LRC: the marker is the content,
        // which is what tells a player there is nothing to sing. Nothing was
        // written by planning.
        assert_eq!(plans[0].path.as_deref(), Some(Path::new("/music/Artist/Album/Song.lrc")));
        assert!(!plans[0].path.as_deref().unwrap().exists());

        // A word-level format has no such marker, so it skips.
        let plans = plan_export(
            &library_track("/music/Artist/Album/Song.mp3"),
            &parsed,
            &[ExportFormat::Ttml],
            &ExportTemplate::default(),
        );
        assert!(plans[0].path.is_none());
        assert_eq!(plans[0].action, "instrumental track");
    }

    #[test]
    fn test_generate_txt_content() {
        let parsed = ParsedLyricsfile {
            plain_lyrics: Some("Line 1\nLine 2".to_string()),
            synced_lyrics: None,
            is_instrumental: false,
        };

        let content = generate_txt_content(&parsed);
        assert_eq!(content, Some("Line 1\nLine 2".to_string()));

        // Instrumental should return None
        let instrumental = ParsedLyricsfile {
            plain_lyrics: None,
            synced_lyrics: None,
            is_instrumental: true,
        };
        assert_eq!(generate_txt_content(&instrumental), None);
    }

    #[test]
    fn test_generate_lrc_content() {
        let parsed = ParsedLyricsfile {
            plain_lyrics: None,
            synced_lyrics: Some("[00:12.00]Line 1".to_string()),
            is_instrumental: false,
        };

        let content = generate_lrc_content(&parsed);
        assert_eq!(content, Some("[00:12.00]Line 1".to_string()));

        // Instrumental should return special marker
        let instrumental = ParsedLyricsfile {
            plain_lyrics: None,
            synced_lyrics: None,
            is_instrumental: true,
        };
        assert_eq!(
            generate_lrc_content(&instrumental),
            Some("[au: instrumental]".to_string())
        );
    }
}
