//! Deezer-styled terminal UI: flat dark surfaces, purple accent, sidebar with
//! the library, main page, a right-hand detail/shortcuts column and the player bar.

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap},
};

use crate::{
    app::{full_size_url, fmt_count, fmt_duration, ActivePanel, App, ArtistRow, ArtistTab, ClickRegion, Hit, LibraryView, RepeatMode, SearchCategory},
    config::{AudioQuality, Theme, SIDEBAR_SPLITS},
    theme::MatugenColors,
};

use image::DynamicImage;

/// Colour roles for a theme.
pub struct Palette {
    pub bg: Color,
    pub surface: Color,
    pub accent: Color,
    pub fg: Color,
    pub muted: Color,
    pub dim: Color,
    pub warn: Color,
}

pub fn palette(theme: Theme, matugen: Option<&MatugenColors>) -> Palette {
    match theme {
        Theme::Matugen => match matugen {
            Some(m) => Palette {
                bg: Color::Reset,
                surface: m.surface,
                accent: m.accent,
                fg: m.fg,
                muted: m.muted,
                dim: m.dim,
                warn: m.warn,
            },
            // Palette not generated yet: fall back to the Deezer look.
            None => palette(Theme::Deezer, None),
        },
        Theme::Deezer => Palette {
            // Terminal background shows through (Alacritty opacity / niri blur).
            bg: Color::Reset,
            surface: Color::Rgb(42, 42, 48),
            accent: Color::Rgb(162, 56, 255),
            fg: Color::Rgb(255, 255, 255),
            muted: Color::Rgb(163, 163, 173),
            dim: Color::Rgb(68, 68, 78),
            warn: Color::Rgb(255, 196, 0),
        },
        Theme::SpotifyDark => Palette {
            bg: Color::Rgb(18, 18, 18),
            surface: Color::Rgb(40, 40, 40),
            accent: Color::Rgb(30, 215, 96),
            fg: Color::White,
            muted: Color::Rgb(179, 179, 179),
            dim: Color::Rgb(64, 64, 64),
            warn: Color::Yellow,
        },
        Theme::NcmpcppBlue => Palette {
            bg: Color::Reset,
            surface: Color::DarkGray,
            accent: Color::Blue,
            fg: Color::White,
            muted: Color::Gray,
            dim: Color::DarkGray,
            warn: Color::Yellow,
        },
    }
}

/// Render an image into `area` with Unicode half-blocks (▄): two pixel rows per
/// terminal row, so a W×H cell area shows a W×2H pixel image. Works everywhere.
/// `circular` masks the picture to an ellipse inscribed in the area, like
/// Deezer's round artist photos.
fn render_image(f: &mut ratatui::Frame<'_>, img: &DynamicImage, area: Rect, bg: Color, circular: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let px_w = area.width as u32;
    let px_h = area.height as u32 * 2;
    let resized = img.resize_exact(px_w, px_h, image::imageops::FilterType::Lanczos3);
    let rgb = resized.to_rgb8();
    let (cx, cy) = (px_w as f32 / 2.0, px_h as f32 / 2.0);
    let inside = |x: u32, y: u32| -> bool {
        if !circular {
            return true;
        }
        let dx = (x as f32 + 0.5 - cx) / cx;
        let dy = (y as f32 + 0.5 - cy) / cy;
        dx * dx + dy * dy <= 1.0
    };
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(area.height as usize);
    for row in 0..area.height {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(area.width as usize);
        for col in 0..area.width {
            let (x, y_top, y_bot) = (col as u32, row as u32 * 2, row as u32 * 2 + 1);
            let top = rgb.get_pixel(x, y_top);
            let bot = rgb.get_pixel(x, y_bot);
            let top_c = if inside(x, y_top) { Color::Rgb(top[0], top[1], top[2]) } else { bg };
            let bot_c = if inside(x, y_bot) { Color::Rgb(bot[0], bot[1], bot[2]) } else { bg };
            spans.push(Span::styled("▄", Style::default().fg(bot_c).bg(top_c)));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Darker shade of an RGB colour (other colours fall back to the dim grey).
fn darken(color: Color, factor: f32) -> Color {
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(
            (r as f32 * factor) as u8,
            (g as f32 * factor) as u8,
            (b as f32 * factor) as u8,
        ),
        _ => Color::DarkGray,
    }
}

fn quality_label(quality: AudioQuality) -> &'static str {
    match quality {
        AudioQuality::Kbps128 => "128 kbps",
        AudioQuality::Kbps320 => "320 kbps",
        AudioQuality::Flac => "FLAC",
    }
}

/// A rounded, flat panel. The accent border marks the focused panel.
fn panel(title: Option<&str>, active: bool, p: &Palette) -> Block<'static> {
    let border = if active { p.accent } else { p.dim };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .style(Style::default().bg(p.bg));
    if let Some(title) = title {
        let title_style = if active {
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.muted).add_modifier(Modifier::BOLD)
        };
        block = block.title(Span::styled(format!(" {title} "), title_style));
    }
    block
}

fn row_highlight(p: &Palette) -> Style {
    Style::default().fg(p.fg).bg(p.surface).add_modifier(Modifier::BOLD)
}

fn muted(p: &Palette) -> Style {
    Style::default().fg(p.muted)
}

fn bold(p: &Palette) -> Style {
    Style::default().fg(p.fg).add_modifier(Modifier::BOLD)
}

fn state_with(selected: Option<usize>) -> ListState {
    let mut state = ListState::default();
    state.select(selected);
    state
}

type Hits = Vec<ClickRegion>;

fn hit(hits: &mut Hits, rect: Rect, h: Hit) {
    if rect.width > 0 && rect.height > 0 {
        hits.push(ClickRegion { rect, hit: h });
    }
}

/// Record one click region per visible list row (call after the list was
/// rendered so the state's scroll offset is final). Returns the row rects.
fn register_rows(
    hits: &mut Hits,
    inner: Rect,
    state: &ListState,
    heights: &[u16],
    mk: &dyn Fn(usize) -> Option<Hit>,
) -> Vec<(usize, Rect)> {
    let mut y = inner.y;
    let bottom = inner.y + inner.height;
    let mut rows = Vec::new();
    for (idx, h) in heights.iter().enumerate().skip(state.offset()) {
        if y >= bottom {
            break;
        }
        let rect = Rect { x: inner.x, y, width: inner.width, height: (*h).min(bottom - y) };
        if let Some(h) = mk(idx) {
            hit(hits, rect, h);
        }
        rows.push((idx, rect));
        y += *h;
    }
    rows
}

