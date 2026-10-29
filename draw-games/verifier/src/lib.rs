//! Deterministic production rules and a network-free, independently runnable verifier.
pub mod beacon;
pub mod document;
pub mod lottery;
pub mod proof;
pub mod scratch;
pub mod sicbo;
pub mod zjh;

pub fn verify(proof: &serde_json::Value) -> Result<serde_json::Value, String> {
    match proof.get("kind").and_then(serde_json::Value::as_str) {
        Some("lottery") => lottery::verify(proof),
        Some("scratch") => scratch::verify(proof),
        Some("sicbo" | "sicbo-room") => sicbo::verify(proof),
        Some("zjh") => zjh::verify(proof),
        _ => Err("Unknown proof kind or unsupported legacy record".into()),
    }
}
