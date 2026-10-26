mod api;
mod app;
mod config;
mod crypto;
mod discord_rpc;
mod mpris;
mod player;
mod state;
mod theme;
mod ui;
mod update;

use std::{
    fs,
    io,
    io::Write,
    path::Path,
    path::PathBuf,
    process::{Child, ChildStdin, Command as ProcessCommand, Stdio},
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use anyhow::{anyhow, Context, Result as AnyResult};
use base64::Engine;
use app::{ActivePanel, App, ArtistRow, Command, Hit, LibraryView, NowPlaying, RepeatMode, SearchCategory, UiEvent, ViewState};
use std::collections::VecDeque;
use config::{AudioQuality, Config, Theme};
use api::{AlbumPage, ArtistPage, ArtistRef, HistoryEntry, LibraryAlbum};
use std::collections::HashMap;
use std::sync::Arc;
use state::PlaybackState;
use discord_rpc::{DiscordPresence, DiscordRpcHandle};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    Terminal,
};
use rand::seq::SliceRandom;
use rand::thread_rng;
use serde_json::json;
use mpris_server::{LoopStatus, PlaybackStatus, Server};
use tokio::sync::{mpsc, oneshot};

#[cfg(unix)]
struct StderrSilenceGuard {
    original_stderr_fd: i32,
}

#[cfg(unix)]
impl StderrSilenceGuard {
    fn activate() -> AnyResult<Self> {
        let devnull = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .context("failed to open /dev/null")?;

        let original_stderr_fd = unsafe { libc::dup(libc::STDERR_FILENO) };
        if original_stderr_fd < 0 {
            return Err(anyhow!("failed to dup stderr"));
        }

        let rc = unsafe { libc::dup2(devnull.as_raw_fd(), libc::STDERR_FILENO) };
        if rc < 0 {
            unsafe {
                libc::close(original_stderr_fd);
            }
            return Err(anyhow!("failed to redirect stderr to /dev/null"));
        }

        Ok(Self { original_stderr_fd })
    }
}

#[cfg(unix)]
impl Drop for StderrSilenceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::dup2(self.original_stderr_fd, libc::STDERR_FILENO);
            let _ = libc::close(self.original_stderr_fd);
        }
    }
}

#[cfg(not(unix))]
struct StderrSilenceGuard;

