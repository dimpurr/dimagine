//! The write API (ADR-018) and the settings page (ADR-019 §5).
//!
//! Every route here is a mutation except the settings page, the trash
//! view and the CSRF-token bootstrap, which are reads. The whole module
//! is mounted only when `--write` is on (ADR-018 §1), so by default none
//! of these routes exist and an upgraded binary cannot quietly open a
//! write door. A write reaches a handler only past [`csrf_guard`], which
//! enforces ADR-018 §3–§6; the handler then performs the change through
//! `dimagine-write`'s [`Writer`], exactly as the CLI does, so the two
//! agree about what a write means.
//!
//! The handler bodies mirror `crates/dimagine/src/write.rs` command for
//! command; the equivalence is pinned by the integration tests, not by
//! sharing code, so the CLI's arg-parsing and JSON printing stay in the
//! CLI and this module stays thin.

use axum::{
    body::{Body, Bytes},
    extract::{
        multipart::{Field, Multipart, MultipartError},
        ConnectInfo, State,
    },
    http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Form, Json, Router,
};
use dimagine_core::body_edit::{self, BodyCtx, Embed, FolderSource};
use dimagine_core::fm_edit::{
    add_tags, delete_key, ensure_id, ensure_self_embed, front_matter_range, remove_tags,
    set_rating, set_scalar, FmRange, RatingEdit,
};
use dimagine_core::library::{FileClass, FileEntry, Library};
use dimagine_core::links::Resolver;
use dimagine_core::settings::{self, Change, Settings};
use dimagine_write::journal::{StepKind, StepRecord};
use dimagine_write::op_log::OpEntry;
use dimagine_write::paths::{RelPath, StepPath, TrashPath};
use dimagine_write::trash::{bumped_name, TrashRecord, TrashedFile};
use dimagine_write::{
    Fingerprint, Precondition, ScannedRecord, UploadItem, UploadRequest, Writer, MAX_UPLOAD_BATCH,
};
use std::net::SocketAddr;

use crate::accounts::{is_weak_password, verify_password, AccountsError, WEAK_PASSWORD_LENGTH};
use crate::ui::{escape_html, layout};
use crate::{now_seconds, AppState, AuthMode};

/// The `source` recorded for every operation this module performs.
const SOURCE: &str = "serve";
/// A JSON request body may not exceed 1 MiB (ADR-018 §5).
pub const MAX_JSON_BODY: usize = 1 << 20;
/// A single operation may name at most this many images; more is refused
/// before any file is opened (ADR-018 §5).
pub const MAX_BATCH: usize = 1000;
/// The header that carries the session's CSRF token (ADR-018 §3).
pub const CSRF_HEADER: &str = "x-csrf-token";
/// The one mutating route whose body is multipart rather than JSON
/// (ADR-018 §3): files land here. The CSRF guard's content-type rule
/// and the body-limit split below are both keyed on this path.
pub const UPLOAD_PATH: &str = "/api/write/upload";

/// Build the write sub-router, merged into the main router only when
/// `--write` is on. It carries the request-size cap and the CSRF guard,
/// and is layered *inside* the auth gate, so a request has already been
/// authenticated before it is checked here.
pub(crate) fn write_router(state: &AppState) -> Router<AppState> {
    // The JSON half. A JSON body larger than 1 MiB is refused as it
    // streams in (ADR-018 §5), not after it is buffered.
    let json_routes = Router::new()
        // The read side of the write surface: the token the owner's own
        // pages send back in `x-csrf-token`, and the trash view.
        .route("/api/write/csrf", get(csrf_bootstrap))
        .route("/api/write/trash", get(trash_view).post(trash_images))
        .route("/api/write/move", post(move_image))
        .route("/api/write/restore", post(restore_images))
        .route("/api/write/collections", post(edit_collection))
        .route("/api/write/collections/reorder", post(reorder_collection))
        .route("/api/write/collections/caption", post(caption_collection))
        .route("/api/write/note/body", post(edit_note_body))
        // The mutations. No mutating handler is ever registered for GET
        // (ADR-018 §3), and there is no method-override machinery to honour,
        // so a GET to one of these paths is a 405 or the read that shares
        // its path — the undo toast below, which mutates nothing.
        .route(
            "/api/write/image/front-matter",
            post(edit_front_matter).patch(edit_front_matter),
        )
        .route("/api/write/undo", get(undo_toast).post(undo_op))
        .route("/api/write/tags/plan", post(plan_tags))
        .route(
            "/api/write/tags/validate-proposal",
            post(validate_tag_proposal),
        )
        .route("/api/write/tags/apply", post(apply_tags))
        .route("/api/write/tags/start", post(start_tags))
        .route("/api/write/tags/progress", get(tag_progress))
        .route("/api/write/tags/form", post(tag_form))
        .route("/api/write/tags/undo-form", post(tag_undo_form))
        .route("/api/write/password", post(change_password))
        .route("/api/write/settings", post(save_settings))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_JSON_BODY));
    // The upload route carries files, not JSON: it opts out of the JSON
    // body cap, because the whole-body limit would cap a batch of files
    // one by one's total rather than each file. Its own cap — per file,
    // enforced while the bytes stream in (ADR-018 §5) — lives in
    // [`upload_files`], so the route streams nothing of its own accord.
    let upload_routes = Router::new()
        .route(UPLOAD_PATH, post(upload_files))
        .layer(axum::extract::DefaultBodyLimit::disable());
    Router::new().merge(json_routes).merge(upload_routes).layer(
        axum::middleware::from_fn_with_state(state.clone(), csrf_guard),
    )
}

// Validate every proposal entry with the same parser used by the CLI.
async fn validate_tag_proposal(body: Bytes) -> Response {
    match dimagine_core::tag_batch::parse_tag_list(&body) {
        Ok(_) => Json(serde_json::json!({"valid": true})).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": error})),
        )
            .into_response(),
    }
}

// Batch requests carry the operations again at apply time. The server rebuilds
// the plan and checks the preview digest; Writer checks it again under lock.
#[derive(serde::Deserialize)]
struct TagBatchRequest {
    operations: Vec<TagBatchOperation>,
    #[serde(default)]
    remember_aliases: bool,
    digest: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum TagBatchOperation {
    Rename {
        source: String,
        target: String,
    },
    Merge {
        sources: Vec<String>,
        target: String,
    },
    Move {
        source: String,
        target: String,
    },
    Remove {
        source: String,
    },
    RemoveRedundantParent,
}

fn build_tag_plan(
    root: &std::path::Path,
    request: TagBatchRequest,
) -> Result<(dimagine_core::tag_batch::TagBatchPlan, Option<String>), String> {
    use dimagine_core::tag_batch::{plan_tag_batch, remembered_aliases, TagOperation};
    if request.operations.is_empty() || request.operations.len() > MAX_BATCH {
        return Err("expected between 1 and 1000 operations".into());
    }
    let operations: Vec<_> = request
        .operations
        .into_iter()
        .map(|op| match op {
            TagBatchOperation::Rename { source, target } => TagOperation::Rename { source, target },
            TagBatchOperation::Merge { sources, target } => TagOperation::Merge { sources, target },
            TagBatchOperation::Move { source, target } => TagOperation::Move { source, target },
            TagBatchOperation::Remove { source } => TagOperation::Remove { source },
            TagBatchOperation::RemoveRedundantParent => TagOperation::RemoveRedundantParent,
        })
        .collect();
    // Match reviewed-list semantics: targets may not feed another selected
    // operation, and redundant-parent cleanup always runs last.
    for (i, op) in operations.iter().enumerate() {
        if matches!(op, TagOperation::RemoveRedundantParent) && i + 1 != operations.len() {
            return Err("redundant-parent operation must be last".into());
        }
        let target = match op {
            TagOperation::Rename { target, .. }
            | TagOperation::Move { target, .. }
            | TagOperation::Merge { target, .. } => Some(target),
            _ => None,
        };
        if let Some(target) = target {
            for (j, other) in operations.iter().enumerate() {
                if i == j {
                    continue;
                }
                let sources: Vec<&String> = match other {
                    TagOperation::Rename { source, .. }
                    | TagOperation::Move { source, .. }
                    | TagOperation::Remove { source } => vec![source],
                    TagOperation::Merge { sources, .. } => sources.iter().collect(),
                    TagOperation::RemoveRedundantParent => Vec::new(),
                };
                for source in sources {
                    if dimagine_core::tag_batch::edit_note_tags(
                        b"---\ntags: []\n---\n",
                        &[
                            TagOperation::Remove {
                                source: target.clone(),
                            },
                            TagOperation::Remove {
                                source: source.clone(),
                            },
                        ],
                    )
                    .is_err()
                    {
                        return Err("an operation target is another operation's source".into());
                    }
                }
            }
        }
    }
    let mut plan = plan_tag_batch(root, &operations, None)?;
    if request.remember_aliases && plan.ready() {
        if let Some(aliases) = remembered_aliases(&plan)? {
            plan.set_alias_after(&aliases)?;
        }
    }
    Ok((plan, request.digest))
}

async fn build_tag_plan_blocking(
    root: std::path::PathBuf,
    request: TagBatchRequest,
) -> Result<(dimagine_core::tag_batch::TagBatchPlan, Option<String>), String> {
    tokio::task::spawn_blocking(move || build_tag_plan(&root, request))
        .await
        .map_err(|error| format!("tag plan task failed: {error}"))?
}

fn affected_images(
    library: &Library,
    changed: &[&dimagine_core::tag_batch::PlannedNote],
) -> Vec<String> {
    let notes: std::collections::HashSet<_> =
        changed.iter().map(|note| note.path.as_str()).collect();
    let images: std::collections::HashSet<_> = library
        .files
        .iter()
        .filter(|entry| entry.class == FileClass::Image)
        .map(|entry| entry.path.as_path())
        .collect();
    let mut result: Vec<_> = library
        .files
        .iter()
        .filter(|entry| notes.contains(entry.rel.as_str()))
        .filter_map(|entry| {
            let paired = entry.paired_image_path()?;
            if images.contains(paired.as_path()) {
                entry.paired_image_rel().map(str::to_owned)
            } else {
                None
            }
        })
        .collect();
    result.sort();
    result.dedup();
    result
}

async fn plan_tags(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "writes are off");
    };
    let request: TagBatchRequest = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &format!("bad JSON: {error}")),
    };
    match build_tag_plan_blocking(writer.root_path().to_path_buf(), request).await {
        Ok((plan, _)) => {
            let changed: Vec<_> = plan.notes.iter().filter(|n| n.before != n.after).collect();
            let root = writer.root_path().to_path_buf();
            let library = match tokio::task::spawn_blocking(move || Library::open(&root)).await {
                Ok(Ok(library)) if library.fully_read() => library,
                _ => {
                    return json_error(
                        StatusCode::CONFLICT,
                        "library could not be fully read; plan again",
                    )
                }
            };
            let images = affected_images(&library, &changed);
            Json(serde_json::json!({
                "changed_images": images.len(),
                "sample_images": images.iter().take(12).collect::<Vec<_>>(),
                "digest": plan.digest, "ready": plan.ready(),
                "changed_notes": changed.len(),
                "deduplicated": changed.iter().map(|n| n.deduplicated).sum::<usize>(),
                "sample_notes": changed.iter().take(12).map(|n| &n.path).collect::<Vec<_>>(),
                "conflicts": plan.conflicts, "unreadable": plan.unreadable,
            }))
            .into_response()
        }
        Err(error) => json_error(StatusCode::BAD_REQUEST, &error),
    }
}

async fn apply_tags(State(state): State<AppState>, body: Bytes) -> Response {
    let permit = match crate::acquire_admission(&state) {
        Some(permit) => permit,
        None => return crate::admission_denied(1),
    };
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "writes are off");
    };
    let request: TagBatchRequest = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &format!("bad JSON: {error}")),
    };
    let (plan, digest) =
        match build_tag_plan_blocking(writer.root_path().to_path_buf(), request).await {
            Ok(plan) => plan,
            Err(error) => return json_error(StatusCode::BAD_REQUEST, &error),
        };
    if digest.as_deref() != Some(plan.digest.as_str()) {
        return json_error(StatusCode::CONFLICT, "preview is stale; plan again");
    }
    if !plan.ready() {
        return json_error(StatusCode::CONFLICT, "preview has unresolved issues");
    }
    let writer = writer.clone();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        writer.apply_tag_batch(&plan)
    })
    .await
    {
        Ok(Ok(op)) => Json(serde_json::json!({"op": op})).into_response(),
        Ok(Err(error)) => write_error(error),
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "tag apply task failed"),
    }
}

#[derive(Clone, Default, serde::Serialize)]
pub(crate) struct TagProgress {
    state: &'static str,
    completed: usize,
    total: usize,
    op: Option<String>,
    error: Option<String>,
}

fn start_tag_job(
    state: &AppState,
    plan: dimagine_core::tag_batch::TagBatchPlan,
) -> Result<(), &'static str> {
    let mut status = state.tag_progress.lock().unwrap();
    if status.state == "running" {
        return Err("A tag batch is already running");
    }
    *status = TagProgress {
        state: "running",
        ..TagProgress::default()
    };
    drop(status);
    let progress = state.tag_progress.clone();
    let writer = state.writer.as_ref().unwrap().clone();
    tokio::task::spawn_blocking(move || {
        let outcome = writer.apply_tag_batch_with_progress(&plan, &mut |completed, total| {
            let mut status = progress.lock().unwrap();
            status.completed = completed;
            status.total = total;
        });
        let mut status = progress.lock().unwrap();
        match outcome {
            Ok(op) => {
                status.state = "done";
                status.op = op;
            }
            Err(error) => {
                status.state = "error";
                status.error = Some(error.to_string());
            }
        }
    });
    Ok(())
}

pub(crate) fn tag_progress_html(status: &TagProgress, csrf: Option<&str>) -> String {
    if csrf.is_none() {
        return match status.state {
            "running" => "<p>Tag apply is running.</p>",
            "done" => "<p>Tag apply finished.</p>",
            "error" => "<p>Tag apply failed.</p>",
            _ => "",
        }
        .to_owned();
    }
    match status.state {
        "running" => format!(
            "<p>Apply running: writing {}/{}. Refresh to check progress.</p>",
            status.completed, status.total
        ),
        "done" => {
            let undo = match (&status.op, csrf) {
                (Some(op), Some(csrf)) => format!("<form method=\"post\" action=\"/api/write/tags/undo-form\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><input type=\"hidden\" name=\"op\" value=\"{}\"><button>Undo</button></form>", escape_html(csrf), escape_html(op)),
                _ => String::new(),
            };
            format!("<p>Tag apply finished.</p>{undo}")
        }
        "error" => format!(
            "<p>Tag apply failed: {}</p>",
            escape_html(status.error.as_deref().unwrap_or("Unknown error"))
        ),
        _ => String::new(),
    }
}

