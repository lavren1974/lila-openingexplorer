#![forbid(unsafe_code)]

pub mod api;
pub mod db;
pub mod importer;
pub mod indexer;
pub mod model;
pub mod opening;
pub mod stats;
pub mod util;

use std::{
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use axum::{
    extract::{FromRef, Multipart, Path, Query, State},
    http::StatusCode,
    routing::{get, post, put},
    Json, Router,
};
use clap::Parser;
use futures_util::stream::Stream;
use moka::future::Cache;
use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr};
use shakmaty::{
    san::{San, SanPlus},
    uci::Uci,
    variant::VariantPosition,
    zobrist::ZobristHash,
    Color, EnPassantMode,
};
use tikv_jemallocator::Jemalloc;
use tokio::sync::Semaphore;

use crate::{
    api::{
        Error, ExplorerGame, ExplorerGameWithUci, ExplorerMove, ExplorerResponse, HistoryWanted,
        LichessQuery, MastersQuery, NdJson, PlayPosition, PlayerLimits, PlayerQuery,
        PlayerQueryFilter, WithSource,
    },
    db::{CacheHint, Database, DbOpt, LichessDatabase},
    importer::{LichessGameImport, LichessImporter, MastersImporter},
    indexer::{IndexerOpt, IndexerStub, QueueFull, Ticket},
    model::{GameId, KeyBuilder, KeyPrefix, MastersGame, MastersGameWithId, PreparedMove, UserId},
    opening::{Opening, Openings},
    stats::Stats,
    util::{ply, spawn_blocking, DedupStreamExt as _},
};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[derive(Parser)]
struct Opt {
    /// Binding address. Note that administrative endpoints must be protected
    /// using a reverse proxy.
    #[arg(long, default_value = "127.0.0.1:9002")]
    bind: SocketAddr,
    /// Allow access from all origins.
    #[arg(long)]
    cors: bool,
    /// Maximum number of cached responses for /masters.
    #[arg(long, default_value = "40000")]
    masters_cache: u64,
    /// Maximum number of cached responses for /lichess.
    #[arg(long, default_value = "40000")]
    lichess_cache: u64,
    #[command(flatten)]
    db: DbOpt,
    #[command(flatten)]
    indexer: IndexerOpt,
}

type ExplorerCache<T> = Cache<T, Result<Json<ExplorerResponse>, Error>>;

#[derive(FromRef, Clone)]
struct AppState {
    openings: &'static RwLock<Openings>,
    db: Arc<Database>,
    lichess_cache: ExplorerCache<LichessQuery>,
    masters_cache: ExplorerCache<MastersQuery>,
    stats: &'static Stats,
    lichess_importer: LichessImporter,
    masters_importer: MastersImporter,
    indexer: IndexerStub,
    semaphore: &'static Semaphore,
}

fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::new()
            .filter("EXPLORER_LOG")
            .write_style("EXPLORER_LOG_STYLE"),
    )
    .format_timestamp(None)
    .format_module_path(false)
    .format_target(false)
    .init();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(128)
        .build()
        .expect("tokio runtime")
        .block_on(serve());
}