/// Column layout of a `track_row`: " NN  Title   Artist". Registers the title
/// (opens the album) and the artist name (opens the artist page).
fn register_track_spans(hits: &mut Hits, row: Rect, numbered: bool, title: &str, artist: &str, track_id: &str) {
    let title_len = title.chars().count() as u16;
    let artist_len = artist.chars().count() as u16;
    let title_x = row.x + if numbered { 5 } else { 1 };
    let clip = |x: u16, len: u16| -> Rect {
        let end = (x + len).min(row.x + row.width);
        Rect { x, y: row.y, width: end.saturating_sub(x), height: 1 }
    };
    hit(hits, clip(title_x, title_len), Hit::TrackAlbum(track_id.to_owned()));
    hit(hits, clip(title_x + title_len + 3, artist_len), Hit::TrackArtist(track_id.to_owned()));
}

/// Register click regions for spans laid out left to right on one line.
fn register_spans(hits: &mut Hits, line: Rect, start_x: u16, parts: &[(u16, Option<Hit>)]) {
    let mut x = start_x;
    for (len, h) in parts {
        let end = (x + len).min(line.x + line.width);
        if let Some(h) = h {
            hit(hits, Rect { x, y: line.y, width: end.saturating_sub(x), height: 1 }, h.clone());
        }
        x += len;
    }
}

fn centered_text(text: &str, width: u16) -> String {
    let max = width as usize;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > max {
        let cut = max.saturating_sub(1);
        format!("{}…", chars[..cut].iter().collect::<String>())
    } else {
        text.to_owned()
    }
}

pub fn render(
    f: &mut ratatui::Frame<'_>,
    app: &mut App,
    use_true_image_protocol: bool,
) -> Option<Rect> {
    let p = palette(app.config.theme, app.matugen.as_ref());
    f.render_widget(Block::default().style(Style::default().bg(p.bg).fg(p.fg)), f.size());

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(6)])
        .split(f.size());

    let right_width = if f.size().width >= 110 { 32 } else { 0 };
    let workspace = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(30), Constraint::Min(0), Constraint::Length(right_width)])
        .split(root[0]);

    let mut hits: Hits = Vec::new();
    app.right_panel_visible = right_width > 0;
    if !app.right_panel_visible && app.active_panel == ActivePanel::Shortcuts {
        app.active_panel = ActivePanel::Main;
    }
    render_sidebar(f, app, workspace[0], &p, &mut hits);
    render_main(f, app, workspace[1], &p, &mut hits);
    if right_width > 0 {
        render_right_panel(f, app, workspace[2], &p, &mut hits);
    }
    let art = render_player_bar(f, app, root[1], &p, use_true_image_protocol, &mut hits);
    render_loading_popup(f, app, workspace[1], &p);
    render_image_preview(f, app, &p);
    render_context_menu(f, app, &p, &mut hits);
    app.clicks = hits;
    art
}

/// Large, finer rendering of the artwork under the mouse.
fn render_image_preview(f: &mut ratatui::Frame<'_>, app: &App, p: &Palette) {
    let Some(url) = app.image_preview.as_ref() else { return };
    let now_cover = app.now_playing.as_ref().and_then(|n| n.album_art_url.as_deref()) == Some(url.as_str());
    let img = if now_cover {
        app.cover_art.as_ref()
    } else {
        app.images
            .get(url)
            .filter(|i| i.width() > 1)
            .or_else(|| app.images.get(&url.replace("500x500", "250x250")).filter(|i| i.width() > 1))
    };
    let Some(img) = img else { return };
    let screen = f.size();
    let h = screen.height.saturating_sub(6).min(44);
    let w = (h * 2).min(screen.width.saturating_sub(4));
    let h = w / 2;
    if h < 4 {
        return;
    }
    let rect = Rect {
        x: screen.x + (screen.width - (w + 2)) / 2,
        y: screen.y + (screen.height - (h + 2)) / 2,
        width: w + 2,
        height: h + 2,
    };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(p.accent))
        .style(Style::default().bg(p.surface));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    render_image(f, img, inner, p.surface, false);
}

/// Right-click / `m` menu for a track, drawn at the pointer.
fn render_context_menu(f: &mut ratatui::Frame<'_>, app: &App, p: &Palette, hits: &mut Hits) {
    let Some(menu) = app.context_menu.as_ref() else { return };
    let items = app.menu_items();
    if items.is_empty() {
        return;
    }
    let screen = f.size();
    let title = centered_text(&menu.track.1, 30);
    let width = items
        .iter()
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(10)
        .max(title.chars().count() + 2) as u16
        + 4;
    let max_rows = screen.height.saturating_sub(4).max(3) as usize;
    let visible = items.len().min(max_rows.min(14));
    let scroll = menu.selected.saturating_sub(visible - 1);
    let height = visible as u16 + 2;
    let x = menu.x.min(screen.width.saturating_sub(width));
    let y = menu.y.min(screen.height.saturating_sub(height));
    let rect = Rect { x, y, width, height };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .title(Span::styled(format!(" {title} "), Style::default().fg(p.muted)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(p.accent))
        .style(Style::default().bg(p.surface).fg(p.fg));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let lines: Vec<Line<'static>> = items
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible)
        .map(|(i, item)| {
            let style = if i == menu.selected {
                Style::default().fg(Color::Black).bg(p.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(p.fg)
            };
            let padded = format!(" {:<w$}", item, w = inner.width.saturating_sub(1) as usize);
            Line::from(Span::styled(padded, style))
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
    for (row, i) in (scroll..scroll + visible).enumerate() {
        hit(hits, Rect { x: inner.x, y: inner.y + row as u16, width: inner.width, height: 1 }, Hit::MenuItem(i));
    }
}

/// Small centered popup while a request is in flight. Appears after a short
/// delay so cache hits and fast answers never flash it.
fn render_loading_popup(f: &mut ratatui::Frame<'_>, app: &App, area: Rect, p: &Palette) {
    let Some((label, started)) = &app.loading else { return };
    let elapsed = started.elapsed().as_millis();
    if elapsed < 120 {
        return;
    }
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let frame = FRAMES[(elapsed / 80) as usize % FRAMES.len()];
    let width = (label.chars().count() as u16 + 10).clamp(24, area.width.saturating_sub(4).max(24));
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(5) / 2,
        width,
        height: 5,
    };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(p.accent))
        .style(Style::default().bg(p.surface));
    let text = Paragraph::new(vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("{frame}  "), Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled(label.clone(), Style::default().fg(p.fg).add_modifier(Modifier::BOLD)),
        ]),
    ])
    .alignment(Alignment::Center)
    .block(block);
    f.render_widget(text, rect);
}

// ── Sidebar ───────────────────────────────────────────────────────────────────

