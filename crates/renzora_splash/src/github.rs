//! Lightweight GitHub stats fetch (just the repo star count).
//!
//! Runs once on splash startup. The result is cached in a resource so the
//! UI can show "— stars" while it's loading and the real number once it
//! arrives.

use std::sync::{mpsc, Mutex};

use bevy::prelude::*;
use serde::Deserialize;

use renzora::version::REPOSITORY_API as REPO_API;

#[derive(Deserialize)]
struct RepoResponse {
    stargazers_count: u64,
}

#[derive(Resource, Default)]
pub struct GithubStats {
    pub stars: Option<u64>,
    receiver: Option<Mutex<mpsc::Receiver<u64>>>,
    attempted: bool,
}

impl GithubStats {
    pub fn new() -> Self {
        // Plugin construction can outlast the HTTP watchdog. Only the frame
        // poll may launch this optional request, after backend adoption.
        Self::default()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn kick_off() -> Option<mpsc::Receiver<u64>> {
        let (tx, rx) = mpsc::channel();
        match std::thread::Builder::new()
            .name("renzora-splash-stars".into())
            .spawn(move || {
                if let Some(count) = fetch_stars() {
                    let _ = tx.send(count);
                }
            }) {
            Ok(_) => Some(rx),
            Err(error) => {
                warn!("[splash] could not start GitHub stats worker: {error}");
                None
            }
        }
    }

    pub fn poll(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        self.poll_with(renzora_net::is_available(), Self::kick_off);
        #[cfg(target_arch = "wasm32")]
        self.poll_with(false, || None);
    }

    fn poll_with(&mut self, ready: bool, start: impl FnOnce() -> Option<mpsc::Receiver<u64>>) {
        if self.stars.is_some() {
            return;
        }
        if !self.attempted && ready {
            self.attempted = true;
            self.receiver = start().map(Mutex::new);
        }
        let result = self.receiver.as_ref().map(|rx| {
            rx.lock()
                .map_err(|_| mpsc::TryRecvError::Disconnected)
                .and_then(|rx| rx.try_recv())
        });
        match result {
            Some(Ok(count)) => {
                self.stars = Some(count);
                self.receiver = None;
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => self.receiver = None,
            _ => {}
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn fetch_stars() -> Option<u64> {
    let response = renzora_net::Request::get(REPO_API)
        .header("User-Agent", "renzora-splash")
        .header("Accept", "application/vnd.github+json")
        .send()
        .ok()?;
    let parsed: RepoResponse = response.json().ok()?;
    Some(parsed.stargazers_count)
}

/// Formats a star count compactly: 1234 -> "1.2k".
pub fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_backend_then_starts_once_and_polls_without_blocking() {
        let mut stats = GithubStats::new();
        assert!(!stats.attempted);
        for _ in 0..1_000 {
            stats.poll_with(false, || panic!("worker before backend readiness"));
        }
        let (tx, rx) = mpsc::channel();
        stats.poll_with(true, || Some(rx));
        assert!(stats.attempted);
        assert!(stats.stars.is_none());
        stats.poll_with(true, || panic!("duplicate worker"));
        tx.send(1234).unwrap();
        stats.poll_with(false, || panic!("duplicate worker"));
        assert_eq!(stats.stars, Some(1234));
        assert!(stats.receiver.is_none());
    }

    #[test]
    fn failed_worker_is_retired_without_a_retry_storm() {
        let mut stats = GithubStats::new();
        let (tx, rx) = mpsc::channel();
        drop(tx);
        stats.poll_with(true, || Some(rx));
        assert!(stats.receiver.is_none());
        stats.poll_with(true, || panic!("failed request retried every frame"));
        assert!(stats.stars.is_none());
    }
}
