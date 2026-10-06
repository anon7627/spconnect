//! Audio keys, with retries.
//!
//! Spotify's access point intermittently refuses a key it hands out a
//! moment later (`error audio key 0 1` in librespot's log), or does not
//! answer in time. A request is therefore tried up to [`ATTEMPTS`] times,
//! [`BACKOFF`] apart, before the track is given up as unavailable.

use std::future::Future;
use std::time::Duration;

use librespot_core::audio_key::AudioKey;
use librespot_core::{FileId, Session, SpotifyId};

pub const ATTEMPTS: u32 = 3;
pub const BACKOFF: Duration = Duration::from_secs(1);

/// Where keys come from: the session, or a fake in tests.
pub trait KeySource {
    fn request_key(&self, track: SpotifyId, file: FileId) -> impl Future<Output = Result<AudioKey, librespot_core::Error>> + Send;
}

impl KeySource for Session {
    fn request_key(&self, track: SpotifyId, file: FileId) -> impl Future<Output = Result<AudioKey, librespot_core::Error>> + Send {
        let session = self.clone();
        async move { session.audio_key().request(track, file).await }
    }
}

/// Every attempt failed.
#[derive(Debug)]
pub struct KeyRefused {
    pub attempts: u32,
    pub last: String,
}

impl std::fmt::Display for KeyRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Spotify refused the audio key of this track {} times in a row ({}); \
             it may play again later",
            self.attempts, self.last
        )
    }
}

impl std::error::Error for KeyRefused {}

pub async fn fetch<K: KeySource>(source: &K, track: SpotifyId, file: FileId, backoff: Duration) -> Result<AudioKey, KeyRefused> {
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        match source.request_key(track, file).await {
            Ok(key) => {
                if attempt > 1 {
                    tracing::info!("audio key granted at attempt {attempt}");
                }
                return Ok(key);
            }
            Err(e) => {
                last = e.to_string();
                if attempt < ATTEMPTS {
                    tracing::info!("audio key refused ({last}), retrying in {}s (attempt {attempt}/{ATTEMPTS})", backoff.as_secs());
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }
    tracing::warn!("audio key refused {ATTEMPTS} times: {last}");
    Err(KeyRefused { attempts: ATTEMPTS, last })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Fails `failures` times, then grants the key.
    struct Flaky {
        failures: Mutex<u32>,
        calls: Mutex<u32>,
    }

    impl KeySource for Flaky {
        fn request_key(&self, _: SpotifyId, _: FileId) -> impl Future<Output = Result<AudioKey, librespot_core::Error>> + Send {
            *self.calls.lock().unwrap() += 1;
            let mut f = self.failures.lock().unwrap();
            let r = if *f > 0 {
                *f -= 1;
                Err(librespot_core::Error::unavailable(anyhow::anyhow!("audio key error")))
            } else {
                Ok(AudioKey([7; 16]))
            };
            async move { r }
        }
    }

    fn flaky(failures: u32) -> Flaky {
        Flaky { failures: Mutex::new(failures), calls: Mutex::new(0) }
    }

    #[tokio::test]
    async fn refusals_are_retried_then_given_up() {
        let ids = (SpotifyId::from_raw(&[1; 16]).unwrap(), FileId::from_raw(&[2; 20]));
        let src = flaky(2);
        assert_eq!(fetch(&src, ids.0, ids.1, Duration::ZERO).await.unwrap(), AudioKey([7; 16]));
        assert_eq!(*src.calls.lock().unwrap(), 3, "granted at the third attempt");

        let src = flaky(5);
        let e = fetch(&src, ids.0, ids.1, Duration::ZERO).await.unwrap_err();
        assert_eq!((e.attempts, *src.calls.lock().unwrap()), (3, 3));
        assert!(e.to_string().contains("refused the audio key"));
    }

    #[tokio::test(start_paused = true)]
    async fn attempts_are_a_second_apart() {
        let ids = (SpotifyId::from_raw(&[1; 16]).unwrap(), FileId::from_raw(&[2; 20]));
        let started = tokio::time::Instant::now();
        fetch(&flaky(2), ids.0, ids.1, BACKOFF).await.unwrap();
        assert_eq!(started.elapsed().as_secs(), 2);
    }
}
