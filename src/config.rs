use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    Deezer,
    /// Colours from the DankMaterialShell / matugen wallpaper palette.
    Matugen,
    SpotifyDark,
    NcmpcppBlue,
}

impl Theme {
    pub fn next(self) -> Self {
        match self {
            Theme::Deezer => Theme::Matugen,
            Theme::Matugen => Theme::SpotifyDark,
            Theme::SpotifyDark => Theme::NcmpcppBlue,
            Theme::NcmpcppBlue => Theme::Deezer,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Theme::Deezer => "Deezer",
            Theme::Matugen => "Matugen",
            Theme::SpotifyDark => "Spotify Dark",
            Theme::NcmpcppBlue => "ncmpcpp Blue",
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioQuality {
    Kbps128,
    Kbps320,
    Flac,
}

impl AudioQuality {
    pub const fn format_code(self) -> u8 {
        match self {
            Self::Kbps128 => 1,
            Self::Kbps320 => 3,
            Self::Flac => 9,
        }
    }

    pub const fn from_format_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Kbps128),
            3 => Some(Self::Kbps320),
            9 => Some(Self::Flac),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub theme: Theme,
    pub crossfade_enabled: bool,
    pub crossfade_duration_ms: u64,
    pub default_quality: AudioQuality,
    #[serde(default)]
    pub discord_rpc_enabled: bool,
    #[serde(default)]
    pub arl: String,
    /// Restore the last track / queue (local snapshot or Deezer history) on launch.
    #[serde(default = "default_true")]
    pub resume_on_start: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: Theme::Deezer,
            crossfade_enabled: false,
            crossfade_duration_ms: 0,
            default_quality: AudioQuality::Kbps320,
            discord_rpc_enabled: false,
            arl: String::new(),
            resume_on_start: true,
        }
    }
}
