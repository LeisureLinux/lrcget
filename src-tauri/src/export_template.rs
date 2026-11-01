//! Filename templates for lyrics sidecar exports.
//!
//! The output name of an exported sidecar used to be hardcoded to the audio
//! file's stem: `01. Song.mp3` always produced `01. Song.lrc`. That is the right
//! default, but it is the only thing the app could do — a user who wants
//! `{artist} - {title}.lrc`, or lyrics collected into a `lyrics/` folder, had to
//! reorganise by hand after every export.
//!
//! A template is a relative path with `{variable}` placeholders. It is resolved
//! against the track's own directory, so the default template
//! `{filename}.{ext}` reproduces the previous behaviour exactly.
//!
//! ```text
//! {artist} - {title}.lrc
//! {artist}/{album}/{track_number_padded} - {title}.lrc
//! lyrics/{filename}.{ext}
//! ```
//!
//! ## The `_sanitized` suffix
//!
//! A template may build directories, and a directory that has been sanitised
//! stops being a directory. So sanitising is opt-in per variable rather than
//! always-on: `{title_sanitized}` replaces the characters a filesystem refuses
//! with `_`, and plain `{title}` keeps them. Anything the user writes literally
//! in the template is never touched, for the same reason.
//!
//! ## What is deliberately not here
//!
//! There is no expression language. The variables are a fixed, small set
//! derived from metadata the app already holds, and an unknown name is an error
//! rather than an empty string — a typo that silently produced `{}.lrc` would be
//! worse than one that is reported. The template is validated when it is saved
//! (see `db::set_export_preferences`) so a bad one is caught at the point of
//! entry, not on the fiftieth track of a batch.

use crate::persistent_entities::PersistentTrack;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

/// The template that reproduces the historical `{stem}.{ext}` behaviour.
pub const DEFAULT_TEMPLATE: &str = "{filename}.{ext}";

/// Variables a template may reference.
///
/// `ext` is supplied by the caller — the format being exported decides it — and
/// every other one comes from the track's metadata.
const KNOWN_VARIABLES: &[&str] = &[
    "title",
    "artist",
    "album",
    "album_artist",
    "track_number",
    "track_number_padded",
    "filename",
    "ext",
];

/// Characters a filename component may not contain on the platforms this app
/// ships to.
///
/// `/` is here as well: inside a *value* it is data (`AC/DC` is a band, not a
/// directory), and a template that wants a directory writes the separator
/// literally. Literal text in a template is never passed through `sanitize`, so
/// this cannot eat a separator the user typed.
const ILLEGAL_CHARACTERS: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|', '\0'];

#[derive(Debug, Error)]
pub enum TemplateError {
    #[error("unknown variable {{{0}}}")]
    UnknownVariable(String),

    #[error("unclosed {{ in template")]
    UnclosedBrace,

    #[error("template must be a relative path")]
    AbsolutePath,

    #[error("template must not contain '..'")]
    ParentComponent,

    #[error("template resolved to an empty path")]
    EmptyPath,

    #[error("template resolved to an empty file name")]
    EmptyFileName,
}

/// A parsed template.
///
/// Parsing is separate from resolving so that an invalid template is rejected
/// once, when it is saved, instead of on every track of every export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportTemplate {
    raw: String,
    variables: Vec<String>,
}

impl ExportTemplate {
    /// Parse and validate a template.
    pub fn parse(raw: &str) -> Result<Self, TemplateError> {
        let variables = collect_variables(raw)?;
        Ok(Self {
            raw: raw.to_owned(),
            variables,
        })
    }

    /// The template as written.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// The variable names the template references, in order of appearance.
    ///
    /// Used to tell the user which names a template needs, and by the tests.
    pub fn variables(&self) -> &[String] {
        &self.variables
    }

    /// Expand the template into a path.
    ///
    /// `ext` is the extension of the format being exported, without the dot.
    pub fn resolve(&self, vars: &TemplateVars, ext: &str) -> Result<PathBuf, TemplateError> {
        let expanded = expand(&self.raw, vars, ext)?;
        path_from_expanded(&expanded)
    }
}

impl Default for ExportTemplate {
    fn default() -> Self {
        Self::parse(DEFAULT_TEMPLATE).expect("the default template is valid by construction")
    }
}

/// The values a template can be expanded with.
#[derive(Debug, Clone, Default)]
pub struct TemplateVars {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub track_number: Option<i64>,
    /// The audio file's stem, e.g. `01. Song` for `/music/01. Song.mp3`.
    pub filename: String,
}

