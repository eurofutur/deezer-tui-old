//! Persisted playback state so the TUI can resume where the user left off.

use std::{fs, path::PathBuf, time::{SystemTime, UNIX_EPOCH}};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// Position within the last two seconds of a track counts as "finished".
const END_TOLERANCE_MS: u64 = 2_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackState {
    /// Unix timestamp (seconds) when this snapshot was written.
    pub saved_at: u64,
    pub track_id: String,
    pub title: String,
    pub artist: String,
    pub position_ms: u64,
    pub was_playing: bool,
    #[serde(default = "default_volume")]
    pub volume: u16,
    #[serde(default)]
    pub queue_tracks: Vec<(String, String, String)>,
    #[serde(default)]
    pub queue_index: Option<usize>,
    #[serde(default)]
    pub current_playlist_id: Option<String>,
}

fn default_volume() -> u16 {
    100
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn state_file_path() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|path| path.join(".deezer-tui-state.json"))
        .ok_or_else(|| anyhow!("could not resolve the user's home directory"))
}

pub fn load_state() -> Option<PlaybackState> {
    let path = state_file_path().ok()?;
    let data = fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn save_state(state: &PlaybackState) -> Result<()> {
    let path = state_file_path()?;
    let data = serde_json::to_string_pretty(state).context("failed to serialize playback state")?;
    fs::write(&path, data)
        .with_context(|| format!("failed to write state file at {}", path.display()))
}

impl PlaybackState {
    /// Build a snapshot from the live player values. Returns None when nothing
    /// is worth remembering (no track has been played yet).
    #[allow(clippy::too_many_arguments)]
    pub fn capture(
        track_id: &str,
        title: &str,
        artist: &str,
        current_ms: u64,
        total_ms: u64,
        is_playing: bool,
        volume: u16,
        queue_tracks: &[(String, String, String)],
        queue_index: Option<usize>,
        current_playlist_id: Option<&str>,
    ) -> Self {
        // A track that ran to its end should restart from the top, paused.
        let finished = total_ms > 0 && current_ms + END_TOLERANCE_MS >= total_ms;
        Self {
            saved_at: now_unix(),
            track_id: track_id.to_owned(),
            title: title.to_owned(),
            artist: artist.to_owned(),
            position_ms: if finished { 0 } else { current_ms },
            was_playing: is_playing && !finished,
            volume,
            queue_tracks: queue_tracks.to_vec(),
            queue_index,
            current_playlist_id: current_playlist_id.map(str::to_owned),
        }
    }
}
