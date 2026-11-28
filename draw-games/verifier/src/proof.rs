use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PROTOCOL: &str = "bluffking-draw-v1";

/// SHA-256 of concatenated (u64 big-endian byte length, field bytes) pairs.
/// Callers supply a distinct first field for each purpose.
pub fn sha256_fields(fields: &[&[u8]]) -> [u8; 32] {
    let mut hash = Sha256::new();
    for field in fields {
        hash.update((field.len() as u64).to_be_bytes());
        hash.update(field);
    }
    hash.finalize().into()
}

pub fn seed_bytes(seed: &str) -> Result<[u8; 32], String> {
    if seed.len() != 64
        || !seed
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("Seed must be 32 bytes encoded as lowercase hex".into());
    }
    let mut bytes = [0; 32];
    hex::decode_to_slice(seed, &mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commitment {
    pub protocol: String,
    pub game: String,
    pub draw_id: String,
    pub commitment: String,
}

pub fn commit(game: &str, draw_id: &str, server_seed: &str) -> Result<Commitment, String> {
    if !(matches!(
        game,
        "scratch" | "sicbo" | "sicbo-room" | "yuxiaxie-room" | crate::roulette::ROOM_GAME
    ) || crate::solo::supports(game))
        || draw_id.is_empty()
        || draw_id.len() > 128
        || !draw_id.is_ascii()
    {
        return Err("Invalid commitment context".into());
    }
    let seed = seed_bytes(server_seed)?;
    Ok(Commitment {
        protocol: PROTOCOL.into(),
        game: game.into(),
        draw_id: draw_id.into(),
        commitment: hex::encode(sha256_fields(&[
            b"bluffking-draw-v1/commit",
            game.as_bytes(),
            draw_id.as_bytes(),
            &seed,
        ])),
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reveal {
    #[serde(flatten)]
    pub commitment: Commitment,
    pub server_seed: String,
    pub client_seed: String,
}

impl Reveal {
    pub fn entropy(&self) -> Result<Entropy, String> {
        let expected = commit(
            &self.commitment.game,
            &self.commitment.draw_id,
            &self.server_seed,
        )?;
        if expected != self.commitment {
            return Err("Commitment does not match the revealed seed and context".into());
        }
        let server = seed_bytes(&self.server_seed)?;
        let client = if matches!(
            self.commitment.game.as_str(),
            "sicbo-room" | "yuxiaxie-room" | crate::roulette::ROOM_GAME
        ) && self.client_seed.is_empty()
        {
            // A round no player wagered on folds in no player seed.
            Vec::new()
        } else {
            seed_bytes(&self.client_seed)?.to_vec()
        };
        Ok(Entropy::new(sha256_fields(&[
            b"bluffking-draw-v1/outcome",
            self.commitment.game.as_bytes(),
            self.commitment.draw_id.as_bytes(),
            &server,
            &client,
        ])))
    }
}

/// Versioned SHA-256 counter stream. Each block is hashed with a u64 BE counter
/// starting at zero; words are consumed in BE order, four words per block.
pub struct Entropy {
    seed: [u8; 32],
    counter: u64,
    block: [u8; 32],
    offset: usize,
}

impl Entropy {
    pub fn new(seed: [u8; 32]) -> Self {
        Self {
            seed,
            counter: 0,
            block: [0; 32],
            offset: 32,
        }
    }

    fn word(&mut self) -> u64 {
        if self.offset == 32 {
            self.block = sha256_fields(&[
                b"bluffking-draw-v1/stream",
                &self.seed,
                &self.counter.to_be_bytes(),
            ]);
            self.counter = self.counter.checked_add(1).expect("draw stream exhausted");
            self.offset = 0;
        }
        let word = u64::from_be_bytes(self.block[self.offset..self.offset + 8].try_into().unwrap());
        self.offset += 8;
        word
    }

    /// Rejection sampling eliminates modulo bias. `upper` is exclusive.
    pub fn below(&mut self, upper: u64) -> u64 {
        assert!(upper > 0);
        let threshold = upper.wrapping_neg() % upper;
        loop {
            let word = self.word();
            if word >= threshold {
                return word % upper;
            }
        }
    }
}

/// Fold sorted, distinct bettor seeds into one room contribution. Each game
/// has its own domain label and contribution stream.
pub fn dealer_client_seed(game: &str, player_seeds: &[String]) -> Result<String, String> {
    let domain: &[u8] = match game {
        "sicbo-room" => b"bluffking-draw-v1/sicbo-room-players",
        "yuxiaxie-room" => b"bluffking-draw-v1/yuxiaxie-room-players",
        crate::roulette::ROOM_GAME => b"bluffking-draw-v1/roulette-room-players",
        _ => return Err("Unsupported seeded dealer room".into()),
    };
    if player_seeds.is_empty() {
        return Ok(String::new());
    }
    if player_seeds.len() > 8 {
        return Err("Too many dealer-room player seeds".into());
    }
    if !player_seeds.windows(2).all(|pair| pair[0] < pair[1]) {
        return Err("Dealer-room player seeds must be sorted and distinct".into());
    }
    let seeds = player_seeds
        .iter()
        .map(|seed| seed_bytes(seed))
        .collect::<Result<Vec<_>, _>>()?;
    let mut fields = vec![domain];
    fields.extend(seeds.iter().map(|seed| seed.as_slice()));
    Ok(hex::encode(sha256_fields(&fields)))
}

pub(crate) fn dice(reveal: &Reveal, player_seeds: &[String]) -> Result<[u8; 3], String> {
    if reveal.commitment.game == "sicbo" {
        if !player_seeds.is_empty() {
            return Err("Solo Sic Bo lists no room player seeds".into());
        }
    } else if reveal.client_seed != dealer_client_seed(&reveal.commitment.game, player_seeds)? {
        return Err("Room contribution does not match the player seeds".into());
    }
    let mut entropy = reveal.entropy()?;
    Ok(std::array::from_fn(|_| (entropy.below(6) + 1) as u8))
}
