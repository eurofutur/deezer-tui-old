//! Self-update from the latest Codeberg release (Obtainium-style).

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

const API: &str = "https://codeberg.org/api/v1/repos/pyratt/deezer-tui";
/// Binary asset uploaded by `release.sh`.
pub const ASSET: &str = "deezer-tui-x86_64-linux";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Release {
    pub tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// The repo is private: a Codeberg token (read:repository) is needed until it goes public.
fn token(config_token: &str) -> Option<String> {
    std::env::var("CODEBERG_TOKEN")
        .ok()
        .or_else(|| Some(config_token.to_string()))
        .filter(|t| !t.trim().is_empty())
}

fn get(url: &str, token: &Option<String>) -> reqwest::RequestBuilder {
    let req = reqwest::Client::new()
        .get(url)
        .header("User-Agent", concat!("deezer-tui/", env!("CARGO_PKG_VERSION")));
    match token {
        Some(t) => req.header("Authorization", format!("token {}", t.trim())),
        None => req,
    }
}

fn parse_version(s: &str) -> Vec<u64> {
    s.trim_start_matches('v').split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

pub fn is_newer(tag: &str, current: &str) -> bool {
    parse_version(tag) > parse_version(current)
}

/// Latest release if it is newer than the running build.
pub async fn check(config_token: &str) -> Result<Option<Release>> {
    let token = token(config_token);
    let resp = get(&format!("{API}/releases/latest"), &token).send().await?;
    match resp.status().as_u16() {
        404 if token.is_none() => bail!("no access to releases (set codeberg_token in config)"),
        404 => return Ok(None),
        _ => {}
    }
    let release: Release = resp.error_for_status()?.json().await?;
    Ok(is_newer(&release.tag_name, env!("CARGO_PKG_VERSION")).then_some(release))
}

/// Download the release binary and atomically replace the running executable.
pub async fn install(config_token: &str, release: &Release) -> Result<()> {
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == ASSET)
        .ok_or_else(|| anyhow!("release {} has no {ASSET} asset", release.tag_name))?;
    let bytes = get(&asset.browser_download_url, &token(config_token))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    // Guard against saving an HTML login page as the binary.
    if !bytes.starts_with(b"\x7fELF") {
        bail!("downloaded asset is not a Linux binary");
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    let tmp = exe.with_extension("new");
    std::fs::write(&tmp, &bytes).with_context(|| format!("cannot write {}", tmp.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    // rename() over a running binary is safe on Linux: the old inode lives until exit.
    std::fs::rename(&tmp, &exe).with_context(|| format!("cannot replace {}", exe.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn version_order() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("v0.10.0", "0.9.3"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.0.9", "0.1.0"));
    }
}
