#![allow(dead_code)]

use anyhow::{anyhow, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, COOKIE, USER_AGENT};
use serde_json::{json, Value};
use std::collections::HashSet;
use crate::config::AudioQuality;

const GATEWAY_URL: &str = "https://www.deezer.com/ajax/gw-light.php";
const MEDIA_URL_API: &str = "https://media.deezer.com/v1/get_url";

#[derive(Clone, Debug)]
pub struct DeezerClient {
    http: reqwest::Client,
    arl: String,
    session_id: String,
    license_token: String,
    api_token: Option<String>,
    user_id: Option<String>,
    loved_tracks_id: Option<String>,
}

/// One entry of the user's Deezer listening history (most recent first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: String,
    pub title: String,
    pub artist: String,
    /// Unix timestamp (seconds) of the listen, as reported by Deezer.
    pub ts: u64,
}

pub fn cover_url(hash: &str, size: u32) -> Option<String> {
    (!hash.is_empty()).then(|| {
        format!("https://e-cdns-images.dzcdn.net/images/cover/{hash}/{size}x{size}-000000-80-0-0.jpg")
    })
}

pub fn artist_picture_url(hash: &str, size: u32) -> Option<String> {
    (!hash.is_empty()).then(|| {
        format!("https://e-cdns-images.dzcdn.net/images/artist/{hash}/{size}x{size}-000000-80-0-0.jpg")
    })
}

fn picture_url(kind: &str, hash: &str, size: u32) -> Option<String> {
    (!hash.is_empty()).then(|| {
        format!("https://e-cdns-images.dzcdn.net/images/{kind}/{hash}/{size}x{size}-000000-80-0-0.jpg")
    })
}

/// Deezer sends ids either as strings or as numbers depending on the endpoint.
fn str_or_num(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|n| n.to_string()))
}

