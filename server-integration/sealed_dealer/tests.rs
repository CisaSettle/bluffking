use super::*;
use futures::{SinkExt, StreamExt};

#[test]
fn sealed_hand_reveals_only_in_stage_order() {
    let hand_id = Uuid::new_v4();
    let (mut hand, tickets) = SealedHand::new(hand_id, vec![0, 1, 2]).expect("valid hand");
    assert_eq!(tickets.len(), 3);
    assert!(hand.reveal_board(engine::Street::Turn).is_err());
    let flop = hand.reveal_board(engine::Street::Flop).expect("flop");
    assert_eq!(hand.reveal_board(engine::Street::Flop).unwrap(), flop);
    assert!(hand.reveal_board(engine::Street::River).is_err());
    let turn = hand.reveal_board(engine::Street::Turn).expect("turn");
    assert_eq!(hand.reveal_board(engine::Street::Turn).unwrap(), turn);
    let river = hand.reveal_board(engine::Street::River).expect("river");
    assert_eq!((flop.len(), turn.len(), river.len()), (3, 1, 1));
    let shown = hand.reveal_showdown(vec![0, 2]).expect("showdown");
    assert_eq!(shown.len(), 2);
    assert_eq!(hand.reveal_showdown(vec![2, 0]).unwrap(), shown);
    assert!(
        hand.reveal_showdown(vec![0, 1]).is_err(),
        "retry cannot add a folded seat"
    );
    assert_eq!(hand.reveal_board(engine::Street::Flop).unwrap(), flop);
    assert_eq!(hand.stage, DealerStage::Showdown);
    let mut all = flop;
    all.extend(turn);
    all.extend(river);
    all.extend(shown.iter().flat_map(|entry| entry.cards));
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), 9, "revealed board and holes must be unique");
}

#[test]
fn sealed_hand_accepts_sparse_seats_and_rejects_duplicates() {
    let id = Uuid::new_v4();
    let (hand, _) = SealedHand::new(id, vec![0, 2]).expect("sparse physical seats");
    assert!(hand.hole_cards(0).is_some());
    assert!(hand.hole_cards(2).is_some());
    assert!(hand.hole_cards(1).is_none());
    assert!(SealedHand::new(id, vec![0, 0]).is_err());
}

