//! The large probability matrices live in read-only executable pages. Each
//! exact dealt stack vector loads only its node/edge directory on first use.
use super::{Blueprint, Edge, Move, Node, CLASSES};
use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{LazyLock, OnceLock},
};

#[repr(align(4))]
struct Aligned<const N: usize>([u8; N]);
static DATA: Aligned<{ include_bytes!(concat!(env!("OUT_DIR"), "/stacks.bin")).len() }> =
    Aligned(*include_bytes!(concat!(env!("OUT_DIR"), "/stacks.bin")));

struct Entry {
    body: &'static [u8],
    policy: OnceLock<Blueprint>,
}

fn take(input: &mut &'static [u8], count: usize) -> &'static [u8] {
    let (value, remaining) = input.split_at(count);
    *input = remaining;
    value
}

fn number(input: &mut &'static [u8]) -> usize {
    u32::from_le_bytes(take(input, 4).try_into().expect("four bytes")) as usize
}

fn weights(input: &mut &'static [u8], count: usize) -> Cow<'static, [f32]> {
    let bytes = take(input, count * 4);
    #[cfg(target_endian = "little")]
    {
        Cow::Borrowed(bytemuck::try_cast_slice(bytes).expect("aligned policy weights"))
    }
    #[cfg(target_endian = "big")]
    {
        Cow::Owned(
            bytes
                .chunks_exact(4)
                .map(|p| f32::from_le_bytes(p.try_into().unwrap()))
                .collect(),
        )
    }
}

fn decode(mut body: &'static [u8], seats: usize) -> Blueprint {
    let root = number(&mut body);
    let node_count = number(&mut body);
    let mut nodes = Vec::with_capacity(node_count);
    for _ in 0..node_count {
        let actor = number(&mut body);
        let count = number(&mut body);
        let posted = (0..seats)
            .map(|_| u16::try_from(number(&mut body)).unwrap())
            .collect();
        let edges = (0..count)
            .map(|_| {
                let play = match number(&mut body) {
                    0 => Move::Fold,
                    1 => Move::Check,
                    2 => Move::Call,
                    raise => Move::Raise(u16::try_from(raise - 3).unwrap()),
                };
                let child = number(&mut body);
                Edge {
                    play,
                    next: (child != u32::MAX as usize).then_some(child),
                }
            })
            .collect();
        nodes.push(Node {
            actor,
            posted,
            reach: weights(&mut body, CLASSES),
            edges,
            strategy: weights(&mut body, count * CLASSES),
        });
    }
    assert!(body.is_empty() && root < nodes.len());
    Blueprint {
        root,
        nodes,
        handoff_calls: Vec::new(),
    }
}

static ENTRIES: LazyLock<HashMap<Vec<u16>, Entry>> = LazyLock::new(|| {
    let mut bytes = DATA.0.as_slice();
    assert_eq!(take(&mut bytes, 4), b"SPF1");
    let mut entries = HashMap::new();
    while !bytes.is_empty() {
        let seats = number(&mut bytes);
        assert!((2..=9).contains(&seats));
        let meta = number(&mut bytes);
        let len = number(&mut bytes);
        let key = (0..seats)
            .map(|_| u16::try_from(number(&mut bytes)).unwrap())
            .collect();
        take(&mut bytes, meta); // solver config, iterations and independent per-seat gaps
        let entry = Entry {
            body: take(&mut bytes, len),
            policy: OnceLock::new(),
        };
        assert!(
            entries.insert(key, entry).is_none(),
            "duplicate stack policy"
        );
    }
    entries
});

pub(super) fn lookup(depths: &[u16]) -> Option<&'static Blueprint> {
    let entry = ENTRIES.get(depths)?;
    Some(
        entry
            .policy
            .get_or_init(|| decode(entry.body, depths.len())),
    )
}

#[cfg(test)]
pub(super) fn profiles() -> impl Iterator<Item = (&'static [u16], &'static Blueprint)> {
    ENTRIES.iter().map(|(depths, entry)| {
        (
            depths.as_slice(),
            entry
                .policy
                .get_or_init(|| decode(entry.body, depths.len())),
        )
    })
}
