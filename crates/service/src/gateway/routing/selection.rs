use codexmanager_core::storage::{now_ts, Account, Storage, Token, UsageSnapshotRecord};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use crate::account_availability::is_available;
use crate::usage_account_meta::{derive_account_meta, patch_account_meta_in_place};

static CANDIDATE_SNAPSHOT_CACHE: OnceLock<Mutex<Option<CandidateSnapshotCache>>> = OnceLock::new();
static SELECTION_CONFIG_LOADED: OnceLock<()> = OnceLock::new();
static CANDIDATE_CACHE_TTL_MS: AtomicU64 = AtomicU64::new(DEFAULT_CANDIDATE_CACHE_TTL_MS);
static CURRENT_DB_PATH: OnceLock<RwLock<String>> = OnceLock::new();
const DEFAULT_CANDIDATE_CACHE_TTL_MS: u64 = 500;
const CANDIDATE_CACHE_TTL_ENV: &str = "CODEXMANAGER_CANDIDATE_CACHE_TTL_MS";
const WEEKLY_WINDOW_MINUTES: i64 = 7 * 24 * 60;

#[derive(Clone)]
struct CandidateSnapshotCache {
    db_path: String,
    expires_at: Instant,
    candidates: Vec<(Account, Token)>,
}

pub(crate) fn collect_gateway_candidates(
    storage: &Storage,
) -> Result<Vec<(Account, Token)>, String> {
    if let Some(cached) = read_candidate_cache() {
        return Ok(cached);
    }

    let candidates = collect_gateway_candidates_uncached(storage)?;
    write_candidate_cache(candidates.clone());
    Ok(candidates)
}

fn collect_gateway_candidates_uncached(storage: &Storage) -> Result<Vec<(Account, Token)>, String> {
    // 选择可用账号作为网关上游候选
    let accounts = storage.list_accounts().map_err(|e| e.to_string())?;
    let tokens = storage.list_tokens().map_err(|e| e.to_string())?;
    let snaps = storage
        .latest_usage_snapshots_by_account()
        .map_err(|e| e.to_string())?;
    let mut token_map = HashMap::new();
    for token in tokens {
        token_map.insert(token.account_id.clone(), token);
    }
    let mut snap_map = HashMap::new();
    for snap in snaps {
        snap_map.insert(snap.account_id.clone(), snap);
    }

    let mut out = Vec::new();
    for account in &accounts {
        if account.status != "active" {
            continue;
        }
        let token = match token_map.get(&account.id) {
            Some(token) => token.clone(),
            None => continue,
        };
        let usage = snap_map.get(&account.id);
        if !is_available(usage) {
            continue;
        }
        let mut candidate_account = account.clone();
        let (chatgpt_account_id, workspace_id) = derive_account_meta(&token);
        if patch_account_meta_in_place(&mut candidate_account, chatgpt_account_id, workspace_id) {
            candidate_account.updated_at = now_ts();
            let _ = storage.insert_account(&candidate_account);
        }
        out.push((candidate_account, token));
    }
    if out.is_empty() {
        log_no_candidates(&accounts, &token_map, &snap_map);
    }
    maybe_sort_candidates_by_expiry(out.as_mut_slice(), &snap_map);
    Ok(out)
}

#[derive(Clone, Copy)]
struct ExpiryPriority {
    resets_at: i64,
    used_percent: f64,
}

fn maybe_sort_candidates_by_expiry(
    candidates: &mut [(Account, Token)],
    snap_map: &HashMap<String, UsageSnapshotRecord>,
) {
    if candidates.len() <= 1 || super::route_hint::current_route_strategy() != "expiry_first" {
        return;
    }
    candidates.sort_by(|(left_account, _), (right_account, _)| {
        compare_expiry_priority(
            snap_map.get(&left_account.id),
            snap_map.get(&right_account.id),
            left_account,
            right_account,
        )
    });
}

