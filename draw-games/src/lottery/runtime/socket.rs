//! Lottery JSON sockets reuse the card-game authentication and revocation loop.
use super::*;
use crate::{app_state::AppState, auth::middleware::AuthUser, game_socket::SocketAuth};
use axum::{
    extract::{ws::WebSocketUpgrade, ConnectInfo, Query, State as AxumState},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use tokio::sync::mpsc;
#[derive(Deserialize)]
pub struct Parameters {
    game: String,
    client_source: Option<String>,
}
#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum Client {
    Ping,
    Sync,
    Buy {
        body: Value,
    },
    Reveal {
        ticket_id: Uuid,
        index: Option<usize>,
    },
    Refill,
}
fn code(e: &Error) -> &'static str {
    match e {
        Error::InvalidEntry => "invalid_entry",
        Error::SalesClosed => "sales_closed",
        Error::TooManyTickets => "too_many_tickets",
        Error::InsufficientBalance => "insufficient_balance",
        Error::RequestConflict => "request_conflict",
        Error::DailyAlreadyClaimed => "daily_already_claimed",
        Error::TicketNotFound => "ticket_not_found",
        Error::AlreadySettled => "already_settled",
        Error::FairnessRequired => "fairness_required",
        _ => "storage_unavailable",
    }
}
async fn frame(pool: &PgPool, user: Uuid, game: Game) -> Result<String, String> {
    view(pool, user, game)
        .await
        .map(|view| json!({"t":"state","view":view}).to_string())
        .map_err(|e| json!({"t":"error","code":code(&e)}).to_string())
}
pub async fn handler(
    AxumState(app): AxumState<AppState>,
    user: AuthUser,
    Query(q): Query<Parameters>,
    headers: HeaderMap,
    peer: Option<ConnectInfo<std::net::SocketAddr>>,
    ws: WebSocketUpgrade,
) -> Response {
    if !crate::game_socket::origin_allowed(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(game) = Game::parse(&q.game) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !ready(&app.pool) || player(&app.pool, user.user_id).is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Some(auth) =
        SocketAuth::capture(&app, &headers, peer.map(|p| p.0.ip()), user.user_id).await
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let guard =
        match crate::admission::ws_admission().try_admit_kind(crate::admission::Arrival::New) {
            Ok(g) => g,
            Err(e) => {
                return crate::admission::ws_refusal(
                    e,
                    crate::admission::ws_admission().retry_after_sec(),
                )
            }
        };
    let auth = auth
        .with_source(
            game.as_str(),
            "lottery",
            user.user_id,
            q.client_source.as_deref(),
        )
        .with_timing_endpoint("/ws/lottery");
    ws.max_message_size(65536).on_upgrade(move|socket|async move{let _guard=guard;let pool=app.pool;let owner=user.user_id;let attach_pool=pool.clone();let outbound:Arc<Mutex<Option<mpsc::Sender<String>>>>=Arc::new(Mutex::new(None));let attach_out=outbound.clone();
 crate::game_socket::serve::<Client,_,_,_>(socket,auth,16,move|out|async move{
  let state=state(&attach_pool);let mut notices=state.changed.subscribe();let wallet_changed=beans_read_memory::changed(&attach_pool);out.send(frame(&attach_pool,owner,game).await?).await.map_err(|_|"closed".to_string())?;*attach_out.lock().unwrap_or_else(|e|e.into_inner())=Some(out.clone());
  let publisher=attach_out.clone();
  tokio::spawn(async move{
   let mut heartbeat=tokio::time::interval(Duration::from_secs(20));heartbeat.tick().await;let mut retry=false;let mut wallet_generation=beans_read_memory::user_generation(&attach_pool,owner);
   tokio::pin!(wallet_changed);
   loop{
    let send=tokio::select!{
     _=out.closed()=>break,
     notice=notices.recv()=>match notice{Ok(n)=>{let _=n.revision;n.game==game.as_str() || n.owner==Some(owner)},Err(tokio::sync::broadcast::error::RecvError::Lagged(_))=>true,Err(_)=>break},
     _=&mut wallet_changed=>{wallet_changed.set(beans_read_memory::changed(&attach_pool));let next=beans_read_memory::user_generation(&attach_pool,owner);let changed=next!=wallet_generation;wallet_generation=next;changed},
     _=heartbeat.tick()=>true,
     _=tokio::time::sleep(Duration::from_millis(100)),if retry=>true,
    };
    if send {match frame(&attach_pool,owner,game).await{Ok(frame)=>{retry=false;if out.try_send(frame).is_err(){break}},Err(frame)=>{retry=true;if out.try_send(frame).is_err(){break}}}}
   }
   *publisher.lock().unwrap_or_else(|e|e.into_inner())=None;
  });Ok(())
 },move|msg|{let pool=pool.clone();let out=outbound.lock().unwrap_or_else(|e|e.into_inner()).clone();async move{
  let result:Result<Value,Error>=match msg {
   Client::Ping=>Ok(json!({"pong":true})),
   Client::Sync=>view(&pool,owner,game).await,
   Client::Buy{body}=>{
    let p=if game==Game::Scratch {
     match serde_json::from_value(body) { Ok(b)=>buy_scratch(&pool,owner,&b).await, Err(_)=>Err(Error::InvalidEntry) }
    } else {
     match serde_json::from_value(body) { Ok(b)=>buy_numbers(&pool,owner,game,&b).await, Err(_)=>Err(Error::InvalidEntry) }
    };
    p.map(|p|json!({"purchased":p.purchased,"charged":p.charged,"balance_before":p.balance_before,"balance_after":p.balance_after}))
   },
   Client::Reveal{ticket_id,index}=>reveal(&pool,owner,ticket_id,index).await.map(|(t,b)|json!({"ticket":t,"balance":b})),
   Client::Refill=>refill(&pool,owner).await.map(|balance|json!({"balance":balance,"granted":super::super::REFILL}))
  };
  let frame=match result{Ok(result)=>json!({"t":"result","result":result}),Err(e)=>json!({"t":"error","code":code(&e)})};if let Some(out)=out{out.try_send(frame.to_string()).is_ok()}else{false}
 }}).await;
 })
}
