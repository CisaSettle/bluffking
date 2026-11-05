//! Dealing-protocol party identifiers shared by the engine-blind coordinator
//! and clients.

/// Party id for the player in `seat` (`party:<seat>`), as carried in
/// `mp_deal_start` and every `mp_*` message.
pub fn party_id(seat: u8) -> String {
    format!("party:{seat}")
}