fn compare_expiry_priority(
    left_usage: Option<&UsageSnapshotRecord>,
    right_usage: Option<&UsageSnapshotRecord>,
    left_account: &Account,
    right_account: &Account,
) -> Ordering {
    match (
        expiry_priority_for_snapshot(left_usage),
        expiry_priority_for_snapshot(right_usage),
    ) {
        (Some(left), Some(right)) => left
            .resets_at
            .cmp(&right.resets_at)
            .then_with(|| right.used_percent.total_cmp(&left.used_percent))
            .then_with(|| left_account.sort.cmp(&right_account.sort))
            .then_with(|| left_account.id.cmp(&right_account.id)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left_account
            .sort
            .cmp(&right_account.sort)
            .then_with(|| left_account.id.cmp(&right_account.id)),
    }
}

fn expiry_priority_for_snapshot(snap: Option<&UsageSnapshotRecord>) -> Option<ExpiryPriority> {
    let snap = snap?;
    if let (Some(resets_at), Some(used_percent), Some(_window_minutes)) = (
        snap.secondary_resets_at,
        snap.secondary_used_percent,
        snap.secondary_window_minutes,
    ) {
        return Some(ExpiryPriority {
            resets_at,
            used_percent,
        });
    }
    if let (Some(window_minutes), Some(resets_at), Some(used_percent)) =
        (snap.window_minutes, snap.resets_at, snap.used_percent)
    {
        // 中文注释：部分免费账号只返回单个 7 天窗口，此时用 primary 作为“周额度”排序依据。
        if window_minutes >= WEEKLY_WINDOW_MINUTES {
            return Some(ExpiryPriority {
                resets_at,
                used_percent,
            });
        }
    }
    None
}

fn read_candidate_cache() -> Option<Vec<(Account, Token)>> {
    let ttl = candidate_cache_ttl();
    if ttl.is_zero() {
        return None;
    }
    let db_path = current_db_path();
    let now = Instant::now();
    let mutex = CANDIDATE_SNAPSHOT_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!("candidate snapshot cache lock poisoned; dropping cache and continuing");
            let mut guard = poisoned.into_inner();
            *guard = None;
            guard
        }
    };
    let cached = guard.as_ref()?;
    if cached.db_path != db_path || cached.expires_at <= now {
        *guard = None;
        return None;
    }
    Some(cached.candidates.clone())
}

fn write_candidate_cache(candidates: Vec<(Account, Token)>) {
    let ttl = candidate_cache_ttl();
    if ttl.is_zero() {
        return;
    }
    let db_path = current_db_path();
    let expires_at = Instant::now() + ttl;
    let mutex = CANDIDATE_SNAPSHOT_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!("candidate snapshot cache lock poisoned; recovering");
            poisoned.into_inner()
        }
    };
    *guard = Some(CandidateSnapshotCache {
        db_path,
        expires_at,
        candidates,
    });
}

pub(super) fn invalidate_candidate_cache() {
    if let Some(mutex) = CANDIDATE_SNAPSHOT_CACHE.get() {
        let mut guard = match mutex.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                log::warn!("candidate snapshot cache lock poisoned; recovering");
                poisoned.into_inner()
            }
        };
        *guard = None;
    }
}

fn candidate_cache_ttl() -> Duration {
    ensure_selection_config_loaded();
    let ttl_ms = CANDIDATE_CACHE_TTL_MS.load(AtomicOrdering::Relaxed);
    Duration::from_millis(ttl_ms)
}

fn current_db_path() -> String {
    ensure_selection_config_loaded();
    crate::lock_utils::read_recover(current_db_path_cell(), "current_db_path").clone()
}

fn log_no_candidates(
    accounts: &[Account],
    token_map: &HashMap<String, Token>,
    snap_map: &HashMap<String, UsageSnapshotRecord>,
) {
    let db_path = current_db_path();
    log::warn!(
        "gateway no candidates: db_path={}, accounts={}, tokens={}, snapshots={}",
        db_path,
        accounts.len(),
        token_map.len(),
        snap_map.len()
    );
    for account in accounts {
        let usage = snap_map.get(&account.id);
        log::warn!(
            "gateway account: id={}, status={}, has_token={}, primary=({:?}/{:?}) secondary=({:?}/{:?})",
            account.id,
            account.status,
            token_map.contains_key(&account.id),
            usage.and_then(|u| u.used_percent),
            usage.and_then(|u| u.window_minutes),
            usage.and_then(|u| u.secondary_used_percent),
            usage.and_then(|u| u.secondary_window_minutes),
        );
    }
}

pub(super) fn reload_from_env() {
    let ttl_ms = std::env::var(CANDIDATE_CACHE_TTL_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_CANDIDATE_CACHE_TTL_MS);
    CANDIDATE_CACHE_TTL_MS.store(ttl_ms, AtomicOrdering::Relaxed);

    let db_path = std::env::var("CODEXMANAGER_DB_PATH").unwrap_or_else(|_| "<unset>".to_string());
    let mut cached = crate::lock_utils::write_recover(current_db_path_cell(), "current_db_path");
    *cached = db_path;
}

