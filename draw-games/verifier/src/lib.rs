//! Deterministic production rules and a network-free, independently runnable verifier.
pub mod baccarat;
pub mod beacon;
pub mod board;
pub mod claw;
pub mod document;
pub mod evidence;
pub mod fruit;
pub mod lottery;
pub mod proof;
pub mod puzzle2048;
pub mod roulette;
pub mod scratch;
pub mod sicbo;
pub mod slots;
pub mod solo;
pub mod yuxiaxie;

pub fn verify(proof: &serde_json::Value) -> Result<serde_json::Value, String> {
    match proof.get("kind").and_then(serde_json::Value::as_str) {
        Some("baccarat-shoe") => baccarat::verify(proof),
        Some("board-replay") => board::verify(proof),
        Some("lottery") => lottery::verify(proof),
        Some("scratch") => scratch::verify(proof),
        Some(
            "sicbo" | "sicbo-room" | sicbo::VOID_KIND | sicbo::ACCEPTANCE_KIND | sicbo::ROSTER_KIND,
        ) => sicbo::verify(proof),
        Some("yuxiaxie-room" | yuxiaxie::BEACON_GAME) => yuxiaxie::verify(proof),
        Some(kind) if solo::supports(kind) => solo::verify(proof),
        _ => Err("Unknown proof kind".into()),
    }
}
