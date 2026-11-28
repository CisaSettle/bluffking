use serde::{Deserialize, Serialize};

pub mod fixed_array {
    use super::*;
    pub fn serialize<T: Serialize, S: serde::Serializer, const N: usize>(
        value: &[T; N],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.as_slice().serialize(serializer)
    }
    pub fn deserialize<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>, const N: usize>(
        deserializer: D,
    ) -> Result<[T; N], D::Error> {
        Vec::<T>::deserialize(deserializer)?
            .try_into()
            .map_err(|_| serde::de::Error::custom("invalid board size"))
    }
}

pub fn chess_reason_value(value: &str) -> Option<&'static str> {
    match value {
        "checkmate" => Some("checkmate"),
        "stalemate" => Some("stalemate"),
        "perpetual_check" => Some("perpetual_check"),
        "resign" => Some("resign"),
        "timeout" => Some("timeout"),
        "abandon" => Some("abandon"),
        "move_limit" => Some("move_limit"),
        "repetition" => Some("repetition"),
        "agreement" => Some("agreement"),
        "material" => Some("material"),
        _ => None,
    }
}
