//! Bounded retrieval of the preselected public pulse. No response can select a
//! different round, and an unavailable relay leaves the same draw pending.
use draw_games::beacon::{Beacon, CHAIN_HASH};
use std::{
    collections::BTreeMap,
    sync::OnceLock,
    time::{Duration, Instant},
};

struct Cached {
    checked: Instant,
    beacon: Option<Beacon>,
}

pub async fn fetch(round: u64) -> Result<Option<Beacon>, String> {
    static CACHE: OnceLock<tokio::sync::Mutex<BTreeMap<u64, Cached>>> = OnceLock::new();
    static SLOTS: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    {
        let cache = cache.lock().await;
        if let Some(entry) = cache.get(&round) {
            if entry.beacon.is_some() || entry.checked.elapsed() < Duration::from_secs(5) {
                return Ok(entry.beacon.clone());
            }
        }
    }
    let Ok(_permit) = SLOTS
        .get_or_init(|| tokio::sync::Semaphore::new(1))
        .try_acquire()
    else {
        return Ok(None);
    };
    // Reserve the attempt before awaiting HTTP, bounding retries on every error.
    {
        let mut cache = cache.lock().await;
        if cache.len() >= 64 {
            cache.pop_first();
        }
        cache.insert(
            round,
            Cached {
                checked: Instant::now(),
                beacon: None,
            },
        );
    }
    let result = fetch_once(round).await;
    if let Ok(Some(beacon)) = &result {
        cache.lock().await.insert(
            round,
            Cached {
                checked: Instant::now(),
                beacon: Some(beacon.clone()),
            },
        );
    }
    result
}

async fn fetch_once(round: u64) -> Result<Option<Beacon>, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    let client = CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(4))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(Clone::clone)?;
    let mut response = client
        .get(format!("https://api.drand.sh/{CHAIN_HASH}/public/{round}"))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(format!("beacon relay returned {}", response.status()));
    }
    const MAX_BYTES: usize = 8_192;
    if response
        .content_length()
        .is_some_and(|len| len > MAX_BYTES as u64)
    {
        return Err("beacon response exceeds limit".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        if bytes.len() + chunk.len() > MAX_BYTES {
            return Err("beacon response exceeds limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| "invalid beacon JSON")?;
    let beacon = Beacon {
        round: value["round"].as_u64().ok_or("missing beacon round")?,
        signature: value["signature"]
            .as_str()
            .ok_or("missing beacon signature")?
            .to_owned(),
    };
    if beacon.round != round {
        return Err("relay returned a different beacon round".into());
    }
    draw_games::beacon::verify(&beacon)?;
    Ok(Some(beacon))
}
