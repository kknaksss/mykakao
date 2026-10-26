//! axum HTTP 서버 — SPEC-003 §FE Contract 5개 엔드포인트 + `ui/` 정적 서빙(localhost).
//!
//! 응답 키·경로는 계약 그대로 고정한다. 키·대화 본문은 응답/로그에 마스킹 없이 노출하지 않는다.
//! (본문은 `/api/messages` 로만 로컬 렌더 — 외부 전송 없음.)

use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use tower_http::services::ServeDir;

use crate::kakao::{self, RoomFile};
use crate::store::{MessageRow, Store};

/// P2 실시간 fan-out 이벤트: 한 방에 새로 축적된 메시지 1행.
#[derive(Debug, Clone)]
pub struct StreamEvent {
    pub chat_id: i64,
    pub row: MessageRow,
}

/// SSE broadcast 채널 용량(바운드). lagging 수신자는 drop 된다(무한 버퍼 금지).
pub const STREAM_CAPACITY: usize = 256;

/// 로그인 상태 코드(WORK-006).
pub const LIFE_DOWN: u8 = 0;
pub const LIFE_OUT: u8 = 1;
pub const LIFE_IN: u8 = 2;

/// 상태 코드 → 계약 문자열.
pub fn life_str(code: u8) -> &'static str {
    match code {
        LIFE_IN => "UP_LOGGED_IN",
        LIFE_OUT => "UP_LOGGED_OUT",
        _ => "DOWN",
    }
}

/// 세션 키 캐시(RAM only). pid 로 무효화(재시작 시 miss). 키는 절대 디스크/로그에 안 남긴다.
type KeyCache = std::sync::Arc<HashSet<crate::kakao::memkey::RawKey>>;

/// 공유 앱 상태.
pub struct AppState {
    pub store: Arc<Store>,
    pub account: Option<PathBuf>,
    /// 실시간 새 메시지 fan-out(watch → /api/stream). bounded broadcast.
    pub tx: broadcast::Sender<StreamEvent>,
    last_refresh: Mutex<Option<Instant>>,
    /// 이름 재해석(제목/닉네임) 백그라운드 작업 진행 중 플래그(중복 방지).
    refreshing: AtomicBool,
    /// 무거운 카톡 작업(키회수·복호·import·enrich) 직렬화 — 동시 harvest(메모리 폭증)·전이오류 방지.
    work_lock: Mutex<()>,
    /// WORK-006: 현재 로그인 상태(LIFE_*). tray/state 가 lock-free 로 읽는다.
    life: AtomicU8,
    /// 세션 키 캐시((pid, 후보집합)). RAM only. 재시작/로그아웃 시 무효화.
    key_cache: Mutex<Option<(u32, KeyCache)>>,
    /// 본인 닉네임 캐시(로그인 시 1회 해석). 값은 UI 표시용, 로그 금지.
    me_cache: Mutex<Option<String>>,
    /// 본인 userId(로컬 UI 말풍선 정렬용). 0=미상.
    my_uid: AtomicI64,
    /// 방별 raw key 캐시(chat_id→key). 폴링 시 후보 스윕을 건너뛰어 재복호를 저렴하게 한다.
    /// RAM only. 로그아웃/다운 시 무효화.
    room_keys: Mutex<HashMap<i64, crate::kakao::memkey::RawKey>>,
    /// 작성자 닉네임 맵 캐시(세션 1회 로드). SSE 이름 부착용. 값은 로그 금지.
    nick_cache: Mutex<Option<Arc<HashMap<i64, String>>>>,
    /// 사진 백그라운드 다운로드가 돌고 있는 방 집합(방당 1개만 — 중복 스레드 금지).
    photo_busy: Mutex<HashSet<i64>>,
    /// 능동 수집(POST /api/collect)이 진행 중인 방 집합 — 클릭 연타·중복 요청 방지.
    collect_busy: Mutex<HashSet<i64>>,
    /// 마지막 강제 재harvest 시각 — 연타로 카톡 메모리 스윕이 반복되지 않게 최소 간격을 둔다.
    last_harvest: Mutex<Option<Instant>>,
    /// 백그라운드 키 재harvest 진행 중 플래그(동시 1개만 — 메모리 스윕은 비싸다).
    harvesting: AtomicBool,
    /// 세션 키 후보집합 세대(harvest 할 때마다 +1). 후보가 그대로면 실패한 스윕을 다시 돌 필요가 없다.
    cands_gen: AtomicU64,
    /// 방별 "이 세대 후보로 스윕했지만 키를 못 찾음" 표식(chat_id → 세대).
    /// 후보가 바뀌기 전까지 같은 방의 비싼 재스윕을 건너뛴다(클릭 연타 대비).
    sweep_fail: Mutex<HashMap<i64, u64>>,
    /// 사진 스윕 게이트(chat_id → (마지막 델타, 마지막 스윕)). talkmedia 반영 지연 보정용.
    photo_sweep: Mutex<HashMap<i64, (Instant, Option<Instant>)>>,
}