#[cfg(not(unix))]
impl StderrSilenceGuard {
    fn activate() -> AnyResult<Self> {
        Ok(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageProtocol {
    None,
    Kitty,
    Ueberzugpp,
}

/// Ask the terminal whether it speaks the kitty graphics protocol: send a
/// tiny query image, then DA1 so that every terminal answers something and
/// the wait ends quickly. Must run in raw mode, before the event loop reads.
#[cfg(unix)]
fn probe_kitty_graphics() -> bool {
    let mut out = io::stdout();
    if write!(out, "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c").is_err() || out.flush().is_err() {
        return false;
    }
    let mut buf: Vec<u8> = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(400);
    let mut chunk = [0u8; 256];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
        let ready = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis() as i32) };
        if ready <= 0 {
            break;
        }
        let n = unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
        // The DA1 reply ("ESC [ ? ... c") always comes last.
        if let Some(pos) = buf.windows(3).position(|w| w == b"\x1b[?") {
            if buf[pos..].contains(&b'c') {
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).contains("_Gi=31;OK")
}

#[cfg(not(unix))]
fn probe_kitty_graphics() -> bool {
    false
}

fn detect_image_protocol() -> ImageProtocol {
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default().to_lowercase();
    let kitty_window = std::env::var("KITTY_WINDOW_ID").ok();

    if kitty_window.is_some()
        || term.contains("xterm-kitty")
        || term_program.contains("wezterm")
        || term_program.contains("ghostty")
    {
        ImageProtocol::Kitty
    } else if ProcessCommand::new("ueberzugpp")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
    {
        ImageProtocol::Ueberzugpp
    } else {
        ImageProtocol::None
    }
}

struct UeberzugppLayer {
    child: Child,
    stdin: ChildStdin,
}

const IMAGE_SLOTS: usize = 3;

fn slot_identifier(slot: usize) -> String {
    format!("deezer-tui-{slot}")
}

impl UeberzugppLayer {
    fn start() -> Option<Self> {
        let mut child = ProcessCommand::new("ueberzugpp")
            .arg("layer")
            .arg("--silent")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        Some(Self { child, stdin })
    }

    fn add(&mut self, slot: usize, path: &Path, area: ratatui::layout::Rect) -> io::Result<()> {
        if area.width == 0 || area.height == 0 {
            return Ok(());
        }
        let payload = json!({
            "action": "add",
            "identifier": slot_identifier(slot),
            "path": path.display().to_string(),
            "x": area.x,
            "y": area.y,
            "width": area.width,
            "height": area.height,
            "scaler": "fit_contain"
        });
        writeln!(self.stdin, "{}", payload)?;
        self.stdin.flush()
    }

    fn remove(&mut self, slot: usize) -> io::Result<()> {
        let payload = json!({
            "action": "remove",
            "identifier": slot_identifier(slot)
        });
        writeln!(self.stdin, "{}", payload)?;
        self.stdin.flush()
    }

    fn shutdown(mut self) {
        for slot in 0..IMAGE_SLOTS {
            let _ = self.remove(slot);
        }
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_cover_png_temp(png: &[u8], slot: usize) -> io::Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("deezer-tui-image-{slot}.png"));
    fs::write(&path, png)?;
    Ok(path)
}

fn kitty_delete_image(image_id: u32) -> io::Result<()> {
    let mut out = io::stdout();
    write!(out, "\x1b_Ga=d,d=I,i={}\x1b\\", image_id)?;
    out.flush()
}

fn kitty_draw_png_at_rect(png: &[u8], image_id: u32, area: ratatui::layout::Rect) -> io::Result<()> {
    if area.width == 0 || area.height == 0 {
        return Ok(());
    }

    let payload = base64::engine::general_purpose::STANDARD.encode(png);
    let mut out = io::stdout();

    // Save/restore cursor to avoid disturbing TUI input focus.
    write!(out, "\x1b[s\x1b[{};{}H", area.y + 1, area.x + 1)?;

    let mut idx = 0usize;
    let chunk = 4096usize;
    let mut first = true;
    while idx < payload.len() {
        let end = (idx + chunk).min(payload.len());
        let part = &payload[idx..end];
        let more = if end < payload.len() { 1 } else { 0 };
        if first {
            write!(
                out,
                "\x1b_Ga=T,f=100,i={},c={},r={},m={};{}\x1b\\",
                image_id,
                area.width,
                area.height,
                more,
                part
            )?;
            first = false;
        } else {
            write!(out, "\x1b_Gm={};{}\x1b\\", more, part)?;
        }
        idx = end;
    }

    write!(out, "\x1b[u")?;
    out.flush()
}

struct MprisHandle {
    server: Server<mpris::MprisPlayer>,
    event_rx: crossbeam_channel::Receiver<mpris::MprisEvent>,
    last_update: Instant,
}

async fn create_mpris_handle() -> Option<MprisHandle> {
    let (server, event_rx) = mpris::create_server().await?;
    Some(MprisHandle { server, event_rx, last_update: Instant::now() })
}

#[derive(Debug, Clone, Copy)]
enum PlayerControl {
    Pause,
    Resume,
    Stop,
    FadeOutStop(u64),
    SetVolume(f32),
}

/// Show a view taken from the back / forward history without pushing it again.
fn restore_view(app: &mut App, view: ViewState) {
    app.active_panel = ActivePanel::Main;
    app.navigating_back = true;
    match view {
        ViewState::Library(view) => {
            app.navigating_back = false;
            app.show_library(view);
        }
        ViewState::Settings => {
            app.navigating_back = false;
            app.leave_special_views();
            app.showing_search_results = false;
            app.viewing_settings = true;
        }
        ViewState::Search => {
            app.navigating_back = false;
            app.leave_special_views();
            app.viewing_settings = false;
            app.showing_search_results = true;
            app.main_state.select(Some(0));
        }
        ViewState::Artist(id) => {
            let _ = app.command_sender.send(Command::LoadArtist(id));
        }
        ViewState::Album(id) => {
            let _ = app.command_sender.send(Command::LoadAlbum(id));
        }
        ViewState::Tracks(source) => {
            let cmd = match source.as_deref() {
                Some("__home__") => Command::LoadHome,
                Some("__explore__") => Command::LoadExplore,
                Some(id) if !id.is_empty() => {
                    app.current_playlist_id = Some(id.to_owned());
                    Command::LoadPlaylist(id.to_owned())
                }
                _ => Command::LoadFavorites,
            };
            if matches!(cmd, Command::LoadHome | Command::LoadExplore | Command::LoadFavorites) {
                app.current_playlist_id = match cmd {
                    Command::LoadHome => Some("__home__".into()),
                    Command::LoadExplore => Some("__explore__".into()),
                    _ => None,
                };
            }
            let _ = app.command_sender.send(cmd);
        }
    }
}

fn next_quality(current: AudioQuality, forward: bool) -> AudioQuality {
    match (current, forward) {
        (AudioQuality::Kbps128, true) => AudioQuality::Kbps320,
        (AudioQuality::Kbps320, true) => AudioQuality::Flac,
        (AudioQuality::Flac, true) => AudioQuality::Kbps128,
        (AudioQuality::Kbps128, false) => AudioQuality::Flac,
        (AudioQuality::Kbps320, false) => AudioQuality::Kbps128,
        (AudioQuality::Flac, false) => AudioQuality::Kbps320,
    }
}

fn sync_discord_presence(discord_rpc: &DiscordRpcHandle, app: &App) {
    if !app.discord_rpc_enabled {
        discord_rpc.clear();
        return;
    }

    if let Some(now) = app.now_playing.as_ref() {
        discord_rpc.update(DiscordPresence {
            title: now.title.clone(),
            artist: now.artist.clone(),
            track_id: now.id.clone(),
            album_art_url: now.album_art_url.clone(),
            current_ms: now.current_ms,
            total_ms: now.total_ms,
            is_playing: app.is_playing,
        });
    } else {
        discord_rpc.clear();
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    ensure_verified_arl().await?;

    // Suppress noisy native audio warnings (e.g. ALSA underruns) while TUI is active.
    let _stderr_silence = StderrSilenceGuard::activate()?;

    enable_raw_mode()?;
    // Real images when the terminal supports them, coloured text blocks otherwise.
    let kitty_graphics = probe_kitty_graphics();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (command_sender, command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<UiEvent>();

    let config = load_config();
    let initial_quality = config.default_quality;
    let initial_crossfade_enabled = config.crossfade_enabled;
    let initial_crossfade_duration_ms = config.crossfade_duration_ms;
    let resume_on_start = config.resume_on_start;
    let mut app = App::new(config, command_sender);
    if resume_on_start {
        app.saved_state = state::load_state();
    } else {
        app.resume_done = true;
    }
    let discord_rpc = DiscordRpcHandle::new();

    // Spawn task to fetch user playlists on startup
    let playlists_event_tx = event_tx.clone();
    tokio::spawn(async move {
        use api::DeezerClient;
        
        let arl = match load_saved_arl() {
            Ok(arl) => arl,
            Err(err) => {
                let _ = playlists_event_tx.send(UiEvent::Error(format!("ARL Error: {}", err)));
                return;
            }
        };

        let _ = playlists_event_tx.send(UiEvent::Error("Status: Initializing...".into()));

        match DeezerClient::new(arl) {
            Ok(mut client) => {
                match client.fetch_api_token().await {
                    Ok(_) => {
                        let _ = playlists_event_tx.send(UiEvent::Error("Status: Auth success, fetching playlists...".into()));
                        
                        if let Some(user_id) = client.user_id() {
                            match client.fetch_user_playlists(user_id).await {
                                Ok(playlists) => {
                                    let own = playlists
                                        .iter()
                                        .filter(|(_, _, own)| *own)
                                        .map(|(id, title, _)| (id.clone(), title.clone()))
                                        .collect();
                                    let all = playlists.into_iter().map(|(id, title, _)| (id, title)).collect();
                                    let _ = playlists_event_tx.send(UiEvent::PlaylistsLoaded(all));
                                    let _ = playlists_event_tx.send(UiEvent::OwnPlaylistsLoaded(own));
                                }
                                Err(err) => {
                                    let _ = playlists_event_tx.send(UiEvent::Error(format!("API Error fetching playlists: {}", err)));
                                }
                            }
                            if resume_on_start {
                                // Deezer history is used to decide whether something newer was
                                // played on another device since the last local snapshot.
                                let history = client
                                    .fetch_listening_history(user_id, 10)
                                    .await
                                    .unwrap_or_default();
                                let _ = playlists_event_tx.send(UiEvent::HistoryLoaded(history));
                            }
                        } else {
                            let _ = playlists_event_tx.send(UiEvent::Error("API Error: No user ID".into()));
                        }
                    }
                    Err(err) => {
                        let _ = playlists_event_tx.send(UiEvent::Error(format!("Auth Error: {}", err)));
                    }
                }
            }
            Err(err) => {
                let _ = playlists_event_tx.send(UiEvent::Error(format!("Client Error: {}", err)));
            }
        }
    });

    let update_tx = event_tx.clone();
    spawn_update_check(update_tx.clone(), false);
    let mut audio_task = Some(tokio::spawn(audio_worker_loop(command_rx, event_tx)));
    // Sync worker's current_quality with the loaded config
    let _ = app.command_sender.send(Command::SetQuality(initial_quality));
    let _ = app.command_sender.send(Command::SetCrossfade {
        enabled: initial_crossfade_enabled,
        duration_ms: initial_crossfade_duration_ms,
    });
    let mut mpris_handle = create_mpris_handle().await;
    let run_result = run_tui_loop(
        &mut terminal,
        &mut app,
        &mut event_rx,
        update_tx,
        &mut audio_task,
        &mut mpris_handle,
        &discord_rpc,
        kitty_graphics,
    ).await;

    drop(app);
    let restore_result = restore_terminal(&mut terminal);

    let join_result = if let Some(handle) = audio_task.take() {
        Some(handle.await)
    } else {
        None
    };

    discord_rpc.shutdown();

    if let Err(err) = restore_result {
        return Err(err.into());
    }

    if let Some(result) = join_result {
        if let Err(join_err) = result {
            return Err(anyhow!("background audio task failed: {join_err}").into());
        }
    }

    if let Err(err) = run_result {
        return Err(err.into());
    }

    Ok(())
}

async fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    event_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    update_tx: mpsc::UnboundedSender<UiEvent>,
    audio_task: &mut Option<tokio::task::JoinHandle<()>>,
    mpris: &mut Option<MprisHandle>,
    discord_rpc: &DiscordRpcHandle,
    kitty_graphics: bool,
) -> AnyResult<()> {
    let mut last_tick = Instant::now();
    let mut last_rpc_refresh = Instant::now();
    let mut last_state_save = Instant::now();
    let mut last_click: Option<(Instant, Hit)> = None;
    let mut matugen = theme::MatugenWatcher::new();
    app.matugen = matugen.colors.clone();
    let mut image_protocol = if kitty_graphics { ImageProtocol::Kitty } else { detect_image_protocol() };
    let mut ueberzugpp_layer = match image_protocol {
        ImageProtocol::Ueberzugpp => UeberzugppLayer::start(),
        _ => None,
    };
    if matches!(image_protocol, ImageProtocol::Ueberzugpp) && ueberzugpp_layer.is_none() {
        image_protocol = ImageProtocol::None;
    }
    let use_true_image_protocol = image_protocol != ImageProtocol::None;
    app.status_message = match image_protocol {
        ImageProtocol::Kitty => "Images: kitty graphics protocol".into(),
        ImageProtocol::Ueberzugpp => "Images: ueberzugpp".into(),
        ImageProtocol::None => "Images: text blocks (terminal without graphics)".into(),
    };
    let kitty_image_id: u32 = 1337;
    // Per slot: what is currently drawn (key + rect), to redraw only on change.
    let mut drawn_images: [Option<(String, u16, u16, u16, u16)>; IMAGE_SLOTS] = Default::default();
    let mut force_full_redraw = true;
    // Channel for background cover-art downloads.
    let (cover_tx, mut cover_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, Vec<u8>)>();
    // Generic thumbnails (artist photos, album covers) for the artist and library pages.
    let (image_tx, mut image_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, Option<image::DynamicImage>)>();

    'ui: loop {
        let elapsed = last_tick.elapsed();
        last_tick = Instant::now();

        if app.is_playing {
            if let Some(now) = app.now_playing.as_mut() {
                now.current_ms = (now.current_ms + elapsed.as_millis() as u64).min(now.total_ms);
            }
        }

        // Heartbeat refresh: re-publish Discord presence periodically in case
        // Discord clears it unexpectedly during seek/crossfade transitions.
        if last_rpc_refresh.elapsed() >= Duration::from_secs(5) {
            sync_discord_presence(discord_rpc, app);
            last_rpc_refresh = Instant::now();
        }

        if matugen.poll() {
            app.matugen = matugen.colors.clone();
        }

        // Periodic snapshot so a crash or a killed terminal still resumes close
        // to where playback was.
        if last_state_save.elapsed() >= Duration::from_secs(10) {
            if app.now_playing.is_some() {
                persist_state(app);
            }
            last_state_save = Instant::now();
        }

        if app.is_playing
            && app.config.crossfade_enabled
            && app.config.crossfade_duration_ms > 0
            && !app.auto_transition_armed
        {
            if let (Some(now), Some(current_idx)) = (app.now_playing.as_ref(), app.queue_index) {
                if !app.queue_tracks.is_empty() {
                    let next_idx = if current_idx + 1 < app.queue_tracks.len() {
                        Some(current_idx + 1)
                    } else if app.repeat_mode == RepeatMode::All {
                        Some(0)
                    } else {
                        None
                    };

                    if let Some(next_idx) = next_idx {
                        let remaining_ms = now.total_ms.saturating_sub(now.current_ms);
                        if remaining_ms <= app.config.crossfade_duration_ms {
                            if let Some((next_track_id, _, _)) = app.queue_tracks.get(next_idx) {
                                app.command_sender
                                    .send(Command::AutoPlayTrack(next_track_id.clone()))
                                    .map_err(|_| anyhow!("failed to send auto crossfade track command"))?;
                                app.queue_index = Some(next_idx);
                                app.queue_state.select(Some(next_idx));
                                app.auto_transition_armed = true;
                                app.status_message = format!(
                                    "Crossfading to queue item {}/{}",
                                    next_idx + 1,
                                    app.queue_tracks.len()
                                );
                            }
                        }
                    }
                }
            }
        }

        // Process MPRIS media control events from the OS/desktop environment
        if let Some(mpris_handle) = mpris.as_mut() {
            while let Ok(event) = mpris_handle.event_rx.try_recv() {
                match event {
                    mpris::MprisEvent::Play => {
                        if app.now_playing.is_some() && !app.is_playing {
                            app.command_sender
                                .send(Command::Resume)
                                .map_err(|_| anyhow!("MPRIS: failed to resume"))?;
                        }
                    }
                    mpris::MprisEvent::Pause => {
                        if app.is_playing {
                            app.command_sender
                                .send(Command::Pause)
                                .map_err(|_| anyhow!("MPRIS: failed to pause"))?;
                        }
                    }
                    mpris::MprisEvent::Toggle => {
                        if app.is_playing {
                            app.command_sender
                                .send(Command::Pause)
                                .map_err(|_| anyhow!("MPRIS: failed to toggle pause"))?;
                        } else if app.now_playing.is_some() {
                            app.command_sender
                                .send(Command::Resume)
                                .map_err(|_| anyhow!("MPRIS: failed to toggle resume"))?;
                        }
                    }
                    mpris::MprisEvent::Next => {
                        if let Some(current_idx) = app.queue_index {
                            let next_idx = current_idx + 1;
                            if next_idx < app.queue_tracks.len() {
                                app.queue_index = Some(next_idx);
                                app.queue_state.select(Some(next_idx));
                                if let Some((track_id, _, _)) = app.queue_tracks.get(next_idx) {
                                    app.command_sender
                                        .send(Command::PlayTrack(track_id.clone()))
                                        .map_err(|_| anyhow!("MPRIS: failed to play next"))?;
                                    app.is_playing = true;
                                }
                            }
                        }
                    }
                    mpris::MprisEvent::Previous => {
                        if let Some(current_idx) = app.queue_index {
                            if current_idx > 0 {
                                let prev_idx = current_idx - 1;
                                app.queue_index = Some(prev_idx);
                                app.queue_state.select(Some(prev_idx));
                                if let Some((track_id, _, _)) = app.queue_tracks.get(prev_idx) {
                                    app.command_sender
                                        .send(Command::PlayTrack(track_id.clone()))
                                        .map_err(|_| anyhow!("MPRIS: failed to play previous"))?;
                                    app.is_playing = true;
                                }
                            }
                        }
                    }
                    mpris::MprisEvent::SeekBy(offset_us) => {
                        if let Some(now) = app.now_playing.as_ref() {
                            if now.quality != AudioQuality::Flac {
                                let current_us = now.current_ms as i64 * 1_000;
                                let new_us = (current_us + offset_us)
                                    .max(0)
                                    .min((now.total_ms as i64 - 1) * 1_000);
                                let seek_ms = (new_us / 1_000) as u64;
                                app.command_sender
                                    .send(Command::PlayTrackAt {
                                        track_id: now.id.clone(),
                                        quality: now.quality,
                                        seek_ms,
                                    })
                                    .map_err(|_| anyhow!("MPRIS: failed to seek"))?;
                            }
                        }
                    }
                    mpris::MprisEvent::SetPosition(pos_us) => {
                        if let Some(now) = app.now_playing.as_ref() {
                            if now.quality != AudioQuality::Flac {
                                let seek_ms =
                                    ((pos_us.max(0) / 1_000) as u64).min(now.total_ms.saturating_sub(1));
                                app.command_sender
                                    .send(Command::PlayTrackAt {
                                        track_id: now.id.clone(),
                                        quality: now.quality,
                                        seek_ms,
                                    })
                                    .map_err(|_| anyhow!("MPRIS: failed to set position"))?;
                            }
                        }
                    }
                    mpris::MprisEvent::SetShuffle(shuffle) => {
                        if shuffle && !app.queue_tracks.is_empty() {
                            let current_id = app
                                .queue_index
                                .and_then(|i| app.queue_tracks.get(i))
                                .map(|t| t.0.clone());
                            app.queue_tracks.shuffle(&mut thread_rng());
                            if let Some(current) = current_id {
                                app.queue_index =
                                    app.queue_tracks.iter().position(|t| t.0 == current);
                            }
                            app.queue = app
                                .queue_tracks
                                .iter()
                                .map(|(_, title, artist)| format!("{} - {}", title, artist))
                                .collect();
                            if let Some(i) = app.queue_index {
                                app.queue_state.select(Some(i));
                            }
                            app.status_message = "Queue shuffled".into();
                        }
                    }
                    mpris::MprisEvent::SetLoopStatus(loop_status) => {
                        app.repeat_mode = match loop_status {
                            LoopStatus::None => RepeatMode::Off,
                            LoopStatus::Track => RepeatMode::One,
                            LoopStatus::Playlist => RepeatMode::All,
                        };
                        app.status_message = format!("Repeat: {:?}", app.repeat_mode);
                    }
                    mpris::MprisEvent::SetVolume(v) => {
                        app.volume = (v * 100.0).round().clamp(0.0, 100.0) as u16;
                        app.command_sender
                            .send(Command::SetVolume(app.volume))
                            .map_err(|_| anyhow!("MPRIS: failed to set volume"))?;
                    }
                }
            }

            // Rate-limited position update (every ~1s) — no signal needed,
            // D-Bus clients poll Position directly.
            if mpris_handle.last_update.elapsed() >= Duration::from_secs(1) {
                if let Some(now) = app.now_playing.as_ref() {
                    mpris::update_position(&mpris_handle.server, now.current_ms);
                }
                mpris_handle.last_update = Instant::now();
            }
        }

        if let Some(handle) = audio_task.as_ref() {
            if handle.is_finished() {
                let finished = audio_task
                    .take()
                    .ok_or_else(|| anyhow!("audio task missing after completion check"))?;

                if let Err(join_err) = finished.await {
                    return Err(anyhow!("background audio task panicked: {join_err}"));
                }
            }
        }

        // Receive any completed cover-art downloads and decode them.
        while let Ok((track_id, bytes)) = cover_rx.try_recv() {
            if app.now_playing.as_ref().map(|n| n.id.as_str()) == Some(track_id.as_str()) {
                if let Ok(img) = image::load_from_memory(&bytes) {
                    let mut png = Vec::new();
                    let mut cursor = io::Cursor::new(&mut png);
                    if img.write_to(&mut cursor, image::ImageFormat::Png).is_ok() {
                        app.cover_art_png = Some(Arc::new(png));
                    } else {
                        app.cover_art_png = None;
                    }
                    app.cover_art = Some(img);
                    app.cover_art_track_id = Some(track_id);
                }
            }
        }

        // Decoded thumbnails arriving from background downloads.
        while let Ok((url, img)) = image_rx.try_recv() {
            app.pending_images.remove(&url);
            if app.images.len() > 200 {
                app.images.clear();
            }
            // A failed download gets a 1x1 placeholder so it is not retried every tick.
            app.images.insert(url, img.unwrap_or_else(|| image::DynamicImage::new_rgb8(1, 1)));
        }
        for url in app.wanted_images() {
            if app.images.contains_key(&url) || app.pending_images.contains(&url) {
                continue;
            }
            app.pending_images.insert(url.clone());
            let tx = image_tx.clone();
            tokio::spawn(async move {
                let img = match reqwest::get(&url).await {
                    Ok(resp) => resp
                        .bytes()
                        .await
                        .ok()
                        .and_then(|bytes| image::load_from_memory(&bytes).ok())
                        .map(|img| {
                            // Thumbnails stay tiny; previews keep enough pixels for a big popup.
                            let max = if url.contains("500x500") { 320 } else { 64 };
                            img.thumbnail(max, max)
                        }),
                    Err(_) => None,
                };
                let _ = tx.send((url, img));
            });
        }

        if force_full_redraw {
            terminal.clear()?;
            force_full_redraw = false;
        }
        // Graphics protocol: encode the full-size artworks the screen wants.
        if use_true_image_protocol {
            for url in app.wanted_images() {
                if app.images_png.contains_key(&url) {
                    continue;
                }
                if let Some(img) = app.images.get(&url).filter(|i| i.width() > 1) {
                    let mut png = Vec::new();
                    let mut cursor = io::Cursor::new(&mut png);
                    if img.write_to(&mut cursor, image::ImageFormat::Png).is_ok() {
                        app.images_png.insert(url, Arc::new(png));
                    }
                }
            }
        }

        let mut protocol_images = Vec::new();
        terminal.draw(|f| {
            protocol_images = ui::render(f, app, use_true_image_protocol);
        })?;

        if use_true_image_protocol {
            let mut seen = [false; IMAGE_SLOTS];
            for img in &protocol_images {
                if img.slot >= IMAGE_SLOTS {
                    continue;
                }
                seen[img.slot] = true;
                // ui already fitted artworks to a centred square in cells.
                let rect = img.rect;
                let sig = (img.key.clone(), rect.x, rect.y, rect.width, rect.height);
                if drawn_images[img.slot].as_ref() == Some(&sig) {
                    continue;
                }
                match image_protocol {
                    ImageProtocol::Kitty => {
                        let _ = kitty_draw_png_at_rect(&img.png, kitty_image_id + img.slot as u32, rect);
                    }
                    ImageProtocol::Ueberzugpp => {
                        if let (Some(layer), Ok(path)) =
                            (ueberzugpp_layer.as_mut(), write_cover_png_temp(&img.png, img.slot))
                        {
                            let _ = layer.add(img.slot, &path, rect);
                        }
                    }
                    ImageProtocol::None => {}
                }
                drawn_images[img.slot] = Some(sig);
            }
            for slot in 0..IMAGE_SLOTS {
                if seen[slot] || drawn_images[slot].is_none() {
                    continue;
                }
                match image_protocol {
                    ImageProtocol::Kitty => {
                        let _ = kitty_delete_image(kitty_image_id + slot as u32);
                    }
                    ImageProtocol::Ueberzugpp => {
                        if let Some(layer) = ueberzugpp_layer.as_mut() {
                            let _ = layer.remove(slot);
                        }
                    }
                    ImageProtocol::None => {}
                }
                drawn_images[slot] = None;
            }
        }

        let mut processed_input = false;
        while event::poll(if processed_input {
            Duration::from_millis(0)
        } else {
            Duration::from_millis(50)
        })? {
            processed_input = true;
            let mut pending_keys: VecDeque<(KeyCode, KeyModifiers)> = VecDeque::new();
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    pending_keys.push_back((key.code, key.modifiers));
                }
                Event::Mouse(mouse_event) => {
                    handle_mouse(app, &mouse_event, &mut pending_keys, &mut last_click);
                }
                Event::Resize(_, _) => {
                    // Resize often cleans up stray lines, keep that behavior explicit.
                    // Also re-assert the terminal modes: when the TUI is re-attached to a
                    // new terminal (dtach), that terminal never saw them.
                    let _ = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture);
                    force_full_redraw = true;
                    // A re-attached terminal never saw the images: send them again.
                    drawn_images = Default::default();
                }
                _ => {}
            }
            while let Some((code, mods)) = pending_keys.pop_front() {
                    let in_search = app.is_searching || app.active_panel == ActivePanel::Search;
                    let menu_open = app.context_menu.is_some();
                    match code {
                        KeyCode::Esc if menu_open => app.context_menu = None,
                        KeyCode::Up | KeyCode::Char('k') if menu_open => app.menu_move(-1),
                        KeyCode::Down | KeyCode::Char('j') if menu_open => app.menu_move(1),
                        KeyCode::Enter | KeyCode::Right if menu_open => app.menu_activate(),
                        KeyCode::Left if menu_open => app.menu_back(),
                        _ if menu_open => {}
                        // m: start a mix from the selected track.
                        KeyCode::Char('m') if !in_search => match app.selected_track() {
                            Some(track) => app.start_mix(track),
                            None => app.status_message = "Select a track first".into(),
                        },
                        // M: track menu for the selected row (same as right-click).
                        KeyCode::Char('M') if !in_search => {
                            if let Some(track) = app.selected_track() {
                                let in_queue = app.active_panel == ActivePanel::Queue;
                                let queue_pos = if in_queue { app.queue_state.selected() } else { None };
                                let selected = app.main_state.selected();
                                let anchor = app
                                    .clicks
                                    .iter()
                                    .find(|c| match c.hit {
                                        Hit::QueueItem(i) => in_queue && Some(i) == queue_pos,
                                        Hit::MainRow { index, .. } => !in_queue && Some(index) == selected,
                                        _ => false,
                                    })
                                    .map(|c| (c.rect.x + 6, c.rect.y + 1));
                                let (x, y) = anchor.unwrap_or((40, 10));
                                app.open_track_menu(track, x, y, queue_pos);
                            } else {
                                app.status_message = "Select a track first".into();
                            }
                        }
                        // z: play the selected track right after the current one.
                        KeyCode::Char('z') if !in_search => match app.selected_track() {
                            Some(track) => app.enqueue(track, true),
                            None => app.status_message = "Select a track first".into(),
                        },
                        // Ctrl+Backspace clears the query (Alacritty and others send Ctrl+H).
                        KeyCode::Backspace | KeyCode::Char('h') if in_search && mods.contains(KeyModifiers::CONTROL) => {
                            app.search_query.clear();
                        }
                        // Alt+Backspace (or Ctrl+W) deletes the last word.
                        KeyCode::Backspace | KeyCode::Char('w')
                            if in_search && (mods.contains(KeyModifiers::ALT) || mods.contains(KeyModifiers::CONTROL)) =>
                        {
                            let trimmed_len = app.search_query.trim_end().len();
                            app.search_query.truncate(trimmed_len);
                            let cut = app.search_query.rfind(' ').map(|i| i + 1).unwrap_or(0);
                            app.search_query.truncate(cut);
                        }
                        KeyCode::Char('q') if !in_search => {
                            persist_state(app);
                            discord_rpc.clear();
                            let _ = app.command_sender.send(Command::Shutdown);
                            break 'ui;
                        }
                        KeyCode::Char(c) if app.is_searching || app.active_panel == ActivePanel::Search => {
                            app.search_query.push(c);
                        }
                        KeyCode::Backspace if app.is_searching || app.active_panel == ActivePanel::Search => {
                            app.search_query.pop();
                        }
                        KeyCode::Esc if app.is_searching || app.active_panel == ActivePanel::Search => {
                            app.is_searching = false;
                            app.active_panel = ActivePanel::Main;
                        }
                        KeyCode::Enter if app.is_searching || app.active_panel == ActivePanel::Search => {
                            app.is_searching = false;
                            app.active_panel = ActivePanel::Main;
                            app.main_state.select(Some(0));
                            app.command_sender
                                .send(Command::Search(app.search_query.clone()))
                                .map_err(|_| anyhow!("failed to send search command"))?;
                            app.status_message = "Searching...".into();
                        }
                        KeyCode::Char('/') if !app.is_searching => {
                            app.is_searching = true;
                            app.active_panel = ActivePanel::Search;
                            app.search_query.clear();
                        }
                        KeyCode::Char('t') if !app.is_searching => {
                            app.config.theme = app.config.theme.next();
                            let _ = save_config(&app.config);
                            app.status_message = format!("Theme: {}", app.config.theme.label());
                        }
                        // a / e: artist / album of the focused track — the one playing when
                        // the player has focus, otherwise the one selected in the focused list.
                        KeyCode::Char(c @ ('a' | 'e')) if !app.is_searching => {
                            let player_focused = matches!(
                                app.active_panel,
                                ActivePanel::Player | ActivePanel::PlayerProgress | ActivePanel::PlayerInfo
                            );
                            let track_id = if player_focused {
                                app.now_playing.as_ref().map(|n| n.id.clone())
                            } else {
                                match app.active_panel {
                                    ActivePanel::Queue | ActivePanel::Main => app.selected_track().map(|t| t.0),
                                    _ => None,
                                }
                            };
                            match track_id {
                                Some(track_id) if c == 'a' => {
                                    app.status_message = "Opening artist...".into();
                                    let _ = app.command_sender.send(Command::OpenArtistOfTrack(track_id));
                                }
                                Some(track_id) => {
                                    app.status_message = "Opening album...".into();
                                    let _ = app.command_sender.send(Command::OpenAlbumOfTrack(track_id));
                                }
                                None if player_focused => app.status_message = "Nothing is playing".into(),
                                None => app.status_message = "Select a track first".into(),
                            }
                        }
                        // Esc / PageUp: previous page; PageDown: next page (browser-style history).
                        KeyCode::Esc | KeyCode::PageUp if !in_search && !app.history.is_empty() => {
                            if let Some(previous) = app.history.pop() {
                                let current = app.current_view();
                                app.forward.push(current);
                                restore_view(app, previous);
                            }
                        }
                        KeyCode::PageDown if !in_search && !app.forward.is_empty() => {
                            if let Some(next) = app.forward.pop() {
                                let current = app.current_view();
                                app.history.push(current);
                                restore_view(app, next);
                            }
                        }
                        KeyCode::Char('p') | KeyCode::Char(' ') if !app.is_searching => {
                            if app.now_playing.is_none() {
                                app.command_sender
                                    .send(Command::PlayTrack("3135556".to_string()))
                                    .map_err(|_| anyhow!("failed to send play command to audio worker"))?;
                                app.is_playing = true;
                            } else if app.is_playing {
                                app.command_sender
                                    .send(Command::Pause)
                                    .map_err(|_| anyhow!("failed to send pause command to audio worker"))?;
                            } else {
                                app.command_sender
                                    .send(Command::Resume)
                                    .map_err(|_| anyhow!("failed to send resume command to audio worker"))?;
                            }
                        }
                        KeyCode::Tab if !app.is_searching => {
                            app.active_panel = match app.active_panel {
                                ActivePanel::Navigation => ActivePanel::Playlists,
                                ActivePanel::Playlists => ActivePanel::Queue,
                                ActivePanel::Queue => ActivePanel::Search,
                                ActivePanel::Search => ActivePanel::Main,
                                ActivePanel::Main if app.right_panel_visible => ActivePanel::Shortcuts,
                                ActivePanel::Main => ActivePanel::Player,
                                ActivePanel::Shortcuts => ActivePanel::Player,
                                ActivePanel::PlayerProgress => ActivePanel::Navigation,
                                ActivePanel::Player => ActivePanel::Navigation,
                                ActivePanel::PlayerInfo => ActivePanel::Navigation,
                            };
                        }
                        KeyCode::BackTab if !app.is_searching => {
                            app.active_panel = match app.active_panel {
                                ActivePanel::Navigation => ActivePanel::Player,
                                ActivePanel::Playlists => ActivePanel::Navigation,
                                ActivePanel::Queue => ActivePanel::Playlists,
                                ActivePanel::Search => ActivePanel::Queue,
                                ActivePanel::Main => ActivePanel::Search,
                                ActivePanel::Shortcuts => ActivePanel::Main,
                                ActivePanel::PlayerProgress => ActivePanel::Main,
                                ActivePanel::Player if app.right_panel_visible => ActivePanel::Shortcuts,
                                ActivePanel::Player => ActivePanel::Main,
                                ActivePanel::PlayerInfo => ActivePanel::Main,
                            };
                        }
                        KeyCode::Down | KeyCode::Char('j') if !app.is_searching => app.handle_down(),
                        KeyCode::Up | KeyCode::Char('k') if !app.is_searching => app.handle_up(),
                        KeyCode::Left if !app.is_searching => {
                            if app.active_panel == ActivePanel::PlayerProgress {
                                if let Some(now) = app.now_playing.as_ref() {
                                    if now.quality == AudioQuality::Flac {
                                        app.status_message = "FLAC seek is disabled".into();
                                        continue;
                                    }
                                    let seek_ms = now.current_ms.saturating_sub(5_000);
                                    app.command_sender
                                        .send(Command::PlayTrackAt {
                                            track_id: now.id.clone(),
                                            quality: now.quality,
                                            seek_ms,
                                        })
                                        .map_err(|_| anyhow!("failed to seek track"))?;
                                    if let Some(mpris_handle) = mpris.as_mut() {
                                        mpris::notify_seeked(&mpris_handle.server, seek_ms).await;
                                    }
                                    app.status_message = format!("Seek: {}s", seek_ms / 1000);
                                }
                            } else if app.active_panel == ActivePanel::PlayerInfo {
                                match app.player_info_index {
                                    0 => {
                                        app.volume = app.volume.saturating_sub(5);
                                        app.command_sender
                                            .send(Command::SetVolume(app.volume))
                                            .map_err(|_| anyhow!("failed to set volume"))?;
                                    }
                                    1 => {
                                        if let Some(now) = app.now_playing.as_ref() {
                                            let quality = next_quality(now.quality, false);
                                            let seek_ms = if quality == AudioQuality::Flac {
                                                0
                                            } else {
                                                now.current_ms
                                            };
                                            app.command_sender
                                                .send(Command::PlayTrackAt {
                                                    track_id: now.id.clone(),
                                                    quality,
                                                    seek_ms,
                                                })
                                                .map_err(|_| anyhow!("failed to switch quality"))?;
                                            app.status_message = format!("Quality: {:?}", quality);
                                        }
                                    }
                                    _ => {}
                                }
                            } else {
                                app.handle_left();
                            }
                        }
                        KeyCode::Right if !app.is_searching => {
                            if app.active_panel == ActivePanel::Main && app.viewing_artist {
                                app.artist_tab_next();
                            } else if app.active_panel == ActivePanel::Main && app.showing_search_results {
                                app.switch_search_category_right();
                            } else if app.active_panel == ActivePanel::PlayerProgress {
                                if let Some(now) = app.now_playing.as_ref() {
                                    if now.quality == AudioQuality::Flac {
                                        app.status_message = "FLAC seek is disabled".into();
                                        continue;
                                    }
                                    let seek_ms = (now.current_ms + 5_000).min(now.total_ms.saturating_sub(1));
                                    app.command_sender
                                        .send(Command::PlayTrackAt {
                                            track_id: now.id.clone(),
                                            quality: now.quality,
                                            seek_ms,
                                        })
                                        .map_err(|_| anyhow!("failed to seek track"))?;
                                    if let Some(mpris_handle) = mpris.as_mut() {
                                        mpris::notify_seeked(&mpris_handle.server, seek_ms).await;
                                    }
                                    app.status_message = format!("Seek: {}s", seek_ms / 1000);
                                }
                            } else if app.active_panel == ActivePanel::PlayerInfo {
                                match app.player_info_index {
                                    0 => {
                                        app.volume = (app.volume + 5).min(100);
                                        app.command_sender
                                            .send(Command::SetVolume(app.volume))
                                            .map_err(|_| anyhow!("failed to set volume"))?;
                                    }
                                    1 => {
                                        if let Some(now) = app.now_playing.as_ref() {
                                            let quality = next_quality(now.quality, true);
                                            let seek_ms = if quality == AudioQuality::Flac {
                                                0
                                            } else {
                                                now.current_ms
                                            };
                                            app.command_sender
                                                .send(Command::PlayTrackAt {
                                                    track_id: now.id.clone(),
                                                    quality,
                                                    seek_ms,
                                                })
                                                .map_err(|_| anyhow!("failed to switch quality"))?;
                                            app.status_message = format!("Quality: {:?}", quality);
                                        }
                                    }
                                    _ => {}
                                }
                            } else {
                                app.handle_right();
                            }
                        }
                        KeyCode::Enter if !app.is_searching => {
                            match app.active_panel {
                                ActivePanel::Navigation => {
                                    let nav_idx = app.nav_state.selected().unwrap_or(0);
                                    match nav_idx {
                                        0 => {
                                            app.command_sender
                                                .send(Command::LoadHome)
                                                .map_err(|_| anyhow!("failed to send load home command"))?;
                                            app.current_playlist_id = Some("__home__".to_string());
                                            app.active_panel = ActivePanel::Main;
                                            app.status_message = "Loading Home recommendations...".into();
                                        }
                                        1 => {
                                            app.command_sender
                                                .send(Command::LoadExplore)
                                                .map_err(|_| anyhow!("failed to send load explore command"))?;
                                            app.current_playlist_id = Some("__explore__".to_string());
                                            app.active_panel = ActivePanel::Main;
                                            app.status_message = "Loading Explore recommendations...".into();
                                        }
                                        2 => {
                                            app.command_sender
                                                .send(Command::LoadFavorites)
                                                .map_err(|_| anyhow!("failed to send load favorites command"))?;
                                            app.active_panel = ActivePanel::Main;
                                            app.status_message = "Loading favorites...".into();
                                        }
                                        3 => {
                                            app.active_panel = ActivePanel::Playlists;
                                            if !app.playlists.is_empty() && app.playlist_state.selected().is_none() {
                                                app.playlist_state.select(Some(0));
                                            }
                                        }
                                        4 => {
                                            app.command_sender
                                                .send(Command::LoadFavoriteAlbums)
                                                .map_err(|_| anyhow!("failed to send load albums command"))?;
                                            app.status_message = "Loading your albums...".into();
                                        }
                                        5 => {
                                            app.command_sender
                                                .send(Command::LoadFavoriteArtists)
                                                .map_err(|_| anyhow!("failed to send load artists command"))?;
                                            app.status_message = "Loading your artists...".into();
                                        }
                                        6 => {
                                            app.leave_special_views();
                                            app.viewing_settings = true;
                                            app.active_panel = ActivePanel::Main;
                                        }
                                        _ => {}
                                    }
                                }
                                ActivePanel::Playlists => {
                                    if let Some(idx) = app.playlist_state.selected() {
                                        if idx < app.playlists.len() {
                                            let (playlist_id, _) = &app.playlists[idx];
                                            app.current_playlist_id = Some(playlist_id.clone());
                                            app.command_sender
                                                .send(Command::LoadPlaylist(playlist_id.clone()))
                                                .map_err(|_| anyhow!("failed to send load playlist command"))?;
                                            app.active_panel = ActivePanel::Main;
                                        }
                                    }
                                }
                                ActivePanel::Queue => {
                                    if let Some(idx) = app.queue_state.selected() {
                                        if idx < app.queue_tracks.len() {
                                            app.queue_index = Some(idx);
                                            let (track_id, _, _) = &app.queue_tracks[idx];
                                            app.command_sender
                                                .send(Command::PlayTrack(track_id.clone()))
                                                .map_err(|_| anyhow!("failed to play queued track"))?;
                                            app.is_playing = true;
                                        }
                                    }
                                }
                                ActivePanel::Main => {
                                    if app.viewing_settings {
                                        if let Some(idx) = app.settings_state.selected() {
                                            match idx {
                                                0 => {
                                                    app.config.crossfade_enabled = !app.config.crossfade_enabled;
                                                    app.command_sender
                                                        .send(Command::SetCrossfade {
                                                            enabled: app.config.crossfade_enabled,
                                                            duration_ms: app.config.crossfade_duration_ms,
                                                        })
                                                        .map_err(|_| anyhow!("failed to set crossfade"))?;
                                                    let _ = save_config(&app.config);
                                                }
                                                1 => {
                                                    let presets = [1000u64, 3000, 5000, 8000, 10000, 13000];
                                                    let current = app.config.crossfade_duration_ms;
                                                    let next = presets
                                                        .iter()
                                                        .copied()
                                                        .find(|value| *value > current)
                                                        .unwrap_or(presets[0]);
                                                    app.config.crossfade_duration_ms = next;
                                                    app.command_sender
                                                        .send(Command::SetCrossfade {
                                                            enabled: app.config.crossfade_enabled,
                                                            duration_ms: app.config.crossfade_duration_ms,
                                                        })
                                                        .map_err(|_| anyhow!("failed to set crossfade duration"))?;
                                                    let _ = save_config(&app.config);
                                                }
                                                2 => {
                                                    app.config.default_quality = match app.config.default_quality {
                                                        AudioQuality::Kbps128 => AudioQuality::Kbps320,
                                                        AudioQuality::Kbps320 => AudioQuality::Flac,
                                                        AudioQuality::Flac => AudioQuality::Kbps128,
                                                    };
                                                    app.command_sender
                                                        .send(Command::SetQuality(app.config.default_quality))
                                                        .map_err(|_| anyhow!("failed to set quality"))?;
                                                    let _ = save_config(&app.config);
                                                }
                                                3 => {
                                                    app.discord_rpc_enabled = !app.discord_rpc_enabled;
                                                    app.config.discord_rpc_enabled = app.discord_rpc_enabled;
                                                    let _ = save_config(&app.config);
                                                    sync_discord_presence(discord_rpc, app);
                                                }
                                                4 => {
                                                    app.config.resume_on_start = !app.config.resume_on_start;
                                                    let _ = save_config(&app.config);
                                                    app.status_message = if app.config.resume_on_start {
                                                        "Resume on start enabled".into()
                                                    } else {
                                                        "Resume on start disabled".into()
                                                    };
                                                }
                                                5 => {
                                                    let new_arl = app.search_query.trim();
                                                    if new_arl.is_empty() {
                                                        app.status_message = "Type new ARL in search box first".into();
                                                    } else if let Err(err) = save_arl(new_arl) {
                                                        app.status_message = format!("Failed to save ARL: {}", err);
                                                    } else {
                                                        app.status_message = "ARL updated".into();
                                                        app.search_query.clear();
                                                    }
                                                }
                                                6 => match app.update.clone() {
                                                    Some(release) => {
                                                        app.status_message = format!("Downloading {}...", release.tag_name);
                                                        let tx = update_tx.clone();
                                                        tokio::spawn(async move {
                                                            let msg = match update::install(&release).await {
                                                                Ok(()) => format!("Status: Updated to {} - restart deezer-tui", release.tag_name),
                                                                Err(err) => format!("Status: Update failed: {err:#}"),
                                                            };
                                                            let _ = tx.send(UiEvent::Error(msg));
                                                        });
                                                    }
                                                    None => {
                                                        app.status_message = "Checking for updates...".into();
                                                        spawn_update_check(update_tx.clone(), true);
                                                    }
                                                },
                                                _ => {}
                                            }
                                        }
                                    } else if app.viewing_artist {
                                        let row = app.selected_artist_row().cloned();
                                        let page = app.artist_page.clone();
                                        if let (Some(row), Some(page)) = (row, page) {
                                            match row {
                                                ArtistRow::PlayAll | ArtistRow::Track(_) => {
                                                    let idx = if let ArtistRow::Track(i) = row { i } else { 0 };
                                                    app.queue_tracks = page
                                                        .top_tracks
                                                        .iter()
                                                        .map(|t| (t.id.clone(), t.title.clone(), t.artist.clone()))
                                                        .collect();
                                                    app.queue = app
                                                        .queue_tracks
                                                        .iter()
                                                        .map(|(_, title, artist)| format!("{} - {}", title, artist))
                                                        .collect();
                                                    app.queue_index = Some(idx);
                                                    app.queue_state.select(Some(idx));
                                                    app.current_playlist_id = Some(format!("__artist__:{}", page.id));
                                                    if let Some((track_id, _, _)) = app.queue_tracks.get(idx) {
                                                        app.command_sender
                                                            .send(Command::PlayTrack(track_id.clone()))
                                                            .map_err(|_| anyhow!("failed to play artist track"))?;
                                                        app.is_playing = true;
                                                    }
                                                }
                                                ArtistRow::Album(i) => {
                                                    if let Some(album) = page.albums.get(i) {
                                                        app.current_playlist_id = Some(format!("__album__:{}", album.id));
                                                        app.command_sender
                                                            .send(Command::LoadAlbum(album.id.clone()))
                                                            .map_err(|_| anyhow!("failed to load album"))?;
                                                        app.status_message = format!("Loading album: {}", album.title);
                                                    }
                                                }
                                                ArtistRow::Related(i) => {
                                                    if let Some(artist) = page.related.get(i) {
                                                        app.command_sender
                                                            .send(Command::LoadArtist(artist.id.clone()))
                                                            .map_err(|_| anyhow!("failed to load artist"))?;
                                                        app.status_message = format!("Loading artist: {}", artist.name);
                                                    }
                                                }
                                                ArtistRow::Playlist(i) => {
                                                    if let Some(playlist) = page.playlists.get(i) {
                                                        app.current_playlist_id = Some(playlist.id.clone());
                                                        app.command_sender
                                                            .send(Command::LoadPlaylist(playlist.id.clone()))
                                                            .map_err(|_| anyhow!("failed to load playlist"))?;
                                                        app.status_message = format!("Loading playlist: {}", playlist.title);
                                                    }
                                                }
                                                ArtistRow::Header(_) => {}
                                            }
                                        }
                                    } else if let Some(view) = app.library_view {
                                        if let Some(idx) = app.main_state.selected() {
                                            match view {
                                                LibraryView::Albums => {
                                                    if let Some(album) = app.library_albums.get(idx).cloned() {
                                                        app.current_playlist_id = Some(format!("__album__:{}", album.id));
                                                        app.command_sender
                                                            .send(Command::LoadAlbum(album.id.clone()))
                                                            .map_err(|_| anyhow!("failed to load album"))?;
                                                        app.status_message = format!("Loading album: {}", album.title);
                                                    }
                                                }
                                                LibraryView::Artists => {
                                                    if let Some(artist) = app.library_artists.get(idx).cloned() {
                                                        app.command_sender
                                                            .send(Command::LoadArtist(artist.id.clone()))
                                                            .map_err(|_| anyhow!("failed to load artist"))?;
                                                        app.status_message = format!("Loading artist: {}", artist.name);
                                                    }
                                                }
                                            }
                                        }
                                    } else if app.showing_search_results {
                                        if let Some(idx) = app.main_state.selected() {
                                            match app.search_category {
                                                SearchCategory::Tracks => {
                                                    if idx == 0 {
                                                        app.queue_tracks = app.current_tracks.clone();
                                                        app.queue = app
                                                            .queue_tracks
                                                            .iter()
                                                            .map(|(_, title, artist)| format!("{} - {}", title, artist))
                                                            .collect();
                                                        app.queue_state.select(Some(0));
                                                        app.queue_index = Some(0);

                                                        if let Some((track_id, _, _)) = app.queue_tracks.first() {
                                                            app.command_sender
                                                                .send(Command::PlayTrack(track_id.clone()))
                                                                .map_err(|_| anyhow!("failed to send play track command"))?;
                                                            app.is_playing = true;
                                                        }
                                                    } else {
                                                        let track_idx = idx - 1;
                                                        if track_idx < app.current_tracks.len() {
                                                            let selected = app.current_tracks[track_idx].clone();
                                                            app.queue_tracks = vec![selected.clone()];
                                                            app.queue = vec![format!("{} - {}", selected.1, selected.2)];
                                                            app.queue_state.select(Some(0));
                                                            app.queue_index = Some(0);

                                                            app.command_sender
                                                                .send(Command::PlayTrack(selected.0))
                                                                .map_err(|_| anyhow!("failed to send play track command"))?;
                                                            app.is_playing = true;
                                                        }
                                                    }
                                                }
                                                SearchCategory::Playlists => {
                                                    if idx < app.search_playlists.len() {
                                                        let (playlist_id, title) = &app.search_playlists[idx];
                                                        app.current_playlist_id = Some(playlist_id.clone());
                                                        app.command_sender
                                                            .send(Command::LoadPlaylist(playlist_id.clone()))
                                                            .map_err(|_| anyhow!("failed to load playlist from search"))?;
                                                        app.status_message = format!("Loading playlist: {}", title);
                                                    }
                                                }
                                                SearchCategory::Artists => {
                                                    if idx < app.search_artists.len() {
                                                        let (artist_id, name) = app.search_artists[idx].clone();
                                                        app.command_sender
                                                            .send(Command::LoadArtist(artist_id))
                                                            .map_err(|_| anyhow!("failed to load artist"))?;
                                                        app.status_message = format!("Loading artist: {}", name);
                                                    }
                                                }
                                            }
                                        }
                                    } else if !app.current_tracks.is_empty() {
                                        if let Some(idx) = app.main_state.selected() {
                                            if idx == 0 {
                                                app.queue_tracks = app.current_tracks.clone();
                                                app.queue = app
                                                    .queue_tracks
                                                    .iter()
                                                    .map(|(_, title, artist)| format!("{} - {}", title, artist))
                                                    .collect();
                                                app.queue_state.select(Some(0));
                                                app.queue_index = Some(0);

                                                if let Some((track_id, _, _)) = app.queue_tracks.first() {
                                                    app.command_sender
                                                        .send(Command::PlayTrack(track_id.clone()))
                                                        .map_err(|_| anyhow!("failed to send play track command"))?;
                                                    app.is_playing = true;
                                                }
                                            } else {
                                                let track_idx = idx - 1;
                                                if track_idx < app.current_tracks.len() {
                                                    let selected = app.current_tracks[track_idx].clone();
                                                    app.queue_tracks = vec![selected.clone()];
                                                    app.queue = vec![format!("{} - {}", selected.1, selected.2)];
                                                    app.queue_state.select(Some(0));
                                                    app.queue_index = Some(0);

                                                    app.command_sender
                                                        .send(Command::PlayTrack(selected.0))
                                                        .map_err(|_| anyhow!("failed to send play track command"))?;
                                                    app.is_playing = true;
                                                }
                                            }
                                        }
                                    }
                                }
                                ActivePanel::Player => {
                                    match app.player_button_index {
                                        0 => {
                                            if !app.queue_tracks.is_empty() {
                                                let current_id = app
                                                    .queue_index
                                                    .and_then(|i| app.queue_tracks.get(i))
                                                    .map(|t| t.0.clone());
                                                app.queue_tracks.shuffle(&mut thread_rng());
                                                if let Some(current) = current_id {
                                                    app.queue_index = app
                                                        .queue_tracks
                                                        .iter()
                                                        .position(|t| t.0 == current);
                                                }
                                                app.queue = app
                                                    .queue_tracks
                                                    .iter()
                                                    .map(|(_, title, artist)| format!("{} - {}", title, artist))
                                                    .collect();
                                                if let Some(i) = app.queue_index {
                                                    app.queue_state.select(Some(i));
                                                }
                                                app.status_message = "Queue shuffled".into();
                                            }
                                        }
                                        1 => {
                                            if let Some(current_idx) = app.queue_index {
                                                if current_idx > 0 {
                                                    let prev_idx = current_idx - 1;
                                                    app.queue_index = Some(prev_idx);
                                                    app.queue_state.select(Some(prev_idx));
                                                    if let Some((track_id, _, _)) = app.queue_tracks.get(prev_idx) {
                                                        app.command_sender
                                                            .send(Command::PlayTrack(track_id.clone()))
                                                            .map_err(|_| anyhow!("failed to play previous track"))?;
                                                        app.is_playing = true;
                                                    }
                                                }
                                            }
                                        }
                                        2 => {
                                            if app.is_playing {
                                                app.command_sender
                                                    .send(Command::Pause)
                                                    .map_err(|_| anyhow!("failed to pause"))?;
                                            } else {
                                                app.command_sender
                                                    .send(Command::Resume)
                                                    .map_err(|_| anyhow!("failed to resume"))?;
                                            }
                                        }
                                        3 => {
                                            if let Some(current_idx) = app.queue_index {
                                                let next_idx = current_idx + 1;
                                                if next_idx < app.queue_tracks.len() {
                                                    app.queue_index = Some(next_idx);
                                                    app.queue_state.select(Some(next_idx));
                                                    if let Some((track_id, _, _)) = app.queue_tracks.get(next_idx) {
                                                        app.command_sender
                                                            .send(Command::PlayTrack(track_id.clone()))
                                                            .map_err(|_| anyhow!("failed to play next track"))?;
                                                        app.is_playing = true;
                                                    }
                                                }
                                            }
                                        }
                                        4 => {
                                            app.repeat_mode = match app.repeat_mode {
                                                RepeatMode::Off => RepeatMode::All,
                                                RepeatMode::All => RepeatMode::One,
                                                RepeatMode::One => RepeatMode::Off,
                                            };
                                            if let Some(mpris_handle) = mpris.as_mut() {
                                                let loop_status = match app.repeat_mode {
                                                    RepeatMode::Off => LoopStatus::None,
                                                    RepeatMode::One => LoopStatus::Track,
                                                    RepeatMode::All => LoopStatus::Playlist,
                                                };
                                                mpris::update_loop_and_shuffle(&mpris_handle.server, loop_status, false).await;
                                            }
                                            app.status_message = format!("Repeat mode: {:?}", app.repeat_mode);
                                        }
                                        _ => {}
                                    }
                                }
                                ActivePanel::Search => {
                                    app.command_sender
                                        .send(Command::Search(app.search_query.clone()))
                                        .map_err(|_| anyhow!("failed to send search command"))?;
                                    app.active_panel = ActivePanel::Main;
                                }
                                ActivePanel::PlayerProgress => {
                                    // Progress bar uses Left/Right for seek; Enter is a no-op.
                                }
                                ActivePanel::PlayerInfo => {}
                                ActivePanel::Shortcuts => {}
                            }
                        }
                        _ => {}
                    }
            }
        }

