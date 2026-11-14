//! Private dealer key and encrypted shuffle shared by all proof manifests.
use crate::crypto_real::{
    decrypt::{partial_decrypt, ThresholdDecryptionProof},
    dkg::DkgParty,
    ec::Ct,
    shuffle::Shuffle,
};
use curve25519_dalek::{
    constants::RISTRETTO_BASEPOINT_POINT, ristretto::RistrettoPoint, scalar::Scalar,
};
use rand_core::{CryptoRng, RngCore};

pub(crate) struct LockedShuffle {
    key: Scalar,
    pub public_key: RistrettoPoint,
    pub shuffle: Shuffle,
}
impl LockedShuffle {
    pub fn new<R: RngCore + CryptoRng>(key: Scalar, input: Vec<Ct>, rng: &mut R) -> Self {
        let public_key = key * RISTRETTO_BASEPOINT_POINT;
        let shuffle = Shuffle::perform(input, &public_key, rng);
        Self {
            key,
            public_key,
            shuffle,
        }
    }

    /// Sorted unique final positions, physical indices and their fresh DLEQ proofs.
    pub fn openings<R: RngCore + CryptoRng>(
        &self,
        order: &[usize],
        positions: &[usize],
        party_id: String,
        scheme: &str,
        rng: &mut R,
    ) -> Option<Vec<(usize, usize, ThresholdDecryptionProof)>> {
        let mut positions = positions.to_vec();
        positions.sort_unstable();
        positions.dedup();
        let party = DkgParty {
            party_id,
            x_i: self.key,
            q_i: self.public_key,
        };
        positions
            .into_iter()
            .map(|position| {
                let index = *order.get(position)?;
                let card = *self.shuffle.pi.get(index)?;
                let ct = self.shuffle.output.get(index)?;
                Some((
                    position,
                    card,
                    ThresholdDecryptionProof {
                        scheme: scheme.into(),
                        shares: vec![partial_decrypt(&party, position as u32, ct, rng)],
                    },
                ))
            })
            .collect()
    }
}