fn render_sidebar(f: &mut ratatui::Frame<'_>, app: &mut App, area: Rect, p: &Palette, hits: &mut Hits) {
    let (playlists_pct, queue_pct) = SIDEBAR_SPLITS[app.config.sidebar_split % SIDEBAR_SPLITS.len()];
    let sidebar = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(12),
            Constraint::Ratio(playlists_pct as u32, 100),
            Constraint::Ratio(queue_pct as u32, 100),
            Constraint::Length(1),
        ])
        .split(area);

    // Wordmark
    // Hovering the wordmark lights it up; clicking it toggles Matugen / Deezer.
    let logo_rect = Rect { x: sidebar[0].x, y: sidebar[0].y, width: 11.min(sidebar[0].width), height: 1 };
    let (mark_style, text_style) = if app.logo_hover {
        let on_white = Style::default().fg(Color::Black).bg(Color::White).add_modifier(Modifier::BOLD);
        (on_white, on_white)
    } else {
        (Style::default().fg(p.accent).add_modifier(Modifier::BOLD), bold(p))
    };
    let logo = Paragraph::new(Line::from(vec![
        Span::styled(" ▶ ", mark_style),
        Span::styled("DEEZER ", text_style),
        Span::styled(
            format!(" {}", if app.config.theme == Theme::Matugen { "matugen" } else { "tui" }),
            muted(p),
        ),
    ]));
    f.render_widget(logo, sidebar[0]);
    hit(hits, logo_rect, Hit::Logo);

    // Navigation: Home / Explore, then the library, then settings.
    let nav_active = app.active_panel == ActivePanel::Navigation;
    let nav_block = panel(None, nav_active, p);
    let nav_inner = nav_block.inner(sidebar[1]);
    f.render_widget(nav_block, sidebar[1]);
    hit(hits, sidebar[1], Hit::Focus(ActivePanel::Navigation));
    let nav_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(nav_inner);
    let selected = app.nav_state.selected().unwrap_or(0);
    let nav_list = |items: &[(&str, &str)], p: &Palette| -> List<'static> {
        List::new(
            items
                .iter()
                .map(|(icon, label)| {
                    ListItem::new(Line::from(vec![
                        Span::styled(format!(" {icon} "), Style::default().fg(p.accent)),
                        Span::styled(label.to_string(), Style::default().fg(p.fg)),
                    ]))
                })
                .collect::<Vec<_>>(),
        )
        .highlight_style(row_highlight(p))
    };
    f.render_stateful_widget(
        nav_list(&[("⌂", "Home"), ("◎", "Explore")], p),
        nav_rows[0],
        &mut state_with((selected < 2).then_some(selected)),
    );
    f.render_widget(
        Paragraph::new(Span::styled(" LIBRARY", muted(p).add_modifier(Modifier::BOLD))),
        nav_rows[2],
    );
    f.render_stateful_widget(
        nav_list(
            &[("♥", "Favorite tracks"), ("≡", "Playlists"), ("◫", "Albums"), ("◯", "Artists")],
            p,
        ),
        nav_rows[3],
        &mut state_with((2..=5).contains(&selected).then(|| selected - 2)),
    );
    f.render_stateful_widget(
        nav_list(&[("⚙", "Settings")], p),
        nav_rows[5],
        &mut state_with((selected == 6).then_some(0)),
    );
    for k in 0..2u16 {
        hit(hits, Rect { y: nav_rows[0].y + k, height: 1, ..nav_rows[0] }, Hit::NavItem(k as usize));
    }
    for k in 0..4u16 {
        hit(hits, Rect { y: nav_rows[3].y + k, height: 1, ..nav_rows[3] }, Hit::NavItem(2 + k as usize));
    }
    hit(hits, nav_rows[5], Hit::NavItem(6));

    // Playlists
    let playlist_items: Vec<ListItem<'_>> = app
        .playlists
        .iter()
        .map(|(_, title)| ListItem::new(Line::from(vec![Span::raw(" "), Span::raw(title.as_str())])))
        .collect();
    let playlist_block = panel(Some("Playlists"), app.active_panel == ActivePanel::Playlists, p);
    let playlist_inner = playlist_block.inner(sidebar[2]);
    let playlist_list = List::new(playlist_items)
        .style(Style::default().fg(p.fg))
        .highlight_style(row_highlight(p))
        .block(playlist_block);
    f.render_stateful_widget(playlist_list, sidebar[2], &mut app.playlist_state);
    hit(hits, sidebar[2], Hit::Focus(ActivePanel::Playlists));
    let heights = vec![1u16; app.playlists.len()];
    register_rows(hits, playlist_inner, &app.playlist_state, &heights, &|i| Some(Hit::PlaylistItem(i)));
    hit(hits, Rect { height: 1, ..sidebar[2] }, Hit::SplitCycle);

    // Queue
    let current = app.queue_index;
    let queue_items: Vec<ListItem<'_>> = app
        .queue
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let marker = if current == Some(i) { "▶" } else { " " };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker} "), Style::default().fg(p.accent)),
                Span::styled(
                    name.as_str(),
                    if current == Some(i) { Style::default().fg(p.accent) } else { Style::default().fg(p.fg) },
                ),
            ]))
        })
        .collect();
    let queue_block = panel(
        Some(&format!("Queue · {}", app.queue.len())),
        app.active_panel == ActivePanel::Queue,
        p,
    );
    let queue_inner = queue_block.inner(sidebar[3]);
    let queue_list = List::new(queue_items).highlight_style(row_highlight(p)).block(queue_block);
    f.render_stateful_widget(queue_list, sidebar[3], &mut app.queue_state);
    hit(hits, sidebar[3], Hit::Focus(ActivePanel::Queue));
    let heights = vec![1u16; app.queue.len()];
    register_rows(hits, queue_inner, &app.queue_state, &heights, &|i| Some(Hit::QueueItem(i)));
    hit(hits, Rect { height: 1, ..sidebar[3] }, Hit::SplitCycle);

    // Status line
    let status_style = if app.status_message.starts_with("Error") || app.status_message.contains("rror:") {
        Style::default().fg(p.warn)
    } else {
        muted(p)
    };
    f.render_widget(
        Paragraph::new(format!(" {}", app.status_message)).style(status_style),
        sidebar[4],
    );
}

// ── Main column ───────────────────────────────────────────────────────────────

fn render_main(f: &mut ratatui::Frame<'_>, app: &mut App, area: Rect, p: &Palette, hits: &mut Hits) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    // Search bar
    let search_active = app.is_searching || app.active_panel == ActivePanel::Search;
    let search_line = if app.search_query.is_empty() && !app.is_searching {
        Line::from(vec![
            Span::styled("⌕  ", Style::default().fg(p.accent)),
            Span::styled("Search tracks, artists, playlists…", muted(p)),
        ])
    } else {
        Line::from(vec![
            Span::styled("⌕  ", Style::default().fg(p.accent)),
            Span::styled(app.search_query.clone(), Style::default().fg(p.fg)),
            Span::styled(if app.is_searching { "▏" } else { "" }, Style::default().fg(p.accent)),
        ])
    };
    f.render_widget(
        Paragraph::new(search_line).block(panel(None, search_active, p).padding(Padding::new(1, 1, 0, 0))),
        sections[0],
    );

    hit(hits, sections[0], Hit::SearchBar);
    let content = sections[1];
    hit(hits, content, Hit::Focus(ActivePanel::Main));
    let main_active = app.active_panel == ActivePanel::Main;

    if app.viewing_settings {
        render_settings(f, app, content, p, main_active, hits);
    } else if app.viewing_artist {
        render_artist(f, app, content, p, main_active, hits);
    } else if let Some(view) = app.library_view {
        render_library(f, app, view, content, p, main_active, hits);
    } else if app.showing_search_results {
        render_search_results(f, app, content, p, main_active, hits);
    } else if !app.current_tracks.is_empty() {
        render_track_list(f, app, content, p, main_active, hits);
    } else {
        render_welcome(f, app, content, p, main_active);
    }
}