fn num(value: &Value) -> u64 {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

fn year_of(value: &Value) -> String {
    value
        .as_str()
        .filter(|s| s.len() >= 4)
        .map(|s| s[..4].to_owned())
        .unwrap_or_default()
}

fn strip_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_owned()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtistTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub duration_secs: u64,
    pub cover_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtistAlbum {
    pub id: String,
    pub title: String,
    pub year: String,
    pub kind: String,
    pub cover_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtistRef {
    pub id: String,
    pub name: String,
    pub nb_fan: u64,
    pub picture_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistRef {
    pub id: String,
    pub title: String,
    pub nb_fan: u64,
    pub cover_url: Option<String>,
}

/// Everything shown on an artist page (mirrors Deezer's own artist view).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtistPage {
    pub id: String,
    pub name: String,
    pub nb_fan: u64,
    pub picture_url: Option<String>,
    pub top_tracks: Vec<ArtistTrack>,
    pub albums: Vec<ArtistAlbum>,
    pub related: Vec<ArtistRef>,
    pub playlists: Vec<PlaylistRef>,
    pub bio: String,
    pub latest_release: Option<String>,
}

/// An album with its track list (gateway `deezer.pageAlbum`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlbumPage {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub artist_id: Option<String>,
    pub cover_url: Option<String>,
    pub tracks: Vec<(String, String, String)>,
}

/// A card of a Deezer web page (Home, Explore, a genre channel…).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowseItem {
    /// album, playlist, artist, smarttracklist, flow, channel or track.
    pub kind: String,
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub picture_url: Option<String>,
    /// Page path for channels ("channels/alternative").
    pub target: String,
    /// "dd/mm/yyyy" for albums.
    pub released: String,
    pub explicit: bool,
    /// Tile colour of channels.
    pub color: Option<(u8, u8, u8)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowseSection {
    pub title: String,
    /// "See all" page path, when the web shows that button.
    pub more: Option<String>,
    /// Wrapped grid (Explore categories) instead of a one-row carousel.
    pub grid: bool,
    pub items: Vec<BrowseItem>,
}

/// A page of the web app (gateway `page.get`), as stacked sections.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowsePage {
    pub path: String,
    pub title: String,
    pub sections: Vec<BrowseSection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryAlbum {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub year: String,
    pub cover_url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TrackMetadata {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub artist_id: Option<String>,
    pub album_id: Option<String>,
    pub track_token: String,
    pub duration_secs: Option<u64>,
    pub album_art_url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PlaylistMetadata {
    pub id: String,
    pub title: String,
    pub tracks: Vec<TrackMetadata>,
}

impl DeezerClient {
    pub fn new(arl: impl Into<String>) -> Result<Self> {
        let arl = arl.into();

        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36",
            ),
        );
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!("arl={arl}"))
                .context("failed to build Deezer cookie header")?,
        );

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .cookie_store(true)
            .build()
            .context("failed to build reqwest client")?;

        Ok(Self {
            http,
            arl,
            session_id: String::new(),
            license_token: String::new(),
            api_token: None,
            user_id: None,
            loved_tracks_id: None,
        })
    }

    pub fn arl(&self) -> &str {
        &self.arl
    }

    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    pub fn loved_tracks_id(&self) -> Option<&str> {
        self.loved_tracks_id.as_deref()
    }

    pub async fn fetch_api_token(&mut self) -> Result<String> {
        let url = format!(
            "{GATEWAY_URL}?method=deezer.getUserData&api_version=1.0&api_token="
        );
        let response_text = self
            .http
            .post(&url)
            .header(reqwest::header::COOKIE, format!("arl={}", self.arl))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body("{}")
            .send()
            .await
            .context("gateway request failed for method deezer.getUserData")?
            .error_for_status()
            .context("gateway returned an error for method deezer.getUserData")?
            .text()
            .await
            .context("failed to read Deezer getUserData response body")?;

        let response = serde_json::from_str::<Value>(&response_text)
            .context("failed to decode Deezer getUserData response")?;

        self.session_id = response["results"]["SESSION_ID"]
            .as_str()
            .map(ToOwned::to_owned)
            .or_else(|| response["results"]["SESSION_ID"].as_i64().map(|id| id.to_string()))
            .unwrap_or_default();

        self.license_token = response["results"]["USER"]["OPTIONS"]["license_token"]
            .as_str()
            .unwrap_or_default()
            .to_owned();

        self.user_id = response["results"]["USER"]["USER_ID"]
            .as_i64()
            .map(|id| id.to_string())
            .or_else(|| response["results"]["USER"]["USER_ID"].as_str().map(ToOwned::to_owned));

        self.loved_tracks_id = response["results"]["USER"]["LOVEDTRACKS_ID"]
            .as_i64()
            .map(|id| id.to_string())
            .or_else(|| {
                response["results"]["USER"]["LOVEDTRACKS_ID"]
                    .as_str()
                    .map(ToOwned::to_owned)
            });

        let token = response["results"]["checkForm"]
            .as_str()
            .ok_or_else(|| anyhow!("gateway response did not contain checkForm api token"))?
            .to_owned();

        self.api_token = Some(token.clone());
        Ok(token)
    }

    pub async fn fetch_track_metadata(&self, track_id: &str) -> Result<TrackMetadata> {
        let api_token = self
            .api_token
            .as_deref()
            .ok_or_else(|| anyhow!("api token not loaded; call fetch_api_token first"))?;

        let url = format!(
            "{GATEWAY_URL}?method=deezer.pageTrack&api_version=1.0&api_token={api_token}"
        );
        let payload = json!({ "sng_id": track_id }).to_string();

        let text = self
            .http
            .post(&url)
            .header(reqwest::header::COOKIE, format!("arl={}; sid={}", self.arl, self.session_id))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(payload)
            .send()
            .await
            .context("gateway request failed for method deezer.pageTrack")?
            .error_for_status()
            .context("gateway returned an error for method deezer.pageTrack")?
            .text()
            .await?;

        let response = serde_json::from_str::<Value>(&text)
            .context("failed to decode Deezer pageTrack response")?;

        let result = response["results"].get("DATA").unwrap_or(&response["results"]);
        let track_token = match result["TRACK_TOKEN"].as_str() {
            Some(value) => value.to_owned(),
            None => return Err(anyhow!("track metadata missing TRACK_TOKEN")),
        };

        let title = result["SNG_TITLE"]
            .as_str()
            .or_else(|| response["results"]["SNG_TITLE"].as_str())
            .unwrap_or("Unknown track")
            .to_owned();

        let artist = result["ART_NAME"]
            .as_str()
            .or_else(|| response["results"]["ART_NAME"].as_str())
            .unwrap_or("Unknown artist")
            .to_owned();

        let duration_secs = result["DURATION"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| result["DURATION"].as_u64());

        let album_art_url = result["ALB_PICTURE"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|hash| format!("https://e-cdns-images.dzcdn.net/images/cover/{hash}/500x500-000000-80-0-0.jpg"));

        Ok(TrackMetadata {
            id: result["SNG_ID"]
                .as_str()
                .unwrap_or(track_id)
                .to_owned(),
            title,
            artist,
            artist_id: str_or_num(&result["ART_ID"]),
            album_id: str_or_num(&result["ALB_ID"]),
            track_token,
            duration_secs,
            album_art_url,
        })
    }

    pub async fn fetch_media_url(&self, track_token: &str, quality: AudioQuality) -> Result<String> {
        if self.license_token.is_empty() {
            return Err(anyhow!("license token not loaded; call fetch_api_token first"));
        }

        let format = match quality {
            AudioQuality::Kbps128 => "MP3_128",
            AudioQuality::Kbps320 => "MP3_320",
            AudioQuality::Flac => "FLAC",
        };

        let payload = json!({
            "license_token": self.license_token.clone(),
            "media": [{
                "type": "FULL",
                "formats": [{ "cipher": "BF_CBC_STRIPE", "format": format }]
            }],
            "track_tokens": [track_token]
        });

        let response = self
            .http
            .post(MEDIA_URL_API)
            .header(reqwest::header::COOKIE, format!("arl={}; sid={}", self.arl, self.session_id))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&payload)
            .send()
            .await
            .context("media.get_url request failed")?
            .error_for_status()
            .context("media.get_url returned error status")?
            .json::<Value>()
            .await
            .context("failed to decode media.get_url response")?;

        response["data"][0]["media"][0]["sources"][0]["url"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("media.get_url response missing signed source URL"))
    }

    pub async fn fetch_encrypted_bytes_from_signed_url(&self, url: &str) -> Result<Vec<u8>> {
        self.http
            .get(url)
            .send()
            .await
            .context("failed to download encrypted Deezer audio stream")?
            .error_for_status()
            .context("signed CDN request returned an error status")?
            .bytes()
            .await
            .context("failed to read encrypted Deezer audio bytes")
            .map(|bytes| bytes.to_vec())
    }

    pub async fn open_signed_stream(&self, url: &str) -> Result<reqwest::Response> {
        self.http
            .get(url)
            .send()
            .await
            .context("failed to open signed Deezer audio stream")?
            .error_for_status()
            .context("signed Deezer audio stream returned an error status")
    }

    pub async fn fetch_playlist_metadata(&self, playlist_id: &str) -> Result<PlaylistMetadata> {
        let response = self
            .authenticated_gateway_call(
                "deezer.pagePlaylist",
                Some(json!({ "playlist_id": playlist_id, "lang": "en" })),
            )
            .await?;

        let result = &response["results"];
        let title = result["DATA"]["TITLE"]
            .as_str()
            .unwrap_or("Unknown playlist")
            .to_owned();

        let tracks = result["SONGS"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|track| {
                Some(TrackMetadata {
                    id: track["SNG_ID"].as_str()?.to_owned(),
                    title: track["SNG_TITLE"]
                        .as_str()
                        .unwrap_or("Unknown track")
                        .to_owned(),
                    artist: track["ART_NAME"]
                        .as_str()
                        .unwrap_or("Unknown artist")
                        .to_owned(),
                    track_token: track["TRACK_TOKEN"].as_str()?.to_owned(),
                    duration_secs: track["DURATION"]
                        .as_str()
                        .and_then(|s| s.parse().ok())
                        .or_else(|| track["DURATION"].as_u64()),
                    artist_id: str_or_num(&track["ART_ID"]),
                    album_id: str_or_num(&track["ALB_ID"]),
                    album_art_url: track["ALB_PICTURE"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(|hash| format!("https://e-cdns-images.dzcdn.net/images/cover/{hash}/500x500-000000-80-0-0.jpg")),
                })
            })
            .collect();

        Ok(PlaylistMetadata {
            id: playlist_id.to_owned(),
            title,
            tracks,
        })
    }

    async fn authenticated_gateway_call(&self, method: &str, payload: Option<Value>) -> Result<Value> {
        let api_token = self
            .api_token
            .as_deref()
            .ok_or_else(|| anyhow!("api token not loaded; call fetch_api_token first"))?;

        self.gateway_call(method, payload, Some(api_token)).await
    }

    async fn authenticated_gateway_call_with_raw(
        &self,
        method: &str,
        payload: Option<Value>,
    ) -> Result<(Value, String)> {
        let api_token = self
            .api_token
            .as_deref()
            .ok_or_else(|| anyhow!("api token not loaded; call fetch_api_token first"))?;

        self.gateway_call_with_raw(method, payload, Some(api_token)).await
    }

    async fn gateway_call(
        &self,
        method: &str,
        payload: Option<Value>,
        api_token: Option<&str>,
    ) -> Result<Value> {
        let (response, _) = self
            .gateway_call_with_raw(method, payload, api_token)
            .await?;

        Ok(response)
    }

    async fn gateway_call_with_raw(
        &self,
        method: &str,
        payload: Option<Value>,
        api_token: Option<&str>,
    ) -> Result<(Value, String)> {
        let text = self
            .http
            .post(GATEWAY_URL)
            .query(&[
                ("method", method),
                ("api_version", "1.0"),
                ("input", "3"),
                ("api_token", api_token.unwrap_or("null")),
            ])
            .json(&payload.unwrap_or_else(|| json!({})))
            .send()
            .await
            .with_context(|| format!("gateway request failed for method {method}"))?
            .error_for_status()
            .with_context(|| format!("gateway returned an error for method {method}"))?
            .text()
            .await
            .context("failed to read Deezer gateway response body")?;

        let response = serde_json::from_str::<Value>(&text)
            .context("failed to decode Deezer gateway response")?;

        Ok((response, text))
    }

    /// POST to the gateway with the session cookie, like the page* calls above.
    async fn gw_post(&self, method: &str, payload: Value) -> Result<Value> {
        self.gw_post_query(method, &[], payload).await
    }

    async fn gw_post_query(&self, method: &str, query: &[(&str, &str)], payload: Value) -> Result<Value> {
        let api_token = self
            .api_token
            .as_deref()
            .ok_or_else(|| anyhow!("api token not loaded; call fetch_api_token first"))?;
        let json = self
            .http
            .post(format!(
                "{GATEWAY_URL}?method={method}&api_version=1.0&api_token={api_token}"
            ))
            .query(&[("input", "3")])
            .query(query)
            .header(
                reqwest::header::COOKIE,
                format!("arl={}; sid={}", self.arl, self.session_id),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("gateway request failed for {method}"))?
            .error_for_status()
            .with_context(|| format!("gateway returned error for {method}"))?
            .json::<Value>()
            .await
            .with_context(|| format!("failed to decode {method} response"))?;
        if let Some(err) = json.get("error").and_then(|e| e.as_object()).filter(|e| !e.is_empty()) {
            return Err(anyhow!("gateway error for {method}: {}", Value::Object(err.clone())));
        }
        Ok(json)
    }

    /// Artist page: header data, top tracks, discography, similar artists,
    /// related playlists and biography (gateway `deezer.pageArtist`).
    pub async fn fetch_artist_page(&self, artist_id: &str) -> Result<ArtistPage> {
        let json = self
            .gw_post("deezer.pageArtist", json!({ "ART_ID": artist_id, "LANG": "en", "tab": 0 }))
            .await?;
        let r = &json["results"];
        let data = &r["DATA"];

        let top_tracks = r["TOP"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| {
                Some(ArtistTrack {
                    id: str_or_num(&t["SNG_ID"])?,
                    title: t["SNG_TITLE"].as_str()?.to_owned(),
                    artist: t["ART_NAME"].as_str().unwrap_or("Unknown artist").to_owned(),
                    duration_secs: num(&t["DURATION"]),
                    cover_url: t["ALB_PICTURE"].as_str().and_then(|h| cover_url(h, 250)),
                })
            })
            .collect();

        let albums = r["ALBUMS"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| {
                let date = [&a["ORIGINAL_RELEASE_DATE"], &a["PHYSICAL_RELEASE_DATE"], &a["DIGITAL_RELEASE_DATE"]]
                    .into_iter()
                    .map(year_of)
                    .find(|y| !y.is_empty())
                    .unwrap_or_default();
                let kind = match str_or_num(&a["TYPE"]).as_deref() {
                    Some("0") => "Single",
                    Some("1") => "Album",
                    Some("2") => "Compilation",
                    Some("3") => "EP",
                    _ => "",
                };
                Some(ArtistAlbum {
                    id: str_or_num(&a["ALB_ID"])?,
                    title: a["ALB_TITLE"].as_str()?.to_owned(),
                    year: date,
                    kind: kind.to_owned(),
                    cover_url: a["ALB_PICTURE"].as_str().and_then(|h| cover_url(h, 250)),
                })
            })
            .collect();

        let related = r["RELATED_ARTISTS"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| {
                Some(ArtistRef {
                    id: str_or_num(&a["ART_ID"])?,
                    name: a["ART_NAME"].as_str()?.to_owned(),
                    nb_fan: num(&a["NB_FAN"]),
                    picture_url: a["ART_PICTURE"].as_str().and_then(|h| artist_picture_url(h, 250)),
                })
            })
            .collect();

        let playlists = r["RELATED_PLAYLIST"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| {
                let kind = p["PICTURE_TYPE"].as_str().unwrap_or("playlist");
                Some(PlaylistRef {
                    id: str_or_num(&p["PLAYLIST_ID"])?,
                    title: p["TITLE"].as_str()?.to_owned(),
                    nb_fan: num(&p["NB_FAN"]),
                    cover_url: p["PLAYLIST_PICTURE"].as_str().and_then(|h| picture_url(kind, h, 250)),
                })
            })
            .collect();

        let bio_raw = r["BIO"]["BIO"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| r["BIO"]["RESUME"].as_str())
            .unwrap_or("");

        let latest_release = if r["HIGHLIGHT"]["TYPE"].as_str() == Some("album") {
            let item = &r["HIGHLIGHT"]["ITEM"];
            item["ALB_TITLE"].as_str().map(|t| {
                let year = year_of(&item["ORIGINAL_RELEASE_DATE"]);
                if year.is_empty() { t.to_owned() } else { format!("{t} ({year})") }
            })
        } else {
            None
        };

        Ok(ArtistPage {
            id: str_or_num(&data["ART_ID"]).unwrap_or_else(|| artist_id.to_owned()),
            name: data["ART_NAME"].as_str().unwrap_or("Unknown artist").to_owned(),
            nb_fan: num(&data["NB_FAN"]),
            picture_url: data["ART_PICTURE"].as_str().and_then(|h| artist_picture_url(h, 250)),
            top_tracks,
            albums,
            related,
            playlists,
            bio: strip_html(bio_raw),
            latest_release,
        })
    }

    /// Album header and track list (gateway `deezer.pageAlbum`).
    pub async fn fetch_album_page(&self, album_id: &str) -> Result<AlbumPage> {
        let json = self
            .gw_post(
                "deezer.pageAlbum",
                json!({ "ALB_ID": album_id, "LANG": "en", "header": true, "tab": 0 }),
            )
            .await?;
        let data = &json["results"]["DATA"];
        let tracks = json["results"]["SONGS"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| {
                Some((
                    str_or_num(&t["SNG_ID"])?,
                    t["SNG_TITLE"].as_str()?.to_owned(),
                    t["ART_NAME"].as_str().unwrap_or("Unknown artist").to_owned(),
                ))
            })
            .collect();
        Ok(AlbumPage {
            id: str_or_num(&data["ALB_ID"]).unwrap_or_else(|| album_id.to_owned()),
            title: data["ALB_TITLE"].as_str().unwrap_or("Album").to_owned(),
            artist: data["ART_NAME"].as_str().unwrap_or("Unknown artist").to_owned(),
            artist_id: str_or_num(&data["ART_ID"]),
            cover_url: data["ALB_PICTURE"].as_str().and_then(|h| cover_url(h, 250)),
            tracks,
        })
    }

    async fn fetch_profile_tab(&self, user_id: &str, tab: &str, nb: usize) -> Result<Vec<Value>> {
        let profile_id = self
            .user_id
            .as_deref()
            .unwrap_or(user_id)
            .parse::<u64>()
            .unwrap_or(0);
        let json = self
            .gw_post(
                "deezer.pageProfile",
                json!({ "profile_id": profile_id, "user_id": profile_id, "USER_ID": profile_id, "tab": tab, "nb": nb }),
            )
            .await?;
        Ok(json["results"]["TAB"][tab]["data"]
            .as_array()
            .cloned()
            .unwrap_or_default())
    }

    /// Albums the user added to their library.
    pub async fn fetch_favorite_albums(&self, user_id: &str) -> Result<Vec<LibraryAlbum>> {
        Ok(self
            .fetch_profile_tab(user_id, "albums", 500)
            .await?
            .iter()
            .filter_map(|a| {
                Some(LibraryAlbum {
                    id: str_or_num(&a["ALB_ID"])?,
                    title: a["ALB_TITLE"].as_str()?.to_owned(),
                    artist: a["ART_NAME"].as_str().unwrap_or("Unknown artist").to_owned(),
                    year: year_of(&a["PHYSICAL_RELEASE_DATE"]),
                    cover_url: a["ALB_PICTURE"].as_str().and_then(|h| cover_url(h, 250)),
                })
            })
            .collect())
    }

    /// Artists the user follows.
    pub async fn fetch_favorite_artists(&self, user_id: &str) -> Result<Vec<ArtistRef>> {
        Ok(self
            .fetch_profile_tab(user_id, "artists", 500)
            .await?
            .iter()
            .filter_map(|a| {
                Some(ArtistRef {
                    id: str_or_num(&a["ART_ID"])?,
                    name: a["ART_NAME"].as_str()?.to_owned(),
                    nb_fan: num(&a["NB_FAN"]),
                    picture_url: a["ART_PICTURE"].as_str().and_then(|h| artist_picture_url(h, 250)),
                })
            })
            .collect())
    }

    /// Fetch the account's listening history ("Just heard"), most recent first.
    /// Deezer only reports which tracks were played and when, not the position.
    pub async fn fetch_listening_history(&self, user_id: &str, nb: usize) -> Result<Vec<HistoryEntry>> {
        let api_token = self
            .api_token
            .as_deref()
            .ok_or_else(|| anyhow!("api token not loaded; call fetch_api_token first"))?;
        let effective_user_id = self.user_id.as_deref().unwrap_or(user_id);
        let profile_id = effective_user_id.parse::<u64>().unwrap_or(0);
        let payload = json!({
            "profile_id": profile_id,
            "user_id": profile_id,
            "USER_ID": profile_id,
            "tab": "history",
            "nb": nb
        });

        let json = self
            .http
            .post(format!(
                "{GATEWAY_URL}?method=deezer.pageProfile&api_version=1.0&api_token={api_token}"
            ))
            .query(&[("input", "3")])
            .header(
                reqwest::header::COOKIE,
                format!("arl={}; sid={}", self.arl, self.session_id),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&payload)
            .send()
            .await?
            .error_for_status()
            .context("gateway returned error for deezer.pageProfile (history)")?
            .json::<Value>()
            .await
            .context("failed to decode history response")?;

        if let Some(err) = json.get("error").filter(|e| e.is_object() && !e.as_object().unwrap().is_empty()) {
            return Err(anyhow!("gateway error for history: {err}"));
        }

        let entries = json["results"]["TAB"]["history"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let id = item["SNG_ID"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| item["SNG_ID"].as_i64().map(|n| n.to_string()))?;
                let title = item["SNG_TITLE"].as_str()?.to_owned();
                let artist = item["ART_NAME"].as_str().unwrap_or("Unknown artist").to_owned();
                let ts = item["TS"]
                    .as_u64()
                    .or_else(|| item["TS"].as_str().and_then(|s| s.parse().ok()))
                    .unwrap_or(0);
                Some(HistoryEntry { id, title, artist, ts })
            })
            .collect();

        Ok(entries)
    }

    /// A web page ("home", "channels/explore", "channels/module/<id>"…) as the
    /// site renders it. `page.get` only reads its input from the query string.
    pub async fn fetch_page(&self, path: &str) -> Result<BrowsePage> {
        const KINDS: [&str; 7] = ["album", "playlist", "artist", "smarttracklist", "flow", "channel", "track"];
        let input = json!({
            "PAGE": path,
            "VERSION": "2.5",
            "SUPPORT": {
                "grid": KINDS,
                "horizontal-grid": KINDS,
                "slideshow": KINDS,
                "filterable-grid": ["flow"],
            },
            "LANG": "en",
        })
        .to_string();
        let json = self.gw_post_query("page.get", &[("gateway_input", &input)], json!({})).await?;
        let results = &json["results"];
        let sections = results["sections"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| {
                let items: Vec<BrowseItem> = s["items"].as_array()?.iter().filter_map(browse_item).collect();
                (!items.is_empty()).then(|| BrowseSection {
                    title: s["title"].as_str().unwrap_or_default().to_owned(),
                    more: s["related"]["target"].as_str().map(|t| t.trim_start_matches('/').to_owned()),
                    grid: s["layout"].as_str() == Some("grid"),
                    items,
                })
            })
            .collect();
        Ok(BrowsePage {
            path: path.to_owned(),
            title: results["title"].as_str().unwrap_or_default().to_owned(),
            sections,
        })
    }

    /// Tracks of a generated mix ("new-releases", "inspired-by-1"…).
    pub async fn fetch_smart_tracklist(&self, id: &str) -> Result<Vec<(String, String, String)>> {
        let json = self
            .gw_post("smartTracklist.getSongs", json!({ "smartTracklist_id": id, "nb": 100, "start": 0 }))
            .await?;
        Ok(extract_tracks_recursive(&json["results"]["data"], 200))
    }

    /// A batch of Flow (default, or a mood / genre flow like "happy").
    pub async fn fetch_flow(&self, config_id: &str) -> Result<Vec<(String, String, String)>> {
        let user_id = self.user_id().ok_or_else(|| anyhow!("user id not loaded"))?;
        let mut payload = json!({ "user_id": user_id });
        if config_id != "default" {
            payload["config_id"] = json!(config_id);
        }
        let json = self.gw_post("radio.getUserRadio", payload).await?;
        Ok(extract_tracks_recursive(&json["results"]["data"], 200))
    }

    /// Deezer's mix seeded by a track, that track first (gateway `song.getSearchTrackMix`).
    pub async fn fetch_track_mix(&self, track_id: &str) -> Result<Vec<(String, String, String)>> {
        let json = self
            .gw_post("song.getSearchTrackMix", json!({ "sng_id": track_id, "start_with_input_track": true }))
            .await?;
        Ok(extract_tracks_recursive(&json["results"]["data"], 200))
    }

    /// Add one track to a playlist the user owns (gateway `playlist.addSongs`).
    pub async fn add_track_to_playlist(&self, playlist_id: &str, track_id: &str) -> Result<()> {
        let json = self
            .gw_post(
                "playlist.addSongs",
                json!({ "playlist_id": playlist_id, "songs": [[track_id, 0]], "offset": -1 }),
            )
            .await?;
        if json["results"].as_bool() == Some(true) {
            Ok(())
        } else {
            Err(anyhow!("Deezer refused the addition: {}", json["results"]))
        }
    }

    /// (id, title, owned by the user) for every playlist in the library.
    pub async fn fetch_user_playlists(&self, user_id: &str) -> Result<Vec<(String, String, bool)>> {
        let api_token = self
            .api_token
            .as_deref()
            .ok_or_else(|| anyhow!("api token not loaded; call fetch_api_token first"))?;
        let effective_user_id = self.user_id.as_deref().unwrap_or(user_id);
        let profile_id = effective_user_id.parse::<u64>().unwrap_or(0);
        let payload = json!({
            "profile_id": profile_id,
            "user_id": profile_id,
            "USER_ID": profile_id,
            "tab": "playlists",
            "nb": 40
        });

        let json = self
            .http
            .post(format!(
                "{GATEWAY_URL}?method=deezer.pageProfile&api_version=1.0&api_token={api_token}"
            ))
            .query(&[("input", "3")])
            .header(
                reqwest::header::COOKIE,
                format!("arl={}; sid={}", self.arl, self.session_id),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&payload)
            .send()
            .await?;
        let json = json
            .error_for_status()
            .context("gateway returned error for deezer.pageProfile")?
            .json::<Value>()
            .await
            .context("failed to decode playlist response")?;

        let playlists = json["results"]["TAB"]["playlists"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let id = item["PLAYLIST_ID"]
                    .as_str()
                    .map(|s| s.to_string())
                    .or_else(|| item["PLAYLIST_ID"].as_i64().map(|n| n.to_string()))
                    .or_else(|| item["id"].as_str().map(|s| s.to_string()))
                    .or_else(|| item["id"].as_i64().map(|n| n.to_string()))
                    .unwrap_or_else(|| {
                        item["id"]
                            .as_i64()
                            .or_else(|| item["PLAYLIST_ID"].as_i64())
                            .unwrap_or(0)
                            .to_string()
                    });

                let title = item["TITLE"]
                    .as_str()
                    .or(item["title"].as_str())?
                    .to_owned();
                let own = str_or_num(&item["PARENT_USER_ID"]).as_deref() == Some(effective_user_id);
                Some((id, title, own))
            })
            .collect();

        Ok(playlists)
    }

    pub async fn fetch_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<(String, String, String)>> {
        let api_token = self
            .api_token
            .as_deref()
            .ok_or_else(|| anyhow!("api token not loaded; call fetch_api_token first"))?;

        let mut all_tracks: Vec<(String, String, String)> = Vec::new();
        let mut seen_ids: HashSet<String> = HashSet::new();
        let mut start = 0usize;
        let page_size = 200usize;
        let mut total_hint: Option<usize> = None;

        loop {
            let payload = json!({
                "playlist_id": playlist_id,
                "lang": "en",
                "header": true,
                "start": start,
                "nb": page_size
            });

            let raw = self
                .http
                .post(format!(
                    "{GATEWAY_URL}?method=deezer.pagePlaylist&api_version=1.0&api_token={api_token}"
                ))
                .query(&[("input", "3")])
                .header(
                    reqwest::header::COOKIE,
                    format!("arl={}; sid={}", self.arl, self.session_id),
                )
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(&payload)
                .send()
                .await
                .context("gateway request failed for deezer.pagePlaylist")?
                .error_for_status()
                .context("gateway returned error for deezer.pagePlaylist")?
                .text()
                .await
                .context("failed to read playlist tracks response body")?;

            let response = serde_json::from_str::<Value>(&raw)
                .context("failed to decode playlist tracks response")?;

            if !response["error"].is_null()
                && response["error"] != json!([])
                && response["error"] != json!({})
            {
                return Err(anyhow!("playlist gateway error: {}", response["error"]));
            }

            let track_list = response["results"]["SONGS"]["data"]
                .as_array()
                .or_else(|| response["results"]["DATA"]["SONGS"]["data"].as_array())
                .or_else(|| response["results"]["tracks"]["data"].as_array())
                .or_else(|| response["results"]["TRACKS"]["data"].as_array())
                .or_else(|| response["results"]["tracks"].as_array())
                .or_else(|| response["results"]["SONGS"].as_array());

            let current_page_tracks: Vec<(String, String, String)> = track_list
                .into_iter()
                .flatten()
                .filter_map(|track| {
                    let id = track["SNG_ID"]
                        .as_str()
                        .map(|s| s.to_owned())
                        .or_else(|| track["SNG_ID"].as_i64().map(|n| n.to_string()))
                        .or_else(|| track["id"].as_str().map(|s| s.to_owned()))
                        .or_else(|| track["id"].as_i64().map(|n| n.to_string()))?;

                    let title = track["SNG_TITLE"]
                        .as_str()
                        .or(track["title"].as_str())
                        .unwrap_or("Unknown track")
                        .to_owned();

                    let artist = track["ART_NAME"]
                        .as_str()
                        .or(track["artist"]["name"].as_str())
                        .or(track["ARTISTS"][0]["ART_NAME"].as_str())
                        .unwrap_or("Unknown artist")
                        .to_owned();

                    Some((id, title, artist))
                })
                .collect();

            if total_hint.is_none() {
                total_hint = response["results"]["SONGS"]["total"]
                    .as_u64()
                    .map(|n| n as usize)
                    .or_else(|| response["results"]["SONGS"]["count"].as_u64().map(|n| n as usize))
                    .or_else(|| response["results"]["tracks"]["total"].as_u64().map(|n| n as usize));
            }

            for (id, title, artist) in current_page_tracks.iter() {
                if seen_ids.insert(id.clone()) {
                    all_tracks.push((id.clone(), title.clone(), artist.clone()));
                }
            }

            if current_page_tracks.is_empty() || current_page_tracks.len() < page_size {
                break;
            }

            if let Some(total) = total_hint {
                if all_tracks.len() >= total {
                    break;
                }
            }

            start += page_size;
        }

        Ok(all_tracks)
    }

    pub async fn fetch_favorite_tracks(&self) -> Result<Vec<(String, String, String)>> {
        let loved_id = self
            .loved_tracks_id
            .as_deref()
            .ok_or_else(|| anyhow!("LOVEDTRACKS_ID missing; call fetch_api_token first"))?;
        self.fetch_playlist_tracks(loved_id).await
    }

    pub async fn fetch_search_results(
        &self,
        query: &str,
    ) -> Result<(
        Vec<(String, String, String)>,
        Vec<(String, String)>,
        Vec<(String, String)>,
    )> {
        let api_token = self
            .api_token
            .as_deref()
            .ok_or_else(|| anyhow!("api token not loaded; call fetch_api_token first"))?;

        let payload = json!({
            "query": query,
            "QUERY": query,
            "start": 0,
            "nb": 50,
            "suggest": true,
            "artist_suggest": true,
            "top_tracks": true
        });

        let raw = self
            .http
            .post(format!(
                "{GATEWAY_URL}?method=deezer.pageSearch&api_version=1.0&api_token={api_token}"
            ))
            .query(&[("input", "3")])
            .header(
                reqwest::header::COOKIE,
                format!("arl={}; sid={}", self.arl, self.session_id),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&payload)
            .send()
            .await
            .context("gateway request failed for deezer.pageSearch")?
            .error_for_status()
            .context("gateway returned error for deezer.pageSearch")?
            .text()
            .await
            .context("failed to read search response body")?;

        let response = serde_json::from_str::<Value>(&raw)
            .context("failed to decode search response")?;

        if !response["error"].is_null()
            && response["error"] != json!([])
            && response["error"] != json!({})
        {
            return Err(anyhow!("search gateway error: {}", response["error"]));
        }

        let tracks = response["results"]["TRACK"]["data"]
            .as_array()
            .or_else(|| response["results"]["SONGS"]["data"].as_array())
            .or_else(|| response["results"]["tracks"]["data"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|track| {
                let id = track["SNG_ID"]
                    .as_str()
                    .map(|s| s.to_owned())
                    .or_else(|| track["SNG_ID"].as_i64().map(|n| n.to_string()))?;
                let title = track["SNG_TITLE"].as_str()?.to_owned();
                let artist = track["ART_NAME"].as_str()?.to_owned();
                Some((id, title, artist))
            })
            .collect();

        let playlists = response["results"]["PLAYLIST"]["data"]
            .as_array()
            .or_else(|| response["results"]["PLAYLISTS"]["data"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|playlist| {
                let id = playlist["PLAYLIST_ID"]
                    .as_str()
                    .map(|s| s.to_owned())
                    .or_else(|| playlist["PLAYLIST_ID"].as_i64().map(|n| n.to_string()))?;
                let title = playlist["TITLE"]
                    .as_str()
                    .or(playlist["title"].as_str())?
                    .to_owned();
                Some((id, title))
            })
            .collect();

        let artists = response["results"]["ARTIST"]["data"]
            .as_array()
            .or_else(|| response["results"]["ARTISTS"]["data"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|artist| {
                let id = artist["ART_ID"]
                    .as_str()
                    .map(|s| s.to_owned())
                    .or_else(|| artist["ART_ID"].as_i64().map(|n| n.to_string()))?;
                let name = artist["ART_NAME"]
                    .as_str()
                    .or(artist["name"].as_str())?
                    .to_owned();
                Some((id, name))
            })
            .collect();

        Ok((tracks, playlists, artists))
    }
}

/// One card of a `page.get` section; None for kinds the player cannot open
/// (podcasts, videos, external links).
fn browse_item(item: &Value) -> Option<BrowseItem> {
    let kind = item["type"].as_str()?;
    if !["album", "playlist", "artist", "smarttracklist", "flow", "channel", "track"].contains(&kind) {
        return None;
    }
    let data = &item["data"];
    let pic = if kind == "channel" { &item["image_linked_item"] } else { &item["pictures"][0] };
    let picture_url = pic["md5"].as_str().and_then(|md5| picture_url(pic["type"].as_str().unwrap_or("cover"), md5, 250));
    // The web shows the bare artist under albums ("description"), the subtitle elsewhere.
    let subtitle = match kind {
        "album" => item["description"].as_str().or(item["subtitle"].as_str()),
        _ => item["subtitle"].as_str(),
    };
    let released = data["ORIGINAL_RELEASE_DATE"]
        .as_str()
        .or(data["DIGITAL_RELEASE_DATE"].as_str())
        .and_then(|d| Some(format!("{}/{}/{}", d.get(8..10)?, d.get(5..7)?, d.get(..4)?)))
        .unwrap_or_default();
    // 1 = explicit, 4 = partially explicit: both get the EXPLICIT tag on the web.
    let explicit = matches!(num(&data["EXPLICIT_ALBUM_CONTENT"]["EXPLICIT_LYRICS_STATUS"]), 1 | 4);
    let color = item["background_color"]
        .as_str()
        .and_then(|c| u32::from_str_radix(c.trim_start_matches('#'), 16).ok())
        .map(|c| ((c >> 16) as u8, (c >> 8) as u8, c as u8));
    Some(BrowseItem {
        kind: kind.to_owned(),
        id: str_or_num(&item["id"])?,
        title: item["title"].as_str().unwrap_or_default().to_owned(),
        subtitle: subtitle.unwrap_or_default().to_owned(),
        picture_url,
        target: item["target"].as_str().unwrap_or_default().trim_start_matches('/').to_owned(),
        released,
        explicit,
        color,
    })
}

fn extract_tracks_recursive(value: &Value, limit: usize) -> Vec<(String, String, String)> {
    fn walk(
        node: &Value,
        out: &mut Vec<(String, String, String)>,
        seen: &mut HashSet<String>,
        limit: usize,
    ) {
        if out.len() >= limit {
            return;
        }

        match node {
            Value::Object(map) => {
                let id = map
                    .get("SNG_ID")
                    .and_then(|v| v.as_str().map(ToOwned::to_owned).or_else(|| v.as_i64().map(|n| n.to_string())))
                    .or_else(|| map.get("id").and_then(|v| v.as_str().map(ToOwned::to_owned).or_else(|| v.as_i64().map(|n| n.to_string()))));

                let title = map
                    .get("SNG_TITLE")
                    .and_then(Value::as_str)
                    .or_else(|| map.get("title").and_then(Value::as_str));

                let artist = map
                    .get("ART_NAME")
                    .and_then(Value::as_str)
                    .or_else(|| map.get("artist").and_then(|a| a.get("name")).and_then(Value::as_str))
                    .or_else(|| map.get("ARTISTS").and_then(Value::as_array).and_then(|arr| arr.first()).and_then(|a| a.get("ART_NAME")).and_then(Value::as_str));

                if let (Some(id), Some(title), Some(artist)) = (id, title, artist) {
                    if seen.insert(id.clone()) {
                        out.push((id, title.to_owned(), artist.to_owned()));
                        if out.len() >= limit {
                            return;
                        }
                    }
                }

                for child in map.values() {
                    walk(child, out, seen, limit);
                    if out.len() >= limit {
                        break;
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, out, seen, limit);
                    if out.len() >= limit {
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    walk(value, &mut out, &mut seen, limit);
    out
}