fn ensure_selection_config_loaded() {
    let _ = SELECTION_CONFIG_LOADED.get_or_init(|| reload_from_env());
}

fn current_db_path_cell() -> &'static RwLock<String> {
    CURRENT_DB_PATH.get_or_init(|| RwLock::new("<unset>".to_string()))
}

#[cfg(test)]
fn clear_candidate_cache_for_tests() {
    invalidate_candidate_cache();
}

#[cfg(test)]
mod tests {
    use super::{
        clear_candidate_cache_for_tests, collect_gateway_candidates, CANDIDATE_CACHE_TTL_ENV,
    };
    use codexmanager_core::storage::{now_ts, Account, Storage, Token, UsageSnapshotRecord};
    use std::sync::Mutex;

    static CANDIDATE_CACHE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn candidate_snapshot_cache_reuses_recent_snapshot() {
        let _guard = CANDIDATE_CACHE_TEST_LOCK.lock().expect("lock");
        let previous_ttl = std::env::var(CANDIDATE_CACHE_TTL_ENV).ok();
        std::env::set_var(CANDIDATE_CACHE_TTL_ENV, "2000");
        super::reload_from_env();
        clear_candidate_cache_for_tests();

        let storage = Storage::open_in_memory().expect("open");
        storage.init().expect("init");
        storage
            .insert_account(&Account {
                id: "acc-cache-1".to_string(),
                label: "cached".to_string(),
                issuer: "issuer".to_string(),
                chatgpt_account_id: None,
                workspace_id: None,
                group_name: None,
                sort: 0,
                status: "active".to_string(),
                created_at: now_ts(),
                updated_at: now_ts(),
            })
            .expect("insert account");
        storage
            .insert_token(&Token {
                account_id: "acc-cache-1".to_string(),
                id_token: "id".to_string(),
                access_token: "access".to_string(),
                refresh_token: "refresh".to_string(),
                api_key_access_token: None,
                last_refresh: now_ts(),
            })
            .expect("insert token");
        storage
            .insert_usage_snapshot(&UsageSnapshotRecord {
                account_id: "acc-cache-1".to_string(),
                used_percent: Some(10.0),
                window_minutes: Some(300),
                resets_at: None,
                secondary_used_percent: None,
                secondary_window_minutes: None,
                secondary_resets_at: None,
                credits_json: None,
                captured_at: now_ts(),
            })
            .expect("insert snapshot");

        let first = collect_gateway_candidates(&storage).expect("first candidates");
        assert_eq!(first.len(), 1);

        storage
            .update_account_status("acc-cache-1", "inactive")
            .expect("mark inactive");
        let second = collect_gateway_candidates(&storage).expect("second candidates");
        assert_eq!(second.len(), 1);

        clear_candidate_cache_for_tests();
        if let Some(value) = previous_ttl {
            std::env::set_var(CANDIDATE_CACHE_TTL_ENV, value);
        } else {
            std::env::remove_var(CANDIDATE_CACHE_TTL_ENV);
        }
        super::reload_from_env();
    }

    #[test]
    fn candidates_follow_account_sort_order() {
        let _guard = CANDIDATE_CACHE_TEST_LOCK.lock().expect("lock");
        let previous_ttl = std::env::var(CANDIDATE_CACHE_TTL_ENV).ok();
        std::env::set_var(CANDIDATE_CACHE_TTL_ENV, "0");
        super::reload_from_env();
        clear_candidate_cache_for_tests();

        let storage = Storage::open_in_memory().expect("open");
        storage.init().expect("init");

        let now = now_ts();
        let accounts = vec![
            ("acc-sort-10", 10_i64),
            ("acc-sort-0", 0_i64),
            ("acc-sort-1", 1_i64),
        ];
        for (id, sort) in &accounts {
            storage
                .insert_account(&Account {
                    id: (*id).to_string(),
                    label: (*id).to_string(),
                    issuer: "issuer".to_string(),
                    chatgpt_account_id: None,
                    workspace_id: None,
                    group_name: None,
                    sort: *sort,
                    status: "active".to_string(),
                    created_at: now,
                    updated_at: now,
                })
                .expect("insert account");
            storage
                .insert_token(&Token {
                    account_id: (*id).to_string(),
                    id_token: "id".to_string(),
                    access_token: "access".to_string(),
                    refresh_token: "refresh".to_string(),
                    api_key_access_token: None,
                    last_refresh: now,
                })
                .expect("insert token");
            storage
                .insert_usage_snapshot(&UsageSnapshotRecord {
                    account_id: (*id).to_string(),
                    used_percent: Some(10.0),
                    window_minutes: Some(300),
                    resets_at: None,
                    secondary_used_percent: None,
                    secondary_window_minutes: None,
                    secondary_resets_at: None,
                    credits_json: None,
                    captured_at: now,
                })
                .expect("insert usage");
        }

        let candidates = collect_gateway_candidates(&storage).expect("collect candidates");
        let ordered_ids = candidates
            .iter()
            .map(|(account, _)| account.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ordered_ids, vec!["acc-sort-0", "acc-sort-1", "acc-sort-10"]);

        clear_candidate_cache_for_tests();
        if let Some(value) = previous_ttl {
            std::env::set_var(CANDIDATE_CACHE_TTL_ENV, value);
        } else {
            std::env::remove_var(CANDIDATE_CACHE_TTL_ENV);
        }
        super::reload_from_env();
    }