/// Inner text area of a header block (border + padding 2,2,1,0).
fn header_inner(area: Rect) -> Rect {
    panel(None, false, &palette(Theme::Deezer, None)).padding(Padding::new(2, 2, 1, 0)).inner(area)
}

fn header_block(f: &mut ratatui::Frame<'_>, area: Rect, lines: Vec<Line<'static>>, p: &Palette, active: bool) {
    f.render_widget(
        Paragraph::new(lines).block(panel(None, active, p).padding(Padding::new(2, 2, 1, 0))),
        area,
    );
}

fn split_header_body(area: Rect, header_height: u16) -> (Rect, Rect) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(header_height), Constraint::Min(0)])
        .split(area);
    (parts[0], parts[1])
}

fn track_row(index: Option<usize>, title: &str, artist: &str, trailing: Option<String>, p: &Palette) -> ListItem<'static> {
    let mut spans = vec![Span::raw(" ")];
    if let Some(i) = index {
        spans.push(Span::styled(format!("{:>2}  ", i), muted(p)));
    }
    spans.push(Span::styled(title.to_owned(), Style::default().fg(p.fg)));
    spans.push(Span::styled(format!("   {artist}"), muted(p)));
    if let Some(t) = trailing {
        spans.push(Span::styled(format!("   {t}"), muted(p)));
    }
    ListItem::new(Line::from(spans))
}

fn play_all_row(label: &str, p: &Palette) -> ListItem<'static> {
    ListItem::new(Line::from(vec![
        Span::raw(" "),
        Span::styled(format!("▶  {label}"), Style::default().fg(p.accent).add_modifier(Modifier::BOLD)),
    ]))
}

fn section_row(label: &str, p: &Palette) -> ListItem<'static> {
    ListItem::new(vec![
        Line::from(""),
        Line::from(Span::styled(format!(" {}", label.to_uppercase()), muted(p).add_modifier(Modifier::BOLD))),
    ])
}

fn render_settings(f: &mut ratatui::Frame<'_>, app: &mut App, area: Rect, p: &Palette, active: bool, hits: &mut Hits) {
    let (header, body) = split_header_body(area, 5);
    header_block(
        f,
        header,
        vec![
            Line::from(Span::styled("Settings", bold(p))),
            Line::from(Span::styled("Playback, quality and integrations", muted(p))),
        ],
        p,
        active,
    );
    let on_off = |b: bool| if b { "On" } else { "Off" };
    let items = vec![
        format!("Crossfade: {}", on_off(app.config.crossfade_enabled)),
        format!("Crossfade duration: {} ms", app.config.crossfade_duration_ms),
        format!("Quality: {}", quality_label(app.config.default_quality)),
        format!("Discord Rich Presence: {}", on_off(app.discord_rpc_enabled)),
        format!("Resume on start: {}", on_off(app.config.resume_on_start)),
        "Set ARL from the search box".to_string(),
    ];
    let heights = vec![1u16; items.len()];
    let block = panel(None, false, p);
    let inner = block.inner(body);
    let list = List::new(
        items
            .into_iter()
            .map(|s| ListItem::new(Line::from(vec![Span::raw(" "), Span::raw(s)])))
            .collect::<Vec<_>>(),
    )
    .style(Style::default().fg(p.fg))
    .highlight_style(row_highlight(p))
    .block(block);
    f.render_stateful_widget(list, body, &mut app.settings_state);
    register_rows(hits, inner, &app.settings_state, &heights, &|i| Some(Hit::MainRow { index: i, opens: true }));
}

fn render_welcome(f: &mut ratatui::Frame<'_>, app: &mut App, area: Rect, p: &Palette, active: bool) {
    let (header, body) = split_header_body(area, 5);
    header_block(
        f,
        header,
        vec![
            Line::from(Span::styled("Home", bold(p))),
            Line::from(Span::styled("Pick something from the library or search Deezer", muted(p))),
        ],
        p,
        active,
    );
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  ▶ ", Style::default().fg(p.accent)),
            Span::styled("Home", bold(p)),
            Span::styled("  recommendations from your Deezer account", muted(p)),
        ]),
        Line::from(vec![
            Span::styled("  ◎ ", Style::default().fg(p.accent)),
            Span::styled("Explore", bold(p)),
            Span::styled("  trending and discovery", muted(p)),
        ]),
        Line::from(vec![
            Span::styled("  ♥ ", Style::default().fg(p.accent)),
            Span::styled("Library", bold(p)),
            Span::styled("  favorite tracks, playlists, albums and artists", muted(p)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", if app.playlists.is_empty() { "Connecting to Deezer…" } else { "Shortcuts are listed in the right-hand panel." }),
            muted(p),
        )),
    ];
    f.render_widget(Paragraph::new(lines).block(panel(None, false, p)), body);
}

fn render_track_list(f: &mut ratatui::Frame<'_>, app: &mut App, area: Rect, p: &Palette, active: bool, hits: &mut Hits) {
    let source = app.current_playlist_id.clone().unwrap_or_default();
    let (title, subtitle) = if source == "__home__" {
        ("Home".to_string(), "Recommended for you".to_string())
    } else if source == "__explore__" {
        ("Explore".to_string(), "Trending and discovery".to_string())
    } else if let Some(id) = source.strip_prefix("__album__:") {
        let name = app
            .current_album
            .as_ref()
            .filter(|(aid, _, _)| aid == id)
            .map(|(_, title, artist)| format!("{title} · {artist}"))
            .unwrap_or_else(|| "Album".to_string());
        (name, "Album".to_string())
    } else {
        let name = app
            .playlists
            .iter()
            .chain(app.search_playlists.iter())
            .find(|(pid, _)| *pid == source)
            .map(|(_, t)| t.clone())
            .unwrap_or_else(|| if source.is_empty() { "Favorite tracks".to_string() } else { "Playlist".to_string() });
        (name, "Playlist".to_string())
    };
    let (header, body) = split_header_body(area, 5);
    header_block(
        f,
        header,
        vec![
            Line::from(Span::styled(title, bold(p))),
            Line::from(Span::styled(format!("{subtitle} · {} tracks", app.current_tracks.len()), muted(p))),
        ],
        p,
        active,
    );
    let mut items = vec![play_all_row("Play all", p)];
    items.extend(
        app.current_tracks
            .iter()
            .enumerate()
            .map(|(i, (_, title, artist))| track_row(Some(i + 1), title, artist, None, p)),
    );
    let heights = vec![1u16; items.len()];
    let block = panel(None, false, p);
    let inner = block.inner(body);
    let list = List::new(items).highlight_style(row_highlight(p)).block(block);
    f.render_stateful_widget(list, body, &mut app.main_state);
    let rows = register_rows(hits, inner, &app.main_state, &heights, &|i| Some(Hit::MainRow { index: i, opens: i == 0 }));
    for (idx, rect) in rows {
        if let Some((id, title, artist)) = idx.checked_sub(1).and_then(|i| app.current_tracks.get(i)) {
            register_track_spans(hits, rect, true, title, artist, id);
        }
    }
}