pub type Shared = Arc<AppState>;

/// 500 으로 변환되는 얇은 에러(상세는 로그 없이 일반 메시지만).
struct AppError(anyhow::Error);
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // 진단(WIN_APP_DIAG 설정 시에만 상세 노출; 값/본문 없음, 에러 메시지 텍스트만).
        // 개발 진단(WIN_APP_ERRDETAIL 설정 시에만) — 에러 메시지 텍스트만(값/본문 없음).
        let detail = if std::env::var_os("WIN_APP_ERRDETAIL").is_some() {
            format!("{:#}", self.0)
        } else {
            "internal error".to_string()
        };
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": detail })),
        )
            .into_response()
    }
}
impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

/// 라우터 구성.
pub fn router(state: Shared) -> Router {
    let ui_dir = resolve_ui_dir();
    Router::new()
        .route("/api/state", get(get_state))
        .route("/api/rooms", get(get_rooms))
        .route("/api/rooms/select", post(post_select))
        .route("/api/collect", post(post_collect))
        .route("/api/import", post(post_import))
        .route("/api/messages", get(get_messages))
        .route("/api/media/:chat_id/:log_id", get(get_media))
        .route("/api/stream", get(get_stream))
        .with_state(state)
        .fallback_service(ServeDir::new(ui_dir).append_index_html_on_directories(true))
}

impl AppState {
    pub fn new(store: Arc<Store>, account: Option<PathBuf>) -> Self {
        let (tx, _rx) = broadcast::channel(STREAM_CAPACITY);
        Self {
            store,
            account,
            tx,
            last_refresh: Mutex::new(None),
            refreshing: AtomicBool::new(false),
            work_lock: Mutex::new(()),
            life: AtomicU8::new(LIFE_DOWN),
            key_cache: Mutex::new(None),
            me_cache: Mutex::new(None),
            my_uid: AtomicI64::new(0),
            room_keys: Mutex::new(HashMap::new()),
            nick_cache: Mutex::new(None),
            photo_busy: Mutex::new(HashSet::new()),
            collect_busy: Mutex::new(HashSet::new()),
            last_harvest: Mutex::new(None),
            harvesting: AtomicBool::new(false),
            cands_gen: AtomicU64::new(0),
            sweep_fail: Mutex::new(HashMap::new()),
            photo_sweep: Mutex::new(HashMap::new()),
        }
    }

    /// 방별 캐시된 raw key(있으면). 폴링 재복호에서 후보 스윕을 건너뛰게 한다.
    pub fn room_key(&self, chat_id: i64) -> Option<crate::kakao::memkey::RawKey> {
        self.room_keys.lock().unwrap_or_else(|e| e.into_inner()).get(&chat_id).copied()
    }
    /// 방 key 캐시에 저장(성공적으로 복호된 뒤).
    pub fn cache_room_key(&self, chat_id: i64, key: crate::kakao::memkey::RawKey) {
        self.room_keys.lock().unwrap_or_else(|e| e.into_inner()).insert(chat_id, key);
    }
    /// 세션 닉네임 맵(캐시 우선). 없으면 `load` 로 1회 만들어 캐싱한다.
    pub fn nick_map_cached(
        &self,
        load: impl FnOnce() -> HashMap<i64, String>,
    ) -> Arc<HashMap<i64, String>> {
        let mut c = self.nick_cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = c.as_ref() {
            return m.clone();
        }
        let m = Arc::new(load());
        *c = Some(m.clone());
        m
    }

