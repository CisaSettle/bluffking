//! Salted, position-bound Merkle openings. Domains keep the games' proofs separate.
use crate::proof::{seed_bytes, sha256_fields};
use serde::{Deserialize, Serialize};

pub(crate) struct Domain {
    pub salt: &'static [u8],
    pub card: &'static [u8],
    pub node: &'static [u8],
    pub padding: &'static [u8],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Opening {
    pub index: usize,
    pub card: u8,
    salt: String,
    path: Vec<String>,
}

fn salt(domain: &Domain, secret: &[u8; 32], index: usize) -> [u8; 32] {
    sha256_fields(&[domain.salt, secret, &(index as u64).to_be_bytes()])
}

fn leaf(domain: &Domain, id: &str, index: usize, card: u8, salt: &[u8; 32]) -> [u8; 32] {
    sha256_fields(&[
        domain.card,
        id.as_bytes(),
        &(index as u64).to_be_bytes(),
        &[card],
        salt,
    ])
}

pub struct Tree {
    nodes: Vec<[u8; 32]>,
}

impl Tree {
    pub(crate) fn new(
        domain: &Domain,
        depth: usize,
        id: &str,
        cards: impl Iterator<Item = u8>,
        secret: &[u8; 32],
    ) -> Self {
        let leaves = 1 << depth;
        let mut nodes = vec![[0; 32]; leaves * 2];
        let mut cards = cards;
        for (index, node) in nodes[leaves..].iter_mut().enumerate() {
            *node = match cards.next() {
                Some(card) => leaf(domain, id, index, card, &salt(domain, secret, index)),
                None => {
                    sha256_fields(&[domain.padding, id.as_bytes(), &(index as u64).to_be_bytes()])
                }
            };
        }
        for index in (1..leaves).rev() {
            nodes[index] = sha256_fields(&[domain.node, &nodes[2 * index], &nodes[2 * index + 1]]);
        }
        Self { nodes }
    }

    pub fn root(&self) -> [u8; 32] {
        self.nodes[1]
    }

    pub(crate) fn open(
        &self,
        domain: &Domain,
        index: usize,
        card: u8,
        secret: &[u8; 32],
    ) -> Opening {
        let mut node = self.nodes.len() / 2 + index;
        let mut path = Vec::new();
        while node > 1 {
            path.push(hex::encode(self.nodes[node ^ 1]));
            node /= 2;
        }
        Opening {
            index,
            card,
            salt: hex::encode(salt(domain, secret, index)),
            path,
        }
    }
}

impl Opening {
    pub(crate) fn verifies(
        &self,
        domain: &Domain,
        depth: usize,
        id: &str,
        root: &[u8; 32],
    ) -> bool {
        if self.path.len() != depth || self.index >= (1 << depth) {
            return false;
        }
        let Ok(salt) = seed_bytes(&self.salt) else {
            return false;
        };
        let mut node = leaf(domain, id, self.index, self.card, &salt);
        for (depth, sibling) in self.path.iter().enumerate() {
            let Ok(sibling) = seed_bytes(sibling) else {
                return false;
            };
            node = if self.index & (1 << depth) == 0 {
                sha256_fields(&[domain.node, &node, &sibling])
            } else {
                sha256_fields(&[domain.node, &sibling, &node])
            };
        }
        node == *root
    }
}