async fn spawn_worker() -> (
    SealedDealerClient,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral port");
    let addr = probe.local_addr().expect("local addr");
    drop(probe);
    let token = "test-internal-token".to_string();
    let worker = tokio::spawn(run_worker(addr, token.clone()));
    let http = reqwest::Client::new();
    for _ in 0..50 {
        if http
            .get(format!("http://{addr}/healthz"))
            .send()
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (
        SealedDealerClient::new(format!("http://{addr}"), token),
        worker,
    )
}

/// ADR-064 on the sealed dealer: the deck is fixed by the commit plus the
/// players' seeds, players can reproduce it from the settlement reveal,
/// and the worker refuses to deal from seeds it never committed against.
#[tokio::test]
async fn committed_deal_is_reproducible_from_the_reveal() {
    let (client, worker) = spawn_worker().await;
    let hand_id = Uuid::new_v4();
    let seed_commit = client.commit_hand(hand_id).await.unwrap();
    assert_eq!(
        client.commit_hand(hand_id).await.unwrap(),
        seed_commit,
        "replay keeps the commit"
    );
    let seeds = BTreeMap::from([(0u8, [0xA1; 32]), (3u8, [0xB2; 32])]);
    let request = CreateHandRequest {
        hand_id,
        seats: vec![0, 3, 5],
        bot_seats: Vec::new(),
        client_seeds: seeds.clone(),
        test_seed: None,
    };
    let deal = client.create_hand(&request).await.unwrap();
    assert_eq!(
        client.create_hand(&request).await.unwrap(),
        deal,
        "replay keeps the deal"
    );
    let mut other = request.clone();
    other.client_seeds.insert(5, [0xC3; 32]);
    assert!(
        client.create_hand(&other).await.is_err(),
        "seeds cannot change after the deal"
    );

    let deck = client.settle(hand_id).await.unwrap();
    validate_final_deck(hand_id, &deal.commitment, &deck).unwrap();
    let pf = verified_reveal(hand_id, seed_commit, &deck).unwrap();
    assert_eq!(pf.client_seeds, seeds);
    // An independent verifier reproduces the deal from the revealed seeds.
    let card = |id: u8| mental_poker::card_id::id_to_card(id).unwrap();
    let record = mental_poker::pf::HandRecord {
        hand_id: hand_id.to_string(),
        server_seed: hex::encode(pf.server_seed),
        seed_commit: hex::encode(seed_commit),
        client_seeds: seeds
            .iter()
            .map(|(s, v)| (s.to_string(), hex::encode(v)))
            .collect(),
        num_players: 3,
        dealt_seats: vec![0, 3, 5],
        hole_cards: BTreeMap::from([(
            "3".to_string(),
            vec![card(deck.cards[1]), card(deck.cards[4])],
        )]),
        community: deck.cards[6..11].iter().map(|id| card(*id)).collect(),
    };
    mental_poker::pf::verify_hand(&record).expect("revealed deal verifies");

    // A revealed seed set that differs from the dealt one fails closed.
    let mut forged = deck.clone();
    forged.client_seeds.remove(&3);
    assert!(verified_reveal(hand_id, seed_commit, &forged).is_err());
    assert!(verified_reveal(hand_id, [0; 32], &deck).is_err());

    // Seeds without a prior commit are refused rather than ignored.
    let uncommitted = CreateHandRequest {
        hand_id: Uuid::new_v4(),
        ..request
    };
    assert!(client.create_hand(&uncommitted).await.is_err());
    worker.abort();
}

#[test]
fn settlement_guard_rejects_cards_not_at_their_deck_positions() {
    let mut hand = engine::GameHand::new_blind(
        vec![
            (engine::PlayerId(1), engine::Chips(100), 0),
            (engine::PlayerId(2), engine::Chips(100), 3),
        ],
        0,
        engine::Chips(20),
        engine::Chips(10),
        [0; 32],
    );
    hand.start().unwrap();
    let deck = deck_ids([7; 32]);
    let card = |id: u8| mental_poker::card_id::id_to_card(id).unwrap();
    hand.apply_action(engine::PlayerId(1), engine::PlayerAction::AllIn)
        .unwrap();
    hand.apply_action(engine::PlayerId(2), engine::PlayerAction::Call)
        .unwrap();
    while let Some(street) = hand.pending_board_street() {
        let range = match street {
            engine::Street::Flop => 4..7,
            engine::Street::Turn => 7..8,
            _ => 8..9,
        };
        let cards: Vec<_> = deck[range].iter().map(|id| card(*id)).collect();
        hand.inject_board_for_street(&cards).unwrap();
    }
    for (pid, p) in [(1, 0usize), (2, 1)] {
        hand.inject_showdown_reveal(
            engine::PlayerId(pid),
            engine::HoleCards::new(card(deck[p + 2]), card(deck[p])),
        )
        .unwrap();
    }
    let result = hand.finish_blind().unwrap();
    let seat_of = |pid: engine::PlayerId| Some(if pid.0 == 1 { 0 } else { 3 });
    check_settled_cards(&[0, 3], &deck, &result, seat_of).unwrap();
    let mut swapped = deck.clone();
    swapped.swap(4, 9);
    assert!(check_settled_cards(&[0, 3], &swapped, &result, seat_of).is_err());
    swapped = deck.clone();
    swapped.swap(1, 20);
    assert!(check_settled_cards(&[0, 3], &swapped, &result, seat_of).is_err());
}

#[tokio::test]
async fn worker_delivers_holes_directly_and_opens_deck_only_at_settlement() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral port");
    let addr = probe.local_addr().expect("local addr");
    drop(probe);
    let token = "test-internal-token".to_string();
    let worker = tokio::spawn(run_worker(addr, token.clone()));
    let http = reqwest::Client::new();
    let health = format!("http://{addr}/healthz");
    for _ in 0..50 {
        if http.get(&health).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Public health alone cannot admit a worker with a stale token.
    let ready = format!("http://{addr}/internal/readyz");
    for stale_token in [None, Some("previous-worker-token")] {
        let mut request = http.get(&ready);
        if let Some(stale_token) = stale_token {
            request = request.header(INTERNAL_TOKEN_HEADER, stale_token);
        }
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        http.get(&ready)
            .header(INTERNAL_TOKEN_HEADER, &token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );

    let client = SealedDealerClient::new(format!("http://{addr}"), token);
    let (mut oversized, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("connect unauthenticated socket");
    oversized
        .send(tokio_tungstenite::tungstenite::Message::Text(
            "x".repeat(513),
        ))
        .await
        .expect("send oversized ticket");
    let reply = tokio::time::timeout(Duration::from_secs(1), oversized.next())
        .await
        .expect("oversized ticket must close promptly");
    assert!(!matches!(
        reply,
        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_)))
    ));
    let (mut idle, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("connect idle unauthenticated socket");
    let closed =
        tokio::time::timeout(HOLE_DELIVERY_TIMEOUT + Duration::from_secs(1), idle.next())
            .await
            .expect("idle connection must release its admission slot");
    assert!(!matches!(
        closed,
        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_)))
    ));
    let hand_id = Uuid::new_v4();
    let deal = client
        .create_hand(&CreateHandRequest {
            hand_id,
            seats: vec![0, 3],
            bot_seats: Vec::new(),
            client_seeds: Default::default(),
            #[cfg(any(test, feature = "test-support"))]
            test_seed: None,
        })
        .await
        .expect("create sealed hand");
    let retry_request = CreateHandRequest {
        hand_id,
        seats: vec![3, 0], // Request ordering does not change seat ownership.
        bot_seats: Vec::new(),
        client_seeds: Default::default(),
        #[cfg(any(test, feature = "test-support"))]
        test_seed: None,
    };
    let (retry_a, retry_b) = tokio::join!(
        client.create_hand(&retry_request),
        client.create_hand(&retry_request),
    );
    assert_eq!(retry_a.unwrap(), deal);
    assert_eq!(retry_b.unwrap(), deal);
    for conflicting in [
        CreateHandRequest {
            seats: vec![0, 2],
            ..retry_request.clone()
        },
        CreateHandRequest {
            bot_seats: vec![0],
            ..retry_request.clone()
        },
    ] {
        assert!(client.create_hand(&conflicting).await.is_err());
    }

    assert!(!client.delivery_received(hand_id, 3).await.unwrap());
    assert!(client.delivery_received(hand_id, 8).await.is_err());
    let ticket = deal
        .tickets
        .iter()
        .find(|ticket| ticket.seat == 3)
        .expect("seat ticket");
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("connect direct dealer websocket");
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({"ticket": ticket.ticket}).to_string(),
        ))
        .await
        .expect("send ticket");
    let raw = socket
        .next()
        .await
        .expect("delivery frame")
        .expect("valid frame")
        .into_text()
        .expect("text frame");
    let delivery: HoleDelivery = serde_json::from_str(&raw).expect("hole delivery");
    assert_eq!((delivery.hand_id, delivery.seat), (hand_id, 3));
    assert_ne!(delivery.cards[0], delivery.cards[1]);
    assert_eq!(delivery.commitment, deal.commitment);
    assert!(
        client.delivery_received(hand_id, 3).await.unwrap(),
        "delivered player cannot demand a refund by reporting failure"
    );
    // The same socket serves the next hand; an unknown ticket gets an
    // error frame, and repeated unknown tickets close the socket.
    let next_hand = Uuid::new_v4();
    let next = client
        .create_hand(&CreateHandRequest {
            hand_id: next_hand,
            seats: vec![0, 3],
            bot_seats: Vec::new(),
            client_seeds: Default::default(),
            #[cfg(any(test, feature = "test-support"))]
            test_seed: None,
        })
        .await
        .expect("create second sealed hand");
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({"ticket": next.tickets.iter().find(|t| t.seat == 3).unwrap().ticket}).to_string(),
        ))
        .await
        .expect("send second ticket on the same socket");
    let raw = socket.next().await.unwrap().unwrap().into_text().unwrap();
    let second: HoleDelivery = serde_json::from_str(&raw).expect("second delivery");
    assert_eq!((second.hand_id, second.seat), (next_hand, 3));
    for n in 0..HOLE_MAX_MISSES {
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::json!({"ticket": "nope"}).to_string(),
            ))
            .await
            .expect("send unknown ticket");
        let reply = tokio::time::timeout(Duration::from_secs(1), socket.next())
            .await
            .expect("unknown ticket answered promptly");
        if n + 1 < HOLE_MAX_MISSES {
            let text = reply.unwrap().unwrap().into_text().unwrap();
            assert!(text.contains("unknown_ticket"));
        } else {
            assert!(!matches!(
                reply,
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_)))
            ));
        }
    }
    client.delete_hand(next_hand).await;

    assert!(client
        .reveal_board(hand_id, engine::Street::Turn)
        .await
        .is_err());
    assert_eq!(
        client
            .reveal_board(hand_id, engine::Street::Flop)
            .await
            .expect("flop")
            .len(),
        3
    );
    // Even a delayed replay after the flop preserves delivery and street
    // state; the following turn would fail if creation reset this hand.
    assert_eq!(client.create_hand(&retry_request).await.unwrap(), deal);
    assert!(client.delivery_received(hand_id, 3).await.unwrap());
    client
        .reveal_board(hand_id, engine::Street::Turn)
        .await
        .expect("turn");
    client
        .reveal_board(hand_id, engine::Street::River)
        .await
        .expect("river");
    assert_eq!(
        client
            .reveal_showdown(hand_id, vec![0, 3])
            .await
            .expect("showdown")
            .len(),
        2
    );
    let opened = client.settle(hand_id).await.expect("settled deck");
    assert_eq!(opened.cards.len(), 52);
    let mut commitment = Sha256::new();
    commitment.update(b"bluffking:sealed-deck:v1");
    commitment.update(hand_id.as_bytes());
    commitment.update(opened.seed);
    commitment.update(opened.nonce);
    commitment.update(&opened.cards);
    assert_eq!(hex::encode(commitment.finalize()), deal.commitment);

    // Bot tables use opaque engine slots too. Only the bot's own cards can be fetched.
    let bot_hand_id = Uuid::new_v4();
    let mut hand = engine::GameHand::new_blind(
        vec![
            (engine::PlayerId(1), engine::Chips(100), 0),
            (engine::PlayerId(2), engine::Chips(100), 3),
        ],
        0,
        engine::Chips(20),
        engine::Chips(10),
        [0; 32],
    );
    hand.start().unwrap();
    let active = ActiveHand::create_with(
        bot_hand_id,
        &hand,
        &std::collections::HashSet::from([3]),
        &HashMap::new(),
        client.clone(),
        format!("ws://{addr}/ws"),
        Some(3),
        None,
    )
    .await
    .unwrap();
    assert!(hand.seats().iter().all(|s| s.hole_cards().is_none()));
    assert_eq!(active.bot_holes.len(), 1);
    assert!(
        client.bot_hole(bot_hand_id, 0).await.is_err(),
        "human holes cannot be obtained through bot API"
    );
    let wire = serde_json::to_value(&active.deal).unwrap();
    assert!(wire.get("recovery").is_none());
    assert!(wire.get("cards").is_none());
    hand.apply_action(engine::PlayerId(1), engine::PlayerAction::AllIn)
        .unwrap();
    hand.apply_action(engine::PlayerId(2), engine::PlayerAction::Call)
        .unwrap();
    active.advance(&mut hand).await.unwrap();
    let (result, pf) = active.finish(&mut hand).await.unwrap();
    assert!(pf.is_none(), "a pinned test deck is never revealed");
    assert_eq!(result.final_stacks.values().sum::<u32>(), 200);
    assert_eq!(result.board.count(), 5);
    assert_eq!(completed_holes(&result).len(), 2);
    active.delete().await;

    worker.abort();
}