        while let Ok(event) = event_rx.try_recv() {
            match event {
                UiEvent::UpdateChecked(release) => {
                    app.status_message = match &release {
                        Some(r) => format!("Update {} available - Settings > Install update", r.tag_name),
                        None => format!("deezer-tui v{} is up to date", env!("CARGO_PKG_VERSION")),
                    };
                    app.update = release;
                }
                UiEvent::HistoryLoaded(history) => {
                    apply_resume(app, &history);
                }
                UiEvent::OwnPlaylistsLoaded(playlists) => {
                    app.own_playlists = playlists;
                }
                UiEvent::Loading(label) => {
                    app.loading = label.map(|l| (l, Instant::now()));
                }
                UiEvent::AlbumLoaded(album) => {
                    app.loading = None;
                    app.remember_view(&ViewState::Album(album.id.clone()));
                    app.status_message = format!("Album: {} · {} · {} tracks", album.title, album.artist, album.tracks.len());
                    app.current_tracks = album.tracks;
                    app.current_playlist_id = Some(format!("__album__:{}", album.id));
                    app.current_album = Some((album.id, album.title, album.artist));
                    app.showing_search_results = false;
                    app.search_playlists.clear();
                    app.search_artists.clear();
                    app.leave_special_views();
                    app.viewing_settings = false;
                    app.main_state.select(Some(0));
                    app.active_panel = ActivePanel::Main;
                }
                UiEvent::ArtistLoaded(page) => {
                    app.loading = None;
                    app.remember_view(&ViewState::Artist(page.id.clone()));
                    app.status_message = format!("Artist: {} · {} fans", page.name, app::fmt_count(page.nb_fan));
                    app.show_artist(page);
                }
                UiEvent::FavoriteAlbumsLoaded(albums) => {
                    app.loading = None;
                    app.remember_view(&ViewState::Library(LibraryView::Albums));
                    app.status_message = format!("{} albums in your library", albums.len());
                    app.library_albums = albums;
                    app.show_library(LibraryView::Albums);
                }
                UiEvent::FavoriteArtistsLoaded(artists) => {
                    app.loading = None;
                    app.remember_view(&ViewState::Library(LibraryView::Artists));
                    app.status_message = format!("{} artists in your library", artists.len());
                    app.library_artists = artists;
                    app.show_library(LibraryView::Artists);
                }
                UiEvent::TrackChanged {
                    id,
                    title,
                    artist,
                    artist_id,
                    quality,
                    album_art_url,
                    initial_ms,
                } => {
                    // Clear stale cover art immediately; new art arrives asynchronously.
                    app.cover_art = None;
                    app.cover_art_png = None;
                    app.cover_art_track_id = None;
                    if let Some(ref url) = album_art_url {
                        let url = url.clone();
                        let tid = id.clone();
                        let tx = cover_tx.clone();
                        tokio::spawn(async move {
                            if let Ok(resp) = reqwest::get(&url).await {
                                if let Ok(bytes) = resp.bytes().await {
                                    let _ = tx.send((tid, bytes.to_vec()));
                                }
                            }
                        });
                    }
                    if let Some(mpris_handle) = mpris.as_mut() {
                        let can_go_next = app.queue_index
                            .map(|i| i + 1 < app.queue_tracks.len())
                            .unwrap_or(false);
                        let can_go_previous = app.queue_index.unwrap_or(0) > 0;
                        let meta = mpris::build_metadata(&id, &title, &artist, album_art_url.as_deref(), 0);
                        mpris::set_track_metadata(&mpris_handle.server, meta, initial_ms, can_go_next, can_go_previous).await;
                        mpris_handle.last_update = Instant::now();
                    }
                    app.now_playing = Some(NowPlaying {
                        id,
                        title,
                        artist,
                        artist_id,
                        album_art_url,
                        quality,
                        current_ms: initial_ms,
                        total_ms: 224_000,
                    });
                    app.is_playing = true;
                    app.auto_transition_armed = false;
                    sync_discord_presence(discord_rpc, app);
                }
                UiEvent::PlaybackProgress {
                    current_ms,
                    total_ms,
                } => {
                    let prev_total = app.now_playing.as_ref().map(|n| n.total_ms).unwrap_or(0);
                    if let Some(now) = app.now_playing.as_mut() {
                        now.current_ms = current_ms;
                        now.total_ms = total_ms.max(1);
                    }
                    // On the first real total_ms, update the MPRIS metadata with proper duration
                    if prev_total == 224_000 && total_ms != 224_000 {
                        if let (Some(mpris_handle), Some(now)) = (mpris.as_mut(), app.now_playing.as_ref()) {
                            let meta = mpris::build_metadata(
                                &now.id, &now.title, &now.artist,
                                now.album_art_url.as_deref(), total_ms,
                            );
                            let can_go_next = app.queue_index.map(|i| i + 1 < app.queue_tracks.len()).unwrap_or(false);
                            let can_go_previous = app.queue_index.unwrap_or(0) > 0;
                            mpris::set_track_metadata(&mpris_handle.server, meta, current_ms, can_go_next, can_go_previous).await;
                        }
                        sync_discord_presence(discord_rpc, app);
                    }
                }
                UiEvent::PlaybackPaused => {
                    if let Some(mpris_handle) = mpris.as_mut() {
                        let pos_ms = app.now_playing.as_ref().map(|n| n.current_ms).unwrap_or(0);
                        mpris::set_playback_status(&mpris_handle.server, PlaybackStatus::Paused, pos_ms).await;
                    }
                    app.is_playing = false;
                    sync_discord_presence(discord_rpc, app);
                }
                UiEvent::PlaybackResumed => {
                    if let Some(mpris_handle) = mpris.as_mut() {
                        let pos_ms = app.now_playing.as_ref().map(|n| n.current_ms).unwrap_or(0);
                        mpris::set_playback_status(&mpris_handle.server, PlaybackStatus::Playing, pos_ms).await;
                    }
                    app.is_playing = true;
                    sync_discord_presence(discord_rpc, app);
                }
                UiEvent::PlaybackStopped => {
                    if let Some(current_idx) = app.queue_index {
                        let next_idx = current_idx + 1;
                        if next_idx < app.queue_tracks.len() {
                            app.queue_index = Some(next_idx);
                            app.queue_state.select(Some(next_idx));

                            if let Some((next_track_id, _, _)) = app.queue_tracks.get(next_idx) {
                                app.command_sender
                                    .send(Command::AutoPlayTrack(next_track_id.clone()))
                                    .map_err(|_| anyhow!("failed to send next queued track command"))?;
                                app.is_playing = true;
                                app.status_message = format!(
                                    "Playing queue item {}/{}",
                                    next_idx + 1,
                                    app.queue_tracks.len()
                                );
                            }
                        } else if !app.queue_tracks.is_empty() {
                            match app.repeat_mode {
                                RepeatMode::One => {
                                    if let Some((track_id, _, _)) = app.queue_tracks.get(current_idx) {
                                        app.command_sender
                                            .send(Command::AutoPlayTrack(track_id.clone()))
                                            .map_err(|_| anyhow!("failed to repeat current track"))?;
                                        app.is_playing = true;
                                    }
                                }
                                RepeatMode::All => {
                                    app.queue_index = Some(0);
                                    app.queue_state.select(Some(0));
                                    if let Some((track_id, _, _)) = app.queue_tracks.first() {
                                        app.command_sender
                                            .send(Command::AutoPlayTrack(track_id.clone()))
                                            .map_err(|_| anyhow!("failed to repeat queue"))?;
                                        app.is_playing = true;
                                    }
                                }
                                RepeatMode::Off => {
                                    app.queue_index = None;
                                    app.is_playing = false;
                                    app.status_message = "Queue finished".into();
                                    if let Some(mpris_handle) = mpris.as_mut() {
                                        mpris::set_playback_status(&mpris_handle.server, PlaybackStatus::Stopped, 0).await;
                                    }
                                }
                            }
                        } else {
                            app.queue_index = None;
                            app.is_playing = false;
                            app.status_message = "Queue finished".into();
                            if let Some(mpris_handle) = mpris.as_mut() {
                                mpris::set_playback_status(&mpris_handle.server, PlaybackStatus::Stopped, 0).await;
                            }
                        }
                    } else {
                        app.is_playing = false;
                        if let Some(mpris_handle) = mpris.as_mut() {
                            mpris::set_playback_status(&mpris_handle.server, PlaybackStatus::Stopped, 0).await;
                        }
                    }
                    if !app.is_playing {
                        discord_rpc.clear();
                    }
                }
                UiEvent::Error(message) => {
                    if !message.starts_with("Status:") {
                        app.loading = None;
                    }
                    let is_status_message = message.starts_with("Status:")
                        || message.contains("Loading")
                        || message.contains("Auth")
                        || message.contains("ARL")
                        || message.contains("Client error")
                        || message.contains("API Error")
                        || message.contains("Search");

                    if is_status_message {
                        app.status_message = message;
                        // Keep playback/RPC state intact for informational status updates.
                        continue;
                    }

                    let noisy_terminal_output = message.to_lowercase().contains("underrun")
                        || message.to_lowercase().contains("error.pcm")
                        || message.to_lowercase().contains("alsa");
                    if noisy_terminal_output {
                        // If audio stack printed junk over the TUI, hard redraw once.
                        force_full_redraw = true;
                    }

                    let looks_like_playback_failure = true;

                    if looks_like_playback_failure {
                        let failed_idx = app.queue_index;
                        // Skip failed track and move forward in queue when possible.
                        if let Some(current_idx) = failed_idx {
                            let mut next_idx = current_idx + 1;
                            if next_idx >= app.queue_tracks.len() {
                                if app.repeat_mode == RepeatMode::All && app.queue_tracks.len() > 1 {
                                    next_idx = 0;
                                } else {
                                    app.is_playing = false;
                                    app.status_message = format!("{} | Skipped failed track", message);
                                    if let Some(mpris_handle) = mpris.as_mut() {
                                        mpris::set_playback_status(
                                            &mpris_handle.server,
                                            PlaybackStatus::Stopped,
                                            0,
                                        )
                                        .await;
                                    }
                                    discord_rpc.clear();
                                    continue;
                                }
                            }

                            app.queue_index = Some(next_idx);
                            app.queue_state.select(Some(next_idx));
                            if let Some((track_id, _, _)) = app.queue_tracks.get(next_idx) {
                                app.command_sender
                                    .send(Command::AutoPlayTrack(track_id.clone()))
                                    .map_err(|_| anyhow!("failed to skip to next track"))?;
                                app.is_playing = true;
                                app.status_message = format!(
                                    "{} | Skipped to {}/{}",
                                    message,
                                    next_idx + 1,
                                    app.queue_tracks.len()
                                );
                                continue;
                            }
                        }
                    }

                    app.status_message = message;
                    app.is_playing = false;
                    if let Some(mpris_handle) = mpris.as_mut() {
                        mpris::set_playback_status(&mpris_handle.server, PlaybackStatus::Stopped, 0)
                            .await;
                    }
                    discord_rpc.clear();
                }
                UiEvent::PlaylistsLoaded(playlists) => {
                    app.playlists = playlists;
                    app.status_message = "Playlists loaded!".into();
                    if !app.playlists.is_empty() {
                        app.playlist_state.select(Some(0));
                    }
                }
                UiEvent::TracksLoaded(tracks) => app.show_tracks(tracks),
                UiEvent::MixLoaded(tracks) => {
                    app.show_tracks(tracks);
                    app.play_current_tracks();
                }
                UiEvent::SearchResultsLoaded {
                    tracks,
                    playlists,
                    artists,
                } => {
                    app.loading = None;
                    app.remember_view(&ViewState::Search);
                    app.current_tracks = tracks;
                    app.search_playlists = playlists;
                    app.search_artists = artists;
                    app.showing_search_results = true;
                    app.leave_special_views();
                    app.search_category = SearchCategory::Tracks;
                    app.main_state.select(Some(0));
                    app.viewing_settings = false;
                    app.active_panel = ActivePanel::Main;
                    app.status_message = format!(
                        "Search: {} tracks, {} playlists, {} artists",
                        app.current_tracks.len(),
                        app.search_playlists.len(),
                        app.search_artists.len()
                    );
                }
            }
        }
    }

    if use_true_image_protocol {
        match image_protocol {
            ImageProtocol::Kitty => {
                for slot in 0..IMAGE_SLOTS {
                    let _ = kitty_delete_image(kitty_image_id + slot as u32);
                }
            }
            ImageProtocol::Ueberzugpp => {
                if let Some(layer) = ueberzugpp_layer.take() {
                    layer.shutdown();
                }
            }
            ImageProtocol::None => {}
        }
    }

    Ok(())
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> AnyResult<()> {
    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
        .context("failed to leave alternate screen")?;
    terminal.show_cursor().context("failed to restore cursor")?;
    Ok(())
}

