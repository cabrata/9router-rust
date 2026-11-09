//! Port of open-sse/executors/perchai.js.
//!
//! Perch AI routes chat completions to
//! `https://app.perchai.app/api/perch-terminal/model-call`, a custom endpoint
//! wrapping OpenAI/Anthropic/Azure/OpenRouter frontier models with reasoning,
//! tool calling and verification.
//!
//! Streaming protocol: SSE with `data: {"type": ...}`:
//!   - `answer_delta`      -> content delta
//!   - `reasoning_delta`   -> reasoning_content delta
//!   - `tool_call_delta` / `tool_use_end` -> tool_calls delta
//!   - `done`              -> finish chunk + [DONE]
//!
//! Auth: bearer token from OAuth or `~/.perch/cli-auth-session.json`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use nr_core::http::{proxy_fetch, FetchRequest, HeaderList, ProxyOptions, UpstreamResponse};
use nr_core::{Credentials, ExecRequest, ExecResult, Executor, Json, Result};
use serde_json::json;
use tokio::sync::mpsc;

const DEFAULT_APP_URL: &str = "https://app.perchai.app";
const PERCH_USER_AGENT: &str = "perchai-cli/2.4.118";

/// Plan-gate code: every Perch call is rejected with 403
/// `tier_selection_required` until the account has picked a plan once
/// (Starter free / Pro). The Perch CLI does this at first login.
pub const PERCH_TIER_GATE_CODE: &str = "tier_selection_required";
const PERCH_PLAN_SIGNUP_URL: &str = "https://chat.perchai.app";

/// Surface gate: Perch only serves Perch-hosted models to clients it recognises
/// as a real CLI session.
pub const PERCH_SURFACE_GATE_CODE: &str = "perch_surface_required";
pub const PERCH_SURFACE_GATE_NOTICE: &str = "Perch AI rejected this session as direct API access (perch_surface_required). \
Perch-hosted models are licensed for Perch Web, Desktop and CLI only; use the Perch CLI, or pick a model/plan that allows API access";

/// Perch only accepts a request when the system prompt is at least this many
/// characters: a 197-char prompt is rejected with 403 perch_surface_required
/// while 200 chars of padding is accepted. The trigger is the system prompt,
/// not the tools, messages, runId, ticket or headers.
const PERCH_MIN_SYSTEM_PROMPT_CHARS: usize = 200;
const PERCH_SYSTEM_PROMPT_PAD: &str = "Context: this session runs through the Perch CLI gateway. Tools available to the model execute \
locally in the user's workspace, so call them as they are defined and never assume their output. \
Keep answers direct and complete. This note is environment context, not part of the user's request.";

/// Tier ("roost") model ids that route through Perch's own harness.
const ROOST_TIERS: [&str; 5] = ["standard", "pro", "starter", "standard_max", "pro_max"];

pub const PERCH_CLI_PLAN_PATH: &str = "/api/perch-terminal/cli-plan";
const PERCH_PLAN_CACHE_MS: u64 = 10 * 60 * 1000;
const PERCH_TICKET_TTL_MS: u64 = 300_000;

pub const PERCH_SUPABASE_URL: &str = "https://zlfuvsfjtgsdtqcaykia.supabase.co";
pub const PERCH_SUPABASE_ANON_KEY: &str = "sb_publishable_w4R_pNkUpygBFIljZCoOlA_67ACg0FO";
pub const PERCH_REFRESH_URL: &str = "https://zlfuvsfjtgsdtqcaykia.supabase.co/auth/v1/token?grant_type=refresh_token";

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn now_secs() -> u64 {
    now_ms() / 1000
}

/// `perchPlanNotice()`.
pub fn plan_notice() -> String {
    "Perch AI plan not selected for this account yet. Open the Perch AI connection in the dashboard, \
click \"Plan\", and pick Starter (free) or Pro — or open https://chat.perchai.app, sign in, and choose a plan"
        .to_string()
}

/// `isPerchTierGate(status, bodyText)`.
///
/// Only 403 carries the gate. 402/429 are usage limits
/// (`usage_limit_reached`, `starter_model_blocked`), and treating those as
/// "pick a plan" would wipe a perfectly good plan state and swap the real
/// error for plan advice.
pub fn is_tier_gate(status: u16, body_text: &str) -> bool {
    status == 403 && !body_text.is_empty() && body_text.contains(PERCH_TIER_GATE_CODE)
}

/// `perchContentText(content)`.
fn content_text(content: &Json) -> String {
    match content {
        Json::String(s) => s.clone(),
        Json::Array(parts) => parts
            .iter()
            .map(|p| p.as_str().map(String::from).unwrap_or_else(|| p.get("text").and_then(Json::as_str).unwrap_or("").to_string()))
            .collect(),
        _ => String::new(),
    }
}

/// `ensurePerchSystemPromptLength(messages)`.
///
/// Only pads an existing system prompt. Inventing one would shift the message
/// order (breaking multimodal/image index assumptions), and a request without a
/// system prompt is usually plain chat, which Perch does not gate.
pub fn ensure_system_prompt_length(messages: &Json) -> Json {
    let Some(list) = messages.as_array() else { return messages.clone() };
    if list.is_empty() {
        return messages.clone();
    }
    let Some(index) = list.iter().position(|m| m.get("role").and_then(Json::as_str) == Some("system")) else {
        return messages.clone();
    };
    let current = content_text(list[index].get("content").unwrap_or(&Json::Null));
    if current.chars().count() >= PERCH_MIN_SYSTEM_PROMPT_CHARS {
        return messages.clone();
    }

    let mut padded = list.clone();
    let original = list[index].get("content").cloned().unwrap_or(Json::Null);
    let content = match original {
        Json::String(s) => Json::String(format!("{s}\n\n{PERCH_SYSTEM_PROMPT_PAD}")),
        Json::Array(mut parts) => {
            parts.push(json!({ "type": "text", "text": PERCH_SYSTEM_PROMPT_PAD }));
            Json::Array(parts)
        }
        _ => Json::String(PERCH_SYSTEM_PROMPT_PAD.to_string()),
    };
    padded[index]["content"] = content;
    Json::Array(padded)
}

/// `parsePerchErrorPayload(bodyText)` -> (error, errorCode, plan).
pub fn parse_error_payload(body_text: &str) -> (String, String, Option<Json>) {
    let text = body_text.trim();
    if text.is_empty() {
        return (String::new(), String::new(), None);
    }
    match serde_json::from_str::<Json>(text) {
        Ok(parsed) => {
            let error = parsed
                .get("error")
                .and_then(Json::as_str)
                .map(String::from)
                .unwrap_or_default();
            let error_code = parsed
                .get("errorCode")
                .and_then(Json::as_str)
                .map(String::from)
                .or_else(|| parsed.get("limit").and_then(|l| l.get("errorCode")).and_then(Json::as_str).map(String::from))
                .unwrap_or_default();
            let plan = parsed
                .get("limit")
                .and_then(|l| l.get("plan"))
                .or_else(|| parsed.get("plan"))
                .filter(|p| !p.is_null())
                .cloned();
            (error, error_code, plan)
        }
        Err(_) => (text.to_string(), String::new(), None),
    }
}

