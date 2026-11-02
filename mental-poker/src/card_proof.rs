//! Game-independent encrypted decks and selective zero-knowledge openings.
//!
//! A manifest pins physical card identities, including repeated faces. This
//! layer proves deck composition and openings; game adapters must additionally
//! check deal positions, visibility and the commitment witnessed before play.
//! A server-only shuffle does not prove the operator did not select its seed.
use std::collections::{BTreeMap, BTreeSet};

use curve25519_dalek::{
    constants::RISTRETTO_BASEPOINT_POINT as G, ristretto::RistrettoPoint, scalar::Scalar,
};
use rand::{seq::SliceRandom, SeedableRng};
use rand_chacha::ChaCha20Rng;
use rand_core::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    crypto::{ShuffleProof, ShuffleProofProvider},
    crypto_real::{
        decrypt::{
            partial_decrypt, verify_and_open_point, ThresholdDecryptionProof, SCHEME as OPEN_SCHEME,
        },
        dkg::DkgParty,
        ec::{deck_hash, point_from_hex, point_to_hex, Ct},
        shuffle::{output_deck_from_proof, RealShuffleProofProvider, Shuffle, CARD_SCHEME},
    },
    hash::ds_hash,
};

pub const SCHEME: &str = "card-shuffle-v1";
pub const MAX_BUNDLE_BYTES: usize = 8 * 1024 * 1024;