    // ── WORK-008/실시간: 사진 백그라운드 다운로드 조율 ──
    /// 이 방의 다운로드 스레드를 점유한다. 이미 돌고 있으면 false(스폰 금지).
    pub fn photo_begin(&self, chat_id: i64) -> bool {
        self.photo_busy.lock().unwrap_or_else(|e| e.into_inner()).insert(chat_id)
    }
    /// 다운로드 스레드 점유 해제(실패로 스폰 못 했을 때도 반드시 호출).
    pub fn photo_end(&self, chat_id: i64) {
        self.photo_busy.lock().unwrap_or_else(|e| e.into_inner()).remove(&chat_id);
    }
    /// 이 방의 능동 수집을 점유한다. 이미 돌고 있으면 false(호출측이 잠시 뒤 재시도).
    pub fn collect_begin(&self, chat_id: i64) -> bool {
        self.collect_busy.lock().unwrap_or_else(|e| e.into_inner()).insert(chat_id)
    }
    /// 능동 수집 점유 해제.
    pub fn collect_end(&self, chat_id: i64) {
        self.collect_busy.lock().unwrap_or_else(|e| e.into_inner()).remove(&chat_id);
    }
    /// 현재 키 후보집합 세대.
    pub fn cands_gen(&self) -> u64 {
        self.cands_gen.load(Ordering::Relaxed)
    }
    /// 이 방을 현재 세대 후보로 이미 스윕해 봤는데 실패했는지(=재스윕 무의미).
    pub fn sweep_failed_now(&self, chat_id: i64) -> bool {
        let gen = self.cands_gen();
        self.sweep_fail.lock().unwrap_or_else(|e| e.into_inner()).get(&chat_id) == Some(&gen)
    }
    /// 현재 세대 후보로 스윕했지만 키를 못 찾았음을 기록.
    pub fn mark_sweep_failed(&self, chat_id: i64) {
        let gen = self.cands_gen();
        self.sweep_fail.lock().unwrap_or_else(|e| e.into_inner()).insert(chat_id, gen);
    }
    /// 백그라운드 키 재harvest 가 지금 돌고 있는지.
    pub fn harvest_in_flight(&self) -> bool {
        self.harvesting.load(Ordering::Relaxed)
    }
    /// 재harvest 스레드를 점유한다(이미 돌고 있으면 false).
    pub fn harvest_begin(&self) -> bool {
        !self.harvesting.swap(true, Ordering::SeqCst)
    }
    /// 재harvest 점유 해제.
    pub fn harvest_end(&self) {
        self.harvesting.store(false, Ordering::SeqCst);
    }
    /// 지금 강제 재harvest 를 돌려도 되는지(직전 시도 후 `MIN_GAP` 경과). true 면 시각을 기록한다.
    /// 카톡 메모리 스윕은 수십 초가 걸리므로 클릭 연타로 반복되지 않게 하는 가드.
    pub fn harvest_due(&self) -> bool {
        // 재harvest 최소 간격. 백그라운드(논블로킹) 재harvest 라 짧게 잡아 자가치유를 빠르게 한다
        // (재로그인·새 방 오픈으로 키가 새로 로드된 경우 20초 내 자동 반영).
        const MIN_GAP: Duration = Duration::from_secs(20);
        let now = Instant::now();
        let mut t = self.last_harvest.lock().unwrap_or_else(|e| e.into_inner());
        if t.map_or(true, |x| now.duration_since(x) >= MIN_GAP) {
            *t = Some(now);
            true
        } else {
            false
        }
    }
    /// 사진 스윕(talkmedia 재조회)를 돌려도 되는지.
    ///
    /// `fresh_delta` 면 무조건 허용하고 창을 갱신한다. 아니면 마지막 델타 후 `SWEEP_WINDOW` 이내이면서
    /// 직전 스윕으로부터 `SWEEP_COOLDOWN` 이 지났을 때만 허용 — 메시지 행보다 늦게 기록되는
    /// talkmedia 토큰을 잡으되, 반복 복호 비용은 상한을 둔다.
    pub fn photo_sweep_due(&self, chat_id: i64, fresh_delta: bool) -> bool {
        const SWEEP_WINDOW: Duration = Duration::from_secs(60);
        const SWEEP_COOLDOWN: Duration = Duration::from_secs(10);
        let now = Instant::now();
        let mut m = self.photo_sweep.lock().unwrap_or_else(|e| e.into_inner());
        let e = m.entry(chat_id).or_insert((now, None));
        if fresh_delta {
            e.0 = now;
        }
        let within = now.duration_since(e.0) < SWEEP_WINDOW;
        let cooled = e.1.map_or(true, |t| now.duration_since(t) >= SWEEP_COOLDOWN);
        if fresh_delta || (within && cooled) {
            e.1 = Some(now);
            true
        } else {
            false
        }
    }

