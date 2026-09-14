//! Cached HTTP fetch: `If-Modified-Since`, a sanity gate on the payload, and a cache that is
//! RETURNED (never a hard exit) when the network or the upstream misbehaves. An unattended job
//! must survive a truncated upstream file; the python prototype did not.

use anyhow::{anyhow, Result};
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    Live { url: String },
    NotModified { url: String },
    Cached { url: String, why: String },
}

impl Source {
    pub fn url(&self) -> &str {
        match self {
            Source::Live { url } | Source::NotModified { url } | Source::Cached { url, .. } => url,
        }
    }
    pub fn is_live(&self) -> bool {
        !matches!(self, Source::Cached { .. })
    }
}

pub struct Fetched {
    pub text: String,
    pub source: Source,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(90))
        .user_agent(crate::UA)
        .build()
}

fn cache_mtime(cache: &Path) -> Option<u64> {
    std::fs::metadata(cache)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!(
        "tmp-{}",
        std::process::id()
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Fetch `url` into `cache`. `min_lines` is the sanity gate: a shorter body is treated as a
/// failed fetch and the cache is used instead. Returns the text and where it came from.
pub fn fetch_cached(url: &str, cache: &Path, min_lines: usize) -> Result<Fetched> {
    let cached = || -> Option<String> { std::fs::read_to_string(cache).ok() };
    let mut req = agent().get(url);
    if let Some(mt) = cache_mtime(cache) {
        req = req.set("If-Modified-Since", &crate::time::httpdate(mt));
    }
    let attempt: Result<Fetched> = match req.call() {
        Ok(resp) if resp.status() == 304 => match cached() {
            Some(text) => Ok(Fetched { text, source: Source::NotModified { url: url.into() } }),
            None => Err(anyhow!("304 but no cache")),
        },
        Ok(resp) => {
            let text = resp.into_string()?;
            let lines = text.lines().count();
            if lines < min_lines {
                Err(anyhow!("suspiciously short body: {lines} lines (< {min_lines})"))
            } else {
                write_atomic(cache, text.as_bytes())?;
                Ok(Fetched { text, source: Source::Live { url: url.into() } })
            }
        }
        Err(ureq::Error::Status(304, _)) => match cached() {
            Some(text) => Ok(Fetched { text, source: Source::NotModified { url: url.into() } }),
            None => Err(anyhow!("304 but no cache")),
        },
        Err(ureq::Error::Status(code, _)) => Err(anyhow!("HTTP {code}")),
        Err(e) => Err(anyhow!("{e}")),
    };
    match attempt {
        Ok(f) => Ok(f),
        Err(why) => match cached() {
            Some(text) => Ok(Fetched { text, source: Source::Cached { url: url.into(), why: why.to_string() } }),
            None => Err(anyhow!("{url}: {why}, and no cached copy at {}", cache.display())),
        },
    }
}

/// Try several URLs in order (mirrors / back-issues); first success wins.
pub fn fetch_first(urls: &[String], cache: &Path, min_lines: usize) -> Result<Fetched> {
    let mut last = anyhow!("no urls");
    for u in urls {
        match fetch_cached(u, cache, min_lines) {
            Ok(f) if f.source.is_live() || urls.len() == 1 => return Ok(f),
            Ok(f) => {
                // a cached copy for this url is acceptable only once every mirror failed
                last = anyhow!("{}: served from cache", u);
                let _ = f;
            }
            Err(e) => last = e,
        }
    }
    // fall back to the cache regardless of which url filled it
    if let Ok(text) = std::fs::read_to_string(cache) {
        return Ok(Fetched { text, source: Source::Cached { url: urls[0].clone(), why: last.to_string() } });
    }
    Err(last)
}