/// `buildPerchTierGateBody(payload)`.
pub fn build_tier_gate_body(error: Option<&str>, plan: Option<&Json>) -> Json {
    json!({
        "ok": false,
        "error": error.filter(|e| !e.is_empty()).map(String::from).unwrap_or_else(plan_notice),
        "errorCode": PERCH_TIER_GATE_CODE,
        "limit": {
            "errorCode": PERCH_TIER_GATE_CODE,
            "limitKind": PERCH_TIER_GATE_CODE,
            "resetAt": Json::Null,
            "plan": plan.cloned().unwrap_or(Json::Null),
            "url": PERCH_PLAN_SIGNUP_URL,
        },
    })
}

/// Probed plan state for one token.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanState {
    pub required: bool,
    pub plan: Option<Json>,
    pub notice: String,
}

struct PlanCacheEntry {
    state: PlanState,
    expires_at_ms: u64,
}

static PLAN_CACHE: LazyLock<Mutex<HashMap<String, PlanCacheEntry>>> = LazyLock::new(Default::default);

/// `getCachedPerchPlan(token)`.
pub fn get_cached_plan(token: &str) -> Option<PlanState> {
    if token.is_empty() {
        return None;
    }
    let cache = PLAN_CACHE.lock().expect("perch plan cache");
    cache
        .get(token)
        .filter(|e| e.expires_at_ms > now_ms())
        .map(|e| e.state.clone())
}

/// `rememberPerchPlan(token, planState)`.
pub fn remember_plan(token: &str, state: &PlanState) {
    if token.is_empty() {
        return;
    }
    PLAN_CACHE.lock().expect("perch plan cache").insert(
        token.to_string(),
        PlanCacheEntry { state: state.clone(), expires_at_ms: now_ms() + PERCH_PLAN_CACHE_MS },
    );
}

/// `clearPerchPlanCache()` — test helper / on gate-clear.
pub fn clear_plan_cache() {
    PLAN_CACHE.lock().expect("perch plan cache").clear();
}

/// `resolvePerchPlanState(credentials)` — the plan state stored on the connection.
pub fn resolve_plan_state(credentials: &Credentials) -> PlanState {
    let required = credentials
        .psd()
        .and_then(|psd| psd.get("perchPlanRequired"))
        .map(nr_core::json::truthy)
        .unwrap_or(false);
    let plan = credentials
        .psd()
        .and_then(|psd| psd.get("perchPlan"))
        .filter(|p| !p.is_null())
        .cloned();
    let notice = credentials
        .psd()
        .and_then(|psd| psd.get("perchPlanNotice"))
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_string();
    PlanState { required, plan, notice }
}

/// `applyPerchPlanState(credentials, token, planState)` — persist the probe result.
pub fn apply_plan_state(credentials: &mut Credentials, plan_state: &PlanState) {
    let required = plan_state.required;
    let plan = plan_state.plan.clone().unwrap_or(Json::Null);
    let notice = plan_state.notice.clone();
    let psd = credentials.psd_mut();
    psd.insert("perchPlanRequired".into(), Json::Bool(required));
    psd.insert("perchPlan".into(), plan);
    psd.insert("perchPlanNotice".into(), Json::String(notice));
}

fn plan_probe_headers(token: &str) -> HeaderList {
    let mut headers = HeaderList::new();
    headers.insert("Accept".into(), "application/json".into());
    headers.insert("Authorization".into(), format!("Bearer {token}"));
    headers.insert("User-Agent".into(), PERCH_USER_AGENT.into());
    headers.insert("x-perch-locale".into(), "en".into());
    headers
}

/// `probePerchPlan(appUrl, token)` — ask Perch's own CLI plan endpoint.
pub async fn probe_plan(
    app_url: &str,
    token: &str,
    proxy: Option<&ProxyOptions>,
) -> PlanState {
    if let Some(cached) = get_cached_plan(token) {
        return cached;
    }
    let url = format!("{}{PERCH_CLI_PLAN_PATH}", app_url.trim_end_matches('/'));
    let mut req = FetchRequest::get(url);
    req.headers = plan_probe_headers(token);
    req.header_timeout = Some(Duration::from_millis(15_000));

    let state = match proxy_fetch(req, proxy).await {
        Ok(response) if response.ok() => {
            let body = response.json_body().await.unwrap_or(Json::Null);
            // `plan` may be nested in `{ ok, plan: {...} }` or top-level.
            let plan = body
                .get("plan")
                .filter(|p| !p.is_null())
                .cloned()
                .or_else(|| body.get("data").and_then(|d| d.get("plan")).filter(|p| !p.is_null()).cloned());
            let required = body.get("planRequired") == Some(&Json::Bool(true))
                || body.get("required") == Some(&Json::Bool(true))
                || (plan.is_none() && body.get("ok") == Some(&Json::Bool(true)));
            let notice = if required { plan_notice() } else { String::new() };
            PlanState { required, plan, notice }
        }
        Ok(response) => {
            // A 403 carrying the gate code means the account never picked a
            // plan; anything else (transport noise, 5xx) is not a gate.
            let status = response.status;
            let text = response.text().await.unwrap_or_default();
            if is_tier_gate(status, &text) {
                PlanState { required: true, plan: None, notice: plan_notice() }
            } else {
                PlanState::default()
            }
        }
        // Transport failure: do not claim a gate.
        Err(_) => PlanState::default(),
    };
    remember_plan(token, &state);
    state
}

/// Resolved Perch session (token + refresh material).
#[derive(Debug, Clone, Default)]
pub struct PerchSession {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<u64>,
    pub user_id: Option<String>,
    pub email: Option<String>,
    pub app_url: String,
}

fn perch_auth_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".perch").join("cli-auth-session.json"))
}