impl TemplateVars {
    /// Build the variables from a library track.
    ///
    /// `album_artist` falls back to the artist rather than to an empty string:
    /// for a compilation the two differ and the album artist is the one that
    /// names the folder, so preferring it would move the file somewhere the user
    /// did not ask for. Empty stays empty — a template that references a missing
    /// value resolves that variable to nothing, which the file-name check below
    /// turns into an error rather than into a file called `.lrc`.
    pub fn from_track(track: &PersistentTrack) -> Self {
        Self {
            title: track.title.clone(),
            artist: track.artist_name.clone(),
            album: track.album_name.clone(),
            album_artist: track.album_artist_name.clone().unwrap_or_default(),
            track_number: track.track_number,
            filename: Path::new(&track.file_path)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or(&track.file_name)
                .to_owned(),
        }
    }

    fn lookup(&self, name: &str) -> Option<String> {
        match name {
            "title" => Some(self.title.clone()),
            "artist" => Some(self.artist.clone()),
            "album" => Some(self.album.clone()),
            "album_artist" => Some(self.album_artist.clone()),
            "track_number" => self.track_number.map(|n| n.to_string()),
            "track_number_padded" => self
                .track_number
                .map(|n| format!("{:02}", n.clamp(0, 99))),
            // `filename` is a variable like any other, so it is looked up here
            // rather than special-cased in `expand`.
            "filename" => Some(self.filename.clone()),
            // `ext` is supplied by the caller and never comes from metadata.
            _ => None,
        }
    }
}

/// Scan a template for `{name}` occurrences and validate every name.
fn collect_variables(template: &str) -> Result<Vec<String>, TemplateError> {
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let close = after.find('}').ok_or(TemplateError::UnclosedBrace)?;
        let name = after[..close].trim();
        validate_name(name)?;
        if !names.iter().any(|seen| seen == name) {
            names.push(name.to_owned());
        }
        rest = &after[close + 1..];
    }
    Ok(names)
}

/// A name is either a known variable, or one with the `_sanitized` suffix.
fn validate_name(name: &str) -> Result<(), TemplateError> {
    if name.is_empty() {
        return Err(TemplateError::UnknownVariable(String::new()));
    }
    let base = name.strip_suffix("_sanitized").unwrap_or(name);
    if KNOWN_VARIABLES.contains(&base) {
        Ok(())
    } else {
        Err(TemplateError::UnknownVariable(name.to_owned()))
    }
}

/// Substitute every `{name}` with its value.
fn expand(template: &str, vars: &TemplateVars, ext: &str) -> Result<String, TemplateError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after.find('}').ok_or(TemplateError::UnclosedBrace)?;
        let name = after[..close].trim();
        validate_name(name)?;

        // `title_sanitized` is not a variable of its own: it is `title` with the
        // sanitising switched on. Strip the suffix before the lookup, otherwise
        // the lookup misses and the placeholder expands to nothing.
        let base = name.strip_suffix("_sanitized").unwrap_or(name);

        let value = if name == "ext" {
            ext.to_owned()
        } else {
            vars.lookup(base).unwrap_or_default()
        };
        out.push_str(&maybe_sanitize(name, &value));
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Sanitize only when the name asked for it.
///
/// The suffix has already been stripped for the lookup by the time this runs,
/// so the decision is made on the name as the user wrote it.
fn maybe_sanitize(name: &str, value: &str) -> String {
    match name.strip_suffix("_sanitized") {
        Some(_) => sanitize(value),
        None => value.to_owned(),
    }
}

/// Replace the characters a filesystem refuses with `_`.
///
/// Applied to a whole value, `/` included: inside metadata a slash is data
/// (`AC/DC` is a band, not a directory). A template that wants a directory
/// writes the separator literally, and literal text is never passed through
/// here. Trailing dots and spaces are trimmed because Windows refuses them at
/// the end of a name.
pub fn sanitize(value: &str) -> String {
    let mut out: String = value
        .chars()
        .map(|c| {
            if ILLEGAL_CHARACTERS.contains(&c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Turn the expanded string into a path, refusing anything that escapes the
/// track's directory.
fn path_from_expanded(expanded: &str) -> Result<PathBuf, TemplateError> {
    // Both separators are accepted so that a template written on Windows works
    // on Linux and vice versa. A backslash is not legal in a filename on the
    // platforms that matter here, so nothing is lost by treating it as one.
    let normalized = expanded.replace('\\', "/");
    if normalized.trim().is_empty() {
        return Err(TemplateError::EmptyPath);
    }

    // Checked on the string, not on the `PathBuf`: splitting on `/` first would
    // silently drop the leading empty segment and turn an absolute template
    // into a relative one.
    if normalized.starts_with('/') || is_windows_absolute(&normalized) {
        return Err(TemplateError::AbsolutePath);
    }

    let mut path = PathBuf::new();
    for segment in normalized.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return Err(TemplateError::ParentComponent);
        }
        path.push(segment);
    }

    if path.as_os_str().is_empty() {
        return Err(TemplateError::EmptyPath);
    }
    if path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::RootDir | Component::Prefix(_)))
    {
        return Err(TemplateError::AbsolutePath);
    }

    // A name that starts with `.` and carries nothing else is what an empty
    // variable produces (`.lrc`), which no player will read. Reporting it is
    // better than writing a file nobody can open.
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if name.starts_with('.') {
        return Err(TemplateError::EmptyFileName);
    }

    Ok(path)
}