fn clear_tag_result(state: &AppState, op: &str) {
    let mut status = state.tag_progress.lock().unwrap();
    if status.op.as_deref() == Some(op) {
        *status = TagProgress::default();
    }
}

async fn tag_progress(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !owner_view_allowed(&state, &headers) {
        return json_error(StatusCode::FORBIDDEN, "owner access required");
    }
    Json(state.tag_progress.lock().unwrap().clone()).into_response()
}

async fn start_tags(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "writes are off");
    };
    let request: TagBatchRequest = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &format!("bad JSON: {error}")),
    };
    let (plan, digest) =
        match build_tag_plan_blocking(writer.root_path().to_path_buf(), request).await {
            Ok(plan) => plan,
            Err(error) => return json_error(StatusCode::BAD_REQUEST, &error),
        };
    if digest.as_deref() != Some(plan.digest.as_str()) || !plan.ready() {
        return json_error(
            StatusCode::CONFLICT,
            "preview is stale or has unresolved issues; plan again",
        );
    }
    match start_tag_job(&state, plan) {
        Ok(()) => Json(serde_json::json!({"state": "running"})).into_response(),
        Err(error) => json_error(StatusCode::CONFLICT, error),
    }
}

#[derive(serde::Deserialize)]
struct TagForm {
    csrf: String,
    action: String,
    source: String,
    target: String,
    #[serde(default)]
    remember_aliases: bool,
    #[serde(default)]
    digest: String,
    step: String,
}

async fn tag_form(State(state): State<AppState>, Form(form): Form<TagForm>) -> Response {
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "writes are off");
    };
    let operation = match form.action.as_str() {
        "rename" => TagBatchOperation::Rename {
            source: form.source.clone(),
            target: form.target.clone(),
        },
        "move" => TagBatchOperation::Move {
            source: form.source.clone(),
            target: form.target.clone(),
        },
        "merge" => TagBatchOperation::Merge {
            sources: vec![form.source.clone()],
            target: form.target.clone(),
        },
        "remove" => TagBatchOperation::Remove {
            source: form.source.clone(),
        },
        _ => return json_error(StatusCode::BAD_REQUEST, "unknown action"),
    };
    let request = TagBatchRequest {
        operations: vec![operation],
        remember_aliases: form.remember_aliases,
        digest: None,
    };
    let (plan, _) = match build_tag_plan_blocking(writer.root_path().to_path_buf(), request).await {
        Ok(plan) => plan,
        Err(error) => {
            return Html(format!(
                "<p>{}</p><a href=\"/tags\">Back to tags</a>",
                escape_html(&error)
            ))
            .into_response()
        }
    };
    if form.step == "apply" {
        if form.digest != plan.digest || !plan.ready() {
            return (
                StatusCode::CONFLICT,
                Html(
                    "Preview is stale or has unresolved issues. <a href=\"/tags\">Plan again</a>"
                        .to_string(),
                ),
            )
                .into_response();
        }
        return match start_tag_job(&state, plan) {
            Ok(()) => Html("<p>Apply running. Refresh the tags page to check progress and the result.</p><a href=\"/tags\">Check progress</a>".to_string()).into_response(),
            Err(error) => (StatusCode::CONFLICT, Html(escape_html(error))).into_response(),
        };
    }
    if form.step != "preview" {
        return json_error(StatusCode::BAD_REQUEST, "unknown step");
    }
    let changed = plan.notes.iter().filter(|n| n.before != n.after).count();
    let mut html = format!("<h1>Tag change preview</h1><p>{changed} notes affected.</p>");
    for issue in plan.conflicts.iter().chain(&plan.unreadable) {
        html.push_str(&format!(
            "<p>{}: {}</p>",
            escape_html(&issue.path),
            escape_html(&issue.message)
        ));
    }
    if plan.ready() {
        html.push_str(&format!("<form method=\"post\" action=\"/api/write/tags/form\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><input type=\"hidden\" name=\"action\" value=\"{}\"><input type=\"hidden\" name=\"source\" value=\"{}\"><input type=\"hidden\" name=\"target\" value=\"{}\"><input type=\"hidden\" name=\"remember_aliases\" value=\"{}\"><input type=\"hidden\" name=\"digest\" value=\"{}\"><button name=\"step\" value=\"apply\">Apply changes</button></form>", escape_html(&form.csrf), escape_html(&form.action), escape_html(&form.source), escape_html(&form.target), form.remember_aliases, escape_html(&plan.digest)));
    }
    html.push_str("<a href=\"/tags\">Back to tags</a>");
    Html(html).into_response()
}

#[derive(serde::Deserialize)]
struct TagUndoForm {
    op: String,
}
async fn tag_undo_form(State(state): State<AppState>, Form(form): Form<TagUndoForm>) -> Response {
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "writes are off");
    };
    match writer.undo(&form.op, None) {
        Ok(_) => {
            clear_tag_result(&state, &form.op);
            Html("<p>Changes undone.</p><a href=\"/tags\">Back to tags</a>").into_response()
        }
        Err(error) => write_error(error),
    }
}

// ---------------------------------------------------------------------------
// CSRF guard (ADR-018 §3, §6)
// ---------------------------------------------------------------------------

/// Refuse every mutating request that does not clear the whole CSRF bar,
/// and pass reads (the trash view, the token bootstrap) through to their
/// handlers. The auth gate has already run: in account mode an anonymous
/// write never reaches here.
async fn csrf_guard(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    // ADR-018 §3: method-override headers (`X-HTTP-Method-Override`,
    // `X-Method-Override`, `X-HTTP-Method`, `_method`) are ignored, never
    // honoured. axum routes by the real request method, so a GET is a read
    // and can never reach a write handler however these read; there is no
    // override machinery here to fool.
    let method = request.method().clone();
    // A safe method is a read: it carries no request body to police and no
    // side effect to forge. The mutating methods get the full bar.
    if matches!(method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(request).await;
    }

    // ADR-018 §3: a write body is JSON, with one exception — the upload
    // endpoint is `multipart/form-data` (the ADR names it explicitly). The
    // header, not the path, is what must say so, but the multipart form is
    // only ever accepted on the one route that reads it; any other route
    // still demands JSON.
    let is_upload = request.uri().path() == UPLOAD_PATH;
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let is_tag_form = matches!(
        request.uri().path(),
        "/api/write/tags/form" | "/api/write/tags/undo-form"
    );
    let expected = if is_upload {
        "multipart/form-data"
    } else if is_tag_form {
        "application/x-www-form-urlencoded"
    } else {
        "application/json"
    };
    if mime != expected {
        return json_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            &format!("a write body must be {expected}"),
        );
    }

    if is_tag_form {
        let (mut parts, body) = request.into_parts();
        let bytes = match axum::body::to_bytes(body, MAX_JSON_BODY).await {
            Ok(bytes) => bytes,
            Err(_) => return json_error(StatusCode::PAYLOAD_TOO_LARGE, "form is too large"),
        };
        let values: std::collections::HashMap<String, String> =
            match serde_urlencoded::from_bytes(&bytes) {
                Ok(values) => values,
                Err(_) => return json_error(StatusCode::BAD_REQUEST, "invalid form"),
            };
        if let Some(token) = values
            .get("csrf")
            .and_then(|v| HeaderValue::from_str(v).ok())
        {
            parts.headers.insert(CSRF_HEADER, token);
        }
        request = Request::from_parts(parts, Body::from(bytes));
    }

    // ADR-018 §3: the request must come from the configured public origin.
    // `Origin` must equal it exactly; without an `Origin`, the `Referer`'s
    // origin must. A missing both, a `null`, or a look-alike prefix or
    // suffix all fail. This is what a SameSite cookie cannot promise
    // against a hostile sibling subdomain, so it is checked here and not
    // merely relied upon.
    let Some(public_origin) = state.config.public_origin.as_deref() else {
        // `--write` is on without a public origin: the CLI refuses this at
        // startup, but the guard fails closed rather than open.
        return json_error(
            StatusCode::FORBIDDEN,
            "writes are enabled without a public origin",
        );
    };
    if !origin_matches(request.headers(), public_origin) {
        return json_error(
            StatusCode::FORBIDDEN,
            "the request origin is not this server",
        );
    }

    // ADR-018 §2: writes require an owner. In account mode that means an
    // account exists (a passcode-only server, or first-run setup, stays
    // read-only), the session belongs to one of those accounts, and the
    // session carries the server's CSRF token, presented in a header. The
    // account test is per session, not just per server: a session minted
    // while no account existed (or whose account was deleted out-of-band)
    // has no account and stays refused once one appears. In no-login mode
    // writes are allowed only with `--allow-anonymous-write`, and there is
    // no session to hold a token.
    match state.config.auth {
        AuthMode::Account => {
            match state.accounts.has_users() {
                Ok(true) => {}
                Ok(false) => {
                    return json_error(
                        StatusCode::FORBIDDEN,
                        "this server has no owner account yet, so it is read-only",
                    )
                }
                Err(error) => return store_error(error),
            }
            let Some(token) = session_token(&state, request.headers()) else {
                return json_error(StatusCode::UNAUTHORIZED, "a session is required to write");
            };
            // The session's own account, as `change_password` requires: a
            // passcode session has none, and an account deleted out-of-band
            // leaves its sessions behind, so both stay refused here.
            let Some(account) = crate::session_account(&state, &token) else {
                return json_error(
                    StatusCode::FORBIDDEN,
                    "this session is not signed in to an account",
                );
            };
            match state.accounts.find_user_by_email(&account) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return json_error(
                        StatusCode::FORBIDDEN,
                        "this session's account no longer exists",
                    )
                }
                Err(error) => return store_error(error),
            }
            let Some(expected) = crate::session_csrf(&state, &token) else {
                return json_error(StatusCode::UNAUTHORIZED, "this session cannot write");
            };
            let presented = request
                .headers()
                .get(CSRF_HEADER)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
                return json_error(StatusCode::FORBIDDEN, "the CSRF token is missing or wrong");
            }
        }
        AuthMode::None => {
            if !state.config.allow_anonymous_write {
                return json_error(
                    StatusCode::FORBIDDEN,
                    "writes are off on this no-login server",
                );
            }
            // No session means no per-session token to check; the origin
            // check above is the cross-site defence, and the CLI has already
            // confined this mode to a loopback bind (ADR-018 §2).
        }
    }

    next.run(request).await
}

/// Whether the request's origin is exactly `public_origin`, reading
/// `Origin` and falling back to `Referer` (ADR-018 §3). `null` and any
/// non-exact match are refused.
fn origin_matches(headers: &HeaderMap, public_origin: &str) -> bool {
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        // `Origin: null` (a sandboxed or `file://` context) is refused by
        // name, and any origin that is not byte-identical is refused.
        return origin == public_origin;
    }
    if let Some(referer) = headers.get(header::REFERER).and_then(|v| v.to_str().ok()) {
        return origin_of(referer).is_some_and(|origin| origin == public_origin);
    }
    false
}

/// The `scheme://host[:port]` prefix of a URL, for comparing a `Referer`
/// against the public origin. `None` when it has no recognisable origin.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split('/').next().filter(|host| !host.is_empty())?;
    Some(format!("{scheme}://{host}"))
}

// ---------------------------------------------------------------------------
// Reads that live on the write surface
// ---------------------------------------------------------------------------

/// `GET /api/write/csrf` — the session's CSRF token, for the owner's own
/// pages to send back in `x-csrf-token`. It is a read, so it carries no
/// token itself; an anonymous caller (no session) gets an empty token.
async fn csrf_bootstrap(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let token = session_token(&state, &headers)
        .and_then(|token| crate::session_csrf(&state, &token))
        .unwrap_or_default();
    Json(serde_json::json!({ "csrf": token })).into_response()
}