    // ── WORK-006: 상태·세션캐시 접근자 ──
    pub fn set_life(&self, code: u8) {
        self.life.store(code, Ordering::Relaxed);
    }
    pub fn life(&self) -> u8 {
        self.life.load(Ordering::Relaxed)
    }
    pub fn me(&self) -> Option<String> {
        self.me_cache.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn set_me(&self, m: Option<String>) {
        *self.me_cache.lock().unwrap_or_else(|e| e.into_inner()) = m;
    }
    pub fn my_uid(&self) -> i64 {
        self.my_uid.load(Ordering::Relaxed)
    }
    pub fn set_my_uid(&self, uid: i64) {
        self.my_uid.store(uid, Ordering::Relaxed);
    }
    /// 세션 무효화(로그아웃/다운): 키·닉네임 캐시 비움.
    pub fn invalidate_session(&self) {
        *self.key_cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.me_cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.my_uid.store(0, Ordering::Relaxed);
        self.room_keys.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *self.nick_cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.photo_sweep.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
    /// 세션 키 후보(캐시 우선). 현재 pid 로 캐시 유효성 판정; miss 면 harvest 1회 후 캐싱.
    /// harvest 는 work_lock 아래(동시 harvest 방지). 키는 RAM only.
    pub fn cached_candidates(&self) -> Option<KeyCache> {
        let acct = self.account.as_ref()?;
        let pid = kakao::memkey::find_pid(kakao::memkey::PROC_NAME)?;
        {
            let c = self.key_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((cpid, cand)) = c.as_ref() {
                if *cpid == pid {
                    return Some(cand.clone());
                }
            }
        }
        let _work = self.work_lock.lock().unwrap_or_else(|e| e.into_inner());
        // 락 획득 후 재확인(다른 스레드가 채웠을 수 있음).
        {
            let c = self.key_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((cpid, cand)) = c.as_ref() {
                if *cpid == pid {
                    return Some(cand.clone());
                }
            }
        }
        let cand: KeyCache = std::sync::Arc::new(kakao::harvest_candidates(acct)?);
        *self.key_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((pid, cand.clone()));
        self.cands_gen.fetch_add(1, Ordering::SeqCst);
        Some(cand)
    }

    /// 세션 키 캐시를 버리고 **강제 재harvest**. 캐시는 pid 기준이라, 캐시 형성 뒤 새로 열린 방의
    /// 키는 담기지 않는다 → 열린 추적 방인데 import 가 키를 못 찾으면 이걸로 최신 키를 다시 뽑는다.
    /// 키는 RAM only. work_lock 아래(동시 harvest 방지).
    pub fn refresh_candidates(&self) -> Option<KeyCache> {
        let acct = self.account.as_ref()?;
        let pid = kakao::memkey::find_pid(kakao::memkey::PROC_NAME)?;
        let _work = self.work_lock.lock().unwrap_or_else(|e| e.into_inner());
        let cand: KeyCache = std::sync::Arc::new(kakao::harvest_candidates(acct)?);
        *self.key_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((pid, cand.clone()));
        self.cands_gen.fetch_add(1, Ordering::SeqCst);
        Some(cand)
    }
}

fn resolve_ui_dir() -> PathBuf {
    // 우선순위: 환경변수 → exe 옆 ui/ → CWD ui/ → 크레이트 ui/(개발).
    if let Some(d) = std::env::var_os("WIN_APP_UI") {
        return PathBuf::from(d);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("ui");
            if p.is_dir() {
                return p;
            }
        }
    }
    let cwd_ui = PathBuf::from("ui");
    if cwd_ui.is_dir() {
        return cwd_ui;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui")
}

// ── /api/state ──────────────────────────────────────────────────
#[derive(Serialize)]
struct StateResp {
    kakao_running: bool,
    logged_in: bool,
    recoverable_rooms: Vec<i64>,
    account: Option<String>,
    /// WORK-006: "DOWN" | "UP_LOGGED_OUT" | "UP_LOGGED_IN".
    state: String,
    /// WORK-006: 본인 닉네임(있으면).
    me: Option<String>,
    /// WORK-007: 본인 userId(로컬 UI 말풍선 정렬용, 0=미상).
    my_user_id: i64,
}

async fn get_state(State(st): State<Shared>) -> Result<Json<StateResp>, AppError> {
    let resp = tokio::task::spawn_blocking(move || compute_state(&st)).await??;
    Ok(Json(resp))
}

fn compute_state(st: &AppState) -> anyhow::Result<StateResp> {
    // 3상태 감지 = cheap(프로세스 존재 + 계정 DB 잠금 프로브). 메모리 스캔 없음.
    #[cfg(windows)]
    let life = crate::state::detect(st.account.as_deref());
    #[cfg(not(windows))]
    let life = LIFE_DOWN;
    st.set_life(life);

    let running = life != LIFE_DOWN;
    let logged_in = life == LIFE_IN;
    let acct_fp = st.account.as_deref().and_then(kakao::account_fingerprint);

    // recoverable(열린 방) = chatLogs 잠금 프로브(메모리 스캔 없이 cheap).
    let recoverable_rooms = match (logged_in, st.account.as_ref()) {
        #[cfg(windows)]
        (true, Some(acct)) => crate::state::open_rooms(acct),
        _ => Vec::new(),
    };

    Ok(StateResp {
        kakao_running: running,
        logged_in,
        recoverable_rooms,
        account: acct_fp,
        state: life_str(life).to_string(),
        me: st.me(),
        my_user_id: st.my_uid(),
    })
}



/// i64 식별자(chat_id 등)를 **JSON 문자열**로 직렬화한다.
/// 이유: chat_id 는 2^53 을 넘을 수 있어(예: 1.8e16) JSON number 로 주면 JS Number 에서
/// 정밀도가 깨져 select 왕복(추적 저장)·방 매칭이 어긋난다. 문자열이면 정확히 왕복한다.
fn ser_id<S: serde::Serializer>(v: &i64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&v.to_string())
}

/// select 요청의 chat_id 항목: 문자열 또는 숫자 모두 수용(문자열 권장, 숫자는 하위호환).
#[derive(Deserialize)]
#[serde(untagged)]
enum IdVal {
    S(String),
    N(i64),
}
impl IdVal {
    fn to_i64(&self) -> Option<i64> {
        match self {
            IdVal::S(s) => s.trim().parse::<i64>().ok(),
            IdVal::N(n) => Some(*n),
        }
    }
}

// ── /api/rooms ──────────────────────────────────────────────────
#[derive(Serialize)]
struct RoomResp {
    #[serde(serialize_with = "ser_id")]
    chat_id: i64,
    title: Option<String>,
    member_count: Option<i64>,
    /// "group"(단체) / "direct"(1:1) / "memo"(내 메모) / null.
    kind: Option<String>,
    /// 지금 카톡에서 열려 있어 키 회수·import 가능한 방인지.
    open: bool,
    selected: bool,
    /// WORK-007: 추적 대상 여부(= selected).
    tracked: bool,
    /// WORK-007: 수집 큐 상태 idle|collecting|waiting|done|error.
    collect_status: String,
    /// WORK-007: 수집 완료 행수.
    collected_rows: Option<i64>,
}

async fn get_rooms(State(st): State<Shared>) -> Result<Json<Vec<RoomResp>>, AppError> {
    // 이름 재해석(제목/닉네임)은 무겁다(키 스윕) → 백그라운드로. 응답은 캐시된 store 로 즉시.
    maybe_spawn_refresh(st.clone());
    let st2 = st.clone();
    let rooms = tokio::task::spawn_blocking(move || list_rooms_blocking(&st2)).await??;
    Ok(Json(rooms))
}

fn list_rooms_blocking(st: &AppState) -> anyhow::Result<Vec<RoomResp>> {
    let files = st
        .account
        .as_ref()
        .map(|a| kakao::list_room_files(a))
        .unwrap_or_default();

    // 파일에 있는 방을 store 에 최소 등록.
    for r in &files {
        st.store.upsert_room(r.chat_id, None, None, None)?;
    }

    // 열림(회수가능) 방 = chatLogs 잠금 프로브(cheap). 파일 기준.
    let open_set: HashSet<i64> = {
        #[cfg(windows)]
        {
            st.account
                .as_ref()
                .map(|a| crate::state::open_rooms(a).into_iter().collect())
                .unwrap_or_default()
        }
        #[cfg(not(windows))]
        {
            HashSet::new()
        }
    };

    let stored = st.store.list_rooms()?;
    let stored_map: BTreeMap<i64, &crate::store::RoomRow> =
        stored.iter().map(|r| (r.chat_id, r)).collect();

    // 방 목록 = 원본 chatLogs 파일 ∪ store(enrich 로 채워진 chatRoomList 전체).
    let mut ids: Vec<i64> = files.iter().map(|r| r.chat_id).collect();
    for id in stored_map.keys() {
        if !ids.contains(id) {
            ids.push(*id);
        }
    }
    ids.sort_unstable();
    ids.dedup();

    let out = ids
        .into_iter()
        .map(|id| {
            let meta = stored_map.get(&id);
            let tracked = meta.map(|m| m.selected).unwrap_or(false);
            RoomResp {
                chat_id: id,
                title: meta.and_then(|m| m.title.clone()),
                member_count: meta.and_then(|m| m.member_count),
                kind: meta.and_then(|m| m.kind.clone()),
                open: open_set.contains(&id),
                selected: tracked,
                tracked,
                collect_status: meta
                    .and_then(|m| m.collect_status.clone())
                    .unwrap_or_else(|| "idle".to_string()),
                collected_rows: meta.and_then(|m| m.collected_rows),
            }
        })
        .collect();
    Ok(out)
}

/// 이름 재해석을 백그라운드 스레드에서 1회 실행(60s throttle + 중복 방지).
fn maybe_spawn_refresh(st: Shared) {
    if st.account.is_none() {
        return;
    }
    {
        let mut last = st.last_refresh.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = *last {
            if t.elapsed() < Duration::from_secs(60) {
                return;
            }
        }
        *last = Some(Instant::now());
    }
    if st.refreshing.swap(true, Ordering::SeqCst) {
        return; // 이미 진행 중.
    }
    std::thread::spawn(move || {
        refresh_names(&st);
        st.refreshing.store(false, Ordering::SeqCst);
    });
}

/// 실복호로 방 제목·종류·멤버수 + 작성자 닉네임을 재해석해 store 에 반영(백그라운드).
fn refresh_names(st: &AppState) {
    let Some(acct) = st.account.as_ref() else {
        return;
    };
    // 세션 키캐시(harvest 는 캐시 내부에서 1회·직렬).
    let Some(cands_arc) = st.cached_candidates() else {
        return;
    };
    let cands = cands_arc.as_ref();
    // 닉네임 맵(TalkUserDB) — 1:1 방 제목·작성자 이름 공용.
    let nick_map = kakao::talk_user_db_path(acct)
        .map(|p| kakao::import::load_nick_map(&p, cands))
        .unwrap_or_default();
    // 방 제목·종류·멤버수(chatListInfo/chatRoomList).
    if let Some(chat_list) = kakao::chat_list_info_path(acct) {
        let _ = kakao::import::enrich_rooms(&st.store, &chat_list, cands, &nick_map);
    }
    // 이미 저장된 메시지들의 작성자 이름 백필.
    if !nick_map.is_empty() {
        if let Ok(rooms) = st.store.list_rooms() {
            for r in rooms {
                if let Ok(ids) = st.store.distinct_author_ids(r.chat_id) {
                    if !ids.is_empty() {
                        let _ = kakao::import::apply_author_names(&st.store, r.chat_id, &ids, &nick_map);
                    }
                }
            }
        }
    }
}

// ── /api/rooms/select ───────────────────────────────────────────
#[derive(Deserialize)]
struct SelectReq {
    chat_ids: Vec<IdVal>,
}

async fn post_select(
    State(st): State<Shared>,
    Json(req): Json<SelectReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    // 문자열/숫자 id → i64(정밀도 보존). 파싱 불가 항목은 무시.
    let want: Vec<i64> = req.chat_ids.iter().filter_map(IdVal::to_i64).collect();
    let selected = st.store.set_selected(&want)?;
    // WORK-007: 추적 집합 저장 → 큐 동기화(새 방 enqueue=waiting, 해제 방 상태 비움).
    st.store.enqueue_tracked(&selected)?;
    // 백그라운드 수집 처리(열린 방→done, 닫힌 방→waiting 유지). 세션 키캐시 재사용.
    let st2 = st.clone();
    let ids = selected.clone();
    tokio::task::spawn_blocking(move || crate::watch::process_delta(&st2, &ids));
    Ok(Json(json!({ "ok": true, "selected": selected })))
}

// ── /api/collect (대화방 클릭 → 그 방 즉시 수집) ─────────────────
#[derive(Deserialize)]
struct CollectReq {
    chat_id: IdVal,
}

/// 사용자가 대화방을 클릭했을 때의 능동 수집. 백그라운드 재조정이 "닫힘"으로 오판한 방도
/// 여기서는 후보 스윕·재harvest 를 허용해 실제로 열려 있으면 수집한다(진짜 닫힘 → waiting).
///
/// 응답: `{ok, status, rows}` — status 는 done|waiting|collecting|not_tracked|error.
async fn post_collect(
    State(st): State<Shared>,
    Json(req): Json<CollectReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let Some(chat_id) = req.chat_id.to_i64() else {
        return Ok(Json(json!({ "ok": false, "status": "error", "rows": 0 })));
    };
    let st2 = st.clone();
    let out = tokio::task::spawn_blocking(move || crate::watch::collect_room_now(&st2, chat_id))
        .await?;
    Ok(Json(json!({
        "ok": out.status != "error",
        "status": out.status,
        "rows": out.rows,
    })))
}

// ── /api/import ─────────────────────────────────────────────────
#[derive(Deserialize)]
struct ImportReq {
    #[serde(default)]
    chat_id: Option<i64>,
}

async fn post_import(
    State(st): State<Shared>,
    Json(req): Json<ImportReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let st2 = st.clone();
    let entries = tokio::task::spawn_blocking(move || import_blocking(&st2, req.chat_id)).await??;
    // 계약 유지: imported = {chat_id: 신규행수}(가져온 방만). 추가: results = 방별 상태.
    let imported: serde_json::Map<String, serde_json::Value> = entries
        .iter()
        .filter(|e| e.status == "ok")
        .map(|e| (e.chat_id.to_string(), json!(e.imported)))
        .collect();
    let results: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| json!({ "chat_id": e.chat_id, "status": e.status, "imported": e.imported }))
        .collect();
    Ok(Json(json!({ "ok": true, "imported": imported, "results": results })))
}

