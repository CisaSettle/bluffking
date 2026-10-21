//! The dealer's bearer token is not a game-state authority. The game process
//! signs internal operations with a separate in-memory key; the worker only
//! receives its pinned public key. In particular the completion request is
//! signed only after `finish_blind` succeeds. This does not defend against a
//! compromised game process or host administrator.
use super::*;
use axum::{
    body::{to_bytes, Body},
    extract::Request,
    middleware::Next,
    response::Response,
};
use mental_poker::{crypto_real::Ed25519SignatureProvider, signing::SignatureProvider};

const SIGNER: &str = "sealed-game";
const HEADER: &str = "x-sealed-game-signature";
const MAX_BODY: usize = 16 * 1024;

pub(super) struct GameAuthority(Ed25519SignatureProvider);

impl GameAuthority {
    fn from_seed(seed: &[u8; 32]) -> Self {
        Self(Ed25519SignatureProvider::new().with_signer(SIGNER, seed))
    }

    pub(super) fn public_key(&self) -> [u8; 32] {
        hex::decode(self.0.verifying_key_hex(SIGNER).expect("game signing key"))
            .expect("hex public key")
            .try_into()
            .expect("32-byte public key")
    }
}

static AUTHORITY: OnceLock<GameAuthority> = OnceLock::new();

pub(super) fn game_authority() -> &'static GameAuthority {
    AUTHORITY.get_or_init(|| GameAuthority::from_seed(&crate::pf_dealing::os_random_32()))
}

pub fn worker_verifying_key() -> Result<[u8; 32], String> {
    let value = std::env::var("SEALED_DEALER_GAME_VERIFY_KEY")
        .map_err(|_| "SEALED_DEALER_GAME_VERIFY_KEY is required for worker mode")?;
    decode_key(&value)
}

fn decode_key(value: &str) -> Result<[u8; 32], String> {
    hex::decode(value)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| "sealed game key must be 32 bytes encoded as hex".into())
}

pub(super) fn configure_external_authority() -> Result<(), String> {
    // Remote workers must pin the matching public key at boot. The old
    // token-only configuration fails closed; a request cannot register a key.
    let seed = std::env::var("SEALED_DEALER_GAME_SIGNING_KEY")
        .map_err(|_| "SEALED_DEALER_GAME_SIGNING_KEY is required with SEALED_DEALER_URL")?;
    let authority = GameAuthority::from_seed(&decode_key(&seed)?);
    if authority.public_key() != worker_verifying_key()? {
        return Err("sealed dealer game signing and verifying keys do not match".into());
    }
    AUTHORITY
        .set(authority)
        .map_err(|_| "sealed game authority already initialized".to_string())
}

fn claim(method: &str, path: &str, body: &[u8]) -> [u8; 32] {
    mental_poker::hash::ds_hash(
        "sealed:game-command:v1",
        &[method.as_bytes(), path.as_bytes(), body],
    )
}

pub(super) fn sign(request: &mut reqwest::Request) -> Result<(), String> {
    let body = request
        .body()
        .map(|b| b.as_bytes().ok_or("non-replayable dealer body"))
        .transpose()?
        .unwrap_or_default();
    if body.len() > MAX_BODY {
        return Err("dealer request body too large".into());
    }
    let signature = game_authority().0.sign(
        SIGNER,
        &claim(request.method().as_str(), request.url().path(), body),
    );
    request.headers_mut().insert(
        HEADER,
        signature
            .parse()
            .map_err(|_| "invalid game signature header")?,
    );
    Ok(())
}

pub(super) fn verifier(key: &[u8; 32]) -> Result<Ed25519SignatureProvider, std::io::Error> {
    Ed25519SignatureProvider::new()
        .with_verifier(SIGNER, key)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid pinned game verifying key",
            )
        })
}

pub(super) async fn guard(
    State(state): State<WorkerState>,
    request: Request,
    next: Next,
) -> Response {
    if !request.uri().path().starts_with("/internal/") {
        return next.run(request).await;
    }
    if !internal_authorized(request.headers(), &state) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let signature = request
        .headers()
        .get(HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() == 128)
        .map(str::to_owned);
    let Some(signature) = signature else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let (parts, body) = request.into_parts();
    let bytes = match tokio::time::timeout(Duration::from_secs(2), to_bytes(body, MAX_BODY)).await {
        Ok(Ok(bytes)) => bytes,
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    if !state.game_verifier.verify(
        SIGNER,
        &claim(parts.method.as_str(), parts.uri.path(), &bytes),
        &signature,
    ) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}