fn render_search_results(f: &mut ratatui::Frame<'_>, app: &mut App, area: Rect, p: &Palette, active: bool, hits: &mut Hits) {
    let categories = [
        (SearchCategory::Tracks, "Tracks"),
        (SearchCategory::Playlists, "Playlists"),
        (SearchCategory::Artists, "Artists"),
    ];
    let tabs: Vec<Span<'static>> = categories
    .iter()
    .flat_map(|(cat, label)| {
        let style = if app.search_category == *cat {
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            muted(p)
        };
        [Span::styled(label.to_string(), style), Span::raw("    ")]
    })
    .collect();
    let (header, body) = split_header_body(area, 6);
    header_block(
        f,
        header,
        vec![
            Line::from(Span::styled(format!("Results for “{}”", app.search_query), bold(p))),
            Line::from(Span::styled(
                format!(
                    "{} tracks · {} playlists · {} artists",
                    app.current_tracks.len(),
                    app.search_playlists.len(),
                    app.search_artists.len()
                ),
                muted(p),
            )),
            Line::from(tabs),
        ],
        p,
        active,
    );
    let hi = header_inner(header);
    let tab_line = Rect { y: hi.y + 2, height: 1, ..hi };
    let parts: Vec<(u16, Option<Hit>)> = categories
        .iter()
        .flat_map(|(cat, label)| [(label.chars().count() as u16, Some(Hit::SearchCategory(*cat))), (4, None)])
        .collect();
    register_spans(hits, tab_line, hi.x, &parts);
    let items: Vec<ListItem> = match app.search_category {
        SearchCategory::Tracks => {
            let mut items = vec![play_all_row("Play all results", p)];
            items.extend(
                app.current_tracks
                    .iter()
                    .enumerate()
                    .map(|(i, (_, title, artist))| track_row(Some(i + 1), title, artist, None, p)),
            );
            items
        }
        SearchCategory::Playlists => app
            .search_playlists
            .iter()
            .map(|(_, title)| {
                ListItem::new(Line::from(vec![
                    Span::styled(" ≡  ", Style::default().fg(p.accent)),
                    Span::styled(title.clone(), Style::default().fg(p.fg)),
                ]))
            })
            .collect(),
        SearchCategory::Artists => app
            .search_artists
            .iter()
            .map(|(_, name)| {
                ListItem::new(Line::from(vec![
                    Span::styled(" ◯  ", Style::default().fg(p.accent)),
                    Span::styled(name.clone(), Style::default().fg(p.fg)),
                ]))
            })
            .collect(),
    };
    if items.is_empty() {
        f.render_widget(
            Paragraph::new(" No results in this category").style(muted(p)).block(panel(None, false, p)),
            body,
        );
    } else {
        let heights = vec![1u16; items.len()];
        let block = panel(None, false, p);
        let inner = block.inner(body);
        let list = List::new(items).highlight_style(row_highlight(p)).block(block);
        f.render_stateful_widget(list, body, &mut app.main_state);
        let is_tracks = app.search_category == SearchCategory::Tracks;
        let rows = register_rows(hits, inner, &app.main_state, &heights, &|i| {
            Some(Hit::MainRow { index: i, opens: !is_tracks || i == 0 })
        });
        if is_tracks {
            for (idx, rect) in rows {
                if let Some((id, title, artist)) = idx.checked_sub(1).and_then(|i| app.current_tracks.get(i)) {
                    register_track_spans(hits, rect, true, title, artist, id);
                }
            }
        }
    }
}

fn render_library(f: &mut ratatui::Frame<'_>, app: &mut App, view: LibraryView, area: Rect, p: &Palette, active: bool, hits: &mut Hits) {
    let (header, body) = split_header_body(area, 5);
    let (title, count, items): (&str, usize, Vec<ListItem>) = match view {
        LibraryView::Albums => (
            "Albums",
            app.library_albums.len(),
            app.library_albums
                .iter()
                .map(|a| {
                    let year = if a.year.is_empty() { String::new() } else { format!("   {}", a.year) };
                    ListItem::new(Line::from(vec![
                        Span::styled(" ◫  ", Style::default().fg(p.accent)),
                        Span::styled(a.title.clone(), Style::default().fg(p.fg)),
                        Span::styled(format!("   {}{}", a.artist, year), muted(p)),
                    ]))
                })
                .collect(),
        ),
        LibraryView::Artists => (
            "Artists",
            app.library_artists.len(),
            app.library_artists
                .iter()
                .map(|a| {
                    ListItem::new(Line::from(vec![
                        Span::styled(" ◯  ", Style::default().fg(p.accent)),
                        Span::styled(a.name.clone(), Style::default().fg(p.fg)),
                        Span::styled(format!("   {} fans", fmt_count(a.nb_fan)), muted(p)),
                    ]))
                })
                .collect(),
        ),
    };
    header_block(
        f,
        header,
        vec![
            Line::from(Span::styled(title, bold(p))),
            Line::from(Span::styled(format!("{count} in your library · Enter to open"), muted(p))),
        ],
        p,
        active,
    );
    if items.is_empty() {
        f.render_widget(
            Paragraph::new(" Nothing here yet").style(muted(p)).block(panel(None, false, p)),
            body,
        );
    } else {
        let heights = vec![1u16; items.len()];
        let block = panel(None, false, p);
        let inner = block.inner(body);
        let list = List::new(items).highlight_style(row_highlight(p)).block(block);
        f.render_stateful_widget(list, body, &mut app.main_state);
        register_rows(hits, inner, &app.main_state, &heights, &|i| Some(Hit::MainRow { index: i, opens: true }));
    }
}

