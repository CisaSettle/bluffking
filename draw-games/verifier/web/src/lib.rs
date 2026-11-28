use serde_json::{json, Value};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn verify_draw(document: &str, receipt: Option<String>) -> String {
    let result = verify(document, receipt.as_deref());
    match result {
        Ok(verified) => verified.to_string(),
        Err(reason) => json!({"outcome": "rejected", "reason": reason}).to_string(),
    }
}

fn verify(document: &str, receipt: Option<&str>) -> Result<Value, String> {
    if document.len() + receipt.map_or(0, str::len) > 1_048_576 {
        return Err("Proof and receipt exceed 1 MiB".into());
    }
    let document: Value = serde_json::from_str(document).map_err(|error| error.to_string())?;
    let receipt = receipt
        .map(serde_json::from_str::<Value>)
        .transpose()
        .map_err(|error| error.to_string())?;
    if receipt.as_ref().is_some_and(|value| !value.is_object()) {
        return Err("Receipt must be a JSON object".into());
    }
    draw_games::document::verify_document(&document, receipt.as_ref())
}
