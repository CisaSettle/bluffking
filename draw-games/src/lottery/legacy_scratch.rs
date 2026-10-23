//! Account-purge compatibility for retired scratch checkpoints.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

const PRIZES: [i64; 6] = [1, 1, 2, 2, 5, 10];
#[derive(Clone, Serialize, Deserialize)]
struct Ticket {
    cost: i64,
    lucky: u8,
    numbers: [u8; 6],
    revealed: [bool; 6],
}
impl Ticket {
    fn prize(&self) -> i64 {
        self.numbers
            .iter()
            .zip(PRIZES)
            .filter(|(n, _)| **n == self.lucky)
            .map(|(_, m)| m * self.cost)
            .sum()
    }
    fn done(&self) -> bool {
        self.revealed.iter().all(|v| *v)
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Room {
    #[serde(default)]
    throws: crate::game_hub::throws::RoomThrows,
    #[serde(default)]
    social_seq: u64,
    #[serde(default)]
    social_events: Vec<Value>,
    code: String,
    shop: Option<Uuid>,
    customer: Option<Uuid>,
    bot: bool,
    shop_balance: i64,
    customer_balance: i64,
    revision: u64,
    closed: bool,
    tickets: Vec<Ticket>,
    settled: bool,
    sold: u64,
    revenue: i64,
    prizes: i64,
}
impl Room {
    fn settle(&mut self) {
        if !self.settled && self.tickets.iter().all(Ticket::done) {
            let prize: i64 = self.tickets.iter().map(Ticket::prize).sum();
            self.shop_balance -= prize;
            self.customer_balance += prize;
            self.prizes += prize;
            self.settled = true;
        }
    }
}

/// Account deletion follows ordinary scratch departure: settle outstanding
/// tickets and close, retaining the other player's balances and readable result.
pub(crate) fn purge_account_state(value: Value, user: Uuid) -> Result<Value, serde_json::Error> {
    let mut room: Room = serde_json::from_value(value)?;
    if room.shop == Some(user) || room.customer == Some(user) {
        for ticket in &mut room.tickets {
            ticket.revealed.fill(true);
        }
        room.settle();
        if room.shop == Some(user) {
            room.shop = None;
        }
        if room.customer == Some(user) {
            room.customer = None;
        }
        room.closed = true;
        room.throws = Default::default();
        room.social_events.clear();
        room.revision += 1;
    }
    serde_json::to_value(room)
}