async fn serve() {
    let opt = Opt::parse();

    let db = Arc::new(Database::open(opt.db).expect("db"));
    let (indexer, _join_handles) = IndexerStub::spawn(Arc::clone(&db), opt.indexer);

    let app = Router::new()
        .route("/monitor/cf/:cf/:prop", get(cf_prop))
        .route("/monitor/db/:prop", get(db_prop))
        .route("/monitor", get(monitor))
        .route("/compact", post(compact))
        .route("/import/masters", put(masters_import))
        .route("/import/lichess", put(lichess_import))
        .route("/import/openings", put(openings_import))
        .route("/masters/pgn/:id", get(masters_pgn))
        .route("/masters", get(masters))
        .route("/lichess", get(lichess))
        .route("/lichess/history", get(lichess_history)) // bc
        .route("/player", get(player))
        .route("/master/pgn/:id", get(masters_pgn)) // bc
        .route("/master", get(masters)) // bc
        .route("/personal", get(player)) // bc
        .with_state(AppState {
            openings: Box::leak(Box::default()),
            lichess_cache: Cache::builder()
                .max_capacity(opt.lichess_cache)
                .time_to_live(Duration::from_secs(60 * 60 * 12))
                .time_to_idle(Duration::from_secs(60 * 10))
                .build(),
            masters_cache: Cache::builder()
                .max_capacity(opt.masters_cache)
                .time_to_live(Duration::from_secs(60 * 60 * 24))
                .time_to_idle(Duration::from_secs(60 * 10))
                .build(),
            stats: Box::leak(Box::default()),
            lichess_importer: LichessImporter::new(Arc::clone(&db)),
            masters_importer: MastersImporter::new(Arc::clone(&db)),
            indexer,
            db,
            semaphore: Box::leak(Box::new(Semaphore::new(128))),
        });

    let app = if opt.cors {
        app.layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
            axum::http::HeaderValue::from_static("*"),
        ))
    } else {
        app
    };

    axum::Server::bind(&opt.bind)
        .serve(app.into_make_service())
        .await
        .expect("bind");
}

#[derive(Deserialize)]
struct ColumnFamilyProp {
    cf: String,
    prop: String,
}

#[axum::debug_handler(state = AppState)]
async fn cf_prop(
    Path(path): Path<ColumnFamilyProp>,
    State(db): State<Arc<Database>>,
    State(semaphore): State<&'static Semaphore>,
) -> Result<String, StatusCode> {
    spawn_blocking(semaphore, move || {
        db.inner
            .cf_handle(&path.cf)
            .and_then(|cf| {
                db.inner
                    .property_value_cf(cf, &path.prop)
                    .expect("property value")
            })
            .ok_or(StatusCode::NOT_FOUND)
    })
    .await
}

#[axum::debug_handler(state = AppState)]
async fn db_prop(
    Path(prop): Path<String>,
    State(db): State<Arc<Database>>,
    State(semaphore): State<&'static Semaphore>,
) -> Result<String, StatusCode> {
    spawn_blocking(semaphore, move || {
        db.inner
            .property_value(&prop)
            .expect("property value")
            .ok_or(StatusCode::NOT_FOUND)
    })
    .await
}

#[axum::debug_handler(state = AppState)]
async fn monitor(
    State(lichess_cache): State<ExplorerCache<LichessQuery>>,
    State(masters_cache): State<ExplorerCache<MastersQuery>>,
    State(stats): State<&'static Stats>,
    State(indexer): State<IndexerStub>,
    State(db): State<Arc<Database>>,
    State(semaphore): State<&'static Semaphore>,
) -> String {
    spawn_blocking(semaphore, move || {
        format!(
            "opening_explorer {}",
            [
                // Cache entries
                format!("lichess_cache={}u", lichess_cache.entry_count()),
                format!("masters_cache={}u", masters_cache.entry_count()),
                // Request stats
                stats.to_influx_string(),
                // Block cache
                db.stats().expect("db stats").to_influx_string(),
                // Indexer
                format!("indexing={}u", indexer.num_indexing()),
                // Column families
                db.masters()
                    .estimate_stats()
                    .expect("masters stats")
                    .to_influx_string(),
                db.lichess()
                    .estimate_stats()
                    .expect("lichess stats")
                    .to_influx_string(),
            ]
            .join(",")
        )
    })
    .await
}

#[axum::debug_handler(state = AppState)]
async fn compact(State(db): State<Arc<Database>>, State(semaphore): State<&'static Semaphore>) {
    spawn_blocking(semaphore, move || db.compact()).await
}