fn read_cli_session() -> Option<Json> {
    let path = perch_auth_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// `refreshPerchSession(session)` — Supabase refresh_token grant.
pub async fn refresh_session(session: &PerchSession, proxy: Option<&ProxyOptions>) -> Option<PerchSession> {
    let refresh_token = session.refresh_token.clone().filter(|t| !t.is_empty())?;
    let mut headers = HeaderList::new();
    headers.insert("Content-Type".into(), "application/json".into());
    headers.insert("apikey".into(), PERCH_SUPABASE_ANON_KEY.into());
    headers.insert("Authorization".into(), format!("Bearer {PERCH_SUPABASE_ANON_KEY}"));

    let body = json!({ "refresh_token": refresh_token });
    let mut req = FetchRequest::post_json(PERCH_REFRESH_URL, headers, &body);
    req.header_timeout = Some(Duration::from_millis(15_000));

    let response = proxy_fetch(req, proxy).await.ok()?;
    if !response.ok() {
        return None;
    }
    let data = response.json_body().await.ok()?;
    let access = data.get("access_token").and_then(Json::as_str).filter(|t| !t.is_empty())?;
    Some(PerchSession {
        access_token: access.to_string(),
        refresh_token: data
            .get("refresh_token")
            .and_then(Json::as_str)
            .map(String::from)
            .or(Some(refresh_token)),
        expires_at: data
            .get("expires_at")
            .and_then(Json::as_u64)
            .or_else(|| data.get("expires_in").and_then(Json::as_u64).map(|s| now_secs() + s))
            .or(Some(now_secs() + 3600)),
        user_id: data
            .get("user")
            .and_then(|u| u.get("id"))
            .and_then(Json::as_str)
            .map(String::from)
            .or(session.user_id.clone()),
        email: data
            .get("user")
            .and_then(|u| u.get("email"))
            .and_then(Json::as_str)
            .map(String::from)
            .or(session.email.clone()),
        app_url: if session.app_url.is_empty() { DEFAULT_APP_URL.into() } else { session.app_url.clone() },
    })
}

/// `resolvePerchSession(credentials)` — explicit credential first, then the
/// Perch CLI's on-disk session.
pub async fn resolve_session(credentials: &Credentials, proxy: Option<&ProxyOptions>) -> Option<PerchSession> {
    let explicit = credentials.api_key().or_else(|| credentials.access_token());
    let refresh_token = credentials
        .refresh_token()
        .or_else(|| credentials.psd_str("refreshToken"))
        .map(String::from);
    let expires_at = credentials
        .get("expiresAt")
        .and_then(Json::as_u64)
        .or_else(|| credentials.psd().and_then(|p| p.get("expiresAt")).and_then(Json::as_u64));
    let app_url = credentials
        .psd_str("appUrl")
        .or_else(|| credentials.str("appUrl"))
        .unwrap_or(DEFAULT_APP_URL)
        .to_string();

    if let Some(explicit) = explicit.filter(|t| !t.trim().is_empty()) {
        if expires_at.map(|e| e <= now_secs() + 90).unwrap_or(false) && refresh_token.is_some() {
            let seed = PerchSession { refresh_token: refresh_token.clone(), app_url: app_url.clone(), ..PerchSession::default() };
            if let Some(refreshed) = refresh_session(&seed, proxy).await {
                return Some(refreshed);
            }
        }
        return Some(PerchSession {
            access_token: explicit.trim().to_string(),
            refresh_token,
            expires_at,
            user_id: None,
            email: credentials.str("email").map(String::from),
            app_url,
        });
    }

    // Fallback: the Perch CLI's own session file.
    let parsed = read_cli_session()?;
    let access = parsed.get("accessToken").and_then(Json::as_str)?.trim().to_string();
    if access.is_empty() {
        return None;
    }
    let session = PerchSession {
        access_token: access,
        refresh_token: parsed.get("refreshToken").and_then(Json::as_str).map(String::from),
        expires_at: parsed.get("expiresAt").and_then(Json::as_u64),
        user_id: parsed.get("userId").and_then(Json::as_str).map(String::from),
        email: parsed.get("email").and_then(Json::as_str).map(String::from),
        app_url: parsed
            .get("appUrl")
            .and_then(Json::as_str)
            .filter(|u| !u.is_empty())
            .unwrap_or(DEFAULT_APP_URL)
            .to_string(),
    };
    if session.expires_at.map(|e| e <= now_secs() + 90).unwrap_or(false) && session.refresh_token.is_some() {
        if let Some(refreshed) = refresh_session(&session, proxy).await {
            // Best-effort persist so the CLI and this process agree.
            if let Some(path) = perch_auth_path() {
                let mut updated = parsed.clone();
                updated["accessToken"] = Json::String(refreshed.access_token.clone());
                if let Some(rt) = &refreshed.refresh_token {
                    updated["refreshToken"] = Json::String(rt.clone());
                }
                if let Some(exp) = refreshed.expires_at {
                    updated["expiresAt"] = Json::Number(exp.into());
                }
                updated["updatedAt"] = Json::String(chrono::Utc::now().to_rfc3339());
                let _ = std::fs::write(path, serde_json::to_string_pretty(&updated).unwrap_or_default());
            }
            return Some(refreshed);
        }
    }
    Some(session)
}

/// `resolvePerchToken(credentials)` — synchronous token lookup (no refresh).
pub fn resolve_token(credentials: &Credentials) -> Option<String> {
    if let Some(explicit) = credentials.api_key().or_else(|| credentials.access_token()) {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let parsed = read_cli_session()?;
    parsed
        .get("accessToken")
        .and_then(Json::as_str)
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// A Perch turn ticket.
#[derive(Debug, Clone)]
pub struct TurnTicket {
    pub ticket: String,
    pub run_id: Option<String>,
    pub expires_at_ms: u64,
}

struct TicketCacheEntry {
    ticket: TurnTicket,
}

static TICKET_CACHE: LazyLock<Mutex<HashMap<String, TicketCacheEntry>>> = LazyLock::new(Default::default);

/// Result of a turn-ticket request.
pub enum TicketResult {
    Ticket(TurnTicket),
    /// The ticket endpoint carried the plan gate.
    Gate(PlanState),
    None,
}

/// `getPerchTurnTicket(appUrl, token)`.
pub async fn get_turn_ticket(app_url: &str, token: &str, proxy: Option<&ProxyOptions>) -> TicketResult {
    if token.is_empty() {
        return TicketResult::None;
    }
    {
        let cache = TICKET_CACHE.lock().expect("perch ticket cache");
        if let Some(entry) = cache.get(token) {
            if entry.ticket.expires_at_ms > now_ms() + 30_000 {
                return TicketResult::Ticket(entry.ticket.clone());
            }
        }
    }

    let url = format!("{}/api/perch-terminal/turn-ticket", app_url.trim_end_matches('/'));
    let mut headers = HeaderList::new();
    headers.insert("Content-Type".into(), "application/json".into());
    headers.insert("Authorization".into(), format!("Bearer {token}"));
    headers.insert("User-Agent".into(), PERCH_USER_AGENT.into());
    headers.insert("x-perch-locale".into(), "en".into());

    let body = json!({ "surface": "cli", "profile": "standard" });
    let mut req = FetchRequest::post_json(url, headers, &body);
    req.header_timeout = Some(Duration::from_millis(15_000));

    let Ok(response) = proxy_fetch(req, proxy).await else { return TicketResult::None };
    if !response.ok() {
        // A plan gate means auth is fine but the account has not picked a plan:
        // refreshing the token cannot help, so report it instead of returning
        // nothing.
        if response.status == 403 {
            let text = response.text().await.unwrap_or_default();
            if is_tier_gate(403, &text) {
                let (_, _, plan) = parse_error_payload(&text);
                return TicketResult::Gate(PlanState { required: true, plan, notice: plan_notice() });
            }
        }
        return TicketResult::None;
    }
    let data = response.json_body().await.unwrap_or(Json::Null);
    let ticket = data
        .get("ticket")
        .and_then(Json::as_str)
        .or_else(|| data.get("token").and_then(Json::as_str))
        .filter(|t| !t.is_empty());
    if data.get("ok") == Some(&Json::Bool(true)) {
        if let Some(ticket) = ticket {
            let expires_at_ms = data
                .get("expiresAt")
                .and_then(Json::as_str)
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.timestamp_millis().max(0) as u64)
                .unwrap_or_else(|| now_ms() + PERCH_TICKET_TTL_MS);
            let entry = TurnTicket {
                ticket: ticket.to_string(),
                run_id: data.get("runId").and_then(Json::as_str).map(String::from),
                expires_at_ms,
            };
            TICKET_CACHE
                .lock()
                .expect("perch ticket cache")
                .insert(token.to_string(), TicketCacheEntry { ticket: entry.clone() });
            return TicketResult::Ticket(entry);
        }
    }
    TicketResult::None
}

/// `invalidateTurnTicket(token)` — drop a cached ticket (used after a 401).
pub fn invalidate_turn_ticket(token: &str) {
    TICKET_CACHE.lock().expect("perch ticket cache").remove(token);
}

/// `buildPerchRequestBody(model, body, extra)`.
///
/// Tier ("roost") calls are routed through Perch's own harness and always carry
/// a full prompt; padding them would only change their input. The surface gate
/// bites on direct manual-model calls, so pad those.
pub fn build_request_body(model: &str, body: &Json, run_id: Option<&str>, has_ticket: bool) -> Json {
    let clean_model = model
        .strip_prefix("perchai/")
        .or_else(|| {
            let lower = model.to_lowercase();
            if lower.starts_with("perch/") {
                Some(&model[6..])
            } else {
                None
            }
        })
        .unwrap_or(model)
        .trim()
        .to_lowercase();
    let is_tier = ROOST_TIERS.contains(&clean_model.as_str());

    let run_id = run_id
        .map(String::from)
        .unwrap_or_else(|| format!("cli-turn-{}-{:x}", now_ms(), rand::random::<u32>() & 0xff_ffff));

    let messages = body.get("messages").cloned().unwrap_or(Json::Array(Vec::new()));
    let messages = if is_tier { messages } else { ensure_system_prompt_length(&messages) };

    let mut request = json!({ "messages": messages });
    if let Some(tools) = body.get("tools").and_then(Json::as_array).filter(|t| !t.is_empty()) {
        request["tools"] = Json::Array(tools.clone());
    }
    if let Some(v) = body.get("max_tokens").filter(|v| nr_core::json::truthy(v)) {
        request["maxTokens"] = v.clone();
    }
    if let Some(v) = body.get("max_completion_tokens").filter(|v| nr_core::json::truthy(v)) {
        request["maxTokens"] = v.clone();
    }
    if let Some(v) = body.get("temperature").filter(|v| !v.is_null()) {
        request["temperature"] = v.clone();
    }
    if let Some(v) = body.get("tool_choice").filter(|v| nr_core::json::truthy(v)) {
        request["toolChoice"] = v.clone();
    }
    if let Some(v) = body.get("response_format").filter(|v| nr_core::json::truthy(v)) {
        request["responseFormat"] = v.clone();
    }

    let mut payload = json!({
        "request": request,
        "runId": run_id,
        "lane": "chat",
        "strictManual": false,
        "preferredModelId": if is_tier { Json::Null } else { Json::String(clean_model.clone()) },
        "avoidModelIds": [],
        "attribution": Json::Null,
        // The CLI always asks for the held/output payload to be resent; without
        // it Perch can stream a turn that never yields usable output.
        "resendAfterOutput": true,
    });
    if has_ticket {
        payload["clientSurface"] = Json::String("cli".into());
    }
    if is_tier {
        payload["roostModelChoice"] = Json::String(clean_model);
        payload["roostReasoning"] = Json::Bool(true);
        payload["effort"] = json!({ "level": "high", "orchestration": false });
    }
    // The CLI sends no model selector for manual picks: preferredModelId above
    // is what routes the call. `manualModelOptionId` made Perch answer
    // 403 perch_surface_required on every manual-model call.
    payload
}

/// `normalizeToolArguments(rawArgs)` — an empty argument set must be `{}`,
/// because clients reject `arguments: ""` as "arguments must be a JSON object".
pub fn normalize_tool_arguments(raw: Option<&Json>) -> String {
    match raw {
        Some(Json::String(s)) if !s.trim().is_empty() => s.clone(),
        Some(Json::String(_)) | None => "{}".into(),
        Some(Json::Object(_)) | Some(Json::Array(_)) => raw.map(|v| v.to_string()).unwrap_or_else(|| "{}".into()),
        Some(Json::Null) => "{}".into(),
        Some(other) => other.to_string(),
    }
}

fn format_tool_call(tc: &Json, index: usize) -> Json {
    let function = tc.get("function");
    let name = tc
        .get("name")
        .and_then(Json::as_str)
        .or_else(|| function.and_then(|f| f.get("name")).and_then(Json::as_str))
        .unwrap_or("");
    let raw_args = tc
        .get("arguments")
        .or_else(|| function.and_then(|f| f.get("arguments")));
    json!({
        "index": index,
        "id": tc.get("id").and_then(Json::as_str).map(String::from).unwrap_or_else(|| format!("call_{index}")),
        "type": "function",
        "function": {
            "name": name,
            "arguments": normalize_tool_arguments(raw_args),
        },
    })
}

/// `wrapPerchCompletionStream(response, model)` — Perch events -> OpenAI SSE.
pub fn wrap_completion_stream(upstream: UpstreamResponse, model: &str) -> UpstreamResponse {
    let (tx, rx) = mpsc::channel::<std::result::Result<Bytes, std::io::Error>>(16);
    let model = model.to_string();

    tokio::spawn(async move {
        let chunk_id = format!("chatcmpl-perch-{}-{:x}", now_ms(), rand::random::<u32>() & 0xfffff);
        let created = now_secs();
        let mut buffer = String::new();
        let mut done = false;
        // Perch emits the full running tool-call list on every delta. The OpenAI
        // stream format addresses calls by stable index, so remember how many we
        // already sent instead of re-numbering from 0 on each chunk (which made
        // args overwrite).
        let mut announced_tool_calls: usize = 0;

        struct ChunkFactory {
            id: String,
            created: u64,
            model: String,
            first_sent: bool,
        }

        impl ChunkFactory {
            /// First chunk of the turn carries `role: "assistant"`.
            fn make(&mut self, mut delta: Json, finish: Json, usage: Option<Json>) -> Json {
                if !self.first_sent {
                    self.first_sent = true;
                    if let Some(obj) = delta.as_object_mut() {
                        obj.insert("role".into(), json!("assistant"));
                    }
                }
                let mut chunk = json!({
                    "id": self.id,
                    "object": "chat.completion.chunk",
                    "created": self.created,
                    "model": self.model,
                    "choices": [{
                        "index": 0,
                        "delta": delta,
                        "finish_reason": finish,
                    }],
                });
                if let Some(usage) = usage {
                    chunk["usage"] = usage;
                }
                chunk
            }
        }

        let mut factory = ChunkFactory {
            id: chunk_id,
            created,
            model: model.clone(),
            first_sent: false,
        };

        let mut handle_line = |line: &str,
                              buffer_out: &mut String,
                              done: &mut bool,
                              announced: &mut usize| {
            if *done {
                return;
            }
            let mut text = line.trim_end_matches('\r').trim();
            if text.is_empty() {
                return;
            }
            if text.starts_with("data:") {
                text = text[5..].trim_start();
            }
            if text == "[DONE]" {
                buffer_out.push_str("data: [DONE]\n\n");
                *done = true;
                return;
            }
            let Ok(payload) = serde_json::from_str::<Json>(text) else { return };
            if !payload.is_object() {
                return;
            }
            let payload_type = payload.get("type").and_then(Json::as_str).unwrap_or("");

            match payload_type {
                "answer_delta" => {
                    if let Some(text) = payload.get("text").and_then(Json::as_str) {
                        let chunk = factory.make(json!({ "content": text }), Json::Null, None);
                        buffer_out.push_str(&format!("data: {chunk}\n\n"));
                    }
                }
                "reasoning_delta" => {
                    if let Some(text) = payload.get("text").and_then(Json::as_str) {
                        let chunk = factory.make(json!({ "reasoning_content": text }), Json::Null, None);
                        buffer_out.push_str(&format!("data: {chunk}\n\n"));
                    }
                }
                "tool_call_delta" | "tool_use_end" => {
                    let Some(calls) = payload.get("toolCalls").and_then(Json::as_array) else { return };
                    let all: Vec<Json> = calls.iter().enumerate().map(|(i, tc)| format_tool_call(tc, i)).collect();
                    let fresh: Vec<Json> = all.iter().skip(*announced).cloned().collect();
                    *announced = all.len();
                    if fresh.is_empty() {
                        return;
                    }
                    let chunk = factory.make(json!({ "tool_calls": fresh }), Json::Null, None);
                    buffer_out.push_str(&format!("data: {chunk}\n\n"));
                }
                "error" => {
                    let message = payload
                        .get("message")
                        .and_then(Json::as_str)
                        .or_else(|| payload.get("error").and_then(Json::as_str))
                        .unwrap_or("Stream error");
                    let chunk = factory.make(
                        json!({ "content": format!("[Perch AI Error: {message}]") }),
                        Json::String("stop".into()),
                        None,
                    );
                    buffer_out.push_str(&format!("data: {chunk}\n\n"));
                    buffer_out.push_str("data: [DONE]\n\n");
                    *done = true;
                }
                "done" => {
                    if payload.get("ok") == Some(&Json::Bool(false)) {
                        let message = payload.get("error").and_then(Json::as_str).unwrap_or("Perch AI model call failed");
                        let chunk = factory.make(
                            json!({ "content": format!("[Perch AI Error: {message}]") }),
                            Json::String("stop".into()),
                            None,
                        );
                        buffer_out.push_str(&format!("data: {chunk}\n\n"));
                    } else {
                        let calls = payload.get("toolCalls").and_then(Json::as_array);
                        let has_tools = calls.map(|c| !c.is_empty()).unwrap_or(false) || *announced > 0;
                        let finish_reason = if has_tools { "tool_calls" } else { "stop" };
                        // OpenAI streaming spec: client accumulators concatenate
                        // `arguments` across chunks for the same index. Re-sending
                        // already-announced calls in the `done` chunk doubles the
                        // JSON string (`{}{}`), which clients report as
                        // "arguments must be a JSON object, got null".
                        let fresh: Vec<Json> = match calls {
                            Some(calls) => {
                                let all: Vec<Json> =
                                    calls.iter().enumerate().map(|(i, tc)| format_tool_call(tc, i)).collect();
                                let fresh: Vec<Json> = all.iter().skip(*announced).cloned().collect();
                                *announced = all.len();
                                fresh
                            }
                            None => Vec::new(),
                        };
                        let delta = if fresh.is_empty() {
                            json!({})
                        } else {
                            json!({ "tool_calls": fresh })
                        };
                        let usage = payload.get("usage").filter(|u| !u.is_null()).cloned();
                        let chunk = factory.make(delta, Json::String(finish_reason.into()), usage);
                        buffer_out.push_str(&format!("data: {chunk}\n\n"));
                    }
                    buffer_out.push_str("data: [DONE]\n\n");
                    *done = true;
                }
                _ => {}
            }
        };

        let mut stream = upstream.body;
        while !done {
            match futures::StreamExt::next(&mut stream).await {
                Some(Ok(bytes)) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    let mut out = String::new();
                    while let Some(nl) = buffer.find('\n') {
                        let line: String = buffer[..nl].to_string();
                        buffer.drain(..=nl);
                        handle_line(&line, &mut out, &mut done, &mut announced_tool_calls);
                    }
                    if !out.is_empty() && tx.send(Ok(Bytes::from(out))).await.is_err() {
                        return;
                    }
                    if done {
                        break;
                    }
                }
                Some(Err(_)) | None => {
                    let mut out = String::new();
                    if !buffer.trim().is_empty() {
                        let tail = std::mem::take(&mut buffer);
                        handle_line(&tail, &mut out, &mut done, &mut announced_tool_calls);
                    }
                    if !done {
                        out.push_str("data: [DONE]\n\n");
                        done = true;
                    }
                    let _ = tx.send(Ok(Bytes::from(out))).await;
                    break;
                }
            }
        }

        if !done {
            let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
        }
    });

    let mut headers = http::HeaderMap::new();
    headers.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("text/event-stream"));
    headers.insert(http::header::CACHE_CONTROL, http::HeaderValue::from_static("no-cache"));
    UpstreamResponse::from_stream(200, headers, Box::pin(futures::stream::poll_fn({
        let mut rx = rx;
        move |cx| rx.poll_recv(cx)
    })))
}