fn render_artist(f: &mut ratatui::Frame<'_>, app: &mut App, area: Rect, p: &Palette, active: bool, hits: &mut Hits) {
    let Some(page) = app.artist_page.clone() else { return };
    let (header, body) = split_header_body(area, 14);

    // Header: round photo on the left, name / fans / tabs on the right.
    let header_block = panel(None, active, p);
    let inner = header_block.inner(header);
    f.render_widget(header_block, header);
    let photo_cols = 24u16;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(photo_cols + 4), Constraint::Min(0)])
        .split(inner);
    let photo_area = Rect {
        x: cols[0].x + 2,
        y: cols[0].y,
        width: photo_cols.min(cols[0].width.saturating_sub(2)),
        height: 12.min(cols[0].height),
    };
    if let Some(url) = page.picture_url.as_ref() {
        hit(hits, photo_area, Hit::Image(full_size_url(url)));
    }
    match page.picture_url.as_ref().and_then(|u| app.images.get(u)) {
        Some(img) if img.width() > 1 => render_image(f, img, photo_area, p.bg, true),
        _ => f.render_widget(
            Paragraph::new(vec![Line::from(""), Line::from(""), Line::from(""), Line::from(""), Line::from(""), Line::from("◯")])
                .alignment(Alignment::Center)
                .style(Style::default().fg(p.dim)),
            photo_area,
        ),
    }

    let tabs: Vec<Span<'static>> = ArtistTab::ALL
        .iter()
        .flat_map(|tab| {
            let style = if *tab == app.artist_tab {
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                muted(p)
            };
            [Span::styled(tab.label().to_string(), style), Span::raw("   ")]
        })
        .collect();
    let mut info = vec![
        Line::from(""),
        Line::from(Span::styled(page.name.clone(), bold(p))),
        Line::from(Span::styled(format!("{} fans", fmt_count(page.nb_fan)), muted(p))),
        Line::from(""),
    ];
    if let Some(latest) = &page.latest_release {
        info.push(Line::from(vec![
            Span::styled("Latest release  ", muted(p)),
            Span::styled(latest.clone(), Style::default().fg(p.fg)),
        ]));
    } else {
        info.push(Line::from(""));
    }
    info.push(Line::from(""));
    info.push(Line::from(vec![
        Span::styled("▶ Mix  ", Style::default().fg(Color::Black).bg(p.accent).add_modifier(Modifier::BOLD)),
        Span::styled("   ♥   ···", muted(p)),
    ]));
    info.push(Line::from(""));
    info.push(Line::from(tabs));
    let tab_line = Rect { y: cols[1].y + (info.len() as u16 - 1), height: 1, ..cols[1] };
    f.render_widget(Paragraph::new(info), cols[1]);
    let parts: Vec<(u16, Option<Hit>)> = ArtistTab::ALL
        .iter()
        .flat_map(|tab| [(tab.label().chars().count() as u16, Some(Hit::ArtistTab(*tab))), (3, None)])
        .collect();
    register_spans(hits, tab_line, cols[1].x, &parts);
    // Artist name opens nothing new, but the photo / name act as "scroll to top".

    // Body
    if app.artist_tab == ArtistTab::Bio {
        let text = if page.bio.is_empty() { "No biography available.".to_string() } else { page.bio.clone() };
        f.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(p.fg))
                .wrap(Wrap { trim: true })
                .block(panel(None, false, p).padding(Padding::new(2, 2, 1, 1))),
            body,
        );
        return;
    }
    let items: Vec<ListItem> = app
        .artist_rows
        .iter()
        .map(|row| match row {
            ArtistRow::PlayAll => play_all_row("Play top tracks", p),
            ArtistRow::Header(label) => section_row(label, p),
            ArtistRow::Track(i) => {
                let t = &page.top_tracks[*i];
                track_row(Some(i + 1), &t.title, &t.artist, Some(fmt_duration(t.duration_secs)), p)
            }
            ArtistRow::Album(i) => {
                let a = &page.albums[*i];
                ListItem::new(Line::from(vec![
                    Span::styled(" ◫  ", Style::default().fg(p.accent)),
                    Span::styled(a.title.clone(), Style::default().fg(p.fg)),
                    Span::styled(
                        format!("   {}{}{}", a.year, if a.kind.is_empty() { "" } else { " · " }, a.kind),
                        muted(p),
                    ),
                ]))
            }
            ArtistRow::Related(i) => {
                let a = &page.related[*i];
                ListItem::new(Line::from(vec![
                    Span::styled(" ◯  ", Style::default().fg(p.accent)),
                    Span::styled(a.name.clone(), Style::default().fg(p.fg)),
                    Span::styled(format!("   {} fans", fmt_count(a.nb_fan)), muted(p)),
                ]))
            }
            ArtistRow::Playlist(i) => {
                let pl = &page.playlists[*i];
                ListItem::new(Line::from(vec![
                    Span::styled(" ≡  ", Style::default().fg(p.accent)),
                    Span::styled(pl.title.clone(), Style::default().fg(p.fg)),
                    Span::styled(format!("   {} fans", fmt_count(pl.nb_fan)), muted(p)),
                ]))
            }
        })
        .collect();
    if items.is_empty() {
        f.render_widget(
            Paragraph::new(" Nothing in this tab").style(muted(p)).block(panel(None, false, p)),
            body,
        );
    } else {
        let heights: Vec<u16> = app
            .artist_rows
            .iter()
            .map(|r| if matches!(r, ArtistRow::Header(_)) { 2 } else { 1 })
            .collect();
        let block = panel(None, false, p);
        let inner = block.inner(body);
        let list = List::new(items).highlight_style(row_highlight(p)).block(block);
        f.render_stateful_widget(list, body, &mut app.main_state);
        let rows_meta = app.artist_rows.clone();
        let rows = register_rows(hits, inner, &app.main_state, &heights, &|i| match rows_meta.get(i)? {
            ArtistRow::Header(_) => None,
            ArtistRow::Track(_) => Some(Hit::MainRow { index: i, opens: false }),
            _ => Some(Hit::MainRow { index: i, opens: true }),
        });
        for (idx, rect) in rows {
            if let Some(ArtistRow::Track(t)) = rows_meta.get(idx) {
                if let Some(track) = page.top_tracks.get(*t) {
                    register_track_spans(hits, rect, true, &track.title, &track.artist, &track.id);
                }
            }
        }
    }
}

// ── Right column: detail card + shortcuts ─────────────────────────────────────

