#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

#[cfg(feature = "gui")]
pub mod db;
pub mod export;
pub mod export_template;
#[cfg(feature = "gui")]
pub mod library;
pub mod lrclib;
pub mod netease;
pub mod amll;
pub mod lyricsfile;
pub mod parser;
pub mod persistent_entities;
#[cfg(feature = "gui")]
pub mod playlist;
pub mod repair;
#[cfg(feature = "gui")]
pub mod repair_gui;
#[cfg(feature = "gui")]
pub mod player;
pub mod scanner;
pub mod script_convert;
#[cfg(feature = "gui")]
pub mod cover;
#[cfg(feature = "gui")]
pub mod cover_cache;
pub mod cookies;
pub mod http;
pub mod cli;
pub mod cli_support;
#[cfg(feature = "gui")]
mod gui;
#[cfg(feature = "gui")]
pub mod state;
pub mod utils;
pub mod word_segmentation;
pub mod word_sync;
pub mod youtube;
#[cfg(feature = "gui")]
pub mod youtube_gui;
pub mod ytmusic;
pub mod ytdlp;

/// Entry point for the shipped application and the headless CLI.
///
/// `sing-along` with no arguments opens the window; a subcommand runs without
/// one. Under the `cli` feature the GUI half is not compiled at all, so the
/// resulting binary links no GTK, WebKit or audio stack and can be built
/// statically.
fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Three build configurations, all from this one file:
    //
    //   gui + cli  the .deb/.rpm/.AppImage player: a subcommand runs headless,
    //              no subcommand opens the window
    //   gui only   a player with no command line
    //   cli only   the standalone static binary: subcommands only
    //
    // Keeping the combined build is what spares users from downloading twice.
    #[cfg(all(feature = "gui", feature = "cli"))]
    {
        if let Some(code) = cli::run_blocking(&args) {
            std::process::exit(code);
        }
        gui::run();
    }

    #[cfg(all(feature = "cli", not(feature = "gui")))]
    {
        if let Some(code) = cli::run_blocking(&args) {
            std::process::exit(code);
        }
        // No subcommand and no window to open: print the usage rather than
        // explaining that a window is missing, which is noise to someone who
        // downloaded a command-line tool on purpose.
        cli::print_usage();
        std::process::exit(2);
    }

    #[cfg(all(feature = "gui", not(feature = "cli")))]
    {
        let _ = &args;
        gui::run();
    }

    #[cfg(not(any(feature = "gui", feature = "cli")))]
    {
        compile_error!("build with either the `gui` or `cli` feature (or both)");
    }
}