/// `C:` or `C:/`, which `Path::is_absolute` does not recognise on Linux.
fn is_windows_absolute(normalized: &str) -> bool {
    let mut chars = normalized.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && matches!(chars.next(), Some(':'))
        && matches!(chars.next(), None | Some('/'))
}

/// Append the extension when the template did not produce one.
///
/// `{title}` alone would otherwise write a file with no extension, which no
/// player will read. The default template writes `{ext}` explicitly; this is the
/// fallback for the ones that do not.
pub fn ensure_extension(path: PathBuf, ext: &str) -> PathBuf {
    if path.extension().is_some() {
        return path;
    }
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(format!(".{ext}"));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> PersistentTrack {
        PersistentTrack {
            id: 1,
            file_path: "/music/Artist/Album/01. Song.mp3".to_owned(),
            file_name: "01. Song.mp3".to_owned(),
            title: "Song".to_owned(),
            album_name: "Album".to_owned(),
            album_artist_name: None,
            album_id: 1,
            artist_name: "Artist".to_owned(),
            artist_id: 1,
            image_path: None,
            track_number: Some(1),
            txt_lyrics: None,
            lrc_lyrics: None,
            lyricsfile: None,
            lyricsfile_id: None,
            duration: 245.0,
            instrumental: false,
        }
    }

    #[test]
    fn default_template_reproduces_the_old_sidecar_name() {
        // The whole point of the default: it must be indistinguishable from the
        // hardcoded `{stem}.{ext}` behaviour it replaced.
        let template = ExportTemplate::default();
        let vars = TemplateVars::from_track(&track());
        let resolved = template.resolve(&vars, "lrc").unwrap();
        assert_eq!(resolved, PathBuf::from("01. Song.lrc"));
    }

    #[test]
    fn resolves_every_variable() {
        let template = ExportTemplate::parse(
            "{artist}/{album}/{track_number_padded} - {title}.{ext}",
        )
        .unwrap();
        let resolved = template
            .resolve(&TemplateVars::from_track(&track()), "lrc")
            .unwrap();
        assert_eq!(resolved, PathBuf::from("Artist/Album/01 - Song.lrc"));
    }

    #[test]
    fn track_number_pads_to_two_digits() {
        let template = ExportTemplate::parse("{track_number_padded}").unwrap();
        let vars = TemplateVars::from_track(&track());
        assert_eq!(
            ensure_extension(template.resolve(&vars, "lrc").unwrap(), "lrc"),
            PathBuf::from("01.lrc")
        );

        let mut double = track();
        double.track_number = Some(12);
        assert_eq!(
            ensure_extension(
                template.resolve(&TemplateVars::from_track(&double), "lrc").unwrap(),
                "lrc"
            ),
            PathBuf::from("12.lrc")
        );
    }

    #[test]
    fn extension_is_appended_when_the_template_omits_it() {
        let template = ExportTemplate::parse("{title}").unwrap();
        let resolved = ensure_extension(
            template
                .resolve(&TemplateVars::from_track(&track()), "lrc")
                .unwrap(),
            "lrc",
        );
        assert_eq!(resolved, PathBuf::from("Song.lrc"));
    }

    #[test]
    fn explicit_extension_is_not_doubled() {
        let template = ExportTemplate::parse("{title}.lrc").unwrap();
        let resolved = ensure_extension(
            template
                .resolve(&TemplateVars::from_track(&track()), "ttml")
                .unwrap(),
            "ttml",
        );
        assert_eq!(resolved, PathBuf::from("Song.lrc"));
    }

    #[test]
    fn sanitized_suffix_replaces_illegal_characters() {
        let template = ExportTemplate::parse("{title_sanitized}").unwrap();
        let mut t = track();
        t.title = "AC/DC: What's \"Next\"?".to_owned();
        let resolved = template
            .resolve(&TemplateVars::from_track(&t), "lrc")
            .unwrap();
        // `/` is replaced too, so a title with a slash in it stays one file
        // name instead of becoming a directory the user never asked for.
        assert_eq!(resolved, PathBuf::from("AC_DC_ What's _Next__"));
    }

    #[test]
    fn unsanitized_variable_keeps_the_original_text() {
        // The template is what creates the directory, so sanitising it would
        // stop it being one.
        let template = ExportTemplate::parse("lyrics/{title}").unwrap();
        let mut t = track();
        t.title = "Song".to_owned();
        let resolved = template.resolve(&TemplateVars::from_track(&t), "lrc").unwrap();
        assert_eq!(resolved, PathBuf::from("lyrics/Song"));
    }

    #[test]
    fn unknown_variable_is_an_error_not_an_empty_string() {
        let err = ExportTemplate::parse("{titel}.lrc").unwrap_err();
        assert!(matches!(err, TemplateError::UnknownVariable(name) if name == "titel"));
    }

    #[test]
    fn unclosed_brace_is_an_error() {
        assert!(matches!(
            ExportTemplate::parse("{title.lrc").unwrap_err(),
            TemplateError::UnclosedBrace
        ));
    }

    #[test]
    fn parent_directory_escape_is_refused() {
        let template = ExportTemplate::parse("../../{title}").unwrap();
        let resolve_err = template
            .resolve(&TemplateVars::from_track(&track()), "lrc")
            .unwrap_err();
        assert!(matches!(resolve_err, TemplateError::ParentComponent));
    }

    #[test]
    fn absolute_template_is_refused() {
        // POSIX form. The `C:` form is Windows-only, and refusing it here too
        // keeps a template written on one platform from writing outside the
        // track directory on the other.
        let template = ExportTemplate::parse("/etc/{title}").unwrap();
        let err = template
            .resolve(&TemplateVars::from_track(&track()), "lrc")
            .unwrap_err();
        assert!(matches!(err, TemplateError::AbsolutePath));

        let template = ExportTemplate::parse("C:/etc/{title}").unwrap();
        let err = template
            .resolve(&TemplateVars::from_track(&track()), "lrc")
            .unwrap_err();
        assert!(matches!(err, TemplateError::AbsolutePath));
    }

    #[test]
    fn windows_separator_is_treated_as_a_directory() {
        let template = ExportTemplate::parse("{artist}\\{title}").unwrap();
        let resolved = template
            .resolve(&TemplateVars::from_track(&track()), "lrc")
            .unwrap();
        assert_eq!(resolved, PathBuf::from("Artist/Song"));
    }

    #[test]
    fn empty_variable_leaves_no_file_name() {
        // A track with no album and a template that is only the album resolves
        // to nothing, which must be reported rather than written.
        let template = ExportTemplate::parse("{album}.lrc").unwrap();
        let mut t = track();
        t.album_name = String::new();
        let err = template
            .resolve(&TemplateVars::from_track(&t), "lrc")
            .unwrap_err();
        assert!(matches!(err, TemplateError::EmptyFileName));
    }

    #[test]
    fn repeated_variables_are_listed_once() {
        let template = ExportTemplate::parse("{title}/{title}/{artist}").unwrap();
        assert_eq!(template.variables(), ["title", "artist"]);
    }

    #[test]
    fn album_artist_falls_back_to_nothing_when_absent() {
        let template = ExportTemplate::parse("{album_artist}").unwrap();
        let vars = TemplateVars::from_track(&track());
        assert_eq!(vars.album_artist, "");
        assert!(template.resolve(&vars, "lrc").is_err());
    }

    #[test]
    fn surrounding_whitespace_in_a_placeholder_is_ignored() {
        let template = ExportTemplate::parse("{ title }.lrc").unwrap();
        let resolved = ensure_extension(
            template
                .resolve(&TemplateVars::from_track(&track()), "lrc")
                .unwrap(),
            "lrc",
        );
        assert_eq!(resolved, PathBuf::from("Song.lrc"));
    }

    #[test]
    fn literal_text_is_preserved() {
        let template = ExportTemplate::parse("{artist} - {title}.lrc").unwrap();
        let resolved = template
            .resolve(&TemplateVars::from_track(&track()), "lrc")
            .unwrap();
        assert_eq!(resolved, PathBuf::from("Artist - Song.lrc"));
    }
}