/// `GET /api/write/trash` — the trash groups and their records (ADR-017 §7),
/// read through the write core. A record that does not parse is shown as the
/// reader found it, never silently dropped.
async fn trash_view(State(state): State<AppState>) -> Response {
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "there is no trash view here");
    };
    match writer.trash_records() {
        Ok(records) => {
            let groups: Vec<serde_json::Value> = records
                .iter()
                .map(|scanned| match scanned.record.as_ref() {
                    Ok(record) => serde_json::json!({
                        "path": scanned.path.text(),
                        "op_id": record.op_id,
                        "trashed_at": record.trashed_at,
                        "files": record.files.iter().map(|file| serde_json::json!({
                            "original": file.original,
                            "trash": file.trash,
                        })).collect::<Vec<_>>(),
                    }),
                    Err(message) => serde_json::json!({
                        "path": scanned.path.text(),
                        "error": message,
                    }),
                })
                .collect();
            Json(serde_json::json!({ "groups": groups })).into_response()
        }
        Err(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("the trash could not be read: {error}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// Front-matter edit (mirrors the CLI's `tag` / `rate` / `set`)
// ---------------------------------------------------------------------------

/// `POST` or `PATCH /api/write/image/front-matter` — one image's note front
/// matter, edited through a single journaled operation. The body names the
/// image and any of: `add`/`remove` tags, `fields` to set (an empty value
/// deletes the key), and a `rating` (`0`–`5` or `"clear"`).
async fn edit_front_matter(
    State(state): State<AppState>,
    client: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "writes are off");
    };
    let request: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &format!("bad JSON: {error}")),
    };
    let Some(image) = request.get("image").and_then(|v| v.as_str()) else {
        return json_error(StatusCode::BAD_REQUEST, "an \"image\" path is required");
    };
    let aliases = match dimagine_core::tag_aliases::TagAliases::load(writer.root_path()) {
        Ok(aliases) => aliases,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &error),
    };
    let adds = match string_array(&request, "add")
        .iter()
        .map(|tag| aliases.resolve(tag))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(tags) => tags,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &error),
    };
    let removes = string_array(&request, "remove");
    let replacement_tags = match request.get("fields").and_then(|fields| fields.get("tags")) {
        None => None,
        Some(value) => {
            let Some(tags) = value.as_array() else {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "fields.tags must be a list of strings",
                );
            };
            let mut resolved = Vec::new();
            for tag in tags {
                let Some(tag) = tag.as_str() else {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "fields.tags must be a list of strings",
                    );
                };
                let tag = match aliases.resolve(tag) {
                    Ok(tag) => tag,
                    Err(error) => return json_error(StatusCode::BAD_REQUEST, &error),
                };
                resolved.push(tag);
            }
            Some(resolved)
        }
    };
    let fields: Vec<(String, String)> = match request.get("fields") {
        Some(serde_json::Value::Object(map)) => map
            .iter()
            .filter(|(key, _)| key.as_str() != "tags")
            .map(|(key, value)| (key.clone(), value.as_str().unwrap_or("").to_string()))
            .collect(),
        Some(_) => return json_error(StatusCode::BAD_REQUEST, "\"fields\" must be an object"),
        None => Vec::new(),
    };
    let rating = match request.get("rating") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => match rating_edit(value) {
            Some(edit) => Some(edit),
            None => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "\"rating\" must be 0-5 or \"clear\"",
                )
            }
        },
    };
    if adds.is_empty()
        && removes.is_empty()
        && fields.is_empty()
        && replacement_tags.is_none()
        && rating.is_none()
    {
        return json_error(
            StatusCode::BAD_REQUEST,
            "nothing to change: give tags, fields, or a rating",
        );
    }

    let client_addr = crate::resolve_client_ip(
        client.map(|info| info.0),
        &headers,
        &state.config.trusted_proxies,
    );

    let library = match Library::open(writer.root_path()) {
        Ok(library) => library,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let resolver = Resolver::new(&library.files);
    let ctx = BodyCtx::new(&library.files, &resolver);
    let entry = match resolve_image(&library.files, &resolver, image) {
        Ok(entry) => entry,
        Err(message) => return json_error(StatusCode::NOT_FOUND, &message),
    };
    let note_rel = format!("{}.md", entry.rel);
    let note_rel = match RelPath::parse(&note_rel) {
        Ok(rel) => rel,
        Err(error) => return write_error(error),
    };
    let note_present = library
        .files
        .iter()
        .any(|e| e.class == FileClass::ImageNote && e.rel == note_rel.text());

    // Read the current note, then apply the same transform the CLI applies.
    let current = writer.read(&note_rel).ok().map(|(bytes, _)| bytes);
    let base = current.unwrap_or_default();
    let mut edited = base.clone();
    if !adds.is_empty() {
        let refs: Vec<&str> = adds.iter().map(String::as_str).collect();
        edited = match add_tags(&edited, &refs) {
            Ok(bytes) => bytes,
            Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
        };
    }
    if !removes.is_empty() {
        let refs: Vec<&str> = removes.iter().map(String::as_str).collect();
        edited = match remove_tags(&edited, &refs) {
            Ok(bytes) => bytes,
            Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
        };
    }
    if let Some(tags) = &replacement_tags {
        edited = match delete_key(&edited, "tags") {
            Ok(bytes) => bytes,
            Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
        };
        let refs: Vec<&str> = tags.iter().map(String::as_str).collect();
        edited = match add_tags(&edited, &refs) {
            Ok(bytes) => bytes,
            Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
        };
    }
    for (key, value) in &fields {
        // An empty value deletes the key rather than writing `key: ""`
        // (ADR-017 §1) — the same rule the CLI's `set` uses.
        edited = if value.is_empty() {
            match delete_key(&edited, key) {
                Ok(bytes) => bytes,
                Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
            }
        } else {
            match set_scalar(&edited, key, value) {
                Ok(bytes) => bytes,
                Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
            }
        };
    }
    if let Some(edit) = rating {
        edited = match set_rating(&edited, edit) {
            Ok(bytes) => bytes,
            Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
        };
    }
    if edited == base {
        return Json(serde_json::json!({
            "image": entry.rel, "changed": false,
        }))
        .into_response();
    }
    let op_id = ulid::Ulid::new().to_string();
    let with_id = match ensure_id(&edited, &op_id) {
        Ok(bytes) => bytes,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let final_bytes = if note_present {
        with_id
    } else {
        // A new note gains a blank line and a self-embed (ADR-017 §2), exactly
        // as a fresh CLI note does.
        let mut seeded = with_id;
        seeded.push(b'\n');
        let embed = match embed_text(&ctx, &entry.rel) {
            Ok(embed) => embed,
            Err(message) => return json_error(StatusCode::BAD_REQUEST, &message),
        };
        match ensure_self_embed(&seeded, &embed) {
            Ok(bytes) => bytes,
            Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
        }
    };
    let step = StepRecord {
        step_idx: 0,
        kind: if note_present {
            StepKind::Modify
        } else {
            StepKind::Create
        },
        src: None,
        dst: note_rel.text(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: Some(final_bytes.clone()),
    };
    let intent = serde_json::json!({"action": "note_edit", "image": entry.rel});
    // The peer address reaches the op log (ADR-018 §6).
    if let Err(error) = writer.execute_op(&op_id, SOURCE, Some(&client_addr), intent, vec![step]) {
        return write_error(error);
    }
    Json(serde_json::json!({
        "image": entry.rel,
        "changed": true,
        "op": op_id,
        "front_matter": front_matter_text(&final_bytes),
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Undo (mirrors the CLI's `undo`)
// ---------------------------------------------------------------------------

/// `GET /api/write/undo` — the undo toast the UI shows after an operation
/// (ADR-016 §8: ten seconds, then the offer is gone): the newest entry of the
/// op log, summarised, with the request that would undo it. The summary is
/// counts and the intent — never the note bytes the log keeps for rollback.
async fn undo_toast(State(state): State<AppState>) -> Response {
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "there is no undo toast here");
    };
    let entries = match writer.op_log_entries() {
        Ok(entries) => entries,
        Err(error) => return write_error(error),
    };
    let Some(op) = last_op(&entries) else {
        // Nothing has been written through this library yet.
        return Json(serde_json::json!({ "op": serde_json::Value::Null })).into_response();
    };
    Json(serde_json::json!({
        "op": op.id,
        "time": op.time,
        "source": op.source,
        "intent": op.intent,
        "files": op.files,
        "undoable": op.undoable,
        "undo": {
            "method": "POST",
            "url": "/api/write/undo",
            "body": { "op": op.id },
        },
    }))
    .into_response()
}

/// The newest operation in the log. Every step of one operation records the
/// same `op_id`, `time`, `source` and `intent`, and the log is append-only, so
/// the id of the last line names the last operation.
fn last_op(entries: &[OpEntry]) -> Option<LastOp<'_>> {
    let op_id = entries.last()?.op_id.clone();
    let group: Vec<&OpEntry> = entries
        .iter()
        .filter(|entry| entry.op_id == op_id)
        .collect();
    let first = group.first()?;
    let mut files: Vec<&str> = Vec::new();
    for entry in &group {
        if !files.contains(&entry.path.as_str()) {
            files.push(&entry.path);
        }
    }
    // The core's own rule for what can never be undone (ADR-016 §8): a delete
    // that recorded no bytes — the permanent removals `trash purge` and
    // `trash empty` leave behind. Everything else the UI may offer, and
    // `POST /api/write/undo` has the last word if a file has since moved.
    let undoable = !group
        .iter()
        .any(|entry| entry.step_kind == StepKind::Delete && entry.before_bytes.is_none());
    Some(LastOp {
        id: op_id,
        time: &first.time,
        source: &first.source,
        intent: &first.intent,
        files: files.len(),
        undoable,
    })
}

/// The toast's view of one operation.
struct LastOp<'a> {
    id: String,
    time: &'a str,
    source: &'a str,
    intent: &'a serde_json::Value,
    files: usize,
    undoable: bool,
}

/// `POST /api/write/undo` — undo one operation by id. Undoing what cannot be
/// undone (a permanent delete) is refused by the write core, exactly as the
/// CLI reports it.
async fn undo_op(
    State(state): State<AppState>,
    client: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "writes are off");
    };
    let request: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &format!("bad JSON: {error}")),
    };
    let Some(op_id) = request.get("op").and_then(|v| v.as_str()) else {
        return json_error(StatusCode::BAD_REQUEST, "an \"op\" id is required");
    };
    let client_addr = crate::resolve_client_ip(
        client.map(|info| info.0),
        &headers,
        &state.config.trusted_proxies,
    );
    match writer.undo(op_id, Some(&client_addr)) {
        Ok(outcome) => {
            // Undoing a *create* hands the file back to the trash rather than
            // hard-deleting it (ADR-016 §8): the write core returns the files
            // as `TrashThis`, and it is the caller's job to move each into a
            // trash group. The CLI does this too; skipping it here would leave
            // an undone note sitting in the library.
            let mut failures: Vec<String> = Vec::new();
            for item in &outcome.undone {
                if let dimagine_write::UndoneItem::TrashThis(created) = item {
                    if let Err(message) = trash_one(writer, created, &client_addr) {
                        failures.push(format!("{}: {message}", created.text()));
                    }
                }
            }
            if !failures.is_empty() {
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!(
                        "undid {} but {} created file(s) could not be moved to the trash: {}",
                        outcome.op_id,
                        failures.len(),
                        failures.join("; ")
                    ),
                );
            }
            clear_tag_result(&state, op_id);
            Json(serde_json::json!({
                "op": outcome.op_id,
                "undone": outcome.undone.len(),
                "skipped": outcome.skipped.len(),
            }))
            .into_response()
        }
        Err(error) => write_error(error),
    }
}

/// Move one file into the trash as its own group — the undo of a create.
///
/// Ported from the CLI's `trash_one`: the trash slot is chosen before the
/// move so a taken round-1 name cannot leave two records over one file. The
/// peer that asked for the undo is recorded with it (ADR-018 §6), the same
/// address every other journaled write here carries.
fn trash_one(writer: &Writer, path: &RelPath, client_addr: &str) -> Result<(), String> {
    let (_, fp) = writer.read(path).map_err(|error| error.to_string())?;
    let op_id = ulid::Ulid::new().to_string();
    let trashed_at = chrono::Utc::now().to_rfc3339();
    let round = free_trash_round(writer, path.name(), path, std::slice::from_ref(path))?;
    let bumped = bumped_name(path.name(), round);
    let trash = TrashPath::for_original(path)
        .sibling(&bumped)
        .map_err(|e| e.to_string())?;
    let record_path = TrashRecord::path_for(path)
        .sibling(&format!("{bumped}.trash.json"))
        .map_err(|e| e.to_string())?;
    let record = TrashRecord {
        op_id: op_id.clone(),
        trashed_at,
        files: vec![TrashedFile {
            original: path.text(),
            trash: trash.text(),
            mtime_ns: fp.mtime_ns,
        }],
    };
    let record_bytes = record.to_bytes().map_err(|e| e.to_string())?;
    let steps = vec![
        StepRecord {
            step_idx: 0,
            kind: StepKind::Create,
            src: None,
            dst: record_path.text(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(record_bytes),
        },
        StepRecord {
            step_idx: 1,
            kind: StepKind::Move,
            src: Some(path.text()),
            dst: trash.text(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: None,
        },
    ];
    writer
        .execute_op(
            &op_id,
            SOURCE,
            Some(client_addr),
            serde_json::json!({"action": "trash", "undo_of_create": true}),
            steps,
        )
        .map_err(|error| error.to_string())
}

/// The next free trash round for a group's name (the CLI's `free_trash_round`).
fn free_trash_round(
    writer: &Writer,
    image_name: &str,
    image_rel: &RelPath,
    members: &[RelPath],
) -> Result<usize, String> {
    let mut round = 1usize;
    loop {
        let bumped = bumped_name(image_name, round);
        let all_free = members.iter().all(|member| {
            let suffix = member.name().strip_prefix(image_name).unwrap_or("");
            let target = TrashPath::for_original(member)
                .sibling(&format!("{bumped}{suffix}"))
                .expect("a bumped sibling of a validated path");
            writer.read_step(&StepPath::Trash(target)).is_err()
        });
        if all_free && record_slot_free(writer, image_rel, &bumped)? {
            return Ok(round);
        }
        round += 1;
    }
}

/// Whether `.trash/<the image's folder>/<bumped>.trash.json` is free.
fn record_slot_free(writer: &Writer, image_rel: &RelPath, bumped: &str) -> Result<bool, String> {
    let record = TrashRecord::path_for(image_rel)
        .sibling(&format!("{bumped}.trash.json"))
        .map_err(|e| e.to_string())?;
    Ok(writer.read_step(&StepPath::Trash(record)).is_err())
}

// ---------------------------------------------------------------------------
// Settings save (mirrors the CLI's `config set` / `config reset`)
// ---------------------------------------------------------------------------

/// `POST /api/write/settings` — apply one batch of settings changes through
/// the write core (ADR-019 §5). The body is `{"set": {key: value}, "reset":
/// [key]}`; keys the schema rejects come back as an error and nothing is
/// written. Unknown keys already in the file are preserved by `apply`.
async fn save_settings(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "writes are off");
    };
    let request: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &format!("bad JSON: {error}")),
    };
    let set: Vec<(String, serde_json::Value)> = match request.get("set") {
        Some(serde_json::Value::Object(map)) => {
            map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        }
        Some(_) => return json_error(StatusCode::BAD_REQUEST, "\"set\" must be an object"),
        None => Vec::new(),
    };
    let reset: Vec<String> = match request.get("reset") {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(_) => return json_error(StatusCode::BAD_REQUEST, "\"reset\" must be an array"),
        None => Vec::new(),
    };
    if set.is_empty() && reset.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "nothing to change");
    }
    let file_text = match read_settings_text(&state) {
        Ok(text) => text,
        Err(message) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &message),
    };
    let mut changes: Vec<(String, Change)> = Vec::new();
    for (key, value) in set {
        changes.push((key, Change::Set(value)));
    }
    for key in reset {
        changes.push((key, Change::Reset));
    }
    let changes: Vec<(&str, Change)> = changes
        .iter()
        .map(|(k, c)| (k.as_str(), c.clone()))
        .collect();
    let text = match settings::apply(file_text.as_deref(), &changes) {
        Ok(text) => text,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &apply_error_text(&error)),
    };
    match writer.write_settings(text.as_bytes()) {
        Ok(()) => Json(serde_json::json!({ "written": text })).into_response(),
        Err(error) => write_error(error),
    }
}

// ---------------------------------------------------------------------------
// The settings page (ADR-019 §5)
// ---------------------------------------------------------------------------

/// `GET /settings` — the schema-driven settings page. It is owner-only
/// ([`owner_only`]) and shows whether or not `--write` is on: read-only
/// text without it, an editable form with it. Editing goes through
/// `/api/write/settings`, which only exists with `--write`.
pub(crate) async fn settings_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(refusal) = owner_only(&state, &headers) {
        return refusal;
    }
    let settings = match read_settings_text(&state) {
        Ok(text) => Settings::resolve(text.as_deref()),
        Err(message) => {
            return crate::error_page(&state, StatusCode::INTERNAL_SERVER_ERROR, &message)
        }
    };
    let csrf = session_token(&state, &headers)
        .and_then(|token| crate::session_csrf(&state, &token))
        .unwrap_or_default();
    let editable = state.writer.is_some();
    // Whose password the page offers to change: the signed-in account's own.
    // A passcode session authenticates the server rather than a person, so
    // there is nobody to offer it to and the section is left out rather than
    // shown as a form that could only refuse.
    let account =
        session_token(&state, &headers).and_then(|token| crate::session_account(&state, &token));
    let body = settings_body(&settings, editable, &csrf, account.as_deref());
    page_no_store(Html(layout("Settings", &body)).into_response())
}