/// Restart the current track at `seek_ms`, keeping it paused if it was paused.
fn seek_to(app: &mut App, now: &NowPlaying, seek_ms: u64) {
    let _ = app.command_sender.send(Command::PlayTrackAt {
        track_id: now.id.clone(),
        quality: now.quality,
        seek_ms,
    });
    if !app.is_playing {
        let _ = app.command_sender.send(Command::Pause);
    }
    if let Some(n) = app.now_playing.as_mut() {
        n.current_ms = seek_ms;
    }
    app.status_message = format!("Seek: {}:{:02}", seek_ms / 60_000, (seek_ms / 1000) % 60);
}

/// Resolve a mouse event against the regions recorded by the last render.
/// Actions that already exist as key handlers are queued as synthetic keys.
fn handle_mouse(
    app: &mut App,
    m: &MouseEvent,
    pending: &mut VecDeque<(KeyCode, KeyModifiers)>,
    last_click: &mut Option<(Instant, Hit)>,
) {
    let inside = |r: ratatui::layout::Rect| {
        m.column >= r.x && m.column < r.x + r.width && m.row >= r.y && m.row < r.y + r.height
    };
    match m.kind {
        MouseEventKind::Moved => {
            // Focus follows the mouse: the panel under the cursor becomes active
            // and the hovered row is selected. The search bar only takes focus on
            // click, otherwise letters typed while hovering it would be swallowed.
            if app.is_searching {
                return;
            }
            let region = app.clicks.iter().rev().find(|c| inside(c.rect)).cloned();
            app.logo_hover = matches!(region.as_ref().map(|r| &r.hit), Some(Hit::Logo));
            app.image_preview = match region.as_ref().map(|r| &r.hit) {
                Some(Hit::Image(url)) => Some(url.clone()),
                _ => None,
            };
            if app.context_menu.is_some() {
                if let Some(Hit::MenuItem(i)) = region.as_ref().map(|r| r.hit.clone()) {
                    if let Some(menu) = app.context_menu.as_mut() {
                        menu.selected = i;
                    }
                }
                return;
            }
            let Some(region) = region else { return };
            match region.hit {
                Hit::Logo | Hit::Image(_) | Hit::MenuItem(_) | Hit::SplitCycle => {}
                Hit::Focus(panel) => app.active_panel = panel,
                Hit::NavItem(i) => {
                    app.active_panel = ActivePanel::Navigation;
                    app.nav_state.select(Some(i));
                }
                Hit::PlaylistItem(i) => {
                    app.active_panel = ActivePanel::Playlists;
                    app.playlist_state.select(Some(i));
                }
                Hit::QueueItem(i) => {
                    app.active_panel = ActivePanel::Queue;
                    app.queue_state.select(Some(i));
                }
                Hit::MainRow { .. } | Hit::TrackAlbum(_) | Hit::TrackArtist(_) => {
                    app.active_panel = ActivePanel::Main;
                    if let Hit::MainRow { index, .. } = region.hit {
                        if app.viewing_settings {
                            app.settings_state.select(Some(index));
                        } else {
                            app.main_state.select(Some(index));
                        }
                    } else if let Some(row) = app
                        .clicks
                        .iter()
                        .rev()
                        .find(|c| inside(c.rect) && matches!(c.hit, Hit::MainRow { .. }))
                    {
                        if let Hit::MainRow { index, .. } = row.hit {
                            app.main_state.select(Some(index));
                        }
                    }
                }
                Hit::ArtistTab(_) | Hit::SearchCategory(_) => app.active_panel = ActivePanel::Main,
                Hit::PlayerButton(i) => {
                    app.active_panel = ActivePanel::Player;
                    app.player_button_index = i;
                }
                Hit::Progress => app.active_panel = ActivePanel::PlayerProgress,
                Hit::VolumeField => {
                    app.active_panel = ActivePanel::PlayerInfo;
                    app.player_info_index = 0;
                }
                Hit::QualityField => {
                    app.active_panel = ActivePanel::PlayerInfo;
                    app.player_info_index = 1;
                }
                Hit::SearchBar => {}
            }
        }
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
            let up = m.kind == MouseEventKind::ScrollUp;
            // Wheel over the volume field: adjust the volume. Over the progress bar: seek.
            let target = app.clicks.iter().rev().find(|c| inside(c.rect)).map(|c| c.hit.clone());
            match target {
                Some(Hit::VolumeField) | Some(Hit::QualityField) => {
                    app.active_panel = ActivePanel::PlayerInfo;
                    app.player_info_index = 0;
                    app.volume = if up { (app.volume + 5).min(100) } else { app.volume.saturating_sub(5) };
                    let _ = app.command_sender.send(Command::SetVolume(app.volume));
                    app.status_message = format!("Volume: {}%", app.volume);
                    return;
                }
                Some(Hit::Progress) => {
                    app.active_panel = ActivePanel::PlayerProgress;
                    if let Some(now) = app.now_playing.as_ref() {
                        if now.quality != AudioQuality::Flac {
                            let seek_ms = if up {
                                (now.current_ms + 5_000).min(now.total_ms.saturating_sub(1))
                            } else {
                                now.current_ms.saturating_sub(5_000)
                            };
                            let now = now.clone();
                            seek_to(app, &now, seek_ms);
                        }
                    }
                    return;
                }
                _ => {}
            }
            let panel = app
                .clicks
                .iter()
                .rev()
                .filter(|c| matches!(c.hit, Hit::Focus(_)))
                .find(|c| inside(c.rect))
                .and_then(|c| if let Hit::Focus(p) = c.hit { Some(p) } else { None });
            if let Some(panel) = panel {
                app.active_panel = panel;
                if m.kind == MouseEventKind::ScrollDown {
                    app.handle_down();
                } else {
                    app.handle_up();
                }
            }
        }
        MouseEventKind::Down(MouseButton::Right) => {
            // Context menu for the track under the pointer (row, title or artist span).
            let track = app
                .clicks
                .iter()
                .rev()
                .filter(|c| inside(c.rect))
                .find_map(|c| match c.hit {
                    Hit::MainRow { index, .. } => app.track_at_main_row(index).map(|t| (t, None)),
                    Hit::QueueItem(i) => app.queue_tracks.get(i).cloned().map(|t| (t, Some(i))),
                    _ => None,
                });
            match track {
                Some((track, queue_pos)) => app.open_track_menu(track, m.column, m.row, queue_pos),
                None => app.context_menu = None,
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if app.context_menu.is_some() {
                // Click on an item activates it; anywhere else closes the menu.
                let item = app.clicks.iter().rev().find(|c| inside(c.rect)).and_then(|c| match c.hit {
                    Hit::MenuItem(i) => Some(i),
                    _ => None,
                });
                match item {
                    Some(i) => {
                        if let Some(menu) = app.context_menu.as_mut() {
                            menu.selected = i;
                        }
                        app.menu_activate();
                    }
                    None => app.context_menu = None,
                }
                return;
            }
            let Some(region) = app.clicks.iter().rev().find(|c| inside(c.rect)).cloned() else {
                return;
            };
            let double = last_click
                .as_ref()
                .map(|(t, h)| t.elapsed() < Duration::from_millis(450) && *h == region.hit)
                .unwrap_or(false);
            *last_click = Some((Instant::now(), region.hit.clone()));
            if !matches!(region.hit, Hit::SearchBar) {
                app.is_searching = false;
            }
            match region.hit {
                Hit::Logo => {
                    app.config.theme = if app.config.theme == Theme::Matugen { Theme::Deezer } else { Theme::Matugen };
                    let _ = save_config(&app.config);
                    app.status_message = format!("Theme: {}", app.config.theme.label());
                }
                Hit::Focus(panel) => app.active_panel = panel,
                Hit::SearchBar => {
                    app.active_panel = ActivePanel::Search;
                    app.is_searching = true;
                }
                Hit::NavItem(i) => {
                    app.active_panel = ActivePanel::Navigation;
                    app.nav_state.select(Some(i));
                    pending.push_back((KeyCode::Enter, KeyModifiers::NONE));
                }
                Hit::PlaylistItem(i) => {
                    app.active_panel = ActivePanel::Playlists;
                    app.playlist_state.select(Some(i));
                    pending.push_back((KeyCode::Enter, KeyModifiers::NONE));
                }
                Hit::QueueItem(i) => {
                    app.active_panel = ActivePanel::Queue;
                    app.queue_state.select(Some(i));
                    pending.push_back((KeyCode::Enter, KeyModifiers::NONE));
                }
                Hit::MainRow { index, opens } => {
                    app.active_panel = ActivePanel::Main;
                    if app.viewing_settings {
                        app.settings_state.select(Some(index));
                        pending.push_back((KeyCode::Enter, KeyModifiers::NONE));
                    } else {
                        let same = app.main_state.selected() == Some(index);
                        app.main_state.select(Some(index));
                        if opens || (double && same) {
                            pending.push_back((KeyCode::Enter, KeyModifiers::NONE));
                        }
                    }
                }
                Hit::ArtistTab(tab) => {
                    app.active_panel = ActivePanel::Main;
                    app.artist_tab = tab;
                    app.rebuild_artist_rows();
                }
                Hit::SearchCategory(cat) => {
                    app.active_panel = ActivePanel::Main;
                    app.search_category = cat;
                    app.main_state.select(Some(0));
                }
                Hit::PlayerButton(i) => {
                    app.active_panel = ActivePanel::Player;
                    app.player_button_index = i;
                    pending.push_back((KeyCode::Enter, KeyModifiers::NONE));
                }
                Hit::Progress => {
                    app.active_panel = ActivePanel::PlayerProgress;
                    if let Some(now) = app.now_playing.clone() {
                        if now.quality == AudioQuality::Flac {
                            app.status_message = "FLAC seek is disabled".into();
                        } else {
                            let ratio = (m.column.saturating_sub(region.rect.x)) as f64
                                / region.rect.width.max(1) as f64;
                            let seek_ms = ((now.total_ms as f64) * ratio.clamp(0.0, 1.0)) as u64;
                            seek_to(app, &now, seek_ms);
                        }
                    }
                }
                Hit::VolumeField => {
                    app.active_panel = ActivePanel::PlayerInfo;
                    app.player_info_index = 0;
                }
                Hit::QualityField => {
                    app.active_panel = ActivePanel::PlayerInfo;
                    app.player_info_index = 1;
                }
                Hit::TrackAlbum(track_id) => {
                    app.status_message = "Opening album...".into();
                    let _ = app.command_sender.send(Command::OpenAlbumOfTrack(track_id));
                }
                Hit::TrackArtist(track_id) => {
                    app.status_message = "Opening artist...".into();
                    let _ = app.command_sender.send(Command::OpenArtistOfTrack(track_id));
                }
                Hit::MenuItem(_) | Hit::Image(_) => {}
                Hit::SplitCycle => {
                    let n = config::SIDEBAR_SPLITS.len();
                    app.config.sidebar_split = (app.config.sidebar_split + 1) % n;
                    let (p_pct, q_pct) = config::SIDEBAR_SPLITS[app.config.sidebar_split];
                    let _ = save_config(&app.config);
                    app.status_message = format!("Playlists {p_pct}% / Queue {q_pct}%");
                }
            }
        }
        _ => {}
    }
}