/// import 한 방의 결과. status: "ok"(가져옴) / "closed"(방 닫힘·키없음) / "no_file"(로컬 chatLogs 없음).
struct ImportEntry {
    chat_id: i64,
    status: &'static str,
    imported: usize,
}

fn import_blocking(st: &AppState, chat_id: Option<i64>) -> anyhow::Result<Vec<ImportEntry>> {
    let mut out: Vec<ImportEntry> = Vec::new();
    let Some(acct) = st.account.as_ref() else {
        return Ok(out);
    };
    // 대상 = 지정 방 or ② 선택된 방들(③ 뷰어 아님).
    let want: Vec<i64> = match chat_id {
        Some(id) => vec![id],
        None => st.store.selected_ids()?,
    };
    if want.is_empty() {
        return Ok(out);
    }

    let file_map: HashMap<i64, RoomFile> = kakao::list_room_files(acct)
        .into_iter()
        .map(|r| (r.chat_id, r))
        .collect();

    // 세션 키캐시(harvest 는 캐시 내부에서 1회·직렬).
    let cands: Option<KeyCache> = st.cached_candidates();
    // 닉네임 맵 + 방 제목 enrich(가져온 뒤 이름이 바로 보이도록).
    let nick_map = match cands.as_ref() {
        Some(c) => {
            let cref = &**c;
            let nm = kakao::talk_user_db_path(acct)
                .map(|p| kakao::import::load_nick_map(&p, cref))
                .unwrap_or_default();
            if let Some(chat_list) = kakao::chat_list_info_path(acct) {
                let _ = kakao::import::enrich_rooms(&st.store, &chat_list, cref, &nm);
            }
            nm
        }
        None => HashMap::new(),
    };

    for id in want {
        let Some(room) = file_map.get(&id) else {
            // 로컬 chatLogs 파일 없음 — 카톡에서 방을 한번 열어야 파일 생성.
            out.push(ImportEntry { chat_id: id, status: "no_file", imported: 0 });
            continue;
        };
        let Some(c) = cands.as_ref() else {
            out.push(ImportEntry { chat_id: id, status: "closed", imported: 0 });
            continue;
        };
        match kakao::import::import_room(&st.store, room, &**c)? {
            Some(res) => {
                if !nick_map.is_empty() {
                    if let Ok(ids) = st.store.distinct_author_ids(id) {
                        let _ = kakao::import::apply_author_names(&st.store, id, &ids, &nick_map);
                    }
                }
                out.push(ImportEntry { chat_id: id, status: "ok", imported: res.imported });
            }
            None => {
                // 키 미회수 = 방이 닫혀 있음.
                out.push(ImportEntry { chat_id: id, status: "closed", imported: 0 });
            }
        }
    }
    Ok(out)
}

