use std::collections::{HashMap, HashSet};
use std::time::Instant;

use ratatui::widgets::ListState;
use tokio::sync::mpsc;

use crate::api::{AlbumPage, ArtistPage, ArtistRef, HistoryEntry, LibraryAlbum};
use crate::config::{AudioQuality, Config};
use crate::state::PlaybackState;
use crate::theme::MatugenColors;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    PlayTrack(String),
    AutoPlayTrack(String),
    PlayTrackAt {
        track_id: String,
        quality: AudioQuality,
        seek_ms: u64,
    },
    Pause,
    Resume,
    Next,
    Previous,
    SetVolume(u16),
    SetQuality(AudioQuality),
    SetCrossfade { enabled: bool, duration_ms: u64 },
    ToggleCrossfade,
    LoadPlaylist(String),
    LoadHome,
    LoadExplore,
    LoadFavorites,
    LoadFavoriteAlbums,
    LoadFavoriteArtists,
    LoadArtist(String),
    LoadAlbum(String),
    AddToPlaylist {
        playlist_id: String,
        playlist_title: String,
        track_id: String,
    },
    /// Resolve a track's album / artist from its metadata, then open it.
    OpenAlbumOfTrack(String),
    OpenArtistOfTrack(String),
    Search(String),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    PlaybackProgress {
        current_ms: u64,
        total_ms: u64,
    },
    TrackChanged {
        id: String,
        title: String,
        artist: String,
        artist_id: Option<String>,
        quality: AudioQuality,
        album_art_url: Option<String>,
        initial_ms: u64,
    },
    PlaybackPaused,
    PlaybackResumed,
    PlaybackStopped,
    Error(String),
    PlaylistsLoaded(Vec<(String, String)>),
    /// Playlists the user owns (targets of "Add to playlist").
    OwnPlaylistsLoaded(Vec<(String, String)>),
    TracksLoaded(Vec<(String, String, String)>),
    SearchResultsLoaded {
        tracks: Vec<(String, String, String)>,
        playlists: Vec<(String, String)>,
        artists: Vec<(String, String)>,
    },
    /// Listening history fetched at startup (empty when unavailable).
    HistoryLoaded(Vec<HistoryEntry>),
    /// A background request started (Some(label)) or finished (None).
    Loading(Option<String>),
    ArtistLoaded(ArtistPage),
    AlbumLoaded(AlbumPage),
    FavoriteAlbumsLoaded(Vec<LibraryAlbum>),
    FavoriteArtistsLoaded(Vec<ArtistRef>),
}

/// Tabs of the artist page, in Deezer's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtistTab {
    Discography,
    TopTracks,
    Similar,
    Playlists,
    Bio,
}

impl ArtistTab {
    pub const ALL: [ArtistTab; 5] = [
        ArtistTab::Discography,
        ArtistTab::TopTracks,
        ArtistTab::Similar,
        ArtistTab::Playlists,
        ArtistTab::Bio,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ArtistTab::Discography => "Discography",
            ArtistTab::TopTracks => "Top tracks",
            ArtistTab::Similar => "Similar artists",
            ArtistTab::Playlists => "Playlists",
            ArtistTab::Bio => "Bio",
        }
    }

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// One selectable (or header) line of the artist page body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtistRow {
    PlayAll,
    Header(&'static str),
    Track(usize),
    Album(usize),
    Related(usize),
    Playlist(usize),
}