/// Session-scoped cache of Deezer answers, shared by the API tasks. Pages are
/// immutable enough for a listening session; images are cached in `App`.
#[derive(Default)]
struct ApiCache {
    playlists: HashMap<String, Vec<(String, String, String)>>,
    artists: HashMap<String, ArtistPage>,
    albums: HashMap<String, AlbumPage>,
    /// track id → (album id, artist id)
    track_refs: HashMap<String, (Option<String>, Option<String>)>,
    searches: HashMap<String, (Vec<(String, String, String)>, Vec<(String, String)>, Vec<(String, String)>)>,
    library_albums: Option<Vec<LibraryAlbum>>,
    library_artists: Option<Vec<ArtistRef>>,
}

const FAVORITES_KEY: &str = "__favorites__";

type SharedCache = Arc<std::sync::Mutex<ApiCache>>;

/// One authenticated Deezer client reused by every API task (authenticating
/// used to cost a full round-trip per request), plus the shared cache.
#[derive(Clone)]
struct ApiCtx {
    event_tx: mpsc::UnboundedSender<UiEvent>,
    client: Arc<tokio::sync::Mutex<Option<api::DeezerClient>>>,
    cache: SharedCache,
}

impl ApiCtx {
    fn new(event_tx: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self {
            event_tx,
            client: Arc::new(tokio::sync::Mutex::new(None)),
            cache: Arc::new(std::sync::Mutex::new(ApiCache::default())),
        }
    }