/// Versioned native physical-card alphabets. Changing an encoding requires a
/// new manifest variant; historical deals must always keep the old meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Game {
    Zjh,
    Niuniu,
    Sangong,
    Shortdeck,
    Ddz,
    Guandan,
    Blackjack,
    Baccarat,
    Colorcards,
}
impl Game {
    pub fn size(self) -> usize {
        match self {
            Self::Shortdeck => 36,
            Self::Ddz => 54,
            Self::Guandan | Self::Colorcards => 108,
            Self::Baccarat => 416,
            _ => 52,
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            Self::Zjh => "zjh",
            Self::Niuniu => "niuniu",
            Self::Sangong => "sangong",
            Self::Shortdeck => "shortdeck",
            Self::Ddz => "ddz",
            Self::Guandan => "guandan",
            Self::Blackjack => "blackjack",
            Self::Baccarat => "baccarat",
            Self::Colorcards => "colorcards",
        }
    }
    pub fn parse(id: &str) -> Option<Self> {
        [
            Self::Zjh,
            Self::Niuniu,
            Self::Sangong,
            Self::Shortdeck,
            Self::Ddz,
            Self::Guandan,
            Self::Blackjack,
            Self::Baccarat,
            Self::Colorcards,
        ]
        .into_iter()
        .find(|g| g.id() == id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Context {
    pub game: Game,
    pub hand_id: Uuid,
    /// Ascending seats, fixed before dealing. A solo player occupies seat 0.
    pub seats: Vec<u8>,
    /// Hash of the original seat-to-human/bot identities, salted per first deal.
    pub roster: String,
    #[serde(default)]
    pub epoch: u32,
    #[serde(default)]
    pub previous: Option<String>,
    /// Colorcards reshuffles exactly the public discard set, excluding its top.
    #[serde(default)]
    pub subset: Option<Vec<u16>>,
}
impl Context {
    pub fn validate(&self) -> Result<(), Error> {
        hex32(&self.roster)?;
        let max = match self.game {
            Game::Zjh | Game::Colorcards => 10,
            Game::Ddz => 3,
            Game::Guandan => 4,
            Game::Baccarat => 1,
            _ => 6,
        };
        if self.seats.is_empty()
            || self.seats.len() > max
            || self.seats.iter().any(|s| usize::from(*s) >= max)
            || self.seats.windows(2).any(|w| w[0] >= w[1])
        {
            return Err(Error::Context);
        }
        match (&self.subset, &self.previous, self.epoch) {
            (None, None, 0) => {}
            (Some(cards), Some(previous), epoch)
                if self.game == Game::Colorcards
                    && epoch > 0
                    && !cards.is_empty()
                    && cards.len() <= self.game.size()
                    && cards.iter().all(|c| usize::from(*c) < self.game.size())
                    && cards.windows(2).all(|w| w[0] < w[1]) =>
            {
                hex32(previous)?;
            }
            _ => return Err(Error::Context),
        }
        Ok(())
    }
    pub fn identities(&self) -> Vec<u16> {
        self.subset
            .clone()
            .unwrap_or_else(|| (0..self.game.size() as u16).collect())
    }
    pub fn size(&self) -> usize {
        self.subset.as_ref().map_or(self.game.size(), Vec::len)
    }
    fn binding(&self) -> [u8; 32] {
        let subset: Vec<u8> = self
            .identities()
            .iter()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        ds_hash(
            "cards:context:v1",
            &[
                self.game.id().as_bytes(),
                self.hand_id.as_bytes(),
                &self.seats,
                self.roster.as_bytes(),
                &self.epoch.to_le_bytes(),
                self.previous.as_deref().unwrap_or("").as_bytes(),
                &subset,
            ],
        )
    }
    fn party(&self) -> String {
        format!("cards:{}", hex::encode(self.binding()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Commitment {
    pub scheme: String,
    pub context: Context,
    pub commitment: String,
}

/// Internal persistence only. Never return this value in an API or state frame.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secret {
    pub context: Context,
    pub master: [u8; 32],
    pub client_seeds: BTreeMap<u8, [u8; 32]>,
}

pub struct LockedDeck {
    context: Context,
    key: Scalar,
    public_key: RistrettoPoint,
    shuffle: Shuffle,
    commit: [u8; 32],
    // Baccarat never restricts its eight-deck shoe during rebind. Retain the
    // ciphertext digest prepared off-thread instead of compressing 832 points
    // again on the HTTP task for every offer. Context binding remains fresh.
    baccarat_deck_hash: Option<[u8; 32]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Opening {
    pub position: usize,
    /// Physical identity: 0..manifest size, never merely a repeated face.
    pub card: u16,
    pub proof: ThresholdDecryptionProof,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub commitment: Commitment,
    pub public_key: String,
    pub client_seeds: BTreeMap<u8, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commitments: Option<BTreeMap<u8, String>>,
    pub shuffle_proof: ShuffleProof,
    pub openings: Vec<Opening>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verified {
    pub commitment: Commitment,
    pub client_seeds: BTreeMap<u8, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commitments: Option<BTreeMap<u8, String>>,
    pub cards: BTreeMap<usize, u16>,
}
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid card-proof context")]
    Context,
    #[error("invalid card-proof encoding")]
    Encoding,
    #[error("unsupported card-proof scheme")]
    Scheme,
    #[error("card-proof commitment mismatch")]
    Commitment,
    #[error("invalid card shuffle proof")]
    Shuffle,
    #[error("invalid or duplicate card position")]
    Position,
    #[error("invalid card opening")]
    Opening,
}

fn message(card: u16) -> RistrettoPoint {
    Scalar::from(u64::from(card) + 1) * G
}
fn input(context: &Context) -> Vec<Ct> {
    context
        .identities()
        .into_iter()
        .map(|id| Ct::trivial(&message(id)))
        .collect()
}
fn restrict(shuffle: &mut Shuffle, context: &Context) {
    let Some(subset) = &context.subset else {
        return;
    };
    let chosen: Vec<_> = shuffle
        .pi
        .iter()
        .enumerate()
        .filter_map(|(i, physical)| {
            subset
                .binary_search(&(*physical as u16))
                .ok()
                .map(|index| (i, index))
        })
        .collect();
    shuffle.input = input(context);
    shuffle.output = chosen.iter().map(|(i, _)| shuffle.output[*i]).collect();
    shuffle.rho = chosen.iter().map(|(i, _)| shuffle.rho[*i]).collect();
    shuffle.pi = chosen.iter().map(|(_, index)| *index).collect();
}
fn commit_of(context: &Context, public_key: &RistrettoPoint, deck: &[Ct]) -> [u8; 32] {
    commit_of_deck_hash(context, public_key, &deck_hash(deck))
}
fn commit_of_deck_hash(
    context: &Context,
    public_key: &RistrettoPoint,
    deck_hash: &[u8; 32],
) -> [u8; 32] {
    ds_hash(
        "cards:commit:v1",
        &[
            &context.binding(),
            public_key.compress().as_bytes(),
            deck_hash,
        ],
    )
}
fn random(domain: &str, master: &[u8; 32], context: &Context) -> ChaCha20Rng {
    ChaCha20Rng::from_seed(ds_hash(domain, &[master, context.game.id().as_bytes()]))
}
fn order(
    context: &Context,
    commit: &[u8; 32],
    seeds: &BTreeMap<u8, [u8; 32]>,
) -> Result<Vec<usize>, Error> {
    if seeds.keys().any(|s| !context.seats.contains(s)) {
        return Err(Error::Context);
    }
    let bytes: Vec<u8> = seeds
        .iter()
        .flat_map(|(seat, seed)| std::iter::once(*seat).chain(seed.iter().copied()))
        .collect();
    let mut rng = ChaCha20Rng::from_seed(ds_hash(
        "cards:order:v1",
        &[&context.binding(), commit, &bytes],
    ));
    let mut positions: Vec<usize> = (0..context.size()).collect();
    positions.shuffle(&mut rng);
    Ok(positions)
}
impl LockedDeck {
    pub fn new(context: Context, master: &[u8; 32]) -> Result<Self, Error> {
        context.validate()?;
        let mut rng = random("cards:key:v1", master, &context);
        let mut key = Scalar::random(&mut rng);
        while key == Scalar::ZERO {
            key = Scalar::random(&mut rng);
        }
        let public_key = key * G;
        let full_input = (0..context.game.size())
            .map(|id| Ct::trivial(&message(id as u16)))
            .collect();
        let mut shuffle = Shuffle::perform(
            full_input,
            &public_key,
            &mut random("cards:shuffle:v1", master, &context),
        );
        restrict(&mut shuffle, &context);
        let baccarat_deck_hash =
            (context.game == Game::Baccarat).then(|| deck_hash(&shuffle.output));
        let commit = match &baccarat_deck_hash {
            Some(hash) => commit_of_deck_hash(&context, &public_key, hash),
            None => commit_of(&context, &public_key, &shuffle.output),
        };
        Ok(Self {
            context,
            key,
            public_key,
            shuffle,
            commit,
            baccarat_deck_hash,
        })
    }
    /// Bind a precomputed deck to its actual hand and original roster. The key
    /// and secret shuffle depend on the fresh master and manifest, not routing.
    pub fn rebind(&mut self, context: Context) -> Result<(), Error> {
        context.validate()?;
        if context.game != self.context.game || self.shuffle.input.len() != context.game.size() {
            return Err(Error::Context);
        }
        restrict(&mut self.shuffle, &context);
        self.commit = match &self.baccarat_deck_hash {
            Some(hash) => commit_of_deck_hash(&context, &self.public_key, hash),
            None => commit_of(&context, &self.public_key, &self.shuffle.output),
        };
        self.context = context;
        Ok(())
    }
    pub fn commitment(&self) -> Commitment {
        Commitment {
            scheme: SCHEME.into(),
            context: self.context.clone(),
            commitment: hex::encode(self.commit),
        }
    }
    pub fn cards(&self, seeds: &BTreeMap<u8, [u8; 32]>) -> Result<Vec<u16>, Error> {
        let ids = self.context.identities();
        Ok(order(&self.context, &self.commit, seeds)?
            .iter()
            .map(|&i| ids[self.shuffle.pi[i]])
            .collect())
    }
    pub fn bundle<R: RngCore + CryptoRng>(
        &self,
        seeds: &BTreeMap<u8, [u8; 32]>,
        positions: &[usize],
        rng: &mut R,
    ) -> Result<Bundle, Error> {
        let order = order(&self.context, &self.commit, seeds)?;
        let positions: BTreeSet<_> = positions.iter().copied().collect();
        if positions.iter().any(|p| *p >= order.len()) {
            return Err(Error::Position);
        }
        let party = DkgParty {
            party_id: self.context.party(),
            x_i: self.key,
            q_i: self.public_key,
            blind: Scalar::ZERO,
        };
        let ids = self.context.identities();
        let openings = positions
            .into_iter()
            .map(|position| {
                let index = order[position];
                Opening {
                    position,
                    card: ids[self.shuffle.pi[index]],
                    proof: ThresholdDecryptionProof {
                        scheme: OPEN_SCHEME.into(),
                        shares: vec![partial_decrypt(
                            &party,
                            position as u32,
                            &self.shuffle.output[index],
                            rng,
                        )],
                    },
                }
            })
            .collect();
        Ok(Bundle {
            commitment: self.commitment(),
            commitments: None,
            public_key: point_to_hex(&self.public_key),
            client_seeds: seeds.iter().map(|(s, v)| (*s, hex::encode(v))).collect(),
            shuffle_proof: self
                .shuffle
                .prove_cards(&self.context.party(), 0, rng)
                .ok_or(Error::Shuffle)?,
            openings,
        })
    }
}

fn hex32(value: &str) -> Result<[u8; 32], Error> {
    let mut bytes = [0; 32];
    hex::decode_to_slice(value, &mut bytes).map_err(|_| Error::Encoding)?;
    if hex::encode(bytes) != value {
        return Err(Error::Encoding);
    }
    Ok(bytes)
}
/// The caller must compare this result to its pre-deal receipt and game-specific
/// position layout. Self-consistency alone does not prove pre-deal publication.
pub fn verify(bundle: &Bundle) -> Result<Verified, Error> {
    if bundle.commitment.scheme != SCHEME || bundle.shuffle_proof.scheme != CARD_SCHEME {
        return Err(Error::Scheme);
    }
    let context = &bundle.commitment.context;
    context.validate()?;
    if let Some(commitments) = &bundle.commitments {
        for (seat, commitment) in commitments {
            if !context.seats.contains(seat) {
                return Err(Error::Context);
            }
            hex32(commitment)?;
        }
        for (seat, seed) in &bundle.client_seeds {
            let seed = hex32(seed)?;
            if commitments.get(seat)
                != Some(&hex::encode(crate::pf2_zjh::seed_commitment(
                    &seed,
                    context.hand_id,
                    *seat,
                )))
            {
                return Err(Error::Commitment);
            }
        }
    }
    // Bound parsing/work even for a caller that bypassed the JSON entrypoint.
    if bundle.shuffle_proof.attestation.len() > MAX_BUNDLE_BYTES
        || bundle.openings.len() > context.size()
    {
        return Err(Error::Encoding);
    }
    let commit = hex32(&bundle.commitment.commitment)?;
    hex32(&bundle.public_key)?;
    let key = point_from_hex(&bundle.public_key).ok_or(Error::Encoding)?;
    let seeds: BTreeMap<_, _> = bundle
        .client_seeds
        .iter()
        .map(|(s, seed)| Ok((*s, hex32(seed)?)))
        .collect::<Result<_, Error>>()?;
    let order = order(context, &commit, &seeds)?;
    let deck = output_deck_from_proof(&bundle.shuffle_proof).ok_or(Error::Shuffle)?;
    if deck.len() != context.size() || commit_of(context, &key, &deck) != commit {
        return Err(Error::Commitment);
    }
    if !RealShuffleProofProvider::verifier_with_expected_key(&bundle.public_key).verify_shuffle(
        &context.party(),
        0,
        &deck_hash(&input(context)),
        &deck_hash(&deck),
        Some(&bundle.public_key),
        &bundle.shuffle_proof,
    ) {
        return Err(Error::Shuffle);
    }
    let parties = [(context.party(), key)];
    let mut cards = BTreeMap::new();
    for opening in &bundle.openings {
        let index = *order.get(opening.position).ok_or(Error::Position)?;
        if usize::from(opening.card) >= context.game.size() {
            return Err(Error::Opening);
        }
        let point = verify_and_open_point(
            opening.position as u32,
            &deck[index],
            &parties,
            &opening.proof,
        )
        .map_err(|_| Error::Opening)?;
        if point != message(opening.card) {
            return Err(Error::Opening);
        }
        if cards.insert(opening.position, opening.card).is_some() {
            return Err(Error::Position);
        }
    }
    Ok(Verified {
        commitment: bundle.commitment.clone(),
        client_seeds: bundle.client_seeds.clone(),
        commitments: bundle.commitments.clone(),
        cards,
    })
}

/// Apply newly revealed player entropy to a private prepared deck without
/// repeating curve work. Both permutations are derived from public inputs.
pub fn reorder_cards(
    commitment: &Commitment,
    cards: &[u16],
    old: &BTreeMap<u8, [u8; 32]>,
    new: &BTreeMap<u8, [u8; 32]>,
) -> Result<Vec<u16>, Error> {
    let commit = hex32(&commitment.commitment)?;
    if cards.len() != commitment.context.size() {
        return Err(Error::Position);
    }
    let before = order(&commitment.context, &commit, old)?;
    let after = order(&commitment.context, &commit, new)?;
    let mut inverse = vec![0; cards.len()];
    for (position, index) in before.into_iter().enumerate() {
        inverse[index] = position;
    }
    Ok(after
        .into_iter()
        .map(|index| cards[inverse[index]])
        .collect())
}