/// `wrapPerchNonStreaming(response, model)` — Perch JSON -> OpenAI completion.
pub async fn wrap_non_streaming(upstream: UpstreamResponse, model: &str) -> UpstreamResponse {
    if !upstream.ok() {
        return upstream;
    }
    let body = upstream.json_body().await.unwrap_or(Json::Null);

    if body.get("ok") == Some(&Json::Bool(false)) {
        return UpstreamResponse::json(
            400,
            &json!({
                "error": {
                    "message": body.get("error").and_then(Json::as_str).unwrap_or("Perch AI request failed"),
                    "type": "perchai_error",
                    "code": body.get("errorCode").and_then(Json::as_str).unwrap_or("api_error"),
                }
            }),
        );
    }

    let chunk_id = format!("chatcmpl-perch-{}-{:x}", now_ms(), rand::random::<u32>() & 0xfffff);
    let tool_calls: Vec<Json> = body
        .get("toolCalls")
        .and_then(Json::as_array)
        .map(|calls| {
            calls
                .iter()
                .enumerate()
                .map(|(i, tc)| {
                    let function = tc.get("function");
                    json!({
                        "id": tc.get("id").and_then(Json::as_str).map(String::from).unwrap_or_else(|| format!("call_{i}")),
                        "type": "function",
                        "function": {
                            "name": tc.get("name").and_then(Json::as_str)
                                .or_else(|| function.and_then(|f| f.get("name")).and_then(Json::as_str))
                                .unwrap_or(""),
                            "arguments": normalize_tool_arguments(
                                tc.get("arguments").or_else(|| function.and_then(|f| f.get("arguments")))
                            ),
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let finish_reason = if tool_calls.is_empty() { "stop" } else { "tool_calls" };

    let mut message = json!({
        "role": "assistant",
        "content": body.get("text").and_then(Json::as_str).unwrap_or(""),
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = Json::Array(tool_calls);
    }

    UpstreamResponse::json(
        200,
        &json!({
            "id": chunk_id,
            "object": "chat.completion",
            "created": now_secs(),
            "model": model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": finish_reason,
            }],
            "usage": body.get("usage").filter(|u| !u.is_null()).cloned().unwrap_or_else(|| json!({
                "prompt_tokens": 0,
                "completion_tokens": 0,
                "total_tokens": 0,
            })),
        }),
    )
}

/// `await response.clone().text()` — read the body but keep a usable response.
///
/// The JS executor reads the 401/403 body once ("the body used to be drained
/// twice, which threw and swallowed the retry"), so the Rust port rebuilds the
/// response from the bytes it read instead of leaving a consumed stream behind.
async fn read_error_body(response: UpstreamResponse) -> (String, UpstreamResponse) {
    let status = response.status;
    let headers = response.headers.clone();
    let text = response.text().await.unwrap_or_default();
    let rebuilt = UpstreamResponse::from_bytes(status, headers, text.clone());
    (text, rebuilt)
}

/// Perch AI executor.
pub struct PerchAiExecutor {
    provider: String,
    config: &'static Json,
}

impl PerchAiExecutor {
    pub fn new(provider: &str) -> Self {
        let config = nr_data::provider(provider)
            .or_else(|| nr_data::provider("perchai"))
            .unwrap_or(&EMPTY_CONFIG);
        Self { provider: provider.to_string(), config }
    }

    fn app_url(credentials: &Credentials, session: &PerchSession) -> String {
        credentials
            .psd_str("appUrl")
            .or_else(|| credentials.str("appUrl"))
            .map(String::from)
            .or_else(|| Some(session.app_url.clone()).filter(|u| !u.is_empty()))
            .unwrap_or_else(|| DEFAULT_APP_URL.to_string())
    }

    fn base_headers(token: &str, stream: bool) -> HeaderList {
        let mut headers = HeaderList::new();
        headers.insert("Content-Type".into(), "application/json".into());
        headers.insert("User-Agent".into(), PERCH_USER_AGENT.into());
        headers.insert("x-perch-locale".into(), "en".into());
        headers.insert("Authorization".into(), format!("Bearer {token}"));
        if stream {
            headers.insert("Accept".into(), "text/event-stream".into());
        }
        headers
    }

    async fn post(
        &self,
        url: &str,
        headers: HeaderList,
        body: &Json,
        proxy: &ProxyOptions,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<UpstreamResponse> {
        let mut req = FetchRequest::post_json(url, headers, body);
        req.header_timeout = Some(Duration::from_millis(
            self.config.get("timeoutMs").and_then(Json::as_u64).unwrap_or(180_000),
        ));
        req.cancel = Some(cancel.clone());
        proxy_fetch(req, Some(proxy)).await
    }
}

static EMPTY_CONFIG: std::sync::LazyLock<Json> = std::sync::LazyLock::new(|| Json::Object(Default::default()));

impl nr_exec_base::base::BaseHooks for PerchAiExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn config(&self) -> &Json {
        self.config
    }

    fn build_url(&self, _model: &str, _stream: bool, _url_index: usize, creds: &Credentials) -> Result<String> {
        let app_url = creds
            .psd_str("appUrl")
            .or_else(|| creds.str("appUrl"))
            .unwrap_or(DEFAULT_APP_URL);
        Ok(format!("{}/api/perch-terminal/model-call", app_url.trim_end_matches('/')))
    }
}

#[async_trait]
impl Executor for PerchAiExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    async fn execute(&self, req: ExecRequest<'_>) -> Result<ExecResult> {
        let ExecRequest { model, body, stream, mut credentials, cancel, log, proxy, .. } = req;

        let mut session = match resolve_session(&credentials, Some(&proxy)).await {
            Some(s) => s,
            None => {
                return Err(nr_core::Error::Provider(
                    "Perch AI requires authentication. Please sign in via OAuth or run `perch login` in terminal."
                        .into(),
                ))
            }
        };
        let mut token = session.access_token.clone();
        let app_url = Self::app_url(&credentials, &session);
        let model_url = format!("{}/api/perch-terminal/model-call", app_url.trim_end_matches('/'));

        // Plan gate: the account must pick Starter once, exactly like the Perch
        // CLI setup prompt (checked through its own cli-plan endpoint).
        let stored_plan = resolve_plan_state(&credentials);
        let mut plan_state = probe_plan(&app_url, &token, Some(&proxy)).await;
        if plan_state.plan.is_none() && stored_plan.plan.is_some() {
            plan_state.plan = stored_plan.plan.clone();
        }
        if !plan_state.required && plan_state.plan.is_none() && !stored_plan.notice.is_empty() {
            plan_state.notice = String::new();
        }
        remember_plan(&token, &plan_state);
        apply_plan_state(&mut credentials, &plan_state);
        if plan_state.required {
            return Ok(ExecResult {
                response: UpstreamResponse::json(
                    403,
                    &build_tier_gate_body(Some(&plan_state.notice), plan_state.plan.as_ref()),
                ),
                url: model_url,
                headers: HeaderList::new(),
                transformed_body: body,
                response_format: None,
            });
        }

        // Acquire or reuse the Perch turn ticket (matches CLI ticket minting).
        let mut ticket_data = get_turn_ticket(&app_url, &token, Some(&proxy)).await;
        if let TicketResult::Gate(gate) = &ticket_data {
            remember_plan(&token, gate);
            apply_plan_state(&mut credentials, gate);
            return Ok(ExecResult {
                response: UpstreamResponse::json(
                    403,
                    &build_tier_gate_body(Some(&gate.notice), gate.plan.as_ref()),
                ),
                url: format!("{}/api/perch-terminal/turn-ticket", app_url.trim_end_matches('/')),
                headers: HeaderList::new(),
                transformed_body: body,
                response_format: None,
            });
        }

        // If the turn ticket failed with 401 and we have a refresh_token,
        // refresh the session and retry the ticket.
        let mut ticket = match ticket_data {
            TicketResult::Ticket(t) => Some(t),
            _ => None,
        };
        if ticket.is_none() && session.refresh_token.is_some() {
            if let Some(refreshed) = refresh_session(&session, Some(&proxy)).await {
                if !refreshed.access_token.is_empty() {
                    session = refreshed;
                    token = session.access_token.clone();
                    ticket_data = get_turn_ticket(&app_url, &token, Some(&proxy)).await;
                    ticket = match ticket_data {
                        TicketResult::Ticket(t) => Some(t),
                        _ => None,
                    };
                }
            }
        }

        let payload = build_request_body(model, &body, ticket.as_ref().and_then(|t| t.run_id.as_deref()), ticket.is_some());

        let mut headers = Self::base_headers(&token, stream);
        if let Some(t) = &ticket {
            headers.insert("x-perch-turn-ticket".into(), t.ticket.clone());
        }

        let mut response = self.post(&model_url, headers.clone(), &payload, &proxy, &cancel).await?;

        // Read the 401/403 body once. The body used to be drained twice (the
        // outer handler re-read a consumed stream), which threw and swallowed
        // the retry.
        let mut error_text = String::new();
        if response.status == 401 || response.status == 403 {
            let (text, rebuilt) = read_error_body(response).await;
            error_text = text;
            response = rebuilt;
        }

        // Only real auth failures may retry after refreshing. The surface gate
        // and other gates are not fixed by a new token, so they skip the retry.
        let is_auth_failure = response.status == 401 || error_text.contains("not_authorized");
        if is_auth_failure && session.refresh_token.is_some() {
            if let Some(refreshed) = refresh_session(&session, Some(&proxy)).await {
                if !refreshed.access_token.is_empty() {
                    session = refreshed;
                    token = session.access_token.clone();
                    invalidate_turn_ticket(&token);
                    let retry_ticket = get_turn_ticket(&app_url, &token, Some(&proxy)).await;
                    let mut retry_payload = payload.clone();
                    let mut retry_headers = Self::base_headers(&token, stream);
                    match retry_ticket {
                        TicketResult::Ticket(t) => {
                            retry_headers.insert("x-perch-turn-ticket".into(), t.ticket.clone());
                            if let Some(run_id) = t.run_id {
                                retry_payload["runId"] = Json::String(run_id);
                            }
                            retry_payload["clientSurface"] = Json::String("cli".into());
                        }
                        _ => {
                            retry_headers.shift_remove("x-perch-turn-ticket");
                            if let Some(obj) = retry_payload.as_object_mut() {
                                obj.shift_remove("clientSurface");
                            }
                        }
                    }
                    if let Ok(retried) = self.post(&model_url, retry_headers, &retry_payload, &proxy, &cancel).await {
                        // Keep the first response when the retry did not improve
                        // on it, so the real upstream error is never replaced by
                        // an empty/opaque one.
                        let retried_ok = retried.ok();
                        let first_ok = response.ok();
                        if retried_ok || !first_ok {
                            response = retried;
                            if retried_ok {
                                error_text = String::new();
                            } else {
                                let (text, rebuilt) = read_error_body(response).await;
                                error_text = text;
                                response = rebuilt;
                            }
                        }
                    }
                }
            }
        }

        // Surface gate: Perch refused the session as "direct API access".
        // Report it as a real error instead of handing back a 403 that reads
        // like a dead turn.
        if response.status == 403 && error_text.contains(PERCH_SURFACE_GATE_CODE) {
            let (error, _, _) = parse_error_payload(&error_text);
            return Ok(ExecResult {
                response: UpstreamResponse::json(
                    403,
                    &json!({
                        "ok": false,
                        "error": if error.is_empty() { PERCH_SURFACE_GATE_NOTICE.to_string() } else { error },
                        "errorCode": PERCH_SURFACE_GATE_CODE,
                    }),
                ),
                url: model_url,
                headers: HeaderList::new(),
                transformed_body: payload,
                response_format: None,
            });
        }

        // The gate probe can be right at login time and stale minutes later.
        if response.status == 403 && error_text.contains(PERCH_TIER_GATE_CODE) {
            let (error, _, plan) = parse_error_payload(&error_text);
            let gate = PlanState { required: true, plan, notice: plan_notice() };
            remember_plan(&token, &gate);
            apply_plan_state(&mut credentials, &gate);
            log.warn("PERCH", "plan gate returned by model-call; surfacing plan notice");
            return Ok(ExecResult {
                response: UpstreamResponse::json(403, &build_tier_gate_body(Some(&error), gate.plan.as_ref())),
                url: model_url,
                headers: HeaderList::new(),
                transformed_body: payload,
                response_format: None,
            });
        }

        let wrapped = if response.ok() {
            if stream {
                wrap_completion_stream(response, model)
            } else {
                wrap_non_streaming(response, model).await
            }
        } else {
            response
        };

        Ok(ExecResult {
            response: wrapped,
            url: model_url,
            headers: HeaderList::new(),
            transformed_body: payload,
            response_format: None,
        })
    }
}

/// Executor for the canonical provider id (aliases are resolved in nr-executors).
pub fn create(provider: &str) -> Arc<dyn Executor> {
    Arc::new(PerchAiExecutor::new(provider))
}

use std::sync::Arc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pads_short_system_prompts_only() {
        let messages = json!([
            { "role": "system", "content": "be brief" },
            { "role": "user", "content": "hi" }
        ]);
        let padded = ensure_system_prompt_length(&messages);
        let text = content_text(&padded[0]["content"]);
        assert!(text.chars().count() >= PERCH_MIN_SYSTEM_PROMPT_CHARS);
        assert!(text.starts_with("be brief\n\n"));
        // User message untouched, order preserved.
        assert_eq!(padded[1]["content"], json!("hi"));
    }

    #[test]
    fn never_invents_a_system_prompt() {
        let messages = json!([{ "role": "user", "content": "hi" }]);
        assert_eq!(ensure_system_prompt_length(&messages), messages);
        // Empty input too.
        let empty = json!([]);
        assert_eq!(ensure_system_prompt_length(&empty), empty);
    }

    #[test]
    fn long_system_prompt_is_left_alone() {
        let long = "x".repeat(PERCH_MIN_SYSTEM_PROMPT_CHARS);
        let messages = json!([{ "role": "system", "content": long }]);
        assert_eq!(ensure_system_prompt_length(&messages), messages);
    }

    #[test]
    fn pads_array_content_with_a_text_block() {
        let messages = json!([{
            "role": "system",
            "content": [{ "type": "text", "text": "short" }]
        }]);
        let padded = ensure_system_prompt_length(&messages);
        let parts = padded[0]["content"].as_array().expect("array content");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1]["text"], json!(PERCH_SYSTEM_PROMPT_PAD));
    }

    #[test]
    fn tier_gate_only_matches_403_with_the_code() {
        assert!(is_tier_gate(403, &format!("{{\"errorCode\":\"{PERCH_TIER_GATE_CODE}\"}}")));
        // 402/429 are usage limits, not the plan gate.
        assert!(!is_tier_gate(402, PERCH_TIER_GATE_CODE));
        assert!(!is_tier_gate(429, PERCH_TIER_GATE_CODE));
        // A bare 403 has no usable body.
        assert!(!is_tier_gate(403, ""));
        assert!(!is_tier_gate(403, "{\"error\":\"nope\"}"));
    }

    #[test]
    fn parses_error_payloads() {
        let (error, code, plan) = parse_error_payload(
            "{\"error\":\"pick a plan\",\"limit\":{\"errorCode\":\"tier_selection_required\",\"plan\":{\"id\":\"pro\"}}}",
        );
        assert_eq!(error, "pick a plan");
        assert_eq!(code, "tier_selection_required");
        assert_eq!(plan.unwrap()["id"], json!("pro"));

        let (error, code, plan) = parse_error_payload("plain text");
        assert_eq!(error, "plain text");
        assert!(code.is_empty());
        assert!(plan.is_none());
    }

    #[test]
    fn tier_gate_body_shape() {
        let body = build_tier_gate_body(None, None);
        assert_eq!(body["ok"], json!(false));
        assert_eq!(body["errorCode"], json!("tier_selection_required"));
        assert_eq!(body["limit"]["url"], json!(PERCH_PLAN_SIGNUP_URL));
        assert_eq!(body["limit"]["plan"], Json::Null);
        assert!(body["error"].as_str().unwrap().contains("plan not selected"));
    }

    #[test]
    fn tool_arguments_normalize_to_a_json_object() {
        assert_eq!(normalize_tool_arguments(None), "{}");
        assert_eq!(normalize_tool_arguments(Some(&json!(""))), "{}");
        assert_eq!(normalize_tool_arguments(Some(&json!("  "))), "{}");
        assert_eq!(normalize_tool_arguments(Some(&json!("{\"a\":1}"))), "{\"a\":1}");
        assert_eq!(normalize_tool_arguments(Some(&json!({"a": 1}))), "{\"a\":1}");
        assert_eq!(normalize_tool_arguments(Some(&Json::Null)), "{}");
    }

    #[test]
    fn manual_model_request_shape() {
        let body = json!({
            "messages": [
                { "role": "system", "content": "sys" },
                { "role": "user", "content": "hi" }
            ],
            "max_tokens": 100,
            "temperature": 0.2,
            "tool_choice": "auto",
            "response_format": { "type": "json_object" },
            "tools": [{ "type": "function", "function": { "name": "f", "parameters": {} } }]
        });
        let payload = build_request_body("perchai/Claude-4", &body, Some("run-1"), true);

        assert_eq!(payload["runId"], json!("run-1"));
        assert_eq!(payload["lane"], json!("chat"));
        assert_eq!(payload["strictManual"], json!(false));
        assert_eq!(payload["preferredModelId"], json!("claude-4"));
        assert_eq!(payload["clientSurface"], json!("cli"));
        assert_eq!(payload["resendAfterOutput"], json!(true));
        assert_eq!(payload["avoidModelIds"], json!([]));
        assert_eq!(payload["attribution"], Json::Null);
        assert_eq!(payload["request"]["maxTokens"], json!(100));
        assert_eq!(payload["request"]["temperature"], json!(0.2));
        assert_eq!(payload["request"]["toolChoice"], json!("auto"));
        assert_eq!(payload["request"]["responseFormat"]["type"], json!("json_object"));
        assert_eq!(payload["request"]["tools"][0]["function"]["name"], json!("f"));
        // Manual calls are padded and carry no roost fields.
        assert!(payload.get("roostModelChoice").is_none());
        assert!(payload.get("effort").is_none());
        let system_text = content_text(&payload["request"]["messages"][0]["content"]);
        assert!(system_text.chars().count() >= PERCH_MIN_SYSTEM_PROMPT_CHARS);
    }

    #[test]
    fn tier_model_request_skips_padding_and_adds_roost() {
        let body = json!({
            "messages": [{ "role": "system", "content": "short" }],
            "max_completion_tokens": 64
        });
        let payload = build_request_body("standard", &body, None, false);
        assert_eq!(payload["preferredModelId"], Json::Null);
        assert_eq!(payload["roostModelChoice"], json!("standard"));
        assert_eq!(payload["roostReasoning"], json!(true));
        assert_eq!(payload["effort"]["level"], json!("high"));
        assert_eq!(payload["effort"]["orchestration"], json!(false));
        // Un-padded.
        assert_eq!(payload["request"]["messages"][0]["content"], json!("short"));
        // No ticket → no clientSurface, and the run id is generated.
        assert!(payload.get("clientSurface").is_none());
        assert!(payload["runId"].as_str().unwrap().starts_with("cli-turn-"));
        assert_eq!(payload["request"]["maxTokens"], json!(64));
    }

    #[test]
    fn base_headers_carry_auth_and_locale() {
        let headers = PerchAiExecutor::base_headers("tok", true);
        assert_eq!(headers.get("Authorization").map(String::as_str), Some("Bearer tok"));
        assert_eq!(headers.get("User-Agent").map(String::as_str), Some(PERCH_USER_AGENT));
        assert_eq!(headers.get("x-perch-locale").map(String::as_str), Some("en"));
        assert_eq!(headers.get("Accept").map(String::as_str), Some("text/event-stream"));
        assert!(PerchAiExecutor::base_headers("tok", false).get("Accept").is_none());
    }

    #[test]
    fn build_url_honours_provider_app_url() {
        use nr_exec_base::base::BaseHooks;
        let ex = PerchAiExecutor::new("perchai");
        let mut creds = Credentials::new();
        creds.set("providerSpecificData", json!({ "appUrl": "https://eu.perch.example/" }));
        assert_eq!(
            BaseHooks::build_url(&ex, "m", true, 0, &creds).unwrap(),
            "https://eu.perch.example/api/perch-terminal/model-call"
        );
        let default = BaseHooks::build_url(&ex, "m", true, 0, &Credentials::new()).unwrap();
        assert_eq!(default, "https://app.perchai.app/api/perch-terminal/model-call");
    }

    #[test]
    fn plan_state_round_trips_through_credentials() {
        let mut creds = Credentials::new();
        let state = PlanState { required: true, plan: Some(json!({"id": "pro"})), notice: "pick one".into() };
        apply_plan_state(&mut creds, &state);
        let restored = resolve_plan_state(&creds);
        assert!(restored.required);
        assert_eq!(restored.plan.unwrap()["id"], json!("pro"));
        assert_eq!(restored.notice, "pick one");
    }

    #[test]
    fn plan_cache_expires_and_clears() {
        clear_plan_cache();
        let state = PlanState { required: false, plan: Some(json!("pro")), notice: String::new() };
        remember_plan("token-1", &state);
        assert_eq!(get_cached_plan("token-1").unwrap().plan, Some(json!("pro")));
        assert!(get_cached_plan("other").is_none());
        clear_plan_cache();
        assert!(get_cached_plan("token-1").is_none());
    }

    #[test]
    fn stream_wrapper_maps_perch_events() {
        let frames = [
            json!({"type": "answer_delta", "text": "Hel"}),
            json!({"type": "reasoning_delta", "text": "think"}),
            json!({"type": "tool_call_delta", "toolCalls": [
                {"id": "a", "name": "f", "arguments": ""}
            ]}),
            json!({"type": "done", "ok": true, "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}}),
        ];
        let raw: String = frames
            .iter()
            .map(|f| format!("data: {f}\n\n"))
            .collect::<Vec<_>>()
            .join("");
        let headers = http::HeaderMap::new();
        let upstream = UpstreamResponse::from_bytes(200, headers, raw);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let text = rt.block_on(async { wrap_completion_stream(upstream, "standard").text().await.unwrap() });
        let chunks: Vec<Json> = text
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter(|d| *d != "[DONE]")
            .filter_map(|d| serde_json::from_str::<Json>(d).ok())
            .collect();

        assert_eq!(chunks.len(), 4, "content, reasoning, tool call, done: {text}");
        assert_eq!(chunks[0]["choices"][0]["delta"]["content"], json!("Hel"));
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], json!("assistant"));
        assert_eq!(chunks[1]["choices"][0]["delta"]["reasoning_content"], json!("think"));
        assert!(chunks[1]["choices"][0]["delta"].get("role").is_none());
        assert_eq!(chunks[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], json!("{}"));
        assert_eq!(chunks[3]["choices"][0]["finish_reason"], json!("tool_calls"));
        assert_eq!(chunks[3]["choices"][0]["delta"], json!({}));
        assert_eq!(chunks[3]["usage"]["total_tokens"], json!(7));
        assert!(text.ends_with("data: [DONE]\n\n"));
        assert_eq!(chunks[3]["id"], chunks[0]["id"], "one chunk id per stream");
    }

    #[test]
    fn stream_wrapper_reports_errors_as_text_then_done() {
        let raw = "data: {\"type\":\"error\",\"message\":\"boom\"}\n\n";
        let upstream = UpstreamResponse::from_bytes(200, http::HeaderMap::new(), raw);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let text = rt.block_on(async { wrap_completion_stream(upstream, "standard").text().await.unwrap() });
        assert!(text.contains("[Perch AI Error: boom]"));
        assert!(text.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn non_streaming_wrapper_emits_openai_completion() {
        let raw = json!({
            "ok": true,
            "text": "hello",
            "usage": { "prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3 },
            "toolCalls": [{ "id": "c1", "name": "f", "arguments": {} }]
        });
        let upstream = UpstreamResponse::from_bytes(200, http::HeaderMap::new(), raw.to_string());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let body = rt.block_on(async { wrap_non_streaming(upstream, "standard").await.json_body().await.unwrap() });
        assert_eq!(body["object"], json!("chat.completion"));
        assert_eq!(body["choices"][0]["message"]["content"], json!("hello"));
        assert_eq!(body["choices"][0]["finish_reason"], json!("tool_calls"));
        assert_eq!(body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"], json!("{}"));
        assert_eq!(body["usage"]["total_tokens"], json!(3));
    }

    #[test]
    fn non_streaming_wrapper_turns_ok_false_into_400() {
        let raw = json!({ "ok": false, "error": "nope", "errorCode": "bad" });
        let upstream = UpstreamResponse::from_bytes(200, http::HeaderMap::new(), raw.to_string());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let response = rt.block_on(wrap_non_streaming(upstream, "standard"));
        assert_eq!(response.status, 400);
        let body = rt.block_on(response.json_body()).unwrap();
        assert_eq!(body["error"]["message"], json!("nope"));
        assert_eq!(body["error"]["type"], json!("perchai_error"));
    }
}