    async fn client(&self) -> AnyResult<api::DeezerClient> {
        let mut guard = self.client.lock().await;
        if guard.is_none() {
            let mut client = api::DeezerClient::new(load_saved_arl()?)?;
            client.fetch_api_token().await?;
            *guard = Some(client);
        }
        Ok(guard.clone().expect("client just initialised"))
    }

    /// Run one API call in the background; the UI shows `label` meanwhile.
    /// On failure the session is dropped so the next call re-authenticates.
    fn run<F, Fut>(&self, label: &str, call: F)
    where
        F: FnOnce(api::DeezerClient, SharedCache) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = AnyResult<UiEvent>> + Send,
    {
        let _ = self.event_tx.send(UiEvent::Loading(Some(label.to_owned())));
        let ctx = self.clone();
        tokio::spawn(async move {
            let result: AnyResult<UiEvent> = async {
                let client = ctx.client().await?;
                call(client, ctx.cache.clone()).await
            }
            .await;
            match result {
                Ok(event) => {
                    let _ = ctx.event_tx.send(event);
                }
                Err(err) => {
                    *ctx.client.lock().await = None;
                    let _ = ctx.event_tx.send(UiEvent::Error(format!("Error: {err}")));
                }
            }
        });
    }
}

async fn fetch_artist_cached(client: &api::DeezerClient, cache: &SharedCache, artist_id: &str) -> AnyResult<ArtistPage> {
    if let Some(page) = cache.lock().unwrap().artists.get(artist_id).cloned() {
        return Ok(page);
    }
    let page = client.fetch_artist_page(artist_id).await?;
    cache.lock().unwrap().artists.insert(artist_id.to_owned(), page.clone());
    Ok(page)
}

async fn fetch_album_cached(client: &api::DeezerClient, cache: &SharedCache, album_id: &str) -> AnyResult<AlbumPage> {
    if let Some(album) = cache.lock().unwrap().albums.get(album_id).cloned() {
        return Ok(album);
    }
    let album = client.fetch_album_page(album_id).await?;
    {
        let mut cache = cache.lock().unwrap();
        for (track_id, _, _) in &album.tracks {
            cache
                .track_refs
                .entry(track_id.clone())
                .or_insert((Some(album.id.clone()), None))
                .0
                .get_or_insert(album.id.clone());
        }
        cache.albums.insert(album_id.to_owned(), album.clone());
    }
    Ok(album)
}

async fn track_refs_cached(
    client: &api::DeezerClient,
    cache: &SharedCache,
    track_id: &str,
) -> AnyResult<(Option<String>, Option<String>)> {
    let known = cache.lock().unwrap().track_refs.get(track_id).cloned();
    if let Some(refs @ (Some(_), Some(_))) = known {
        return Ok(refs);
    }
    let meta = client.fetch_track_metadata(track_id).await?;
    let refs = (meta.album_id.clone(), meta.artist_id.clone());
    cache.lock().unwrap().track_refs.insert(track_id.to_owned(), refs.clone());
    Ok(refs)
}