/// A mid-hand joiner can bind a seat a fill bot is playing, or a seat a dealt
/// human vacated. Neither seat's ticket may reach them: redeeming it would
/// expose a live opponent's hole cards.
/// Prod 2026-10-14: idle hole sockets outlive a wait between hands, so only
/// a VALID ticket may extend a socket's lifetime.
#[tokio::test]
async fn hole_socket_lifetime_is_extended_only_by_valid_tickets() {
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral port");
    let addr = probe.local_addr().expect("local addr");
    drop(probe);
    let token = "test-internal-token".to_string();
    let worker = tokio::spawn(run_worker(addr, token.clone()));
    let http = reqwest::Client::new();
    for _ in 0..50 {
        if http
            .get(format!("http://{addr}/healthz"))
            .send()
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let client = SealedDealerClient::new(format!("http://{addr}"), token);
    let hand_id = Uuid::new_v4();
    let deal = client
        .create_hand(&CreateHandRequest {
            hand_id,
            seats: vec![0, 3],
            bot_seats: Vec::new(),
            client_seeds: Default::default(),
            #[cfg(any(test, feature = "test-support"))]
            test_seed: None,
        })
        .await
        .expect("create sealed hand");
    let ticket = deal
        .tickets
        .iter()
        .find(|t| t.seat == 3)
        .unwrap()
        .ticket
        .clone();
    let redeem = serde_json::json!({ "ticket": ticket }).to_string();

    // An unknown ticket is answered but does not extend the first-ticket
    // deadline: the socket still closes within HOLE_DELIVERY_TIMEOUT.
    let (mut stranger, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("connect");
    let started = tokio::time::Instant::now();
    tokio::time::sleep(HOLE_DELIVERY_TIMEOUT / 2).await;
    stranger
        .send(WsMessage::Text(r#"{"ticket":"nope"}"#.into()))
        .await
        .expect("send unknown ticket");
    let reply = stranger.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(reply.contains("unknown_ticket"));
    let closed = tokio::time::timeout(HOLE_DELIVERY_TIMEOUT, async {
        loop {
            match stranger.next().await {
                Some(Ok(WsMessage::Text(_))) => panic!("no further frames expected"),
                Some(Ok(_)) => continue,
                _ => break,
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "unknown ticket must not extend the socket");
    assert!(started.elapsed() < HOLE_DELIVERY_TIMEOUT + Duration::from_secs(1));

    // A valid ticket keeps the socket past the first-ticket window, so the
    // next hand is served without a fresh handshake.
    let (mut kept, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("connect kept socket");
    for round in 0..2 {
        if round == 1 {
            tokio::time::sleep(HOLE_DELIVERY_TIMEOUT + Duration::from_secs(1)).await;
        }
        kept.send(WsMessage::Text(redeem.clone())).await.unwrap();
        let delivery: HoleDelivery =
            serde_json::from_str(&kept.next().await.unwrap().unwrap().into_text().unwrap())
                .expect("delivery on the kept socket");
        assert_eq!((delivery.hand_id, delivery.seat), (hand_id, 3));
    }
    client.delete_hand(hand_id).await;
    worker.abort();
}

#[tokio::test]
async fn tickets_go_only_to_users_dealt_into_the_hand() {
    let mut hand = engine::GameHand::new_blind(
        vec![
            (engine::PlayerId(1), engine::Chips(100), 0),
            (engine::PlayerId(2), engine::Chips(100), 1),
            (engine::PlayerId(104), engine::Chips(100), 3),
        ],
        0,
        engine::Chips(20),
        engine::Chips(10),
        [0; 32],
    );
    hand.start().unwrap();
    let (alice, bob, carol, mallory) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let active = ActiveHand::create(
        Uuid::new_v4(),
        Uuid::new_v4(),
        &hand,
        &std::collections::HashSet::from([3]),
        &HashMap::from([(alice, 0), (bob, 1)]),
        None,
    )
    .await
    .expect("sealed hand");

    // Bob left mid-hand; Carol took his seat and Mallory took the bot seat.
    let live = HashMap::from([(alice, 0), (carol, 1), (mallory, 3)]);
    let mut receivers = HashMap::new();
    let mut senders = HashMap::new();
    for (conn_id, uid) in [alice, carol, mallory].into_iter().enumerate() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        senders.insert(
            uid,
            crate::session_registry::WsSender {
                conn_id: conn_id as u64,
                tx,
            },
        );
        receivers.insert(uid, rx);
    }
    let conns = tokio::sync::Mutex::new(senders);
    active.sync_tickets(&live, &conns).await;

    let mut tickets_for = |uid: Uuid| {
        let rx = receivers.get_mut(&uid).unwrap();
        let mut seats = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            let crate::outbound::Outbound::Text(json) = frame else {
                continue;
            };
            let value: serde_json::Value = serde_json::from_str(&json).unwrap();
            if value["kind"] == "sealed_deal_ticket" {
                seats.push(value["data"]["seat"].as_u64().unwrap());
            }
        }
        seats
    };
    assert_eq!(tickets_for(alice), vec![0]);
    assert!(
        tickets_for(carol).is_empty(),
        "replacement got Bob's ticket"
    );
    assert!(
        tickets_for(mallory).is_empty(),
        "joiner got the bot's ticket"
    );
    active.delete().await;
}