#[axum::debug_handler(state = AppState)]
async fn openings_import(
    State(openings): State<&'static RwLock<Openings>>,
    State(masters_cache): State<ExplorerCache<MastersQuery>>,
    State(lichess_cache): State<ExplorerCache<LichessQuery>>,
    mut multipart: Multipart,
) -> Result<(), Error> {
    let mut new_openings = Openings::empty();

    while let Some(field) = multipart.next_field().await.map_err(Arc::new)? {
        let tsv = field.text().await.map_err(Arc::new)?;
        new_openings.load_tsv(&tsv)?;
    }

    masters_cache.invalidate_all();
    lichess_cache.invalidate_all();

    let new_len = new_openings.len();
    *openings.write().expect("write openings") = new_openings;
    log::info!("loaded {} opening names", new_len);

    Ok(())
}

fn finalize_lichess_moves(
    moves: Vec<PreparedMove>,
    pos: &VariantPosition,
    lichess_db: &LichessDatabase,
) -> Vec<ExplorerMove> {
    moves
        .into_iter()
        .map(|p| ExplorerMove {
            stats: p.stats,
            san: p.uci.to_move(pos).map_or(
                SanPlus {
                    san: San::Null,
                    suffix: None,
                },
                |m| SanPlus::from_move(pos.clone(), &m),
            ),
            uci: p.uci,
            average_rating: p.average_rating,
            average_opponent_rating: p.average_opponent_rating,
            performance: p.performance,
            game: p.game.and_then(|id| {
                lichess_db
                    .game(id)
                    .expect("get game")
                    .map(|info| ExplorerGame::from_lichess(id, info))
            }),
        })
        .collect()
}

fn finalize_lichess_games(
    games: Vec<(Uci, GameId)>,
    lichess_db: &LichessDatabase,
) -> Vec<ExplorerGameWithUci> {
    lichess_db
        .games(games.iter().map(|(_, id)| *id))
        .expect("get games")
        .into_iter()
        .zip(games.into_iter())
        .filter_map(|(info, (uci, id))| {
            info.map(|info| ExplorerGameWithUci {
                uci,
                row: ExplorerGame::from_lichess(id, info),
            })
        })
        .collect()
}

struct PlayerStreamState {
    indexer: IndexerStub,
    ticket: Ticket,
    key: KeyPrefix,
    db: Arc<Database>,
    color: Color,
    filter: PlayerQueryFilter,
    limits: PlayerLimits,
    pos: VariantPosition,
    opening: Option<Opening>,
    first_response: Option<ExplorerResponse>,
    done: bool,
}

#[axum::debug_handler(state = AppState)]
async fn player(
    State(openings): State<&'static RwLock<Openings>>,
    State(db): State<Arc<Database>>,
    State(indexer): State<IndexerStub>,
    State(stats): State<&'static Stats>,
    State(semaphore): State<&'static Semaphore>,
    Query(query): Query<PlayerQuery>,
) -> Result<NdJson<impl Stream<Item = ExplorerResponse>>, Error> {
    let player = UserId::from(query.player);
    let key_builder = KeyBuilder::player(&player, query.color);
    let ticket = indexer
        .index_player(player, semaphore)
        .await
        .map_err(|QueueFull(player)| {
            log::error!(
                "not indexing {} because queue is full",
                player.as_lowercase_str()
            );
            Error::IndexerQueueFull
        })?;
    let PlayPosition { pos, opening } = query
        .play
        .position(&openings.read().expect("read openings"))?;
    let cache_hint = CacheHint::from_ply(ply(&pos));
    let key = key_builder.with_zobrist(pos.variant(), pos.zobrist_hash(EnPassantMode::Legal));

    let state = PlayerStreamState {
        indexer,
        color: query.color,
        filter: query.filter,
        limits: query.limits,
        db,
        ticket,
        opening,
        key,
        pos,
        first_response: None,
        done: false,
    };

    Ok(NdJson(futures_util::stream::unfold(
        state,
        move |mut state| async move {
            if state.done {
                return None;
            }

            let first = state.first_response.is_none();
            state.done = tokio::select! {
                _ = state.ticket.completed() => true,
                _ = tokio::time::sleep(Duration::from_millis(if first { 0 } else { 1000 })) => false,
            };

            let preceding_tickets = state.indexer.preceding_tickets(&state.ticket);

            Some(match state.first_response {
                Some(ref first_response) if preceding_tickets > 0 => {
                    // While indexing has not even started, just repeat the
                    // first response with updated queue position.
                    let response = ExplorerResponse {
                        queue_position: Some(preceding_tickets),
                        ..first_response.clone()
                    };
                    (response, state)
                },
                _ => {
                    spawn_blocking(semaphore, move || {
                        let started_at = Instant::now();

                        let lichess_db = state.db.lichess();
                        let filtered = lichess_db
                            .read_player(&state.key, state.filter.since, state.filter.until, cache_hint)
                            .expect("read player")
                            .prepare(state.color, &state.filter, &state.limits);

                        let response = ExplorerResponse {
                            total: filtered.total,
                            moves: finalize_lichess_moves(filtered.moves, &state.pos, &lichess_db),
                            recent_games: Some(finalize_lichess_games(filtered.recent_games, &lichess_db)),
                            top_games: None,
                            history: None,
                            opening: state.opening.clone(),
                            queue_position: Some(preceding_tickets),
                        };

                        if state.first_response.is_none() {
                            state.first_response = Some(response.clone());
                        }

                        stats.inc_player(started_at.elapsed(), state.done, ply(&state.pos));
                        (response, state)
                    }).await
                }
            })
        },
    ).dedup_by_key(|res| (res.queue_position, res.total.total()))))
}