// ---------------------------------------------------------------------------
// Password change (the signed-in account's own credential)
// ---------------------------------------------------------------------------

/// `POST /api/write/password` — change the signed-in account's own password.
/// The body is `{"old": "…", "new": "…", "confirm": "…", "allow_weak": bool}`.
///
/// It is a write like any other: the route exists only with `--write` and it
/// passes the same bar (JSON body, exact origin, a live owner session, the
/// session's CSRF token in a header). The account is the session's own — a
/// session can never change anyone else's password, and a passcode session
/// has no account to change. On success every *other* session of that account
/// is signed out, so setting a new password because the old one leaked
/// actually ends the sessions it leaked to; the session that made the change
/// stays signed in.
async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // The route is only mounted with `--write` (ADR-018 §1); this is the same
    // belt-and-braces refusal every other handler here opens with. The write
    // core itself is not used — the credential lives in the state directory,
    // not in the library.
    if state.writer.is_none() {
        return json_error(StatusCode::NOT_FOUND, "writes are off");
    }
    if state.config.auth != AuthMode::Account {
        return json_error(
            StatusCode::FORBIDDEN,
            "this server has no account to change a password on",
        );
    }
    let Some(token) = session_token(&state, &headers) else {
        return json_error(StatusCode::UNAUTHORIZED, "a session is required");
    };
    let Some(account) = crate::session_account(&state, &token) else {
        return json_error(
            StatusCode::FORBIDDEN,
            "this session is not signed in to an account",
        );
    };
    let request: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &format!("bad JSON: {error}")),
    };
    let field = |key: &str| {
        request
            .get(key)
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let old = field("old");
    let new = field("new");
    // The form asks for the new password twice; a body that disagrees with
    // itself is refused here, so a mistyped confirmation cannot set a password
    // the owner did not mean.
    if new != field("confirm") {
        return json_error(
            StatusCode::BAD_REQUEST,
            "the two new passwords do not match",
        );
    }
    if new.is_empty() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "the new password must not be empty",
        );
    }
    // The bar the setup form applies: a password shorter than eight
    // characters is accepted only after the owner says they know it is weak.
    let allow_weak = request
        .get("allow_weak")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if is_weak_password(&new) && !allow_weak {
        return json_error(
            StatusCode::BAD_REQUEST,
            &format!(
                "the new password is fewer than {WEAK_PASSWORD_LENGTH} characters and is easy \
                 to guess; confirm to use it anyway"
            ),
        );
    }
    let user = match state.accounts.find_user_by_email(&account) {
        Ok(Some(user)) => user,
        Ok(None) => {
            return json_error(
                StatusCode::FORBIDDEN,
                "this session's account no longer exists",
            )
        }
        Err(error) => return store_error(error),
    };
    // The old password is the proof that the person at the keyboard is the
    // owner, not merely the holder of a session cookie.
    if !verify_password(&old, &user.password_hash) {
        return json_error(StatusCode::FORBIDDEN, "the current password is wrong");
    }
    if let Err(error) = state.accounts.set_password(&user.email, &new) {
        return store_error(error);
    }
    let signed_out = crate::invalidate_other_sessions(&state, &user.email, &token);
    Json(serde_json::json!({ "changed": true, "signed_out": signed_out })).into_response()
}

/// `GET /settings/json` — the "open JSON" view (ADR-019 §5): the settings
/// file exactly as it is on disk, read-only in 3a. Owner-only for the same
/// reason as the page itself.
pub(crate) async fn settings_json_page(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = owner_only(&state, &headers) {
        return refusal;
    }
    let text = match read_settings_text(&state) {
        Ok(text) => text,
        Err(message) => {
            return crate::error_page(&state, StatusCode::INTERNAL_SERVER_ERROR, &message)
        }
    };
    // The file is JSONC: comments and trailing commas are allowed on read
    // (ADR-019 §2), so this shows the bytes, not a reformatting of them.
    let body = match text {
        None => "<p><code>.dimagine/settings.json</code> does not exist yet: every \
                  setting is at its default.</p>"
            .to_string(),
        Some(text) => format!(
            "<pre class=\"settings-json\">{}</pre>\n\
             <p>Read-only here; the form on <a href=\"/settings\">Settings</a> edits it.</p>",
            escape_html(&text)
        ),
    };
    page_no_store(Html(layout("Settings JSON", &body)).into_response())
}

/// Whether the settings surface is showing the owner to whom ADR-019 §5
/// restricts it, or a `403` page saying it is not. In account mode the auth
/// gate has already required a live session, and a session only exists once an
/// owner account was created — so the owner test is the same one the CSRF
/// guard applies to a write (ADR-018 §2), and a passcode-only or first-run
/// server has no owner to show the page to. In no-login mode the only caller
/// with any power over the library is the one `--allow-anonymous-write`
/// admits.
fn owner_only(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    (!owner_view_allowed(state, headers)).then(|| {
        crate::error_page(
            state,
            StatusCode::FORBIDDEN,
            "the settings page is owner-only, and this server has no signed-in owner",
        )
    })
}

/// Whether this request may be shown write controls at all: it is the signed-in
/// owner (ADR-018 §2). Account mode needs an owner to exist and the session to
/// belong to one of its accounts; no-login mode needs `--allow-anonymous-write`
/// (which the CLI confines to a loopback bind). A page that renders for anyone
/// — an image, a collection — uses this to decide whether to draw its editor,
/// leaving the note itself readable to everyone.
fn owner_view_allowed(state: &AppState, headers: &HeaderMap) -> bool {
    match state.config.auth {
        AuthMode::Account => {
            let has_owner = state.accounts.has_users().unwrap_or(false);
            // The same per-session account test the CSRF guard applies to a
            // write (F1): a session minted before any owner existed, or whose
            // account was deleted out-of-band, is not an owner session.
            let owned = session_token(state, headers).is_some_and(|token| {
                crate::session_account(state, &token).is_some_and(|account| {
                    state
                        .accounts
                        .find_user_by_email(&account)
                        .ok()
                        .flatten()
                        .is_some()
                })
            });
            has_owner && owned
        }
        AuthMode::None => state.config.allow_anonymous_write,
    }
}

/// The CSRF token to embed in a page's write controls, or `None` when this
/// request should see no editor: writes must be on (`--write`, ADR-018 §1) and
/// the viewer the owner (ADR-018 §2). The token goes in a header the page's own
/// script sends back; it is never in a URL or a client-settable cookie. In
/// no-login mode there is no session and the token is empty, which the write
/// surface accepts (its cross-site defence is the origin check).
pub(crate) fn write_controls_for(state: &AppState, headers: &HeaderMap) -> Option<String> {
    if state.writer.is_none() || !owner_view_allowed(state, headers) {
        return None;
    }
    Some(
        session_token(state, headers)
            .and_then(|token| crate::session_csrf(state, &token))
            .unwrap_or_default(),
    )
}

/// The headers every settings view carries: this is live state, never cacheable.
fn page_no_store(response: Response) -> Response {
    let mut response = response;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The settings page body: a search box, one block per schema key (grouped
/// by its first dotted component), each showing the effective value, the
/// default, the range and a one-line description, and — when editable — an
/// input plus a restore-default control. `account` is the signed-in account,
/// whose own password the page offers to change; `None` (a passcode session)
/// leaves that section out.
fn settings_body(settings: &Settings, editable: bool, csrf: &str, account: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(message) = &settings.report.parse_error {
        out.push_str(&format!(
            "<p class=\"error\">The settings file could not be read ({}); \
             every setting shows its default.</p>",
            escape_html(message)
        ));
    }
    // The "open JSON" view (ADR-019 §5), the other half of the page.
    out.push_str("<p><a href=\"/settings/json\">Open JSON</a></p>\n");
    // A client-side filter over the rendered rows: the box's `input` event
    // hides any row whose key does not contain the text (ADR-019 §5).
    out.push_str(
        "<p><label>Search <input id=\"settings-search\" type=\"search\"></label></p>\n<div class=\"settings\">",
    );
    for spec in settings::schema() {
        let value = settings.get(spec.key).cloned().unwrap_or_default();
        let changed = value != spec.default;
        let mark = if changed { " (changed)" } else { "" };
        // A file value of the wrong type or out of range falls back to the
        // default and is flagged here in red (ADR-019 §3).
        let invalid = settings.report.invalid.iter().any(|key| key == spec.key);
        let invalid_note = if invalid {
            " <span class=\"error\">Invalid value in the file — the default applies.</span>"
        } else {
            ""
        };
        let min = spec.min.map(|m| m.to_string()).unwrap_or_default();
        let max = spec.max.map(|m| m.to_string()).unwrap_or_default();
        // `value` and `default` are rendered escaped: the only schema kind
        // today is `Int`, but a future string-typed setting must not become
        // an HTML-injection surface through the file value.
        let value_html = escape_html(&value.to_string());
        let default_html = escape_html(&spec.default.to_string());
        out.push_str(&format!(
            "<div class=\"setting\" data-key=\"{key}\">
<label for=\"set-{key}\"><strong>{key}</strong></label>
<p>{description}</p>
<p>Current <b>{value}</b>, default {default}, range {range_min}\u{2013}{range_max}.{mark}{invalid}</p>",
            key = escape_html(spec.key),
            description = escape_html(spec.description_en),
            value = value_html,
            default = default_html,
            range_min = escape_html(if min.is_empty() { "any" } else { &min }),
            range_max = escape_html(if max.is_empty() { "any" } else { &max }),
            mark = escape_html(mark),
            invalid = invalid_note,
        ));
        if editable {
            out.push_str(&format!(
                "<input id=\"set-{key}\" type=\"number\" data-key=\"{key}\" data-default=\"{default}\" \
                 min=\"{min}\" max=\"{max}\" value=\"{value}\"> \
                 <button type=\"button\" data-reset=\"{key}\">Restore default</button>",
                key = escape_html(spec.key),
                default = default_html,
                min = escape_html(&min),
                max = escape_html(&max),
                value = value_html,
            ));
        } else {
            out.push_str(
                "<p><em>Read-only: this server was started without <code>--write</code>.</em></p>",
            );
        }
        out.push_str("</div>\n");
    }
    out.push_str("</div>\n");
    if !settings.report.unknown.is_empty() {
        let keys: Vec<String> = settings
            .report
            .unknown
            .iter()
            .map(|key| format!("<code>{}</code>", escape_html(key)))
            .collect();
        out.push_str(&format!(
            "<p>Unrecognised keys kept verbatim: {}.</p>",
            keys.join(", ")
        ));
    }
    if editable {
        if let Some(account) = account {
            out.push_str(&password_form_html(account));
        }
        // The token is read from the page and sent back in `x-csrf-token` by
        // the same-origin script below; it is never in a URL or a cookie the
        // client sets (ADR-018 §3).
        out.push_str(&format!(
            "<script>window.DIMAGINE_CSRF={};</script>",
            serde_json::json!(csrf)
        ));
        out.push_str(settings_write_script());
    }
    out
}

/// The password section of the settings page: the signed-in account changes
/// its own password, giving the old one and the new one twice. It is a
/// script-driven form because the write surface takes a JSON body with the
/// CSRF token in a header (ADR-018 §3), which a plain form POST cannot carry —
/// and the form is a real `POST` to the same path so that without the script
/// the fields still travel in a request body rather than a URL.
fn password_form_html(account: &str) -> String {
    format!(
        "<h2>Password</h2>\n\
         <form id=\"pw-form\" method=\"post\" action=\"/api/write/password\">\n\
         <p>Signed in as <b>{account}</b>. Changing the password signs out every other session of this account; this one stays signed in.</p>\n\
         <label for=\"pw-old\">Current password</label>\n\
         <input id=\"pw-old\" name=\"old\" type=\"password\" autocomplete=\"current-password\">\n\
         <label for=\"pw-new\">New password</label>\n\
         <input id=\"pw-new\" name=\"new\" type=\"password\" autocomplete=\"new-password\">\n\
         <label for=\"pw-confirm\">New password again</label>\n\
         <input id=\"pw-confirm\" name=\"confirm\" type=\"password\" autocomplete=\"new-password\">\n\
         <label>Use this weak password anyway <input id=\"pw-weak\" name=\"allow_weak\" type=\"checkbox\"></label>\n\
         <p>Any password is accepted, but fewer than {WEAK_PASSWORD_LENGTH} characters is easy to guess: tick the box to use one anyway.</p>\n\
         <button type=\"submit\">Change password</button>\n\
         <p id=\"pw-status\" role=\"status\"></p>\n\
         </form>\n",
        account = escape_html(account),
    )
}

/// The small same-origin script behind the settings page: it wires the search
/// box and posts edits to `/api/write/settings` with the session's CSRF token.
/// A real editor bundle is later work; this is the 3a minimum.
fn settings_write_script() -> &'static str {
    r#"<script>
(function () {
  var CSRF = window.DIMAGINE_CSRF || '';
  var search = document.getElementById('settings-search');
  if (search) {
    search.addEventListener('input', function () {
      var q = search.value;
      document.querySelectorAll('[data-key]').forEach(function (el) {
        el.style.display = el.getAttribute('data-key').indexOf(q) === -1 ? 'none' : '';
      });
    });
  }
  function post(body) {
    return fetch('/api/write/settings', {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'x-csrf-token': CSRF },
      body: JSON.stringify(body),
      credentials: 'same-origin'
    });
  }
  document.querySelectorAll('input[data-key]').forEach(function (input) {
    input.addEventListener('change', function () {
      var set = {};
      set[input.getAttribute('data-key')] = Number(input.value);
      post({ set: set });
    });
  });
  document.querySelectorAll('button[data-reset]').forEach(function (btn) {
    btn.addEventListener('click', function () {
      post({ reset: [btn.getAttribute('data-reset')] });
    });
  });
  // The password section: it changes the signed-in account's own password.
  // The form is a real POST so the fields never reach a URL; with the script
  // they travel as JSON, which is what the write surface accepts.
  var pwForm = document.getElementById('pw-form');
  if (pwForm) {
    pwForm.addEventListener('submit', function (event) {
      event.preventDefault();
      var status = document.getElementById('pw-status');
      var oldField = document.getElementById('pw-old');
      var newField = document.getElementById('pw-new');
      var confirmField = document.getElementById('pw-confirm');
      var weakBox = document.getElementById('pw-weak');
      status.className = '';
      status.textContent = 'Changing\u2026';
      fetch('/api/write/password', {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'x-csrf-token': CSRF },
        body: JSON.stringify({
          old: oldField.value,
          new: newField.value,
          confirm: confirmField.value,
          allow_weak: !!(weakBox && weakBox.checked)
        }),
        credentials: 'same-origin'
      })
        .then(function (response) {
          return response.json().then(function (data) {
            return { ok: response.ok, data: data };
          });
        })
        .then(function (result) {
          if (!result.ok) {
            status.className = 'error';
            status.textContent = (result.data && result.data.error) ||
              'the password could not be changed';
            return;
          }
          // There is no success colour in the palette: the message is the
          // confirmation, and only a refusal is red.
          status.className = '';
          status.textContent = result.data.signed_out
            ? 'Password changed. ' + result.data.signed_out +
              ' other session(s) of this account signed out.'
            : 'Password changed.';
          oldField.value = '';
          newField.value = '';
          confirmField.value = '';
          if (weakBox) { weakBox.checked = false; }
        })
        .catch(function () {
          status.className = 'error';
          status.textContent = 'the password could not be changed';
        });
    });
  }
})();
</script>"#
}