async fn audio_worker_loop(
    mut command_rx: mpsc::UnboundedReceiver<Command>,
    event_tx: mpsc::UnboundedSender<UiEvent>,
) {
    let api = ApiCtx::new(event_tx.clone());
    let mut active_controls: Option<std::sync::mpsc::Sender<PlayerControl>> = None;
    let mut active_playback_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut current_volume: u16 = 100;
    let mut current_quality: AudioQuality = AudioQuality::Kbps320;
    let mut crossfade_enabled = false;
    let mut crossfade_duration_ms: u64 = 0;

    while let Some(cmd) = command_rx.recv().await {
        if let Some(handle) = active_playback_task.as_ref() {
            if handle.is_finished() {
                active_playback_task.take();
                active_controls = None;
            }
        }

        match cmd {
            Command::PlayTrack(track_id) => {
                if active_playback_task
                    .as_ref()
                    .map(|task| !task.is_finished())
                    .unwrap_or(false)
                {
                    if let Some(control_sender) = active_controls.as_ref() {
                        let _ = control_sender.send(PlayerControl::Stop);
                    }

                    if let Some(handle) = active_playback_task.take() {
                        let _ = handle.await;
                    }

                    active_controls = None;
                }

                let event_tx_for_task = event_tx.clone();
                let (controls_ready_tx, controls_ready_rx) =
                    oneshot::channel::<std::sync::mpsc::Sender<PlayerControl>>();

                let quality_for_task = current_quality;
                let handle = tokio::spawn(async move {
                    if let Err(err) = run_play_track_pipeline(
                        track_id,
                        quality_for_task,
                        0,
                        &event_tx_for_task,
                        controls_ready_tx,
                    ).await {
                        let _ = event_tx_for_task.send(UiEvent::Error(err.to_string()));
                    }
                });
                active_playback_task = Some(handle);

                if let Ok(control_sender) = controls_ready_rx.await {
                    let _ = control_sender.send(PlayerControl::SetVolume(current_volume as f32 / 100.0));
                    active_controls = Some(control_sender);
                }
            }
            Command::AutoPlayTrack(track_id) => {
                let should_crossfade = crossfade_enabled && crossfade_duration_ms > 0;
                if active_playback_task
                    .as_ref()
                    .map(|task| !task.is_finished())
                    .unwrap_or(false)
                {
                    if let Some(control_sender) = active_controls.as_ref() {
                        let _ = if should_crossfade {
                            control_sender.send(PlayerControl::FadeOutStop(crossfade_duration_ms))
                        } else {
                            control_sender.send(PlayerControl::Stop)
                        };
                    }

                    if let Some(handle) = active_playback_task.take() {
                        if should_crossfade {
                            tokio::spawn(async move {
                                let _ = handle.await;
                            });
                        } else {
                            let _ = handle.await;
                        }
                    }

                    active_controls = None;
                }

                let event_tx_for_task = event_tx.clone();
                let (controls_ready_tx, controls_ready_rx) =
                    oneshot::channel::<std::sync::mpsc::Sender<PlayerControl>>();

                let quality_for_task = current_quality;
                let handle = tokio::spawn(async move {
                    if let Err(err) = run_play_track_pipeline(
                        track_id,
                        quality_for_task,
                        0,
                        &event_tx_for_task,
                        controls_ready_tx,
                    ).await {
                        let _ = event_tx_for_task.send(UiEvent::Error(err.to_string()));
                    }
                });
                active_playback_task = Some(handle);

                if let Ok(control_sender) = controls_ready_rx.await {
                    let target_volume = current_volume as f32 / 100.0;
                    if should_crossfade {
                        let _ = control_sender.send(PlayerControl::SetVolume(0.0));
                        let fade_sender = control_sender.clone();
                        let step_count: u64 = 20;
                        let step_ms = (crossfade_duration_ms / step_count).max(1);
                        tokio::spawn(async move {
                            for step in 1..=step_count {
                                tokio::time::sleep(Duration::from_millis(step_ms)).await;
                                let next = target_volume * (step as f32 / step_count as f32);
                                if fade_sender.send(PlayerControl::SetVolume(next)).is_err() {
                                    break;
                                }
                            }
                        });
                    } else {
                        let _ = control_sender.send(PlayerControl::SetVolume(target_volume));
                    }
                    active_controls = Some(control_sender);
                }
            }
            Command::PlayTrackAt {
                track_id,
                quality,
                seek_ms,
            } => {
                if active_playback_task
                    .as_ref()
                    .map(|task| !task.is_finished())
                    .unwrap_or(false)
                {
                    if let Some(control_sender) = active_controls.as_ref() {
                        // PlayTrackAt is used for seek/quality jumps and should feel immediate.
                        let _ = control_sender.send(PlayerControl::Stop);
                    }

                    if let Some(handle) = active_playback_task.take() {
                        let _ = handle.await;
                    }

                    active_controls = None;
                }

                let event_tx_for_task = event_tx.clone();
                let (controls_ready_tx, controls_ready_rx) =
                    oneshot::channel::<std::sync::mpsc::Sender<PlayerControl>>();

                let handle = tokio::spawn(async move {
                    if let Err(err) = run_play_track_pipeline(
                        track_id,
                        quality,
                        seek_ms,
                        &event_tx_for_task,
                        controls_ready_tx,
                    )
                    .await
                    {
                        let _ = event_tx_for_task.send(UiEvent::Error(err.to_string()));
                    }
                });
                active_playback_task = Some(handle);

                if let Ok(control_sender) = controls_ready_rx.await {
                    let _ = control_sender.send(PlayerControl::SetVolume(current_volume as f32 / 100.0));
                    active_controls = Some(control_sender);
                }
            }
            Command::Pause => {
                if let Some(control_sender) = active_controls.as_ref() {
                    let _ = control_sender.send(PlayerControl::Pause);
                    let _ = event_tx.send(UiEvent::PlaybackPaused);
                }
            }
            Command::Resume => {
                if let Some(control_sender) = active_controls.as_ref() {
                    let _ = control_sender.send(PlayerControl::Resume);
                    let _ = event_tx.send(UiEvent::PlaybackResumed);
                }
            }
            Command::SetVolume(volume) => {
                current_volume = volume.min(100);
                if let Some(control_sender) = active_controls.as_ref() {
                    let _ = control_sender.send(PlayerControl::SetVolume(current_volume as f32 / 100.0));
                }
            }
            Command::LoadPlaylist(playlist_id) => {
                let cached = api.cache.lock().unwrap().playlists.get(&playlist_id).cloned();
                match cached {
                    Some(tracks) => {
                        let _ = event_tx.send(UiEvent::TracksLoaded(tracks));
                    }
                    None => api.run("Loading playlist…", move |client, cache| async move {
                        let tracks = match playlist_id.strip_prefix("__mix__:") {
                            Some(track_id) => client.fetch_track_mix(track_id).await?,
                            None => client.fetch_playlist_tracks(&playlist_id).await?,
                        };
                        cache.lock().unwrap().playlists.insert(playlist_id, tracks.clone());
                        Ok(UiEvent::TracksLoaded(tracks))
                    }),
                }
            }
            Command::LoadHome => {
                let cached = api.cache.lock().unwrap().playlists.get("__home__").cloned();
                match cached {
                    Some(tracks) => {
                        let _ = event_tx.send(UiEvent::TracksLoaded(tracks));
                    }
                    None => api.run("Loading Home…", |client, cache| async move {
                        let tracks = client.fetch_home_tracks().await?;
                        cache.lock().unwrap().playlists.insert("__home__".to_owned(), tracks.clone());
                        Ok(UiEvent::TracksLoaded(tracks))
                    }),
                }
            }
            Command::LoadExplore => {
                let cached = api.cache.lock().unwrap().playlists.get("__explore__").cloned();
                match cached {
                    Some(tracks) => {
                        let _ = event_tx.send(UiEvent::TracksLoaded(tracks));
                    }
                    None => api.run("Loading Explore…", |client, cache| async move {
                        let tracks = client.fetch_explore_tracks().await?;
                        cache.lock().unwrap().playlists.insert("__explore__".to_owned(), tracks.clone());
                        Ok(UiEvent::TracksLoaded(tracks))
                    }),
                }
            }
            Command::LoadFavorites => {
                let cached = api.cache.lock().unwrap().playlists.get(FAVORITES_KEY).cloned();
                match cached {
                    Some(tracks) => {
                        let _ = event_tx.send(UiEvent::TracksLoaded(tracks));
                    }
                    None => api.run("Loading favorite tracks…", |client, cache| async move {
                        let tracks = client.fetch_favorite_tracks().await?;
                        cache.lock().unwrap().playlists.insert(FAVORITES_KEY.to_owned(), tracks.clone());
                        Ok(UiEvent::TracksLoaded(tracks))
                    }),
                }
            }
            Command::LoadArtist(artist_id) => {
                let cached = api.cache.lock().unwrap().artists.get(&artist_id).cloned();
                match cached {
                    Some(page) => {
                        let _ = event_tx.send(UiEvent::ArtistLoaded(page));
                    }
                    None => api.run("Loading artist…", move |client, cache| async move {
                        Ok(UiEvent::ArtistLoaded(fetch_artist_cached(&client, &cache, &artist_id).await?))
                    }),
                }
            }
            Command::LoadAlbum(album_id) => {
                let cached = api.cache.lock().unwrap().albums.get(&album_id).cloned();
                match cached {
                    Some(album) => {
                        let _ = event_tx.send(UiEvent::AlbumLoaded(album));
                    }
                    None => api.run("Loading album…", move |client, cache| async move {
                        Ok(UiEvent::AlbumLoaded(fetch_album_cached(&client, &cache, &album_id).await?))
                    }),
                }
            }
            Command::StartMix(track_id) => {
                api.run("Loading mix…", move |client, cache| async move {
                    let tracks = client.fetch_track_mix(&track_id).await?;
                    cache.lock().unwrap().playlists.insert(format!("__mix__:{track_id}"), tracks.clone());
                    Ok(UiEvent::MixLoaded(tracks))
                });
            }
            Command::AddToPlaylist { playlist_id, playlist_title, track_id } => {
                api.run("Adding to playlist…", move |client, cache| async move {
                    client.add_track_to_playlist(&playlist_id, &track_id).await?;
                    cache.lock().unwrap().playlists.remove(&playlist_id);
                    Ok(UiEvent::Error(format!("Added to {playlist_title}")))
                });
            }
            Command::OpenAlbumOfTrack(track_id) => {
                let cached = {
                    let cache = api.cache.lock().unwrap();
                    cache
                        .track_refs
                        .get(&track_id)
                        .and_then(|(album_id, _)| album_id.clone())
                        .and_then(|album_id| cache.albums.get(&album_id).cloned())
                };
                match cached {
                    Some(album) => {
                        let _ = event_tx.send(UiEvent::AlbumLoaded(album));
                    }
                    None => api.run("Opening album…", move |client, cache| async move {
                        let (album_id, _) = track_refs_cached(&client, &cache, &track_id).await?;
                        let album_id = album_id.ok_or_else(|| anyhow!("this track has no album"))?;
                        Ok(UiEvent::AlbumLoaded(fetch_album_cached(&client, &cache, &album_id).await?))
                    }),
                }
            }
            Command::OpenArtistOfTrack(track_id) => {
                let cached = {
                    let cache = api.cache.lock().unwrap();
                    cache
                        .track_refs
                        .get(&track_id)
                        .and_then(|(_, artist_id)| artist_id.clone())
                        .and_then(|artist_id| cache.artists.get(&artist_id).cloned())
                };
                match cached {
                    Some(page) => {
                        let _ = event_tx.send(UiEvent::ArtistLoaded(page));
                    }
                    None => api.run("Opening artist…", move |client, cache| async move {
                        let (_, artist_id) = track_refs_cached(&client, &cache, &track_id).await?;
                        let artist_id = artist_id.ok_or_else(|| anyhow!("this track has no artist"))?;
                        Ok(UiEvent::ArtistLoaded(fetch_artist_cached(&client, &cache, &artist_id).await?))
                    }),
                }
            }
            Command::LoadFavoriteAlbums => {
                let cached = api.cache.lock().unwrap().library_albums.clone();
                match cached {
                    Some(albums) => {
                        let _ = event_tx.send(UiEvent::FavoriteAlbumsLoaded(albums));
                    }
                    None => api.run("Loading your albums…", |client, cache| async move {
                        let user_id = client.user_id().ok_or_else(|| anyhow!("no user id"))?.to_owned();
                        let albums = client.fetch_favorite_albums(&user_id).await?;
                        cache.lock().unwrap().library_albums = Some(albums.clone());
                        Ok(UiEvent::FavoriteAlbumsLoaded(albums))
                    }),
                }
            }
            Command::LoadFavoriteArtists => {
                let cached = api.cache.lock().unwrap().library_artists.clone();
                match cached {
                    Some(artists) => {
                        let _ = event_tx.send(UiEvent::FavoriteArtistsLoaded(artists));
                    }
                    None => api.run("Loading your artists…", |client, cache| async move {
                        let user_id = client.user_id().ok_or_else(|| anyhow!("no user id"))?.to_owned();
                        let artists = client.fetch_favorite_artists(&user_id).await?;
                        cache.lock().unwrap().library_artists = Some(artists.clone());
                        Ok(UiEvent::FavoriteArtistsLoaded(artists))
                    }),
                }
            }
            Command::Search(query) => {
                let key = query.trim().to_lowercase();
                let cached = api.cache.lock().unwrap().searches.get(&key).cloned();
                match cached {
                    Some((tracks, playlists, artists)) => {
                        let _ = event_tx.send(UiEvent::SearchResultsLoaded { tracks, playlists, artists });
                    }
                    None => api.run("Searching…", move |client, cache| async move {
                        let (tracks, playlists, artists) = client.fetch_search_results(&query).await?;
                        cache
                            .lock()
                            .unwrap()
                            .searches
                            .insert(key, (tracks.clone(), playlists.clone(), artists.clone()));
                        Ok(UiEvent::SearchResultsLoaded { tracks, playlists, artists })
                    }),
                }
            }
            Command::SetQuality(quality) => {
                current_quality = quality;
            }
            Command::SetCrossfade {
                enabled,
                duration_ms,
            } => {
                crossfade_enabled = enabled;
                crossfade_duration_ms = duration_ms;
            }
            Command::Shutdown => {
                if let Some(control_sender) = active_controls.as_ref() {
                    let _ = control_sender.send(PlayerControl::Stop);
                }
                if let Some(handle) = active_playback_task.take() {
                    let _ = handle.await;
                }
                active_controls = None;
                break;
            }
            Command::Next
            | Command::Previous
            | Command::ToggleCrossfade => {}
        }
    }

    // If command channel closes without an explicit shutdown command,
    // still stop and join the active playback so process exit is immediate.
    if let Some(control_sender) = active_controls.as_ref() {
        let _ = control_sender.send(PlayerControl::Stop);
    }
    if let Some(handle) = active_playback_task.take() {
        let _ = handle.await;
    }
}