#[axum::debug_handler(state = AppState)]
async fn masters_import(
    State(importer): State<MastersImporter>,
    State(semaphore): State<&'static Semaphore>,
    Json(body): Json<MastersGameWithId>,
) -> Result<(), Error> {
    spawn_blocking(semaphore, move || importer.import(body)).await
}

#[serde_as]
#[derive(Deserialize)]
struct MastersGameId(#[serde_as(as = "DisplayFromStr")] GameId);

#[axum::debug_handler(state = AppState)]
async fn masters_pgn(
    Path(MastersGameId(id)): Path<MastersGameId>,
    State(db): State<Arc<Database>>,
    State(semaphore): State<&'static Semaphore>,
) -> Result<MastersGame, StatusCode> {
    spawn_blocking(semaphore, move || {
        match db.masters().game(id).expect("get masters game") {
            Some(game) => Ok(game),
            None => Err(StatusCode::NOT_FOUND),
        }
    })
    .await
}

#[axum::debug_handler(state = AppState)]
async fn masters(
    State(openings): State<&'static RwLock<Openings>>,
    State(db): State<Arc<Database>>,
    State(masters_cache): State<ExplorerCache<MastersQuery>>,
    State(stats): State<&'static Stats>,
    State(semaphore): State<&'static Semaphore>,
    Query(WithSource { query, source }): Query<WithSource<MastersQuery>>,
) -> Result<Json<ExplorerResponse>, Error> {
    masters_cache
        .get_with(query.clone(), async move {
            spawn_blocking(semaphore, move || {
                let started_at = Instant::now();

                let PlayPosition { pos, opening } = query
                    .play
                    .position(&openings.read().expect("read openings"))?;

                let key = KeyBuilder::masters()
                    .with_zobrist(pos.variant(), pos.zobrist_hash(EnPassantMode::Legal));
                let cache_hint = CacheHint::from_ply(ply(&pos));
                let masters_db = db.masters();
                let entry = masters_db
                    .read(key, query.since, query.until, cache_hint)
                    .expect("get masters")
                    .prepare(&query.limits);

                let response = Ok(Json(ExplorerResponse {
                    total: entry.total,
                    moves: entry
                        .moves
                        .into_iter()
                        .map(|p| ExplorerMove {
                            san: p.uci.to_move(&pos).map_or(
                                SanPlus {
                                    san: San::Null,
                                    suffix: None,
                                },
                                |m| SanPlus::from_move(pos.clone(), &m),
                            ),
                            uci: p.uci,
                            average_rating: p.average_rating,
                            average_opponent_rating: p.average_opponent_rating,
                            performance: p.performance,
                            stats: p.stats,
                            game: p.game.and_then(|id| {
                                masters_db
                                    .game(id)
                                    .expect("get masters game")
                                    .map(|info| ExplorerGame::from_masters(id, info))
                            }),
                        })
                        .collect(),
                    top_games: Some(
                        masters_db
                            .games(entry.top_games.iter().map(|(_, id)| *id))
                            .expect("get masters games")
                            .into_iter()
                            .zip(entry.top_games.into_iter())
                            .filter_map(|(info, (uci, id))| {
                                info.map(|info| ExplorerGameWithUci {
                                    uci: uci.clone(),
                                    row: ExplorerGame::from_masters(id, info),
                                })
                            })
                            .collect(),
                    ),
                    opening,
                    recent_games: None,
                    queue_position: None,
                    history: None,
                }));

                stats.inc_masters(started_at.elapsed(), source, ply(&pos));
                response
            })
            .await
        })
        .await
}

#[axum::debug_handler(state = AppState)]
async fn lichess_import(
    State(importer): State<LichessImporter>,
    State(semaphore): State<&'static Semaphore>,
    Json(body): Json<Vec<LichessGameImport>>,
) -> Result<(), Error> {
    spawn_blocking(semaphore, move || importer.import_many(body)).await
}

#[axum::debug_handler(state = AppState)]
async fn lichess(
    State(openings): State<&'static RwLock<Openings>>,
    State(db): State<Arc<Database>>,
    State(lichess_cache): State<ExplorerCache<LichessQuery>>,
    State(stats): State<&'static Stats>,
    State(semaphore): State<&'static Semaphore>,
    Query(WithSource { query, source }): Query<WithSource<LichessQuery>>,
) -> Result<Json<ExplorerResponse>, Error> {
    lichess_cache
        .get_with(query.clone(), async move {
            spawn_blocking(semaphore, move || {
                let started_at = Instant::now();

                let PlayPosition { pos, opening } = query
                    .play
                    .position(&openings.read().expect("read openings"))?;

                let key = KeyBuilder::lichess()
                    .with_zobrist(pos.variant(), pos.zobrist_hash(EnPassantMode::Legal));
                let cache_hint = CacheHint::from_ply(ply(&pos));
                let lichess_db = db.lichess();
                let (filtered, history) = lichess_db
                    .read_lichess(
                        &key,
                        &query.filter,
                        &query.limits,
                        query.history,
                        cache_hint,
                    )
                    .expect("get lichess");

                let response = Ok(Json(ExplorerResponse {
                    total: filtered.total,
                    moves: finalize_lichess_moves(filtered.moves, &pos, &lichess_db),
                    recent_games: Some(finalize_lichess_games(filtered.recent_games, &lichess_db)),
                    top_games: Some(finalize_lichess_games(filtered.top_games, &lichess_db)),
                    opening,
                    queue_position: None,
                    history: Some(history.unwrap_or_default()), // work around
                }));

                stats.inc_lichess(started_at.elapsed(), source, ply(&pos));
                response
            })
            .await
        })
        .await
}

#[axum::debug_handler(state = AppState)]
async fn lichess_history(
    openings: State<&'static RwLock<Openings>>,
    db: State<Arc<Database>>,
    lichess_cache: State<ExplorerCache<LichessQuery>>,
    stats: State<&'static Stats>,
    semaphore: State<&'static Semaphore>,
    Query(mut with_source): Query<WithSource<LichessQuery>>,
) -> Result<Json<ExplorerResponse>, Error> {
    with_source.query.history = HistoryWanted::Yes;
    with_source.query.limits.recent_games = 0;
    with_source.query.limits.top_games = 0;
    with_source.query.limits.moves = 0;
    lichess(
        openings,
        db,
        lichess_cache,
        stats,
        semaphore,
        Query(with_source),
    )
    .await
}