// ── /api/messages ───────────────────────────────────────────────
#[derive(Deserialize)]
struct MessagesQuery {
    chat_id: i64,
    #[serde(default)]
    after: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn get_messages(
    State(st): State<Shared>,
    Query(q): Query<MessagesQuery>,
) -> Result<Json<Vec<crate::store::MessageRow>>, AppError> {
    let after = q.after.unwrap_or(0);
    let limit = q.limit.unwrap_or(500).clamp(1, 5000);
    let mut rows = st.store.messages(q.chat_id, after, limit)?;
    // WORK-008: 사진 표식(kind/media_status) 부착.
    let media = st.store.media_status_map(q.chat_id).unwrap_or_default();
    if !media.is_empty() {
        for r in &mut rows {
            if let Some(status) = media.get(&r.log_id) {
                r.kind = Some("photo".to_string());
                r.media_status = Some(status.clone());
            }
        }
    }
    Ok(Json(rows))
}

/// WORK-008: 저장된 사진 바이트 서빙. saved 아니거나 파일 없으면 404.
async fn get_media(
    State(st): State<Shared>,
    AxPath((chat_id, log_id)): AxPath<(i64, i64)>,
) -> Response {
    let meta = st.store.media_of(chat_id, log_id);
    if let Ok(Some((mime, status, Some(file)))) = meta {
        if status == "saved" {
            let path = crate::kakao::photo::media_root().join(chat_id.to_string()).join(&file);
            if let Ok(bytes) = std::fs::read(&path) {
                let ct = mime.unwrap_or_else(|| "application/octet-stream".to_string());
                return (
                    [(axum::http::header::CONTENT_TYPE, ct)],
                    bytes,
                )
                    .into_response();
            }
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

// ── /api/stream (P2 SSE) ────────────────────────────────────────
#[derive(Deserialize)]
struct StreamQuery {
    chat_id: i64,
}

/// 선택 방의 실시간 새 메시지 SSE. `event: message`, payload = MessageRow shape(계약).
///
/// bounded broadcast 를 구독하고 해당 chat_id 만 필터. lagging 수신자는 자동 drop(무한 버퍼 없음).
/// 클라이언트 연결 종료 시 Stream 이 drop → 수신자 정리(누수 없음).
async fn get_stream(
    State(st): State<Shared>,
    Query(q): Query<StreamQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let chat_id = q.chat_id;
    let rx = st.tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |res| match res {
        // 이 방의 새 행만 message 이벤트로 방출.
        Ok(ev) if ev.chat_id == chat_id => {
            Event::default().event("message").json_data(&ev.row).ok().map(Ok)
        }
        // 다른 방 or Lagged(수신 지연으로 drop) → 스킵.
        _ => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