async fn run_play_track_pipeline(
    track_id: String,
    quality: AudioQuality,
    seek_ms: u64,
    event_tx: &mpsc::UnboundedSender<UiEvent>,
    controls_ready_tx: oneshot::Sender<std::sync::mpsc::Sender<PlayerControl>>,
) -> AnyResult<()> {
    use api::DeezerClient;
    use crossbeam_channel::{unbounded, Receiver};
    use crypto::{decrypt_chunk_in_place_with_key, derive_blowfish_key};
    use player::StreamingPlayer;

    const DEEZER_CHUNK_SIZE: usize = 2048;
    const PREBUFFER_BYTES: usize = 512 * 1024;

    fn start_player_thread(
        player_thread: &mut Option<std::thread::JoinHandle<AnyResult<()>>>,
        receiver: &mut Option<Receiver<Vec<u8>>>,
        control_rx: &mut Option<std::sync::mpsc::Receiver<PlayerControl>>,
        event_tx: mpsc::UnboundedSender<UiEvent>,
    ) -> AnyResult<()> {
        if player_thread.is_some() {
            return Ok(());
        }

        let stream_receiver = receiver
            .take()
            .ok_or_else(|| anyhow!("stream receiver already consumed"))?;
        let control_rx = control_rx
            .take()
            .ok_or_else(|| anyhow!("playback control receiver already consumed"))?;

        let handle = std::thread::spawn(move || -> AnyResult<()> {
            let streaming_player = StreamingPlayer::new()?;
            streaming_player.stop();
            streaming_player.play_stream(stream_receiver)?;
            let mut interrupted = false;
            let mut volume = 1.0f32;

            loop {
                while let Ok(control) = control_rx.try_recv() {
                    match control {
                        PlayerControl::Pause => streaming_player.pause(),
                        PlayerControl::Resume => streaming_player.resume(),
                        PlayerControl::SetVolume(next_volume) => {
                            volume = next_volume;
                            streaming_player.set_volume(next_volume)
                        }
                        PlayerControl::FadeOutStop(duration_ms) => {
                            let steps: u64 = 20;
                            let step_ms = (duration_ms / steps).max(1);
                            for step in (0..steps).rev() {
                                let factor = step as f32 / steps as f32;
                                streaming_player.set_volume((volume * factor).clamp(0.0, 1.0));
                                std::thread::sleep(Duration::from_millis(step_ms));
                            }
                            streaming_player.stop();
                            interrupted = true;
                            break;
                        }
                        PlayerControl::Stop => {
                            streaming_player.stop();
                            interrupted = true;
                            break;
                        }
                    }
                }

                if interrupted {
                    break;
                }

                if streaming_player.is_empty() {
                    break;
                }

                std::thread::sleep(Duration::from_millis(100));
            }

            if !interrupted {
                let _ = event_tx.send(UiEvent::PlaybackStopped);
            }
            Ok(())
        });

        *player_thread = Some(handle);
        Ok(())
    }

    let arl = load_saved_arl()?;

    let mut client = DeezerClient::new(arl)?;
    client.fetch_api_token().await?;

    let metadata = client.fetch_track_metadata(&track_id).await?;

    let _ = event_tx.send(UiEvent::TrackChanged {
        id: metadata.id.clone(),
        title: metadata.title.clone(),
        artist: metadata.artist.clone(),
        artist_id: metadata.artist_id.clone(),
        quality,
        album_art_url: metadata.album_art_url.clone(),
        initial_ms: if quality == AudioQuality::Flac { 0 } else { seek_ms },
    });

    let signed_url = client.fetch_media_url(&metadata.track_token, quality).await?;

    let (control_tx, control_rx) = std::sync::mpsc::channel::<PlayerControl>();
    let _ = controls_ready_tx.send(control_tx);
    let mut control_rx = Some(control_rx);

    let (sender, receiver) = unbounded::<Vec<u8>>();
    let mut receiver = Some(receiver);
    let mut player_thread: Option<std::thread::JoinHandle<AnyResult<()>>> = None;
    let mut queued_bytes = 0usize;

    let mut response = client.open_signed_stream(&signed_url).await?;
    let content_length = response.content_length();
    let track_key = derive_blowfish_key(&metadata.id);
    let effective_seek_ms = if quality == AudioQuality::Flac {
        0
    } else {
        seek_ms
    };
    let track_duration_ms = metadata
        .duration_secs
        .map(|s| s * 1000)
        .unwrap_or_else(|| estimate_total_duration_ms(content_length, quality));
    let seek_ms = effective_seek_ms.min(track_duration_ms.saturating_sub(1));
    let seek_target_bytes = content_length
        .map(|total| total.saturating_mul(seek_ms) / track_duration_ms.max(1))
        .unwrap_or(0);
    let mut skipped_bytes: u64 = 0;

    let _ = event_tx.send(UiEvent::PlaybackProgress {
        current_ms: seek_ms,
        total_ms: track_duration_ms,
    });

    let mut pending = Vec::new();
    let mut chunk_index = 0usize;

    while let Some(network_chunk) = response.chunk().await? {
        pending.extend_from_slice(&network_chunk);

        while pending.len() >= DEEZER_CHUNK_SIZE {
            let mut chunk = pending.drain(..DEEZER_CHUNK_SIZE).collect::<Vec<u8>>();
            decrypt_chunk_in_place_with_key(&track_key, chunk_index, &mut chunk)?;

            if skipped_bytes < seek_target_bytes {
                let remaining_skip = (seek_target_bytes - skipped_bytes) as usize;
                if remaining_skip >= chunk.len() {
                    skipped_bytes = skipped_bytes.saturating_add(chunk.len() as u64);
                    chunk_index += 1;
                    continue;
                }

                let kept = chunk.split_off(remaining_skip);
                skipped_bytes = seek_target_bytes;
                chunk = kept;
            }

            queued_bytes += chunk.len();

            if sender.send(chunk).is_err() {
                return Ok(());
            }

            if player_thread.is_none() && queued_bytes >= PREBUFFER_BYTES {
                start_player_thread(
                    &mut player_thread,
                    &mut receiver,
                    &mut control_rx,
                    event_tx.clone(),
                )?;
            }

            chunk_index += 1;
        }
    }

    if !pending.is_empty() {
        let mut tail_chunk = pending;
        decrypt_chunk_in_place_with_key(&track_key, chunk_index, &mut tail_chunk)?;
        if skipped_bytes < seek_target_bytes {
            let remaining_skip = (seek_target_bytes - skipped_bytes) as usize;
            if remaining_skip >= tail_chunk.len() {
                return Ok(());
            }
            tail_chunk = tail_chunk.split_off(remaining_skip);
        }
        if sender.send(tail_chunk).is_err() {
            return Ok(());
        }
    }

    if player_thread.is_none() {
        start_player_thread(
            &mut player_thread,
            &mut receiver,
            &mut control_rx,
            event_tx.clone(),
        )?;
    }

    drop(sender);

    if let Some(handle) = player_thread {
        let join_result = tokio::task::block_in_place(|| handle.join())
            .map_err(|_| anyhow!("player thread panicked"))?;
        join_result?;
    }

    Ok(())
}

fn estimate_total_duration_ms(total_bytes: Option<u64>, quality: AudioQuality) -> u64 {
    let bytes_per_sec: u64 = match quality {
        AudioQuality::Kbps128 => 16_000,
        AudioQuality::Kbps320 => 40_000,
        AudioQuality::Flac => 90_000, // rough estimate for lossless
    };
    total_bytes
        .map(|bytes| bytes.saturating_mul(1000) / bytes_per_sec)
        .unwrap_or(224_000)
        .max(1)
}

fn load_saved_arl() -> AnyResult<String> {
    let arl = load_config().arl;
    let trimmed = arl.trim().to_owned();
    if trimmed.is_empty() {
        return Err(anyhow!("ARL not set. Go to Settings → Set ARL from search input"));
    }
    Ok(trimmed)
}

fn save_arl(arl: &str) -> AnyResult<()> {
    let mut config = load_config();
    config.arl = arl.trim().to_owned();
    save_config(&config)
}

fn prompt_for_arl() -> AnyResult<String> {
    print!("Enter Deezer ARL: ");
    io::stdout().flush().context("failed to flush stdout")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read ARL from stdin")?;

    let arl = input.trim().to_owned();
    if arl.is_empty() {
        return Err(anyhow!("ARL cannot be empty"));
    }

    Ok(arl)
}

async fn verify_arl(arl: &str) -> AnyResult<()> {
    let mut client = api::DeezerClient::new(arl.to_owned())?;
    client
        .fetch_api_token()
        .await
        .context("failed to verify ARL with Deezer")?;
    if client.user_id().is_none() {
        return Err(anyhow!("ARL is invalid: no Deezer user found for this session"));
    }
    Ok(())
}

async fn ensure_verified_arl() -> AnyResult<()> {
    let mut config = load_config();

    loop {
        let existing_arl = config.arl.trim().to_owned();

        if !existing_arl.is_empty() {
            if verify_arl(&existing_arl).await.is_ok() {
                return Ok(());
            }
            println!("Saved ARL is invalid. Please enter a new ARL.");
        } else {
            println!("No ARL found in config. Please enter your Deezer ARL.");
        }

        let entered = prompt_for_arl()?;
        match verify_arl(&entered).await {
            Ok(_) => {
                config.arl = entered;
                save_config(&config)?;
                println!("ARL saved and verified.");
                return Ok(());
            }
            Err(err) => {
                println!("ARL verification failed: {}", err);
            }
        }
    }
}

/// Snapshot the current player state to disk (no-op when nothing has played).
fn persist_state(app: &App) {
    let Some(now) = app.now_playing.as_ref() else { return };
    let snapshot = PlaybackState::capture(
        &now.id,
        &now.title,
        &now.artist,
        now.current_ms,
        now.total_ms,
        app.is_playing,
        app.volume,
        &app.queue_tracks,
        app.queue_index,
        app.current_playlist_id.as_deref(),
    );
    let _ = state::save_state(&snapshot);
}

/// Decide what to resume on launch: the local snapshot, or the most recent
/// Deezer listen when something was played elsewhere after that snapshot.
/// Runs once, when the startup task reports the listening history.
fn apply_resume(app: &mut App, history: &[HistoryEntry]) {
    if app.resume_done {
        return;
    }
    app.resume_done = true;

    // Don't interrupt anything the user already started by hand.
    if app.now_playing.is_some() {
        return;
    }

    let local = app.saved_state.take();
    let remote = history.first();

    // Deezer only reports plays from other clients (this TUI does not log
    // listens), so a remote timestamp newer than the snapshot means the user
    // moved on elsewhere. A small margin absorbs clock skew.
    let remote_is_newer = match (&local, remote) {
        (Some(local), Some(remote)) => remote.ts > local.saved_at.saturating_add(30),
        (None, Some(_)) => true,
        (_, None) => false,
    };

    let quality = app.config.default_quality;

    if remote_is_newer {
        let remote = remote.expect("checked above");
        app.queue_tracks = vec![(remote.id.clone(), remote.title.clone(), remote.artist.clone())];
        app.queue = vec![format!("{} - {}", remote.title, remote.artist)];
        app.queue_index = Some(0);
        app.queue_state.select(Some(0));
        if let Some(local) = local.as_ref() {
            app.volume = local.volume.min(100);
            let _ = app.command_sender.send(Command::SetVolume(app.volume));
        }
        let _ = app.command_sender.send(Command::PlayTrackAt {
            track_id: remote.id.clone(),
            quality,
            seek_ms: 0,
        });
        // Position is unknown for plays made elsewhere: start paused at 0.
        let _ = app.command_sender.send(Command::Pause);
        app.status_message = format!("Resumed from Deezer: {} - {} (p to play)", remote.title, remote.artist);
        return;
    }

    let Some(local) = local else { return };

    app.volume = local.volume.min(100);
    let _ = app.command_sender.send(Command::SetVolume(app.volume));

    if !local.queue_tracks.is_empty() {
        app.queue = local
            .queue_tracks
            .iter()
            .map(|(_, title, artist)| format!("{} - {}", title, artist))
            .collect();
        app.queue_tracks = local.queue_tracks.clone();
        let idx = local
            .queue_index
            .filter(|i| *i < app.queue_tracks.len())
            .or_else(|| app.queue_tracks.iter().position(|(id, _, _)| *id == local.track_id));
        app.queue_index = idx;
        app.queue_state.select(idx);
    } else {
        app.queue_tracks = vec![(local.track_id.clone(), local.title.clone(), local.artist.clone())];
        app.queue = vec![format!("{} - {}", local.title, local.artist)];
        app.queue_index = Some(0);
        app.queue_state.select(Some(0));
    }

    // Re-open the view the track was launched from so the main panel matches.
    if let Some(source) = local.current_playlist_id.as_deref() {
        let cmd = match source {
            "__home__" => Some(Command::LoadHome),
            "__explore__" => Some(Command::LoadExplore),
            s if s.starts_with("__album__:") => Some(Command::LoadAlbum(s["__album__:".len()..].to_owned())),
            s if s.starts_with("__artist__:") => Some(Command::LoadArtist(s["__artist__:".len()..].to_owned())),
            id if !id.is_empty() => Some(Command::LoadPlaylist(id.to_owned())),
            _ => None,
        };
        if let Some(cmd) = cmd {
            app.current_playlist_id = Some(source.to_owned());
            let _ = app.command_sender.send(cmd);
        }
    }

    let _ = app.command_sender.send(Command::PlayTrackAt {
        track_id: local.track_id.clone(),
        quality,
        seek_ms: local.position_ms,
    });
    if !local.was_playing {
        let _ = app.command_sender.send(Command::Pause);
    }
    app.status_message = format!(
        "Resumed: {} - {} at {}:{:02}{}",
        local.title,
        local.artist,
        local.position_ms / 60_000,
        (local.position_ms / 1000) % 60,
        if local.was_playing { "" } else { " (paused, p to play)" }
    );
}

fn config_file_path() -> AnyResult<PathBuf> {
    dirs::home_dir()
        .map(|path| path.join(".deezer-tui-config.json"))
        .ok_or_else(|| anyhow!("could not resolve the user's home directory"))
}

/// `verbose`: report "up to date" and errors (manual check); startup only reports a new release.
fn spawn_update_check(tx: mpsc::UnboundedSender<UiEvent>, verbose: bool) {
    tokio::spawn(async move {
        match update::check().await {
            Ok(release) if verbose || release.is_some() => {
                let _ = tx.send(UiEvent::UpdateChecked(release));
            }
            Err(err) if verbose => {
                let _ = tx.send(UiEvent::Error(format!("Status: Update check failed: {err:#}")));
            }
            _ => {}
        }
    });
}

fn load_config() -> Config {
    let Ok(path) = config_file_path() else { return Config::default() };
    let Ok(data) = fs::read_to_string(&path) else { return Config::default() };
    serde_json::from_str(&data).unwrap_or_default()
}

fn save_config(config: &Config) -> AnyResult<()> {
    let path = config_file_path()?;
    let data = serde_json::to_string_pretty(config).context("failed to serialize config")?;
    fs::write(&path, data)
        .with_context(|| format!("failed to write config file at {}", path.display()))
}