impl ArtistRow {
    pub fn selectable(&self) -> bool {
        !matches!(self, ArtistRow::Header(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryView {
    Albums,
    Artists,
}

/// What the main panel shows; kept in a history so Esc goes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewState {
    /// A track list: Home, Explore, favorites or a playlist (its source id).
    Tracks(Option<String>),
    Album(String),
    Artist(String),
    Library(LibraryView),
    Search,
    Settings,
}

/// What a mouse click on a screen region means. Regions are recorded by the
/// renderer each frame (`App::clicks`) and resolved in the event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    /// The DEEZER wordmark: hover highlights it, click toggles Matugen / Deezer.
    Logo,
    Focus(ActivePanel),
    SearchBar,
    NavItem(usize),
    PlaylistItem(usize),
    QueueItem(usize),
    /// A row of the main list. `opens` rows act on a single click (albums,
    /// artists, playlists, settings); other rows select on click and act on
    /// double-click (tracks).
    MainRow { index: usize, opens: bool },
    ArtistTab(ArtistTab),
    SearchCategory(SearchCategory),
    PlayerButton(usize),
    Progress,
    VolumeField,
    QualityField,
    /// Title of a track: opens its album. Artist name: opens the artist page.
    TrackAlbum(String),
    TrackArtist(String),
    /// A row of the open context menu.
    MenuItem(usize),
    /// An artwork; hovering shows a large preview (url of the full-size image).
    Image(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuPage {
    Main,
    Playlists,
}

/// Right-click / `m` menu for a track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMenu {
    pub track: (String, String, String),
    pub x: u16,
    pub y: u16,
    pub selected: usize,
    pub page: MenuPage,
}

/// Full-size variant of a Deezer image url.
pub fn full_size_url(url: &str) -> String {
    url.replace("250x250", "500x500")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickRegion {
    pub rect: ratatui::layout::Rect,
    pub hit: Hit,
}

/// "23 159" style thousands grouping, as on Deezer's pages.
pub fn fmt_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push('\u{202f}');
        }
        out.push(ch);
    }
    out
}

pub fn fmt_duration(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchCategory {
    Tracks,
    Playlists,
    Artists,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatMode {
    Off,
    All,
    One,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Library,
    Search,
    Queue,
    Lyrics,
    Settings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowPlaying {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub artist_id: Option<String>,
    pub quality: AudioQuality,
    pub current_ms: u64,
    pub total_ms: u64,
    pub album_art_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivePanel {
    Navigation,
    Playlists,
    Queue,
    Search,
    Main,
    Player,
    PlayerProgress,
    PlayerInfo,
    /// Right-hand shortcuts panel (scrollable).
    Shortcuts,
}

#[derive(Debug, Clone)]
pub struct App {
    pub config: Config,
    pub command_sender: mpsc::UnboundedSender<Command>,
    pub current_route: Route,
    pub now_playing: Option<NowPlaying>,
    pub is_playing: bool,
    pub volume: u16,
    pub discord_rpc_enabled: bool,
    pub active_panel: ActivePanel,
    pub nav_state: ListState,
    pub playlist_state: ListState,
    pub queue_state: ListState,
    pub playlists: Vec<(String, String)>,
    pub queue: Vec<String>,
    pub queue_tracks: Vec<(String, String, String)>,
    pub queue_index: Option<usize>,
    pub current_tracks: Vec<(String, String, String)>,
    pub search_playlists: Vec<(String, String)>,
    pub search_artists: Vec<(String, String)>,
    pub showing_search_results: bool,
    pub search_category: SearchCategory,
    pub main_state: ListState,
    pub settings_state: ListState,
    pub player_button_index: usize,
    pub player_info_index: usize,
    pub repeat_mode: RepeatMode,
    pub viewing_settings: bool,
    pub current_playlist_id: Option<String>,
    pub status_message: String,
    pub is_searching: bool,
    pub search_query: String,
    pub auto_transition_armed: bool,
    /// Decoded cover-art image for the current track (None until downloaded).
    pub cover_art: Option<image::DynamicImage>,
    pub cover_art_png: Option<Vec<u8>>,
    pub cover_art_track_id: Option<String>,
    /// Snapshot from the previous session, consumed once resume has run.
    pub saved_state: Option<PlaybackState>,
    pub resume_done: bool,
    /// Artist page (Deezer-style) shown in the main panel when `viewing_artist`.
    pub artist_page: Option<ArtistPage>,
    pub artist_tab: ArtistTab,
    pub artist_rows: Vec<ArtistRow>,
    pub viewing_artist: bool,
    /// Library pages (Albums / Artists) shown in the main panel.
    pub library_view: Option<LibraryView>,
    pub library_albums: Vec<LibraryAlbum>,
    pub library_artists: Vec<ArtistRef>,
    /// Decoded thumbnails keyed by URL, shared by the artist page and the detail card.
    pub images: HashMap<String, image::DynamicImage>,
    pub pending_images: HashSet<String>,
    /// Album currently listed in the main panel (id, title, artist), if any.
    pub current_album: Option<(String, String, String)>,
    /// Clickable regions of the last rendered frame.
    pub clicks: Vec<ClickRegion>,
    /// Request in flight, shown as a popup after a short delay.
    pub loading: Option<(String, Instant)>,
    /// Scroll offset / limit of the shortcuts panel, and whether it is shown.
    pub shortcuts_scroll: u16,
    pub shortcuts_max_scroll: u16,
    pub right_panel_visible: bool,
    pub own_playlists: Vec<(String, String)>,
    pub context_menu: Option<ContextMenu>,
    /// Full-size image url being previewed (hover on an artwork).
    pub image_preview: Option<String>,
    /// Wallpaper palette (None until DMS has written it).
    pub matugen: Option<MatugenColors>,
    pub logo_hover: bool,
    /// Previous views, most recent last (Esc pops one).
    pub history: Vec<ViewState>,
    /// Set while restoring a view from the history so it is not pushed again.
    pub navigating_back: bool,
}

impl App {
    pub fn new(config: Config, command_sender: mpsc::UnboundedSender<Command>) -> Self {
        let mut nav_state = ListState::default();
        nav_state.select(Some(0));

        let mut playlist_state = ListState::default();
        playlist_state.select(Some(0));

        let mut queue_state = ListState::default();
        queue_state.select(Some(0));

        let mut main_state = ListState::default();
        main_state.select(Some(0));

        let mut settings_state = ListState::default();
        settings_state.select(Some(0));

        let discord_rpc_enabled = config.discord_rpc_enabled;

        Self {
            volume: 100,
            config,
            command_sender,
            current_route: Route::Library,
            now_playing: None,
            is_playing: false,
            discord_rpc_enabled,
            active_panel: ActivePanel::Navigation,
            nav_state,
            playlist_state,
            queue_state,
            playlists: vec![],
            queue: vec![],
            queue_tracks: vec![],
            queue_index: None,
            current_tracks: vec![],
            search_playlists: vec![],
            search_artists: vec![],
            showing_search_results: false,
            search_category: SearchCategory::Tracks,
            main_state,
            settings_state,
            player_button_index: 2,
            player_info_index: 0,
            repeat_mode: RepeatMode::Off,
            viewing_settings: false,
            current_playlist_id: None,
            status_message: "Status: Waiting...".into(),
            is_searching: false,
            search_query: String::new(),
            auto_transition_armed: false,
            cover_art: None,
            cover_art_png: None,
            cover_art_track_id: None,
            saved_state: None,
            resume_done: false,
            artist_page: None,
            artist_tab: ArtistTab::Discography,
            artist_rows: vec![],
            viewing_artist: false,
            library_view: None,
            library_albums: vec![],
            library_artists: vec![],
            images: HashMap::new(),
            pending_images: HashSet::new(),
            current_album: None,
            clicks: Vec::new(),
            loading: None,
            shortcuts_scroll: 0,
            shortcuts_max_scroll: 0,
            right_panel_visible: true,
            own_playlists: vec![],
            context_menu: None,
            image_preview: None,
            matugen: None,
            logo_hover: false,
            history: Vec::new(),
            navigating_back: false,
        }
    }

    pub fn handle_down(&mut self) {
        match self.active_panel {
            ActivePanel::Navigation => {
                let current = self.nav_state.selected().unwrap_or(0);
                if current >= 6 {
                    self.active_panel = ActivePanel::Playlists;
                    if !self.playlists.is_empty() {
                        self.playlist_state.select(Some(0));
                    }
                } else {
                    self.nav_state.select(Some(current + 1));
                }
            }
            ActivePanel::Playlists => {
                if self.playlists.is_empty() {
                    self.active_panel = ActivePanel::Queue;
                    if !self.queue.is_empty() {
                        self.queue_state.select(Some(0));
                    }
                    return;
                }

                let max = self.playlists.len() - 1;
                let current = self.playlist_state.selected().unwrap_or(0);
                if current >= max {
                    self.active_panel = ActivePanel::Queue;
                    if !self.queue.is_empty() {
                        self.queue_state.select(Some(0));
                    }
                } else {
                    self.playlist_state.select(Some(current + 1));
                }
            }
            ActivePanel::Queue => {
                if self.queue.is_empty() {
                    return;
                }

                let max = self.queue.len() - 1;
                let current = self.queue_state.selected().unwrap_or(0);
                self.queue_state.select(Some((current + 1).min(max)));
            }
            ActivePanel::Search => {
                self.active_panel = ActivePanel::Main;
            }
            ActivePanel::Main => {
                if self.viewing_artist {
                    let current = self.main_state.selected().unwrap_or(0);
                    if let Some(next) = self
                        .artist_rows
                        .iter()
                        .enumerate()
                        .skip(current + 1)
                        .find(|(_, row)| row.selectable())
                        .map(|(i, _)| i)
                    {
                        self.main_state.select(Some(next));
                    }
                    return;
                }
                if let Some(view) = self.library_view {
                    let len = match view {
                        LibraryView::Albums => self.library_albums.len(),
                        LibraryView::Artists => self.library_artists.len(),
                    };
                    if len > 0 {
                        let current = self.main_state.selected().unwrap_or(0);
                        self.main_state.select(Some((current + 1).min(len - 1)));
                    }
                    return;
                }
                if self.viewing_settings {
                    let max = 5usize;
                    let current = self.settings_state.selected().unwrap_or(0);
                    self.settings_state.select(Some((current + 1).min(max)));
                } else if self.showing_search_results {
                    let max = match self.search_category {
                        SearchCategory::Tracks => self.current_tracks.len(),
                        SearchCategory::Playlists => self.search_playlists.len().saturating_sub(1),
                        SearchCategory::Artists => self.search_artists.len().saturating_sub(1),
                    };
                    let current = self.main_state.selected().unwrap_or(0);
                    self.main_state.select(Some((current + 1).min(max)));
                } else if !self.current_tracks.is_empty() {
                    // +1 for the top action row: "Play Playlist"
                    let max = self.current_tracks.len();
                    let current = self.main_state.selected().unwrap_or(0);
                    self.main_state.select(Some((current + 1).min(max)));
                } else {
                    self.active_panel = ActivePanel::Player;
                }
            }
            ActivePanel::Player => {
                self.active_panel = ActivePanel::PlayerProgress;
            }
            ActivePanel::PlayerProgress => {}
            ActivePanel::PlayerInfo => {
                self.player_info_index = (self.player_info_index + 1).min(1);
            }
            ActivePanel::Shortcuts => {
                self.shortcuts_scroll = (self.shortcuts_scroll + 1).min(self.shortcuts_max_scroll);
            }
        }
    }

    pub fn handle_up(&mut self) {
        match self.active_panel {
            ActivePanel::Queue => {
                if self.queue.is_empty() {
                    self.active_panel = ActivePanel::Playlists;
                    if !self.playlists.is_empty() {
                        self.playlist_state.select(Some(self.playlists.len() - 1));
                    }
                    return;
                }

                let current = self.queue_state.selected().unwrap_or(0);
                if current == 0 {
                    self.active_panel = ActivePanel::Playlists;
                    if !self.playlists.is_empty() {
                        self.playlist_state.select(Some(self.playlists.len() - 1));
                    }
                } else {
                    self.queue_state.select(Some(current - 1));
                }
            }
            ActivePanel::Playlists => {
                if self.playlists.is_empty() {
                    self.active_panel = ActivePanel::Navigation;
                    self.nav_state.select(Some(6));
                    return;
                }

                let current = self.playlist_state.selected().unwrap_or(0);
                if current == 0 {
                    self.active_panel = ActivePanel::Navigation;
                    self.nav_state.select(Some(6));
                } else {
                    self.playlist_state.select(Some(current - 1));
                }
            }
            ActivePanel::Navigation => {
                let current = self.nav_state.selected().unwrap_or(0);
                self.nav_state.select(Some(current.saturating_sub(1)));
            }
            ActivePanel::Search => {
                self.active_panel = ActivePanel::Navigation;
            }
            ActivePanel::Main => {
                if self.viewing_artist {
                    let current = self.main_state.selected().unwrap_or(0);
                    match self.artist_rows[..current.min(self.artist_rows.len())]
                        .iter()
                        .rposition(|row| row.selectable())
                    {
                        Some(prev) => self.main_state.select(Some(prev)),
                        None => self.active_panel = ActivePanel::Search,
                    }
                    return;
                }
                if self.library_view.is_some() {
                    let current = self.main_state.selected().unwrap_or(0);
                    if current == 0 {
                        self.active_panel = ActivePanel::Search;
                    } else {
                        self.main_state.select(Some(current - 1));
                    }
                    return;
                }
                if self.viewing_settings {
                    let current = self.settings_state.selected().unwrap_or(0);
                    self.settings_state.select(Some(current.saturating_sub(1)));
                } else if !self.current_tracks.is_empty() {
                    let current = self.main_state.selected().unwrap_or(0);
                    if current == 0 {
                        self.active_panel = ActivePanel::Search;
                    } else {
                        self.main_state.select(Some(current.saturating_sub(1)));
                    }
                }
            }
            ActivePanel::Player => {
                self.active_panel = ActivePanel::Main;
            }
            ActivePanel::PlayerProgress => {
                self.active_panel = ActivePanel::Player;
            }
            ActivePanel::PlayerInfo => {
                self.active_panel = ActivePanel::Player;
            }
            ActivePanel::Shortcuts => {
                self.shortcuts_scroll = self.shortcuts_scroll.saturating_sub(1);
            }
        }
    }

    pub fn handle_right(&mut self) {
        match self.active_panel {
            ActivePanel::Navigation | ActivePanel::Playlists | ActivePanel::Queue => {
                self.active_panel = ActivePanel::Search;
            }
            ActivePanel::Search => {
                self.active_panel = ActivePanel::Main;
            }
            ActivePanel::Main => {
                self.active_panel = ActivePanel::Player;
            }
            ActivePanel::Player => {
                if self.player_button_index < 4 {
                    self.player_button_index += 1;
                } else {
                    self.active_panel = ActivePanel::PlayerInfo;
                }
            }
            ActivePanel::PlayerProgress => {
                self.active_panel = ActivePanel::PlayerInfo;
            }
            ActivePanel::Shortcuts => {}
            ActivePanel::PlayerInfo => {
                self.player_info_index = (self.player_info_index + 1).min(1);
            }
        }
    }

    pub fn handle_left(&mut self) {
        match self.active_panel {
            ActivePanel::Main => {
                if self.viewing_artist {
                    self.artist_tab_prev();
                } else if self.showing_search_results {
                    self.search_category = match self.search_category {
                        SearchCategory::Tracks => SearchCategory::Artists,
                        SearchCategory::Playlists => SearchCategory::Tracks,
                        SearchCategory::Artists => SearchCategory::Playlists,
                    };
                    self.main_state.select(Some(0));
                } else {
                    self.active_panel = ActivePanel::Playlists;
                }
            }
            ActivePanel::Player => {
                self.player_button_index = self.player_button_index.saturating_sub(1);
            }
            ActivePanel::PlayerProgress => {
                self.active_panel = ActivePanel::Player;
            }
            ActivePanel::PlayerInfo => {
                if self.player_info_index == 0 {
                    self.active_panel = ActivePanel::Player;
                    self.player_button_index = 4;
                } else {
                    self.player_info_index -= 1;
                }
            }
            _ => {}
        }
    }

    pub fn current_view(&self) -> ViewState {
        if self.viewing_settings {
            ViewState::Settings
        } else if self.viewing_artist {
            ViewState::Artist(self.artist_page.as_ref().map(|p| p.id.clone()).unwrap_or_default())
        } else if let Some(view) = self.library_view {
            ViewState::Library(view)
        } else if self.showing_search_results {
            ViewState::Search
        } else if let Some(id) = self.current_playlist_id.as_deref().and_then(|s| s.strip_prefix("__album__:")) {
            ViewState::Album(id.to_owned())
        } else {
            ViewState::Tracks(self.current_playlist_id.clone())
        }
    }

    /// Push the current view onto the history before switching to `next`.
    pub fn remember_view(&mut self, next: &ViewState) {
        if self.navigating_back {
            self.navigating_back = false;
            return;
        }
        let current = self.current_view();
        if current == *next || matches!(current, ViewState::Tracks(None)) && self.current_tracks.is_empty() {
            return;
        }
        if self.history.last() != Some(&current) {
            self.history.push(current);
            if self.history.len() > 40 {
                self.history.remove(0);
            }
        }
    }

    /// Leave the artist / library pages (used when another view loads).
    pub fn leave_special_views(&mut self) {
        self.viewing_artist = false;
        self.library_view = None;
    }

    pub fn show_artist(&mut self, page: ArtistPage) {
        self.artist_page = Some(page);
        self.artist_tab = ArtistTab::Discography;
        self.viewing_artist = true;
        self.library_view = None;
        self.viewing_settings = false;
        self.showing_search_results = false;
        self.rebuild_artist_rows();
        self.active_panel = ActivePanel::Main;
    }

    pub fn show_library(&mut self, view: LibraryView) {
        self.library_view = Some(view);
        self.viewing_artist = false;
        self.viewing_settings = false;
        self.showing_search_results = false;
        self.main_state.select(Some(0));
        self.active_panel = ActivePanel::Main;
    }

    pub fn rebuild_artist_rows(&mut self) {
        let Some(page) = self.artist_page.as_ref() else {
            self.artist_rows.clear();
            return;
        };
        let mut rows = Vec::new();
        match self.artist_tab {
            ArtistTab::Discography => {
                if !page.top_tracks.is_empty() {
                    rows.push(ArtistRow::Header("Top tracks"));
                    rows.extend((0..page.top_tracks.len().min(5)).map(ArtistRow::Track));
                }
                if !page.albums.is_empty() {
                    rows.push(ArtistRow::Header("Discography"));
                    rows.extend((0..page.albums.len()).map(ArtistRow::Album));
                }
            }
            ArtistTab::TopTracks => {
                if !page.top_tracks.is_empty() {
                    rows.push(ArtistRow::PlayAll);
                    rows.extend((0..page.top_tracks.len()).map(ArtistRow::Track));
                }
            }
            ArtistTab::Similar => rows.extend((0..page.related.len()).map(ArtistRow::Related)),
            ArtistTab::Playlists => rows.extend((0..page.playlists.len()).map(ArtistRow::Playlist)),
            ArtistTab::Bio => {}
        }
        self.artist_rows = rows;
        let first = self.artist_rows.iter().position(|r| r.selectable());
        self.main_state.select(first);
    }

    pub fn artist_tab_next(&mut self) {
        self.artist_tab = self.artist_tab.next();
        self.rebuild_artist_rows();
    }

    pub fn artist_tab_prev(&mut self) {
        self.artist_tab = self.artist_tab.prev();
        self.rebuild_artist_rows();
    }

    pub fn selected_artist_row(&self) -> Option<&ArtistRow> {
        self.artist_rows.get(self.main_state.selected()?)
    }

    /// What the right-hand detail card should show for the current selection:
    /// (image url, title, subtitle).
    pub fn selected_detail(&self) -> Option<(Option<String>, String, String)> {
        if self.viewing_artist {
            let page = self.artist_page.as_ref()?;
            let fans = format!("{} fans", fmt_count(page.nb_fan));
            return Some(match self.selected_artist_row() {
                Some(ArtistRow::Track(i)) => {
                    let t = page.top_tracks.get(*i)?;
                    (
                        t.cover_url.clone(),
                        t.title.clone(),
                        format!("{} · {}", t.artist, fmt_duration(t.duration_secs)),
                    )
                }
                Some(ArtistRow::Album(i)) => {
                    let a = page.albums.get(*i)?;
                    let mut sub = a.kind.clone();
                    if !a.year.is_empty() {
                        if !sub.is_empty() {
                            sub.push_str(" · ");
                        }
                        sub.push_str(&a.year);
                    }
                    (a.cover_url.clone(), a.title.clone(), sub)
                }
                Some(ArtistRow::Related(i)) => {
                    let a = page.related.get(*i)?;
                    (a.picture_url.clone(), a.name.clone(), format!("{} fans", fmt_count(a.nb_fan)))
                }
                Some(ArtistRow::Playlist(i)) => {
                    let p = page.playlists.get(*i)?;
                    (p.cover_url.clone(), p.title.clone(), format!("{} fans", fmt_count(p.nb_fan)))
                }
                _ => (page.picture_url.clone(), page.name.clone(), fans),
            });
        }
        match self.library_view? {
            LibraryView::Albums => {
                let a = self.library_albums.get(self.main_state.selected()?)?;
                let mut sub = a.artist.clone();
                if !a.year.is_empty() {
                    sub.push_str(" · ");
                    sub.push_str(&a.year);
                }
                Some((a.cover_url.clone(), a.title.clone(), sub))
            }
            LibraryView::Artists => {
                let a = self.library_artists.get(self.main_state.selected()?)?;
                Some((a.picture_url.clone(), a.name.clone(), format!("{} fans", fmt_count(a.nb_fan))))
            }
        }
    }

    /// Image URLs the current screen wants decoded (downloaded lazily by main).
    pub fn wanted_images(&self) -> Vec<String> {
        let mut urls = Vec::new();
        if self.viewing_artist {
            if let Some(url) = self.artist_page.as_ref().and_then(|p| p.picture_url.clone()) {
                urls.push(url);
            }
        }
        if let Some((Some(url), _, _)) = self.selected_detail() {
            urls.push(url);
        }
        if let Some(url) = &self.image_preview {
            urls.push(url.clone());
        }
        urls
    }

    /// The track shown on a given row of the main list, if that row is a track.
    pub fn track_at_main_row(&self, index: usize) -> Option<(String, String, String)> {
        if self.viewing_settings || self.library_view.is_some() {
            return None;
        }
        if self.viewing_artist {
            if let Some(ArtistRow::Track(i)) = self.artist_rows.get(index) {
                let t = self.artist_page.as_ref()?.top_tracks.get(*i)?;
                return Some((t.id.clone(), t.title.clone(), t.artist.clone()));
            }
            return None;
        }
        if self.showing_search_results && self.search_category != SearchCategory::Tracks {
            return None;
        }
        self.current_tracks.get(index.checked_sub(1)?).cloned()
    }

    /// Track under the keyboard selection (main list or queue).
    pub fn selected_track(&self) -> Option<(String, String, String)> {
        match self.active_panel {
            ActivePanel::Queue => self.queue_tracks.get(self.queue_state.selected()?).cloned(),
            _ => self.track_at_main_row(self.main_state.selected()?),
        }
    }

    /// Insert a track right after the current one (`next`) or at the end of
    /// the queue. With nothing playing, the track starts right away.
    pub fn enqueue(&mut self, track: (String, String, String), next: bool) {
        let label = format!("{} - {}", track.1, track.2);
        match self.queue_index {
            Some(current) if !self.queue_tracks.is_empty() => {
                let pos = if next { (current + 1).min(self.queue_tracks.len()) } else { self.queue_tracks.len() };
                self.queue_tracks.insert(pos, track.clone());
                self.queue.insert(pos, label);
                self.status_message = if next {
                    format!("Up next: {}", track.1)
                } else {
                    format!("Added to queue: {}", track.1)
                };
            }
            _ => {
                self.queue_tracks.push(track.clone());
                self.queue.push(label);
                let idx = self.queue_tracks.len() - 1;
                self.queue_index = Some(idx);
                self.queue_state.select(Some(idx));
                let _ = self.command_sender.send(Command::PlayTrack(track.0.clone()));
                self.is_playing = true;
                self.status_message = format!("Playing: {}", track.1);
            }
        }
    }

    pub fn open_track_menu(&mut self, track: (String, String, String), x: u16, y: u16) {
        self.context_menu = Some(ContextMenu { track, x, y, selected: 0, page: MenuPage::Main });
    }

    pub fn menu_items(&self) -> Vec<String> {
        match self.context_menu.as_ref().map(|m| &m.page) {
            Some(MenuPage::Main) => vec![
                "▶  Play next".to_string(),
                "≡  Add to queue".to_string(),
                "＋  Add to playlist   ▸".to_string(),
            ],
            Some(MenuPage::Playlists) => {
                let mut items = vec!["◂  Back".to_string()];
                if self.own_playlists.is_empty() {
                    items.push("(no playlist of yours yet)".to_string());
                } else {
                    items.extend(self.own_playlists.iter().map(|(_, title)| format!("≡  {title}")));
                }
                items
            }
            None => vec![],
        }
    }

    pub fn menu_move(&mut self, delta: i32) {
        let n = self.menu_items().len() as i32;
        if let Some(menu) = self.context_menu.as_mut() {
            if n > 0 {
                menu.selected = (menu.selected as i32 + delta).rem_euclid(n) as usize;
            }
        }
    }

    pub fn menu_back(&mut self) {
        if let Some(menu) = self.context_menu.as_mut() {
            if menu.page == MenuPage::Playlists {
                menu.page = MenuPage::Main;
                menu.selected = 2;
            } else {
                self.context_menu = None;
            }
        }
    }

    pub fn menu_activate(&mut self) {
        let Some(menu) = self.context_menu.clone() else { return };
        match menu.page {
            MenuPage::Main => match menu.selected {
                0 => {
                    self.context_menu = None;
                    self.enqueue(menu.track, true);
                }
                1 => {
                    self.context_menu = None;
                    self.enqueue(menu.track, false);
                }
                _ => {
                    if let Some(m) = self.context_menu.as_mut() {
                        m.page = MenuPage::Playlists;
                        m.selected = if self.own_playlists.is_empty() { 0 } else { 1 };
                    }
                }
            },
            MenuPage::Playlists => {
                if menu.selected == 0 {
                    self.menu_back();
                } else if let Some((playlist_id, playlist_title)) = self.own_playlists.get(menu.selected - 1).cloned() {
                    self.status_message = format!("Adding to {playlist_title}…");
                    let _ = self.command_sender.send(Command::AddToPlaylist {
                        playlist_id,
                        playlist_title,
                        track_id: menu.track.0.clone(),
                    });
                    self.context_menu = None;
                } else {
                    self.context_menu = None;
                }
            }
        }
    }

    pub fn switch_search_category_right(&mut self) {
        if self.showing_search_results && self.active_panel == ActivePanel::Main {
            self.search_category = match self.search_category {
                SearchCategory::Tracks => SearchCategory::Playlists,
                SearchCategory::Playlists => SearchCategory::Artists,
                SearchCategory::Artists => SearchCategory::Tracks,
            };
            self.main_state.select(Some(0));
        }
    }
}