// ---------------------------------------------------------------------------
// Trash and restore (mirror the CLI's `trash` / `restore`)
// ---------------------------------------------------------------------------

/// `POST /api/write/trash` — move each named image's group (image, note, raw
/// sidecars) into `.trash/` as one journaled operation (ADR-017 §7).
async fn trash_images(
    State(state): State<AppState>,
    client: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "writes are off");
    };
    let request: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &format!("bad JSON: {error}")),
    };
    let images = string_array(&request, "images");
    if images.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "an \"images\" array is required");
    }
    // A too-large batch is refused here, before any file is opened (ADR-018 §5).
    if images.len() > MAX_BATCH {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            &format!("at most {MAX_BATCH} images per operation"),
        );
    }
    let client_addr = crate::resolve_client_ip(
        client.map(|info| info.0),
        &headers,
        &state.config.trusted_proxies,
    );
    let library = match Library::open(writer.root_path()) {
        Ok(library) => library,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let resolver = Resolver::new(&library.files);
    let op_id = ulid::Ulid::new().to_string();
    let trashed_at = chrono::Utc::now().to_rfc3339();
    let (steps, trashed) = match plan_trash_steps(
        writer,
        &library.files,
        &resolver,
        &images,
        &op_id,
        &trashed_at,
    ) {
        Ok(planned) => planned,
        Err(message) => return json_error(StatusCode::NOT_FOUND, &message),
    };
    let intent = serde_json::json!({"action": "trash", "images": trashed.clone()});
    if let Err(error) = writer.execute_op(&op_id, SOURCE, Some(&client_addr), intent, steps) {
        return write_error(error);
    }
    Json(serde_json::json!({"trashed": trashed, "op": op_id})).into_response()
}

/// `POST /api/write/move` — rename an image and its group and repair every
/// link that resolved to it, in one journaled operation `undo` reverses
/// (ADR-017 §8). `{"from": <path or bare name>, "to": <new library path>}`;
/// `{"dry_run": true}` answers with the plan and changes nothing. A refusal
/// (target exists, ambiguous new bare name, taken link shape) is a 400 with
/// the typed message and no operation; a link that was already ambiguous
/// before the move lands with the move and is listed in `ambiguous`.
async fn move_image(
    State(state): State<AppState>,
    client: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "writes are off");
    };
    let request: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &format!("bad JSON: {error}")),
    };
    let Some(from) = request.get("from").and_then(|v| v.as_str()) else {
        return json_error(StatusCode::BAD_REQUEST, "a \"from\" string is required");
    };
    let Some(to) = request.get("to").and_then(|v| v.as_str()) else {
        return json_error(StatusCode::BAD_REQUEST, "a \"to\" string is required");
    };
    if from.is_empty() || to.is_empty() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "\"from\" and \"to\" must not be empty",
        );
    }
    let dry_run = request
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let client_addr = crate::resolve_client_ip(
        client.map(|info| info.0),
        &headers,
        &state.config.trusted_proxies,
    );
    let library = match Library::open(writer.root_path()) {
        Ok(library) => library,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let resolver = Resolver::new(&library.files);
    let entry = match resolve_image(&library.files, &resolver, from) {
        Ok(entry) => entry,
        Err(message) => return json_error(StatusCode::NOT_FOUND, &message),
    };
    let root = writer.root_path().to_path_buf();
    let disk_root = root.clone();
    let read = move |entry: &FileEntry| -> Result<Vec<u8>, String> {
        let path = root.join(&entry.path);
        let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if !meta.is_file() {
            return Err("not a regular file".to_string());
        }
        if meta.len() > dimagine_core::check::DEFAULT_MAX_NOTE_BYTES {
            return Err("larger than the note read limit".to_string());
        }
        std::fs::read(&path).map_err(|e| e.to_string())
    };
    // Same two probes as the CLI `mv` (write.rs): the destination counts as
    // free when its occupant is the very file the step moves.
    let same_file = dimagine_core::move_plan::same_file_probe(disk_root.clone());
    let occupied = move |rel: &str| disk_root.join(rel).symlink_metadata().is_ok();
    let op_id = ulid::Ulid::new().to_string();
    let plan = match dimagine_core::move_plan::plan_move(
        &library.files,
        &resolver,
        &entry.rel,
        to.trim_start_matches("./"),
        &op_id,
        &read,
        &occupied,
        &same_file,
    ) {
        Ok(plan) => plan,
        Err(refusal) => return json_error(StatusCode::BAD_REQUEST, &refusal.to_string()),
    };
    if dry_run {
        return Json(move_plan_json(&plan, None)).into_response();
    }
    let steps = move_steps(&plan);
    let intent = serde_json::json!({"action": "move", "from": plan.src, "to": plan.dst});
    if let Err(error) = writer.execute_op(&op_id, SOURCE, Some(&client_addr), intent, steps) {
        return write_error(error);
    }
    Json(move_plan_json(&plan, Some(op_id.as_str()))).into_response()
}

/// Land a move plan as journaled steps, in ADR-017 §8's order: the renames,
/// the created note, then the byte edits. The moved note's edit rides its
/// before bytes because the file reaches its path through this operation's
/// own rename (ADR-016 §7, §8).
fn move_steps(plan: &dimagine_core::move_plan::MovePlan) -> Vec<StepRecord> {
    let mut steps: Vec<StepRecord> = Vec::new();
    let mut push = |kind: StepKind,
                    src: Option<String>,
                    dst: String,
                    before: Option<Vec<u8>>,
                    bytes: Option<Vec<u8>>| {
        steps.push(StepRecord {
            step_idx: steps.len(),
            kind,
            src,
            dst,
            before_hash: None,
            after_hash: String::new(),
            before_bytes: before,
            after_bytes: bytes,
        })
    };
    for rename in &plan.renames {
        push(
            StepKind::Move,
            Some(rename.src.clone()),
            rename.dst.clone(),
            None,
            None,
        );
    }
    if let Some(created) = &plan.new_note {
        push(
            StepKind::Create,
            None,
            created.path.clone(),
            None,
            Some(created.bytes.clone()),
        );
    }
    for edit in plan.moved_note.iter().chain(plan.edits.iter()) {
        push(
            StepKind::Modify,
            None,
            edit.path.clone(),
            Some(edit.before.clone()),
            Some(edit.bytes.clone()),
        );
    }
    steps
}

/// The `dimagine.move/0.1` document: the plan, and the op id once it landed
/// (`null` on a dry run). Same shape the CLI prints, so one page reads both.
fn move_plan_json(
    plan: &dimagine_core::move_plan::MovePlan,
    op: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "schema": "dimagine.move/0.1",
        "dry_run": op.is_none(),
        "from": plan.src,
        "to": plan.dst,
        "renames": plan.renames.iter().map(|r| serde_json::json!({"from": r.src, "to": r.dst})).collect::<Vec<_>>(),
        "created_note": plan.new_note.as_ref().map(|n| n.path.clone()),
        "id_assigned": plan.id_assigned,
        "edited": plan
            .moved_note
            .iter()
            .chain(plan.edits.iter())
            .map(|e| serde_json::json!({
                "path": e.path,
                "self_embed_appended": e.self_embed_appended,
                "links": e.replaced.iter().map(|s| serde_json::json!({
                    "line": s.line,
                    "from": s.old_text,
                    "to": s.new_text,
                })).collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
        "ambiguous": plan
            .ambiguous
            .iter()
            .map(|a| serde_json::json!({
                "note": a.note,
                "line": a.line,
                "target": a.target,
                "matches": a.matches,
            }))
            .collect::<Vec<_>>(),
        "warnings": plan
            .warnings
            .iter()
            .map(|w| serde_json::json!({"path": w.path, "message": w.message}))
            .collect::<Vec<_>>(),
        "op": op,
    })
}

/// `POST /api/write/restore` — move each named group back from `.trash/` to its
/// recorded original paths, refusing when an original path is already taken.
async fn restore_images(
    State(state): State<AppState>,
    client: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "writes are off");
    };
    let request: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &format!("bad JSON: {error}")),
    };
    let images = string_array(&request, "images");
    if images.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "an \"images\" array is required");
    }
    if images.len() > MAX_BATCH {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            &format!("at most {MAX_BATCH} images per operation"),
        );
    }
    let client_addr = crate::resolve_client_ip(
        client.map(|info| info.0),
        &headers,
        &state.config.trusted_proxies,
    );
    let records = match writer.trash_records() {
        Ok(records) => records,
        Err(error) => return write_error(error),
    };
    let op_id = ulid::Ulid::new().to_string();
    let mut steps: Vec<StepRecord> = Vec::new();
    let mut restored: Vec<(String, Vec<TrashedFile>)> = Vec::new();
    for image in &images {
        let record = match find_record_for(&records, image) {
            Ok(record) => record,
            Err(message) => return json_error(StatusCode::NOT_FOUND, &message),
        };
        if restored.iter().any(|(rel, _)| *rel == record.label) {
            continue;
        }
        // Every original path must be free before the group moves back.
        for file in &record.files {
            match RelPath::parse(&file.original) {
                Ok(original) if writer.read(&original).is_ok() => {
                    return json_error(
                        StatusCode::CONFLICT,
                        &format!(
                            "cannot restore {}: {} already exists",
                            record.label, file.original
                        ),
                    )
                }
                Ok(_) => {}
                Err(error) => return write_error(error),
            }
        }
        for file in &record.files {
            steps.push(StepRecord {
                step_idx: steps.len(),
                kind: StepKind::Move,
                src: Some(file.trash.clone()),
                dst: file.original.clone(),
                before_hash: None,
                after_hash: String::new(),
                before_bytes: None,
                after_bytes: None,
            });
        }
        steps.push(StepRecord {
            step_idx: steps.len(),
            kind: StepKind::Delete,
            src: None,
            dst: record.path.text(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: Some(record.record_bytes.clone()),
            after_bytes: None,
        });
        restored.push((record.label.clone(), record.files.clone()));
    }
    if restored.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "nothing to restore");
    }
    let intent = serde_json::json!({
        "action": "restore",
        "images": restored.iter().map(|(rel, _)| rel.clone()).collect::<Vec<_>>(),
    });
    if let Err(error) = writer.execute_op(&op_id, SOURCE, Some(&client_addr), intent, steps) {
        return write_error(error);
    }
    // A rename preserves the time, but a restore puts the recorded times back
    // for the belt-and-braces half of the promise (ADR-017 §7).
    for (_, files) in &restored {
        for file in files {
            if let Ok(original) = RelPath::parse(&file.original) {
                let _ = writer.set_mtime(&StepPath::Library(original), file.mtime_ns);
            }
        }
    }
    Json(serde_json::json!({
        "restored": restored.iter().map(|(rel, _)| rel.clone()).collect::<Vec<_>>(),
        "op": op_id,
    }))
    .into_response()
}

/// The library-relative paths of one image's group: the image, its note, and
/// every raw sidecar hanging off the image name (ADR-017 §7). Ported from the
/// CLI's `group_members`.
fn group_members(files: &[FileEntry], image: &FileEntry) -> Vec<RelPath> {
    let mut members = Vec::new();
    if let Ok(rel) = RelPath::parse(&image.rel) {
        members.push(rel);
    }
    let prefix = format!("{}.", image.name);
    for entry in files {
        if entry.dir_path() != image.dir_path() {
            continue;
        }
        let member = match entry.class {
            FileClass::ImageNote => {
                entry.paired_image_path().as_deref() == Some(image.path.as_path())
            }
            FileClass::Raw => entry.name.starts_with(&prefix),
            _ => false,
        };
        if member {
            if let Ok(rel) = RelPath::parse(&entry.rel) {
                members.push(rel);
            }
        }
    }
    members
}

/// Plan one trash operation: each named image's group becomes a block of steps
/// numbered in a single sequence across the whole operation, so no two groups
/// collide on a journal file name (ADR-016 §7). Ported from the CLI.
fn plan_trash_steps(
    writer: &Writer,
    files: &[FileEntry],
    resolver: &Resolver,
    images: &[String],
    op_id: &str,
    trashed_at: &str,
) -> Result<(Vec<StepRecord>, Vec<String>), String> {
    let mut steps: Vec<StepRecord> = Vec::new();
    let mut trashed: Vec<String> = Vec::new();
    for image in images {
        let entry = resolve_image(files, resolver, image)?;
        // A repeated argument names one group once, as the CLI's planner
        // does: planning the same moves twice would fail the second block
        // and report the operation as partially applied.
        if trashed.contains(&entry.rel) {
            continue;
        }
        let members = group_members(files, entry);
        let image_rel = RelPath::parse(&entry.rel).map_err(|e| e.to_string())?;
        let round = free_trash_round(writer, &entry.name, &image_rel, &members)?;
        let group_steps = build_trash_steps(
            writer,
            op_id,
            trashed_at,
            entry,
            &members,
            round,
            steps.len(),
        )?;
        steps.extend(group_steps);
        trashed.push(entry.rel.clone());
    }
    Ok((steps, trashed))
}