    #[test]
    fn expiry_first_prioritizes_near_reset_weekly_window() {
        let _guard = CANDIDATE_CACHE_TEST_LOCK.lock().expect("lock");
        let previous_ttl = std::env::var(CANDIDATE_CACHE_TTL_ENV).ok();
        let previous_strategy = std::env::var("CODEXMANAGER_ROUTE_STRATEGY").ok();
        std::env::set_var(CANDIDATE_CACHE_TTL_ENV, "0");
        std::env::set_var("CODEXMANAGER_ROUTE_STRATEGY", "expiry_first");
        super::reload_from_env();
        crate::gateway::reload_runtime_config_from_env();
        clear_candidate_cache_for_tests();

        let storage = Storage::open_in_memory().expect("open");
        storage.init().expect("init");

        let now = now_ts();
        let accounts = vec![
            (
                "acc-far",
                0_i64,
                Some(30.0),
                Some(10080),
                Some(now + 3 * 24 * 60 * 60),
            ),
            (
                "acc-soon",
                1_i64,
                Some(85.0),
                Some(10080),
                Some(now + 2 * 60 * 60),
            ),
            ("acc-fallback", 2_i64, Some(10.0), Some(300), None),
        ];
        for (id, sort, used_percent, window_minutes, resets_at) in &accounts {
            storage
                .insert_account(&Account {
                    id: (*id).to_string(),
                    label: (*id).to_string(),
                    issuer: "issuer".to_string(),
                    chatgpt_account_id: None,
                    workspace_id: None,
                    group_name: None,
                    sort: *sort,
                    status: "active".to_string(),
                    created_at: now,
                    updated_at: now,
                })
                .expect("insert account");
            storage
                .insert_token(&Token {
                    account_id: (*id).to_string(),
                    id_token: "id".to_string(),
                    access_token: "access".to_string(),
                    refresh_token: "refresh".to_string(),
                    api_key_access_token: None,
                    last_refresh: now,
                })
                .expect("insert token");
            storage
                .insert_usage_snapshot(&UsageSnapshotRecord {
                    account_id: (*id).to_string(),
                    used_percent: *used_percent,
                    window_minutes: *window_minutes,
                    resets_at: *resets_at,
                    secondary_used_percent: None,
                    secondary_window_minutes: None,
                    secondary_resets_at: None,
                    credits_json: None,
                    captured_at: now,
                })
                .expect("insert usage");
        }

        let candidates = collect_gateway_candidates(&storage).expect("collect candidates");
        let ordered_ids = candidates
            .iter()
            .map(|(account, _)| account.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ordered_ids, vec!["acc-soon", "acc-far", "acc-fallback"]);

        clear_candidate_cache_for_tests();
        if let Some(value) = previous_ttl {
            std::env::set_var(CANDIDATE_CACHE_TTL_ENV, value);
        } else {
            std::env::remove_var(CANDIDATE_CACHE_TTL_ENV);
        }
        if let Some(value) = previous_strategy {
            std::env::set_var("CODEXMANAGER_ROUTE_STRATEGY", value);
        } else {
            std::env::remove_var("CODEXMANAGER_ROUTE_STRATEGY");
        }
        crate::gateway::reload_runtime_config_from_env();
        super::reload_from_env();
    }
}