fn render_right_panel(f: &mut ratatui::Frame<'_>, app: &mut App, area: Rect, p: &Palette, hits: &mut Hits) {
    let (label, img, title, subtitle, preview_url): (&str, Option<&DynamicImage>, String, String, Option<String>) =
        match app.selected_detail() {
            Some((url, title, subtitle)) => (
                "Selected",
                url.as_ref().and_then(|u| app.images.get(u)).filter(|i| i.width() > 1),
                title,
                subtitle,
                url.as_deref().map(full_size_url),
            ),
            None => match &app.now_playing {
                Some(now) => ("Now playing", app.cover_art.as_ref(), now.title.clone(), now.artist.clone(), now.album_art_url.clone()),
                None => ("", None, String::new(), String::new(), None),
            },
        };

    let card_height = if label.is_empty() { 0 } else { 16 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(card_height), Constraint::Min(0)])
        .split(area);

    if card_height > 0 {
        let block = panel(Some(label), false, p);
        let inner = block.inner(rows[0]);
        f.render_widget(block, rows[0]);
        let img_w = 20u16.min(inner.width);
        let img_area = Rect {
            x: inner.x + (inner.width.saturating_sub(img_w)) / 2,
            y: inner.y,
            width: img_w,
            height: 10.min(inner.height),
        };
        if let (Some(url), Some(_)) = (preview_url.as_ref(), img) {
            hit(hits, img_area, Hit::Image(url.clone()));
        }
        match img {
            Some(img) => render_image(f, img, img_area, p.bg, false),
            None => f.render_widget(
                Paragraph::new(vec![Line::from(""), Line::from(""), Line::from(""), Line::from(""), Line::from("♪")])
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(p.dim)),
                img_area,
            ),
        }
        let text_area = Rect {
            x: inner.x,
            y: inner.y + img_area.height + 1,
            width: inner.width,
            height: inner.height.saturating_sub(img_area.height + 1),
        };
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(centered_text(&title, inner.width.saturating_sub(2)), bold(p))),
                Line::from(Span::styled(centered_text(&subtitle, inner.width.saturating_sub(2)), muted(p))),
            ])
            .alignment(Alignment::Center),
            text_area,
        );
    }

    // Shortcuts: context first, then global keys.
    let mut keys: Vec<(&str, &str)> = Vec::new();
    if app.is_searching || app.active_panel == ActivePanel::Search {
        keys.extend([
            ("Type", "Search query"),
            ("Enter", "Search"),
            ("C-⌫", "Clear query"),
            ("M-⌫", "Delete word"),
            ("Esc", "Leave search"),
        ]);
    } else {
        match app.active_panel {
            ActivePanel::Navigation => keys.extend([("Enter", "Open page"), ("↑ ↓", "Move")]),
            ActivePanel::Playlists => keys.extend([("Enter", "Open playlist"), ("↑ ↓", "Move")]),
            ActivePanel::Queue => keys.extend([("Enter", "Play item"), ("↑ ↓", "Move")]),
            ActivePanel::Main => {
                if app.viewing_settings {
                    keys.extend([("Enter", "Toggle / cycle"), ("↑ ↓", "Move")]);
                } else if app.viewing_artist {
                    keys.extend([("← →", "Switch tab"), ("Enter", "Play / open"), ("Esc", "Back")]);
                } else if app.library_view.is_some() {
                    keys.extend([("Enter", "Open"), ("Esc", "Back")]);
                } else if app.showing_search_results {
                    keys.extend([("← →", "Category"), ("Enter", "Play / open"), ("Esc", "Back")]);
                } else {
                    keys.extend([("Enter", "Play"), ("↑ ↓", "Move"), ("Esc", "Back")]);
                }
            }
            ActivePanel::Player => keys.extend([("← →", "Choose button"), ("Enter", "Activate"), ("↓", "Progress bar")]),
            ActivePanel::PlayerProgress => keys.extend([("← →", "Seek ±5 s"), ("↑", "Buttons")]),
            ActivePanel::PlayerInfo => keys.extend([("← →", "Adjust"), ("↑ ↓", "Volume / quality")]),
            ActivePanel::Shortcuts => keys.extend([("↑ ↓", "Scroll this list"), ("Wheel", "Scroll this list")]),
            ActivePanel::Search => {}
        }
    }
    let global: [(&str, &str); 11] = [
        ("Tab", "Next panel"),
        ("S-Tab", "Previous panel"),
        ("j k", "Move"),
        ("/", "Search"),
        ("p", "Play / pause"),
        ("a", "Artist of track"),
        ("e", "Album of track"),
        ("m", "Track menu"),
        ("z", "Play next"),
        ("t", "Theme"),
        ("q", "Quit"),
    ];
    let key_span = |k: &str, p: &Palette| Span::styled(format!(" {:<8}", k), Style::default().fg(p.accent).add_modifier(Modifier::BOLD));
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(" HERE", muted(p).add_modifier(Modifier::BOLD))));
    for (k, d) in &keys {
        lines.push(Line::from(vec![key_span(k, p), Span::styled(d.to_string(), Style::default().fg(p.fg))]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(" MOUSE", muted(p).add_modifier(Modifier::BOLD))));
    for (k, d) in [("Hover", "Focus follows"), ("Click", "Select / open"), ("2×Click", "Play track"), ("Title", "Open album"), ("Artist", "Open artist"), ("R-click", "Track menu"), ("Artwork", "Hover: preview"), ("Titles", "Resize lists"), ("Wheel", "Scroll / volume")] {
        lines.push(Line::from(vec![key_span(k, p), Span::styled(d.to_string(), Style::default().fg(p.fg))]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(" EVERYWHERE", muted(p).add_modifier(Modifier::BOLD))));
    for (k, d) in &global {
        lines.push(Line::from(vec![key_span(k, p), Span::styled(d.to_string(), Style::default().fg(p.fg))]));
    }
    let active = app.active_panel == ActivePanel::Shortcuts;
    let block = panel(Some("Shortcuts"), active, p);
    let inner = block.inner(rows[1]);
    app.shortcuts_max_scroll = (lines.len() as u16).saturating_sub(inner.height);
    app.shortcuts_scroll = app.shortcuts_scroll.min(app.shortcuts_max_scroll);
    let scrolled = app.shortcuts_scroll;
    let more_below = scrolled < app.shortcuts_max_scroll;
    f.render_widget(Paragraph::new(lines).scroll((scrolled, 0)).block(block), rows[1]);
    if more_below && inner.height > 0 {
        f.render_widget(
            Paragraph::new("▾ more").alignment(Alignment::Right).style(muted(p)),
            Rect { y: inner.y + inner.height - 1, height: 1, ..inner },
        );
    }
    if scrolled > 0 {
        f.render_widget(
            Paragraph::new("▴").alignment(Alignment::Right).style(muted(p)),
            Rect { height: 1, ..inner },
        );
    }
    hit(hits, rows[1], Hit::Focus(ActivePanel::Shortcuts));
}

// ── Player bar ────────────────────────────────────────────────────────────────

fn render_player_bar(
    f: &mut ratatui::Frame<'_>,
    app: &App,
    area: Rect,
    p: &Palette,
    use_true_image_protocol: bool,
    hits: &mut Hits,
) -> Option<Rect> {
    let player_bar = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(25), Constraint::Percentage(50), Constraint::Percentage(25)])
        .split(area);

    let (track_title, track_artist, current_ms, total_ms, quality) = match &app.now_playing {
        Some(now) => (now.title.clone(), now.artist.clone(), now.current_ms, now.total_ms.max(1), now.quality),
        None => ("Nothing playing".to_string(), "Pick a track to start".to_string(), 0, 1, app.config.default_quality),
    };
    let track_info = Paragraph::new(vec![
        Line::from(Span::styled(track_title, bold(p))),
        Line::from(Span::styled(track_artist, muted(p))),
    ]);

    // Left: cover art + title / artist
    const ART_COLS: u16 = 12;
    let show_art = app.cover_art.is_some() && player_bar[0].width > ART_COLS + 8;
    let mut protocol_art_rect: Option<Rect> = None;
    let mut info_area = player_bar[0];
    if show_art {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(ART_COLS), Constraint::Min(0)])
            .split(player_bar[0]);
        if use_true_image_protocol {
            f.render_widget(panel(None, false, p), split[0]);
            protocol_art_rect = Some(
                Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
                    .split(split[0])[1],
            );
        } else if let Some(img) = &app.cover_art {
            render_image(f, img, split[0], p.bg, false);
        }
        if let Some(url) = app.now_playing.as_ref().and_then(|n| n.album_art_url.clone()) {
            hit(hits, split[0], Hit::Image(url));
        }
        f.render_widget(
            track_info.block(panel(None, false, p).padding(Padding::new(1, 1, 1, 0))),
            split[1],
        );
        info_area = split[1];
    } else {
        f.render_widget(
            track_info.block(panel(None, false, p).padding(Padding::new(1, 1, 1, 0))),
            player_bar[0],
        );
    }
    if let Some(now) = &app.now_playing {
        let inner = panel(None, false, p).padding(Padding::new(1, 1, 1, 0)).inner(info_area);
        hit(hits, Rect { height: 1, ..inner }, Hit::TrackAlbum(now.id.clone()));
        hit(hits, Rect { y: inner.y + 1, height: 1, ..inner }, Hit::TrackArtist(now.id.clone()));
    }

    // Centre: controls + progress
    let centre = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(2), Constraint::Length(2), Constraint::Min(0)])
        .split(player_bar[1]);

    let play_symbol = if app.is_playing { "⏸ Pause" } else { "▶ Play" };
    let repeat_label = match app.repeat_mode {
        RepeatMode::Off => "↻ Repeat",
        RepeatMode::All => "↻ All",
        RepeatMode::One => "↻ One",
    };
    let labels = ["⇄ Shuffle", "⏮ Prev", play_symbol, "⏭ Next", repeat_label];
    let player_active = app.active_panel == ActivePanel::Player;
    let mut control_spans = Vec::new();
    for (i, label) in labels.iter().enumerate() {
        let selected = player_active && app.player_button_index == i;
        let style = if selected {
            Style::default().fg(Color::Black).bg(p.accent).add_modifier(Modifier::BOLD)
        } else if i == 2 {
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
        } else if player_active {
            Style::default().fg(p.fg)
        } else {
            muted(p)
        };
        control_spans.push(Span::styled(format!(" {label} "), style));
        control_spans.push(Span::raw("  "));
    }
    f.render_widget(Paragraph::new(Line::from(control_spans)).alignment(Alignment::Center), centre[1]);
    hit(hits, player_bar[1], Hit::Focus(ActivePanel::Player));
    let total: u16 = labels.iter().map(|l| l.chars().count() as u16 + 4).sum::<u16>() - 2;
    let start_x = centre[1].x + centre[1].width.saturating_sub(total) / 2;
    let parts: Vec<(u16, Option<Hit>)> = labels
        .iter()
        .enumerate()
        .flat_map(|(i, l)| [(l.chars().count() as u16 + 2, Some(Hit::PlayerButton(i))), (2, None)])
        .collect();
    register_spans(hits, centre[1], start_x, &parts);

    let ratio = (current_ms as f64 / total_ms as f64).clamp(0.0, 1.0);
    let seeking = app.active_panel == ActivePanel::PlayerProgress;
    let progress_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(7), Constraint::Min(0), Constraint::Length(7)])
        .split(centre[2]);
    let time_style = if seeking { Style::default().fg(p.warn).add_modifier(Modifier::BOLD) } else { muted(p) };
    let fmt = |ms: u64| format!("{:02}:{:02}", ms / 60_000, (ms / 1_000) % 60);
    f.render_widget(Paragraph::new(fmt(current_ms)).style(time_style).alignment(Alignment::Right), progress_row[0]);
    // Striped progress bar: static "|||||" (alternating light / dark accent)
    // while paused, "/////" stripes drifting to the right while playing. The
    // phase follows the playback position, so it only moves when music does.
    let gauge_rect = Rect { x: progress_row[1].x + 1, width: progress_row[1].width.saturating_sub(2), ..progress_row[1] };
    let bar_w = gauge_rect.width as usize;
    let filled = ((ratio * bar_w as f64).round() as usize).min(bar_w);
    let light = if seeking { p.warn } else { p.accent };
    let dark = darken(light, 0.45);
    let phase = (current_ms / 110) as i64;
    let mut cells: Vec<Span<'static>> = Vec::with_capacity(bar_w);
    for i in 0..bar_w {
        if i < filled {
            // Box-drawing glyphs share the same cell-filling metrics, unlike "/" and "|".
            let (glyph, bright) = if app.is_playing {
                ("╱", (i as i64 - phase).rem_euclid(4) < 2)
            } else {
                ("│", i % 2 == 0)
            };
            cells.push(Span::styled(glyph, Style::default().fg(if bright { light } else { dark }).add_modifier(Modifier::BOLD)));
        } else {
            cells.push(Span::styled("─", Style::default().fg(p.dim)));
        }
    }
    f.render_widget(Paragraph::new(Line::from(cells)), gauge_rect);
    // Generous click target: the whole progress row and the empty row under it.
    let progress_hit = Rect {
        x: gauge_rect.x,
        y: gauge_rect.y,
        width: gauge_rect.width,
        height: (centre[3].height.min(1) + 1).max(1),
    };
    hit(hits, progress_hit, Hit::Progress);
    f.render_widget(Paragraph::new(fmt(total_ms)).style(time_style).alignment(Alignment::Left), progress_row[2]);

    // Right: volume + quality
    let vol_selected = app.active_panel == ActivePanel::PlayerInfo && app.player_info_index == 0;
    let qual_selected = app.active_panel == ActivePanel::PlayerInfo && app.player_info_index == 1;
    let sel = |on: bool, p: &Palette| {
        if on {
            Style::default().fg(Color::Black).bg(p.accent).add_modifier(Modifier::BOLD)
        } else {
            muted(p)
        }
    };
    let vol_bar: String = {
        let filled = (app.volume as usize * 10 / 100).min(10);
        format!("{}{}", "━".repeat(filled), "─".repeat(10 - filled))
    };
    let info = Paragraph::new(vec![
        Line::from(Span::styled(format!(" 🔊 {vol_bar} {:>3}% ", app.volume), sel(vol_selected, p))),
        Line::from(Span::styled(format!(" {} ", quality_label(quality)), sel(qual_selected, p))),
    ])
    .alignment(Alignment::Right)
    .block(panel(None, app.active_panel == ActivePanel::PlayerInfo, p).padding(Padding::new(1, 1, 1, 0)));
    f.render_widget(info, player_bar[2]);
    let info_inner = panel(None, false, p).padding(Padding::new(1, 1, 1, 0)).inner(player_bar[2]);
    hit(hits, player_bar[2], Hit::Focus(ActivePanel::PlayerInfo));
    hit(hits, Rect { height: 1, ..info_inner }, Hit::VolumeField);
    hit(hits, Rect { y: info_inner.y + 1, height: 1, ..info_inner }, Hit::QualityField);

    protocol_art_rect
}