/// The create-record + move-each-member steps of one group's part of a trash
/// operation (ADR-017 §7). Ported from the CLI's `build_trash_steps`.
fn build_trash_steps(
    writer: &Writer,
    op_id: &str,
    trashed_at: &str,
    image: &FileEntry,
    members: &[RelPath],
    round: usize,
    first_idx: usize,
) -> Result<Vec<StepRecord>, String> {
    let image_name = image.name.clone();
    let bumped = bumped_name(&image_name, round);
    let mut files = Vec::new();
    let mut moves = Vec::new();
    for member in members {
        let (_, fp) = match writer.read(member) {
            Ok(read) => read,
            Err(dimagine_write::Error::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        let suffix = member.name().strip_prefix(&image_name).unwrap_or("");
        let trash = TrashPath::for_original(member)
            .sibling(&format!("{bumped}{suffix}"))
            .map_err(|e| e.to_string())?;
        files.push(TrashedFile {
            original: member.text(),
            trash: trash.text(),
            mtime_ns: fp.mtime_ns,
        });
        moves.push((member.text(), trash.text()));
    }
    if files.is_empty() {
        return Err("nothing to trash".to_string());
    }
    let record = TrashRecord {
        op_id: op_id.to_string(),
        trashed_at: trashed_at.to_string(),
        files,
    };
    let record_bytes = record.to_bytes().map_err(|e| e.to_string())?;
    let image_rel = RelPath::parse(&image.rel).map_err(|e| e.to_string())?;
    let record_trash = TrashPath::for_original(&image_rel)
        .sibling(&format!("{bumped}.trash.json"))
        .map_err(|e| e.to_string())?;
    let mut steps = vec![StepRecord {
        step_idx: first_idx,
        kind: StepKind::Create,
        src: None,
        dst: record_trash.text(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: Some(record_bytes),
    }];
    for (index, (src, dst)) in moves.into_iter().enumerate() {
        steps.push(StepRecord {
            step_idx: first_idx + index + 1,
            kind: StepKind::Move,
            src: Some(src),
            dst,
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: None,
        });
    }
    Ok(steps)
}

/// A scanned trash record, materialised with its bytes, path and label for
/// restore. Ported from the CLI's `FoundRecord`.
struct FoundRecord {
    path: TrashPath,
    record_bytes: Vec<u8>,
    files: Vec<TrashedFile>,
    label: String,
}

fn found_record(scanned: &ScannedRecord, record: &TrashRecord) -> Result<FoundRecord, String> {
    let label = record
        .files
        .first()
        .map(|file| file.original.clone())
        .or_else(|| {
            record
                .files
                .iter()
                .map(|file| file.original.as_str())
                .min_by_key(|path| path.len())
                .map(str::to_owned)
        })
        .ok_or_else(|| format!("{} records no files", scanned.path.text()))?;
    Ok(FoundRecord {
        path: scanned.path.clone(),
        record_bytes: record.to_bytes().map_err(|e| e.to_string())?,
        files: record.files.clone(),
        label,
    })
}

/// Resolve a restore argument to its group's trash record: an exact original
/// path, else an unambiguous bare name; the newest of several same-named groups
/// wins. Ported from the CLI's `find_record_for`.
fn find_record_for(records: &[ScannedRecord], arg: &str) -> Result<FoundRecord, String> {
    let trimmed = arg.trim_start_matches("./");
    let mut hits: Vec<(&ScannedRecord, &TrashRecord)> = Vec::new();
    for scanned in records {
        let Ok(record) = scanned.record.as_ref() else {
            continue;
        };
        let exact = record.files.iter().any(|file| file.original == trimmed);
        let named = !trimmed.contains('/')
            && record
                .files
                .iter()
                .any(|file| file.original.rsplit('/').next() == Some(trimmed));
        if exact || named {
            hits.push((scanned, record));
        }
    }
    match hits.len() {
        0 => Err(if trimmed.contains('/') {
            format!("nothing trashed at library path {trimmed}")
        } else {
            format!("nothing trashed named {trimmed}")
        }),
        1 => found_record(hits[0].0, hits[0].1),
        _ => {
            let first: Vec<&str> = hits[0]
                .1
                .files
                .iter()
                .map(|f| f.original.as_str())
                .collect();
            let same_group = hits.iter().all(|(_, record)| {
                record
                    .files
                    .iter()
                    .map(|f| f.original.as_str())
                    .collect::<Vec<_>>()
                    == first
            });
            if same_group {
                let (scanned, record) = hits
                    .iter()
                    .max_by_key(|(_, record)| record.trashed_at.as_str())
                    .copied()
                    .unwrap_or(hits[0]);
                found_record(scanned, record)
            } else {
                Err(format!(
                    "bare name {trimmed} is ambiguous ({} trashed groups); name it by its library path",
                    hits.len()
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Collections (mirror the CLI's `collect`)
// ---------------------------------------------------------------------------

/// `POST /api/write/collections` — collect images into a collection. The body
/// names the images and exactly one of: `new` (a title, creating a collection
/// note), `add` (an existing collection note to add to), or `remove` (one to
/// remove from). Joining a collection gives each image's note an `id`, in the
/// same operation.
async fn edit_collection(
    State(state): State<AppState>,
    client: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "writes are off");
    };
    let request: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &format!("bad JSON: {error}")),
    };
    let images = string_array(&request, "images");
    if images.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "an \"images\" array is required");
    }
    if images.len() > MAX_BATCH {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            &format!("at most {MAX_BATCH} images per operation"),
        );
    }
    let new = request.get("new").and_then(|v| v.as_str());
    let add = request.get("add").and_then(|v| v.as_str());
    let remove = request.get("remove").and_then(|v| v.as_str());
    let given = [new, add, remove].iter().filter(|x| x.is_some()).count();
    if given != 1 {
        return json_error(
            StatusCode::BAD_REQUEST,
            "give exactly one of \"new\", \"add\" or \"remove\"",
        );
    }
    let client_addr = crate::resolve_client_ip(
        client.map(|info| info.0),
        &headers,
        &state.config.trusted_proxies,
    );
    let library = match Library::open(writer.root_path()) {
        Ok(library) => library,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let resolver = Resolver::new(&library.files);
    let ctx = BodyCtx::new(&library.files, &resolver);
    let mut targets: Vec<Target> = Vec::new();
    for image in &images {
        match target(&library.files, &resolver, image) {
            Ok(t) => targets.push(t),
            Err(message) => return json_error(StatusCode::NOT_FOUND, &message),
        }
    }
    if let Some(title) = new {
        return collect_new(writer, title, &targets, &ctx, &client_addr);
    }
    if let Some(collection) = add {
        return collect_edit(writer, collection, &targets, &ctx, false, &client_addr);
    }
    collect_edit(writer, remove.unwrap(), &targets, &ctx, true, &client_addr)
}

/// One image being collected, with whether it already has a note (the CLI's
/// `Target`).
struct Target<'a> {
    entry: &'a FileEntry,
    note_rel: String,
    note_present: bool,
}

/// Resolve one image argument into a `Target`.
fn target<'a>(
    files: &'a [FileEntry],
    resolver: &Resolver,
    arg: &str,
) -> Result<Target<'a>, String> {
    let entry = resolve_image(files, resolver, arg)?;
    let note_rel = format!("{}.md", entry.rel);
    let note_present = files
        .iter()
        .any(|e| e.class == FileClass::ImageNote && e.rel == note_rel);
    Ok(Target {
        entry,
        note_rel,
        note_present,
    })
}

/// Ensure every targeted image's own note carries an `id`, appending a create
/// or modify step for each note that needs one. Ported from the CLI.
fn push_image_id_steps(
    writer: &Writer,
    steps: &mut Vec<StepRecord>,
    targets: &[Target],
    ctx: &BodyCtx,
) -> Result<(), String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for target in targets {
        if !seen.insert(target.note_rel.clone()) {
            continue;
        }
        let note_rel = RelPath::parse(&target.note_rel).map_err(|e| e.to_string())?;
        let exists = target.note_present;
        let base = if exists {
            writer.read(&note_rel).map(|(b, _)| b).unwrap_or_default()
        } else {
            Vec::new()
        };
        let edited = ensure_id(&base, &ulid::Ulid::new().to_string()).map_err(|e| e.to_string())?;
        if exists && edited == base {
            continue;
        }
        let final_bytes = if exists {
            edited
        } else {
            let mut seeded = edited;
            seeded.push(b'\n');
            let embed = embed_text(ctx, &target.entry.rel)?;
            ensure_self_embed(&seeded, &embed).map_err(|e| e.to_string())?
        };
        steps.push(StepRecord {
            step_idx: steps.len(),
            kind: if exists {
                StepKind::Modify
            } else {
                StepKind::Create
            },
            src: None,
            dst: note_rel.text(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(final_bytes),
        });
    }
    Ok(())
}

/// The new-collection folder from `.obsidian/app.json`, read-only (ADR-017 §6).
fn collection_folder_choice(writer: &Writer, ctx: &BodyCtx) -> String {
    let app_json =
        std::fs::read_to_string(writer.root_path().join(".obsidian").join("app.json")).ok();
    let choice = body_edit::collection_folder(app_json.as_deref(), "", ctx);
    if choice.source == FolderSource::RootFallback {
        String::new()
    } else {
        choice.folder
    }
}

fn collect_new(
    writer: &Writer,
    title: &str,
    targets: &[Target],
    ctx: &BodyCtx,
    client_addr: &str,
) -> Response {
    let indices: Vec<usize> = match targets
        .iter()
        .map(|t| {
            ctx.index_of(&t.entry.rel)
                .ok_or_else(|| format!("{} not in the walk", t.entry.rel))
        })
        .collect::<Result<_, _>>()
    {
        Ok(indices) => indices,
        Err(message) => return json_error(StatusCode::NOT_FOUND, &message),
    };
    let body = match body_edit::new_collection_body(title, &indices, ctx) {
        Ok(body) => body,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let folder = collection_folder_choice(writer, ctx);
    let file_name = body_edit::safe_file_name(title);
    let stem = file_name.strip_suffix(".md").unwrap_or(&file_name);
    let parent = if folder.is_empty() {
        None
    } else {
        match RelPath::parse(&folder) {
            Ok(rel) => Some(rel),
            Err(error) => return write_error(error),
        }
    };
    let note_rel = match writer.next_free_name(parent.as_ref(), stem, "md") {
        Ok(rel) => rel,
        Err(error) => return write_error(error),
    };
    let mut steps = vec![StepRecord {
        step_idx: 0,
        kind: StepKind::Create,
        src: None,
        dst: note_rel.text(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: Some(body),
    }];
    if let Err(message) = push_image_id_steps(writer, &mut steps, targets, ctx) {
        return json_error(StatusCode::BAD_REQUEST, &message);
    }
    let op_id = ulid::Ulid::new().to_string();
    let intent =
        serde_json::json!({"action": "collect_new", "title": title, "note": note_rel.text()});
    if let Err(error) = writer.execute_op(&op_id, SOURCE, Some(client_addr), intent, steps) {
        return write_error(error);
    }
    Json(serde_json::json!({
        "collection": note_rel.text(),
        "created": true,
        "op": op_id,
        "images": targets.iter().map(|t| t.entry.rel.clone()).collect::<Vec<_>>(),
    }))
    .into_response()
}

fn collect_edit(
    writer: &Writer,
    collection: &str,
    targets: &[Target],
    ctx: &BodyCtx,
    remove: bool,
    client_addr: &str,
) -> Response {
    let note_rel = match RelPath::parse(collection.trim_start_matches("./")) {
        Ok(rel) => rel,
        Err(error) => return write_error(error),
    };
    let note_index = match ctx.index_of(&note_rel.text()) {
        Some(index) => index,
        None => return json_error(StatusCode::NOT_FOUND, &format!("no note at {collection}")),
    };
    if !matches!(
        ctx.files[note_index].class,
        FileClass::Note | FileClass::ImageNote
    ) {
        return json_error(
            StatusCode::BAD_REQUEST,
            &format!("{collection} is not a note"),
        );
    }
    let (mut new_body, _) = match writer.read(&note_rel) {
        Ok(got) => got,
        Err(error) => return write_error(error),
    };
    let mut steps: Vec<StepRecord> = Vec::new();
    if !remove {
        if let Err(message) = push_image_id_steps(writer, &mut steps, targets, ctx) {
            return json_error(StatusCode::BAD_REQUEST, &message);
        }
    }
    let mut touched = false;
    for target in targets {
        let index = match ctx.index_of(&target.entry.rel) {
            Some(index) => index,
            None => {
                return json_error(
                    StatusCode::NOT_FOUND,
                    &format!("{} is not in the walk", target.entry.rel),
                )
            }
        };
        let result = if remove {
            body_edit::remove_member(&new_body, note_index, index, ctx).map(|r| r.bytes)
        } else {
            body_edit::append_member(&new_body, note_index, index, ctx).map(|a| a.bytes)
        };
        match result {
            Ok(bytes) => {
                if bytes != new_body {
                    touched = true;
                }
                new_body = bytes;
            }
            Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
        }
    }
    if !touched && steps.is_empty() {
        return Json(serde_json::json!({"collection": note_rel.text(), "changed": false}))
            .into_response();
    }
    if touched {
        steps.push(StepRecord {
            step_idx: steps.len(),
            kind: StepKind::Modify,
            src: None,
            dst: note_rel.text(),
            before_hash: None,
            after_hash: String::new(),
            before_bytes: None,
            after_bytes: Some(new_body),
        });
    }
    let op_id = ulid::Ulid::new().to_string();
    let action = if remove {
        "collect_remove"
    } else {
        "collect_add"
    };
    let intent = serde_json::json!({"action": action, "collection": note_rel.text()});
    if let Err(error) = writer.execute_op(&op_id, SOURCE, Some(client_addr), intent, steps) {
        return write_error(error);
    }
    Json(serde_json::json!({"collection": note_rel.text(), "changed": true, "op": op_id}))
        .into_response()
}

/// `POST /api/write/collections/reorder` — permute the member blocks of a
/// collection note (ADR-017 §9). The body is `{"collection": <path>,
/// "hash": <the note's sha256 at open time>,
/// "order": [1-based block ordinals]}`; every block must appear exactly once,
/// and only the blocks move — every other byte of the note stays where it is.
///
/// `hash` is the open-time precondition (ADR-016 §3): a request without one is
/// refused, never treated as an overwrite, and a note that changed since that
/// read is a 409 carrying the note's current ETag — never a retry.
async fn reorder_collection(
    State(state): State<AppState>,
    client: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "writes are off");
    };
    let request: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &format!("bad JSON: {error}")),
    };
    let Some(collection) = request.get("collection").and_then(|v| v.as_str()) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "a \"collection\" string is required",
        );
    };
    let Some(order_json) = request.get("order").and_then(|v| v.as_array()) else {
        return json_error(StatusCode::BAD_REQUEST, "an \"order\" array is required");
    };
    // The note's hash at open time is the precondition (ADR-016 §3); a request
    // without one is refused, never treated as an overwrite.
    let Some(expect) = request
        .get("hash")
        .and_then(|v| v.as_str())
        .and_then(Fingerprint::from_hex)
    else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "a \"hash\" (the note's 64-character sha256 at open time) is required",
        );
    };
    if order_json.len() > MAX_BATCH {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            &format!("at most {MAX_BATCH} block ordinals per operation"),
        );
    }
    let mut order: Vec<usize> = Vec::with_capacity(order_json.len());
    for value in order_json {
        match value.as_u64() {
            Some(n) if n >= 1 => order.push((n - 1) as usize),
            _ => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "every order ordinal must be a whole number of 1 or more",
                )
            }
        }
    }
    let client_addr = crate::resolve_client_ip(
        client.map(|info| info.0),
        &headers,
        &state.config.trusted_proxies,
    );
    let library = match Library::open(writer.root_path()) {
        Ok(library) => library,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let resolver = Resolver::new(&library.files);
    let ctx = BodyCtx::new(&library.files, &resolver);
    let (note_rel, note_index, bytes, current) = match read_note(writer, collection, &ctx) {
        Ok(got) => got,
        Err(response) => return *response,
    };
    // The precondition holds even when this permutation would change nothing:
    // a stale open hash is a conflict, not a silent no-op (ADR-016 §3).
    if !expect.same_bytes(&current) {
        return conflict_response(&current, None);
    }
    let blocks = match body_edit::member_blocks(&bytes, note_index, &ctx) {
        Ok(blocks) => blocks,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let new_body = match body_edit::reorder_members(&bytes, note_index, &ctx, &order) {
        Ok(bytes) => bytes,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    if new_body == bytes {
        return Json(serde_json::json!({"collection": note_rel.text(), "changed": false}))
            .into_response();
    }
    // `order` is a validated permutation by now, so every slot is in range.
    let order_images: Vec<&str> = order
        .iter()
        .map(|&slot| blocks[slot].image.as_str())
        .collect();
    let op_id = ulid::Ulid::new().to_string();
    let intent = serde_json::json!({"action": "collect_reorder", "collection": note_rel.text()});
    let step = StepRecord {
        step_idx: 0,
        kind: StepKind::Modify,
        src: None,
        dst: note_rel.text(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: Some(new_body),
    };
    if let Err(error) = writer.execute_op_guarded(
        &op_id,
        SOURCE,
        Some(&client_addr),
        intent,
        vec![step],
        &[Precondition::from_digest(
            StepPath::Library(note_rel.clone()),
            expect,
        )],
    ) {
        return guarded_write_error(error);
    }
    Json(serde_json::json!({
        "collection": note_rel.text(),
        "changed": true,
        "op": op_id,
        "order": order_images,
    }))
    .into_response()
}

/// `POST /api/write/collections/caption` — add, edit or delete one caption
/// line of a member block (ADR-017 §9). The body is `{"collection": <path>,
/// "hash": <the note's sha256 at open time>, "block": <1-based ordinal>,
/// "line": <offset>, "text": <string|null>}`; `line` 0 is the member line
/// (refused — remove the member instead), 1.. the caption lines, and one past
/// the last caption line appends. `text: null` deletes the line.
///
/// `hash` is the open-time precondition (ADR-016 §3): a request without one is
/// refused, and a note that changed since that read is a 409 carrying the
/// current ETag.
async fn caption_collection(
    State(state): State<AppState>,
    client: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "writes are off");
    };
    let request: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &format!("bad JSON: {error}")),
    };
    let Some(collection) = request.get("collection").and_then(|v| v.as_str()) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "a \"collection\" string is required",
        );
    };
    let Some(block) = request.get("block").and_then(|v| v.as_u64()) else {
        return json_error(StatusCode::BAD_REQUEST, "a \"block\" ordinal is required");
    };
    if block < 1 {
        return json_error(
            StatusCode::BAD_REQUEST,
            "the \"block\" ordinal is 1 or more",
        );
    }
    let Some(line) = request.get("line").and_then(|v| v.as_u64()) else {
        return json_error(StatusCode::BAD_REQUEST, "a \"line\" offset is required");
    };
    let text = match request.get("text") {
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(text)) => Some(text.as_str()),
        Some(_) => return json_error(StatusCode::BAD_REQUEST, "\"text\" must be a string or null"),
        None => return json_error(StatusCode::BAD_REQUEST, "a \"text\" field is required"),
    };
    // The note's hash at open time is the precondition (ADR-016 §3); a request
    // without one is refused, never treated as an overwrite.
    let Some(expect) = request
        .get("hash")
        .and_then(|v| v.as_str())
        .and_then(Fingerprint::from_hex)
    else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "a \"hash\" (the note's 64-character sha256 at open time) is required",
        );
    };
    let client_addr = crate::resolve_client_ip(
        client.map(|info| info.0),
        &headers,
        &state.config.trusted_proxies,
    );
    let library = match Library::open(writer.root_path()) {
        Ok(library) => library,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let resolver = Resolver::new(&library.files);
    let ctx = BodyCtx::new(&library.files, &resolver);
    let (note_rel, note_index, bytes, current) = match read_note(writer, collection, &ctx) {
        Ok(got) => got,
        Err(response) => return *response,
    };
    // The precondition holds even when this caption would change nothing: a
    // stale open hash is a conflict, not a silent no-op (ADR-016 §3).
    if !expect.same_bytes(&current) {
        return conflict_response(&current, None);
    }
    let (new_body, change) = match body_edit::edit_caption(
        &bytes,
        note_index,
        &ctx,
        (block - 1) as usize,
        line as usize,
        text,
    ) {
        Ok(result) => result,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    if new_body == bytes {
        return Json(serde_json::json!({"collection": note_rel.text(), "changed": false}))
            .into_response();
    }
    let verb = match change {
        body_edit::CaptionChange::Edited => "edited",
        body_edit::CaptionChange::Added => "added",
        body_edit::CaptionChange::Deleted => "deleted",
    };
    let op_id = ulid::Ulid::new().to_string();
    let intent = serde_json::json!({"action": "collect_caption", "collection": note_rel.text()});
    let step = StepRecord {
        step_idx: 0,
        kind: StepKind::Modify,
        src: None,
        dst: note_rel.text(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: Some(new_body),
    };
    if let Err(error) = writer.execute_op_guarded(
        &op_id,
        SOURCE,
        Some(&client_addr),
        intent,
        vec![step],
        &[Precondition::from_digest(
            StepPath::Library(note_rel.clone()),
            expect,
        )],
    ) {
        return guarded_write_error(error);
    }
    Json(serde_json::json!({
        "collection": note_rel.text(),
        "changed": true,
        "op": op_id,
        "caption": verb,
    }))
    .into_response()
}

/// Resolve an existing note and read its current bytes, for the whole-replace
/// editors (reorder, caption, body). Mirrors the CLI's `read_note`, but the
/// precondition is the caller's open hash (ADR-016 §3), not this read: the
/// read supplies the body the new bytes are computed from, and the fingerprint
/// that goes with it. A refusal is the ready-made JSON response.
fn read_note(
    writer: &Writer,
    collection: &str,
    ctx: &BodyCtx,
) -> Result<(RelPath, usize, Vec<u8>, Fingerprint), Box<Response>> {
    let note_rel = RelPath::parse(collection.trim_start_matches("./"))
        .map_err(|e| Box::new(write_error(e)))?;
    let note_index = ctx.index_of(&note_rel.text()).ok_or_else(|| {
        Box::new(json_error(
            StatusCode::NOT_FOUND,
            &format!("no note at {collection}"),
        ))
    })?;
    if !matches!(
        ctx.files[note_index].class,
        FileClass::Note | FileClass::ImageNote
    ) {
        return Err(Box::new(json_error(
            StatusCode::BAD_REQUEST,
            &format!("{collection} is not a note"),
        )));
    }
    let (body, fp) = writer
        .read(&note_rel)
        .map_err(|e| Box::new(write_error(e)))?;
    Ok((note_rel, note_index, body, fp))
}

/// `POST /api/write/note/body` — replace a note's whole body (everything after
/// its front matter's closing fence) with the text given, as one journaled
/// operation guarded by an open hash (ADR-017 §10, ADR-016 §3). This is the
/// source-mode body edit the image/note page's textarea posts; front matter is
/// never re-serialized here (it has its own editor), and only the body region
/// changes byte for byte.
///
/// The body is `{"note": <path>, "hash": <the note's sha256 at open time>,
/// "body": <the new body text>}`. `hash` is the open-time precondition (ADR-016
/// §3): a request without one is refused, never treated as an overwrite, and a
/// note that changed since that read is a 409 carrying the note's current hash
/// and ETag — never a retry. "Overwrite anyway" is simply a second request
/// carrying the hash the 409 returned; if the file became a third version in
/// between, that retry conflicts too.
async fn edit_note_body(
    State(state): State<AppState>,
    client: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "writes are off");
    };
    let request: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &format!("bad JSON: {error}")),
    };
    let Some(note) = request.get("note").and_then(|v| v.as_str()) else {
        return json_error(StatusCode::BAD_REQUEST, "a \"note\" path is required");
    };
    // The new body is taken verbatim; a body that is absent is a bad request,
    // but an empty string is a legitimate one (it empties the note).
    let Some(new_body) = request.get("body").and_then(|v| v.as_str()) else {
        return json_error(StatusCode::BAD_REQUEST, "a \"body\" string is required");
    };
    // The note's hash at open time is the precondition (ADR-016 §3); a request
    // without one is refused, never treated as an overwrite.
    let Some(expect) = request
        .get("hash")
        .and_then(|v| v.as_str())
        .and_then(Fingerprint::from_hex)
    else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "a \"hash\" (the note's 64-character sha256 at open time) is required",
        );
    };
    let client_addr = crate::resolve_client_ip(
        client.map(|info| info.0),
        &headers,
        &state.config.trusted_proxies,
    );
    let library = match Library::open(writer.root_path()) {
        Ok(library) => library,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let resolver = Resolver::new(&library.files);
    let ctx = BodyCtx::new(&library.files, &resolver);
    let (note_rel, note_index, bytes, current) = match read_note(writer, note, &ctx) {
        Ok(got) => got,
        Err(response) => return *response,
    };
    // The precondition holds even when the new body equals the old: a stale
    // open hash is a conflict, not a silent no-op (ADR-016 §3). The 409 carries
    // the note's current body, so the prompt can show what changed elsewhere.
    if !expect.same_bytes(&current) {
        return conflict_response(&current, body_edit::body_text(&bytes));
    }
    let replaced = match body_edit::replace_body(&bytes, note_index, new_body.as_bytes(), &ctx) {
        Ok(replaced) => replaced,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    if !replaced.changed {
        return Json(serde_json::json!({
            "note": note_rel.text(),
            "changed": false,
            "hash": current.hex(),
        }))
        .into_response();
    }
    let op_id = ulid::Ulid::new().to_string();
    let intent = serde_json::json!({"action": "edit_body", "note": note_rel.text()});
    // The digest of the bytes this operation writes: a compare-and-swap on the
    // same bytes lands with exactly this hash, so it is reported without
    // reading the file back (ADR-016 §3).
    let after = Fingerprint::of_bytes(&replaced.bytes);
    let step = StepRecord {
        step_idx: 0,
        kind: StepKind::Modify,
        src: None,
        dst: note_rel.text(),
        before_hash: None,
        after_hash: String::new(),
        before_bytes: None,
        after_bytes: Some(replaced.bytes),
    };
    if let Err(error) = writer.execute_op_guarded(
        &op_id,
        SOURCE,
        Some(&client_addr),
        intent,
        vec![step],
        &[Precondition::from_digest(
            StepPath::Library(note_rel.clone()),
            expect,
        )],
    ) {
        return guarded_write_error(error);
    }
    Json(serde_json::json!({
        "note": note_rel.text(),
        "changed": true,
        "op": op_id,
        "hash": after.hex(),
        "bytes": after.size,
        "self_embed_restored": replaced.self_embed_restored,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Upload (mirrors the CLI's `upload`, ADR-017 §11)
// ---------------------------------------------------------------------------

/// The `dimagine <version>` spelling an upload's notes record in
/// `sources` (ADR-017 §11.5), as the viewer's uploads stamp it.
const SERVE_IMPORTER: &str = concat!("dimagine ", env!("CARGO_PKG_VERSION"));

/// The largest a `folder` or `collection` control field may be, in bytes
/// (H3): the field carries a library-relative path, never content, so a
/// generous path length bounds it and the placement fields cannot be used to
/// hold RAM. Enforced while the field streams in, like the per-file cap.
const MAX_UPLOAD_FIELD_BYTES: usize = 4 * 1024;

/// A whole upload request may buffer at most this many copies of the per-file
/// cap across its file fields, so a batch of many large files cannot
/// accumulate unbounded RAM before [`Writer::upload`] runs (H3, ADR-018 §5
/// bounds one field; this bounds the request). Enforced while streaming, like
/// the per-file cap.
const MAX_UPLOAD_TOTAL_CAPS: usize = 4;

/// `POST /api/write/upload` — land files as image groups (ADR-017 §11),
/// the same journaled operation the CLI's `upload` runs. The body is
/// `multipart/form-data` (ADR-018 §3, §5): every file field is an image to
/// upload; the optional `folder` and `collection` text fields name the
/// placement, exactly as the CLI's `--to` and `--to-collection`. A file
/// larger than the configured per-file cap is refused *as it streams in*
/// — the connection is dropped the moment a field crosses the cap, before
/// the whole part is buffered (ADR-018 §5). The response carries the
/// per-file outcomes and the operation id `POST /api/write/undo` takes.
async fn upload_files(
    State(state): State<AppState>,
    client: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    let Some(writer) = state.writer.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "writes are off");
    };
    let cap = state.config.max_upload_bytes;
    // The whole request's file bytes are bounded too, at a fixed multiple of
    // the per-file cap (H3): the per-file cap alone lets a 1000-file batch
    // buffer with no bound.
    let total_cap = cap.saturating_mul(MAX_UPLOAD_TOTAL_CAPS);
    // The two control fields, and the file items, read straight off the
    // stream. Files are buffered only up to `cap` each: the loop refuses
    // a field the instant its running size would cross `cap`, so the rest
    // of that part is never read (ADR-018 §5: enforced while streaming).
    let mut folder_field: Option<String> = None;
    let mut collection_field: Option<String> = None;
    let mut items: Vec<UploadItem> = Vec::new();
    let mut total: usize = 0;
    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => return multipart_error(error),
        };
        let name = field.name().unwrap_or("").to_string();
        if name == "folder" || name == "collection" {
            let text = match read_control_field(&mut field, &name).await {
                Ok(text) => text,
                Err(response) => return *response,
            };
            if name == "folder" {
                folder_field = Some(text);
            } else {
                collection_field = Some(text);
            }
            continue;
        }
        // A file field. The batch limit is refused the moment the field
        // count would pass it — before that field's bytes are read, and
        // so before the library is touched (ADR-018 §5).
        if items.len() >= MAX_UPLOAD_BATCH {
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("an upload may carry at most {MAX_UPLOAD_BATCH} files"),
            );
        }
        let original_name = field.file_name().unwrap_or("").to_string();
        let mut bytes: Vec<u8> = Vec::new();
        let mut over = false;
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    if bytes.len() + chunk.len() > cap {
                        over = true;
                        break;
                    }
                    if total + bytes.len() + chunk.len() > total_cap {
                        return json_error(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            &format!("an upload may carry at most {total_cap} bytes in total"),
                        );
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(error) => return multipart_error(error),
            }
        }
        if over {
            // Refuse now; dropping `multipart` here leaves the rest of the
            // body unread, which is the disconnect the ADR asks for.
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("a file may not exceed {cap} bytes"),
            );
        }
        total += bytes.len();
        items.push(UploadItem {
            original_name,
            bytes,
        });
    }
    if items.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "an upload needs at least one file");
    }

    let client_addr = crate::resolve_client_ip(
        client.map(|info| info.0),
        &headers,
        &state.config.trusted_proxies,
    );
    let library = match Library::open(writer.root_path()) {
        Ok(library) => library,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let resolver = Resolver::new(&library.files);
    let ctx = BodyCtx::new(&library.files, &resolver);

    // The placement (ADR-017 §11.1), the same three branches the CLI's
    // `upload_placement` walks: a named collection (default folder + the
    // join), a named folder, or the default upload folder.
    let request = match upload_placement(writer, &ctx, folder_field, collection_field) {
        Ok(request) => request,
        Err(response) => return *response,
    };

    // Dedupe answers from the index's content hashes (ADR-017 §11.3): a
    // hit names the live image these bytes already are, and no copy is
    // made. An index that cannot answer (unopened) is a miss, so the
    // upload copies — the fail-safe direction.
    let index = state.index.clone();
    let dedupe = move |sha: &str| index.image_path_for_content(sha);
    let op_id = ulid::Ulid::new().to_string();
    match writer.upload(
        &op_id,
        SOURCE,
        Some(&client_addr),
        request,
        items,
        &ctx,
        dedupe,
        SERVE_IMPORTER,
    ) {
        Ok(outcomes) => Json(serde_json::json!({
            "op": op_id,
            "files": outcomes.iter().map(|outcome| serde_json::json!({
                "image": outcome.image,
                "note": outcome.note,
                "renamed": outcome.renamed,
                "duplicate": outcome.duplicate,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(error) => upload_error(error),
    }
}

/// Read a `folder`/`collection` control field, refusing as it streams once
/// the field passes [`MAX_UPLOAD_FIELD_BYTES`] (H3): the placement fields
/// carry a library-relative path, so the cap is generous but real, and like
/// the per-file cap it is enforced before the whole field is buffered. A
/// field that is not UTF-8 is a `400`, not an internal error.
async fn read_control_field(field: &mut Field<'_>, name: &str) -> Result<String, Box<Response>> {
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                if bytes.len() + chunk.len() > MAX_UPLOAD_FIELD_BYTES {
                    return Err(Box::new(json_error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        &format!("the {name} field may not exceed {MAX_UPLOAD_FIELD_BYTES} bytes"),
                    )));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(error) => return Err(Box::new(multipart_error(error))),
        }
    }
    String::from_utf8(bytes).map_err(|_| {
        Box::new(json_error(
            StatusCode::BAD_REQUEST,
            &format!("the {name} field is not valid UTF-8"),
        ))
    })
}

/// The upload's landing spot, mirroring the CLI's `upload_placement`: a
/// named collection lands files in the default upload folder and joins
/// them; otherwise a named folder (the library root for `""`); otherwise
/// the default upload folder. A `Box<Response>` is the refusal to return.
fn upload_placement(
    writer: &Writer,
    ctx: &BodyCtx<'_>,
    folder_field: Option<String>,
    collection_field: Option<String>,
) -> Result<UploadRequest, Box<Response>> {
    // The two placements are exclusive, as the CLI's `--to`/`--to-collection`
    // are (clap `conflicts_with`): a request naming both is a typed refusal,
    // never a silent preference for one (H6, ADR-017 §11.1).
    if folder_field.is_some() && collection_field.is_some() {
        return Err(Box::new(json_error(
            StatusCode::BAD_REQUEST,
            "give a folder or a collection, not both",
        )));
    }
    if let Some(collection) = collection_field {
        let note_rel = match RelPath::parse(collection.trim_start_matches("./")) {
            Ok(rel) => rel,
            Err(error) => return Err(Box::new(write_error(error))),
        };
        let Some(note_index) = ctx.index_of(&note_rel.text()) else {
            return Err(Box::new(json_error(
                StatusCode::NOT_FOUND,
                &format!("no note at {collection}"),
            )));
        };
        if !matches!(
            ctx.files[note_index].class,
            FileClass::Note | FileClass::ImageNote
        ) {
            return Err(Box::new(json_error(
                StatusCode::BAD_REQUEST,
                &format!("{collection} is not a note"),
            )));
        }
        // The collection page's "current folder" is the note's own
        // folder (ADR-017 §6, §11.1): `attachmentFolderPath` of `./`
        // or `./sub` resolves beside the collection, not at the root.
        return Ok(UploadRequest {
            folder: default_upload_folder(writer, ctx, &note_rel.parent_text()),
            collection: Some(note_rel),
        });
    }
    let folder = match folder_field {
        Some(folder) => folder.trim_start_matches("./").to_string(),
        None => default_upload_folder(writer, ctx, ""),
    };
    Ok(UploadRequest {
        folder,
        collection: None,
    })
}

/// The library's default upload folder: `attachmentFolderPath` in
/// `.obsidian/app.json`, resolved the way an interface resolves it for its
/// current folder, else `inbox/` (ADR-017 §11.1). `current_folder` is that
/// folder — the collection note's folder on a collection page, the library
/// root otherwise. dimagine only ever reads `.obsidian/`, never writes it.
fn default_upload_folder(writer: &Writer, ctx: &BodyCtx<'_>, current_folder: &str) -> String {
    let app_json =
        std::fs::read_to_string(writer.root_path().join(".obsidian").join("app.json")).ok();
    body_edit::upload_folder(app_json.as_deref(), current_folder, ctx).folder
}

/// An upload refusal from the write core is the caller's mistake (a bad
/// format, a batch too large, a missing collection), never a server fault;
/// anything else is the 500 it is.
fn upload_error(error: dimagine_write::Error) -> Response {
    match &error {
        dimagine_write::Error::UploadRefused { message } => {
            json_error(StatusCode::BAD_REQUEST, message)
        }
        _ => write_error(error),
    }
}

/// A malformed multipart body — a missing boundary, a bad part header, a
/// stream that stopped early — is the caller's 4xx, never a 500.
fn multipart_error(error: MultipartError) -> Response {
    let status = error.status();
    let status = if status.is_client_error() || status.is_server_error() {
        status
    } else {
        StatusCode::BAD_REQUEST
    };
    json_error(status, &error.body_text())
}

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------
/// The session token carried by the request cookie, if any.
fn session_token(state: &AppState, headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    let token = crate::cookie_value(cookies, &state.config.cookie_name)?;
    // A token counts only while its session is live; this mirrors the auth
    // gate so a just-expired session never yields a CSRF token. The maps are
    // keyed by the salted hash of the token (W81), never by the token itself.
    let now = now_seconds();
    let found = {
        let sessions = state
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::find_session_key(&sessions, &token)
            .and_then(|key| sessions.get(&key).map(|entry| (key, entry.expiry)))
    };
    let live = found.is_some_and(|(_, expiry)| expiry > now);
    live.then_some(token)
}

/// Read the current settings file text, through the write core when it is
/// open and by a plain read otherwise (the read-only settings page still
/// shows the effective values with `--write` off).
fn read_settings_text(state: &AppState) -> Result<Option<String>, String> {
    if let Some(writer) = state.writer.as_ref() {
        return writer
            .read_settings()
            .map(|bytes| bytes.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
            .map_err(|error| format!("the settings file could not be read: {error}"));
    }
    let path = state
        .catalog
        .root()
        .join(dimagine_core::settings::SETTINGS_DIR)
        .join(dimagine_core::settings::SETTINGS_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("the settings file could not be read: {error}")),
    }
}

/// The image a client named, by library-relative path or unambiguous bare
/// name — the same resolution the CLI applies.
fn resolve_image<'a>(
    files: &'a [dimagine_core::library::FileEntry],
    resolver: &Resolver,
    arg: &str,
) -> Result<&'a dimagine_core::library::FileEntry, String> {
    let trimmed = arg.trim_start_matches("./");
    if let Some(entry) = files
        .iter()
        .find(|e| e.class == FileClass::Image && e.rel == trimmed)
    {
        return Ok(entry);
    }
    if trimmed.contains('/') {
        return Err(format!("no image at library path {trimmed}"));
    }
    let hits: Vec<&dimagine_core::library::FileEntry> = resolver
        .bare_name_hits(trimmed)
        .iter()
        .copied()
        .filter_map(|i| files.get(i))
        .filter(|e| e.class == FileClass::Image)
        .collect();
    match hits.len() {
        0 => Err(format!("no image named {trimmed}")),
        1 => Ok(hits[0]),
        _ => Err(format!(
            "bare name {trimmed} is ambiguous ({} matches); name it by its library path",
            hits.len()
        )),
    }
}

/// The self-embed a freshly created note gets (ADR-017 §2, §4).
fn embed_text(ctx: &BodyCtx, image_rel: &str) -> Result<String, String> {
    let index = ctx
        .index_of(image_rel)
        .ok_or_else(|| format!("{image_rel} is not in the library walk"))?;
    match body_edit::link_text_for(index, ctx).map_err(|e| e.to_string())? {
        Embed::Text(text) => Ok(text),
        Embed::RenameOrMove { name, .. } => Err(format!(
            "bare name {name} is ambiguous; move it into a folder or rename it first"
        )),
    }
}

/// The front-matter block of a note, as text (the response body the CLI
/// prints after `tag`/`rate`/`set`).
fn front_matter_text(bytes: &[u8]) -> String {
    match front_matter_range(bytes) {
        FmRange::Closed { close_start, .. } => {
            let end = bytes[close_start..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|off| close_start + off + 1)
                .unwrap_or(bytes.len());
            String::from_utf8_lossy(&bytes[..end]).into_owned()
        }
        _ => String::new(),
    }
}

fn rating_edit(value: &serde_json::Value) -> Option<RatingEdit> {
    match value {
        serde_json::Value::String(word) if matches!(word.as_str(), "none" | "clear") => {
            Some(RatingEdit::Clear)
        }
        serde_json::Value::Number(number) => number.as_i64().and_then(|n| {
            u8::try_from(n)
                .ok()
                .filter(|v| *v <= 5)
                .map(RatingEdit::Set)
        }),
        _ => None,
    }
}

fn string_array(value: &serde_json::Value, key: &str) -> Vec<String> {
    match value.get(key) {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Why a settings change was refused, in one line (the CLI's wording).
fn apply_error_text(error: &settings::ApplyError) -> String {
    match error {
        settings::ApplyError::Unparsable { message } => {
            format!("settings file is not parsable: {message}")
        }
        settings::ApplyError::UnknownKey { key, allowed } => format!(
            "unknown setting {key}; allowed: {}",
            if allowed.is_empty() {
                "(none)".to_string()
            } else {
                allowed.join(", ")
            }
        ),
        settings::ApplyError::InvalidValue {
            key,
            value,
            message,
        } => format!("value {value} for {key} is not allowed: {message}"),
    }
}

/// A JSON error body: every refusal on this surface is JSON, so a client
/// never has to guess whether a status was HTML.
fn json_error(status: StatusCode, message: &str) -> Response {
    let mut response = (status, Json(serde_json::json!({ "error": message }))).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// A write-core error, mapped to a status. A path the core refuses as unsafe
/// is a client error, not a server one; anything else is the 500 it is.
fn write_error(error: dimagine_write::Error) -> Response {
    let message = error.to_string();
    // A path the write core will not touch (outside the root, a symlink, a
    // dot-directory) is the caller's mistake; the core keeps those reasons
    // distinct from an I/O failure (invariant 4).
    let status = if message.contains("symlink")
        || message.contains("outside")
        || message.contains("dot")
        || message.contains("absolute")
        || message.contains("..")
    {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    json_error(status, &message)
}

/// A write error from a whole-replace operation that carries an open-hash
/// precondition (ADR-016 §3). A `Conflict` is the caller's race, so it is the
/// 409 the ADR promises — with the note's current ETag — never a 500 and never
/// a retry; every other error goes through [`write_error`].
fn guarded_write_error(error: dimagine_write::Error) -> Response {
    let dimagine_write::Error::Conflict(conflict) = &error else {
        return write_error(error);
    };
    conflict_response(&conflict.current, None)
}

/// The 409 an open-hash precondition mismatch answers (ADR-016 §3): the note's
/// current sha256 and view ETag, so the client can offer "load the latest and
/// re-edit" or "overwrite with mine" (a retry carrying the hash returned here),
/// and nothing of the request is applied. `current_body` is the changed-elsewhere
/// content the prompt shows when the caller can spell it (the body edit can;
/// a reorder or caption conflict carries only the hash and ETag).
fn conflict_response(current: &Fingerprint, current_body: Option<&str>) -> Response {
    // The note's view ETag is built from size and mtime (media.rs, the source
    // view), so the current fingerprint renders the ETag the client must
    // re-read against. Both callers share one helper, so they agree even for a
    // pre-epoch mtime (W67 D4).
    let etag = crate::media::view_etag(current.size, current.mtime_ns);
    let mut payload = serde_json::json!({
        "error": "the file changed since it was read",
        "hash": current.hex(),
        "etag": etag,
    });
    if let Some(body) = current_body {
        payload["body"] = serde_json::Value::String(body.to_string());
    }
    let mut response = (StatusCode::CONFLICT, Json(payload)).into_response();
    if let Ok(value) = HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Report an accounts-store failure to the client: 500 with the store's own
/// message, which never contains a password or hash.
fn store_error(error: AccountsError) -> Response {
    json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string())
}

/// A length-sensitive equality that does not branch on the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).unwrap_u8() == 1
}

#[cfg(test)]
mod tag_progress_visibility_tests {
    use super::*;

    #[test]
    fn failure_details_are_owner_only() {
        let status = TagProgress {
            state: "error",
            error: Some("private note error".into()),
            ..TagProgress::default()
        };
        assert!(tag_progress_html(&status, Some("csrf")).contains("private note error"));
        assert_eq!(tag_progress_html(&status, None), "<p>Tag apply failed.</p>");
    }
}
