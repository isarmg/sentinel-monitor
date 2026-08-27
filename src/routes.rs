use crate::{
    auth::{
        decode_media_token, expired_session_cookie, hash_password, issue_media_token,
        issue_session, session_cookie, verify_password, CurrentUser,
    },
    background::{camera_path, emit_event, sync_camera},
    error::{AppError, Result},
    models::*,
    onvif, AppState,
};
use async_stream::stream;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{
        header::{
            ACCEPT_RANGES, AUTHORIZATION, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE,
            CONTENT_TYPE, RANGE, SET_COOKIE,
        },
        HeaderMap, HeaderValue, StatusCode,
    },
    response::{sse::Event, IntoResponse, Response, Sse},
    routing::{delete, get, patch, post, put},
    Json, Router,
};
use chrono::{DateTime, Utc};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{convert::Infallible, time::Duration};
use tower_http::{compression::CompressionLayer, services::ServeDir, trace::TraceLayer};
use url::Url;
use uuid::Uuid;

const USER_SELECT: &str = "SELECT id, email, password_hash, role, active, last_login_at, created_at, updated_at FROM users";
const CAMERA_SELECT: &str = "SELECT id, name, location, main_stream_url_enc, sub_stream_url_enc, onvif_url, username, password_enc, enabled, record_enabled, status, last_seen_at, created_at, updated_at FROM cameras";

pub fn router(state: AppState) -> Router {
    let static_dir = state.config.static_dir.clone();
    let api = Router::new()
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/me", get(me))
        .route("/users", get(list_users).post(create_user))
        .route("/users/{id}", put(update_user).delete(delete_user))
        .route("/cameras", get(list_cameras).post(create_camera))
        .route("/cameras/{id}", put(update_camera).delete(delete_camera))
        .route("/cameras/{id}/stream-ticket", get(stream_ticket))
        .route("/cameras/{id}/ptz", post(ptz))
        .route("/discovery/onvif", post(discover_onvif))
        .route("/recordings", get(list_recordings))
        .route("/recordings/play", get(play_recording))
        .route("/events", get(list_events))
        .route("/events/stream", get(event_stream))
        .route("/events/{id}/ack", post(ack_event))
        .route("/audit", get(list_audit))
        .route("/system/status", get(system_status));

    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route("/internal/media/auth", post(media_auth))
        .nest("/api", api)
        .fallback_service(ServeDir::new(static_dir).append_index_html_on_directories(true))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn login(
    State(state): State<AppState>,
    Json(request): Json<LoginRequest>,
) -> Result<impl IntoResponse> {
    let user =
        sqlx::query_as::<_, UserRecord>(&format!("{USER_SELECT} WHERE LOWER(email) = LOWER($1)"))
            .bind(request.email.trim())
            .fetch_optional(&state.pool)
            .await?
            .filter(|user| user.active)
            .ok_or(AppError::Unauthorized)?;
    if !verify_password(&request.password, &user.password_hash) {
        return Err(AppError::Unauthorized);
    }

    sqlx::query("UPDATE users SET last_login_at = NOW() WHERE id = $1")
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    let token = issue_session(user.id, &state.config)?;
    let mut headers = HeaderMap::new();
    headers.insert(
        SET_COOKIE,
        HeaderValue::from_str(&session_cookie(&token, &state.config))
            .map_err(|_| AppError::Internal("session cookie failed".into()))?,
    );
    write_audit(
        &state,
        Some(user.id),
        "auth.login",
        "user",
        Some(user.id),
        json!({}),
    )
    .await;
    Ok((headers, Json(UserView::from(user))))
}

async fn logout(State(state): State<AppState>) -> Result<impl IntoResponse> {
    let mut headers = HeaderMap::new();
    headers.insert(
        SET_COOKIE,
        HeaderValue::from_str(&expired_session_cookie(&state.config))
            .map_err(|_| AppError::Internal("session cookie failed".into()))?,
    );
    Ok((headers, StatusCode::NO_CONTENT))
}

async fn me(user: CurrentUser) -> Json<Value> {
    Json(json!({ "id": user.id, "email": user.email, "role": user.role }))
}

async fn list_users(
    user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<UserView>>> {
    user.require_admin()?;
    let records = sqlx::query_as::<_, UserRecord>(&format!("{USER_SELECT} ORDER BY created_at"))
        .fetch_all(&state.pool)
        .await?;
    Ok(Json(records.into_iter().map(UserView::from).collect()))
}

async fn create_user(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(request): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<UserView>)> {
    user.require_admin()?;
    validate_role(&request.role)?;
    validate_email(&request.email)?;
    let record = sqlx::query_as::<_, UserRecord>(
        "INSERT INTO users (id, email, password_hash, role) VALUES ($1, LOWER($2), $3, $4) \
         RETURNING id, email, password_hash, role, active, last_login_at, created_at, updated_at",
    )
    .bind(Uuid::new_v4())
    .bind(request.email.trim())
    .bind(hash_password(&request.password)?)
    .bind(&request.role)
    .fetch_one(&state.pool)
    .await
    .map_err(map_unique_email)?;
    write_audit(
        &state,
        Some(user.id),
        "user.create",
        "user",
        Some(record.id),
        json!({ "role": record.role }),
    )
    .await;
    Ok((StatusCode::CREATED, Json(UserView::from(record))))
}

async fn update_user(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(request): Json<UpdateUserRequest>,
) -> Result<Json<UserView>> {
    user.require_admin()?;
    let existing = load_user(&state, id).await?;
    let role = request.role.unwrap_or(existing.role.clone());
    validate_role(&role)?;
    let active = request.active.unwrap_or(existing.active);
    if id == user.id && !active {
        return Err(AppError::Conflict("不能停用当前登录账号".into()));
    }
    if existing.role == "admin" && (role != "admin" || !active) {
        ensure_another_admin(&state, id).await?;
    }
    let password_hash = match request.password {
        Some(password) if !password.is_empty() => hash_password(&password)?,
        _ => existing.password_hash,
    };
    let record = sqlx::query_as::<_, UserRecord>(
        "UPDATE users SET role = $2, active = $3, password_hash = $4, updated_at = NOW() WHERE id = $1 \
         RETURNING id, email, password_hash, role, active, last_login_at, created_at, updated_at",
    )
    .bind(id)
    .bind(&role)
    .bind(active)
    .bind(password_hash)
    .fetch_one(&state.pool)
    .await?;
    write_audit(
        &state,
        Some(user.id),
        "user.update",
        "user",
        Some(id),
        json!({ "role": role, "active": active }),
    )
    .await;
    Ok(Json(UserView::from(record)))
}

async fn delete_user(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    user.require_admin()?;
    if id == user.id {
        return Err(AppError::Conflict("不能删除当前登录账号".into()));
    }
    let existing = load_user(&state, id).await?;
    if existing.role == "admin" {
        ensure_another_admin(&state, id).await?;
    }
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await?;
    write_audit(
        &state,
        Some(user.id),
        "user.delete",
        "user",
        Some(id),
        json!({ "email": existing.email }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_cameras(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<CameraView>>> {
    let cameras = sqlx::query_as::<_, CameraRecord>(&format!("{CAMERA_SELECT} ORDER BY name"))
        .fetch_all(&state.pool)
        .await?;
    Ok(Json(cameras.iter().map(CameraView::from).collect()))
}

async fn create_camera(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(request): Json<CreateCameraRequest>,
) -> Result<(StatusCode, Json<CameraMutationResponse>)> {
    user.require_admin()?;
    validate_camera_values(
        &request.name,
        &request.main_stream_url,
        request.sub_stream_url.as_deref(),
        request.onvif_url.as_deref(),
    )?;
    let record = sqlx::query_as::<_, CameraRecord>(
        "INSERT INTO cameras (id, name, location, main_stream_url_enc, sub_stream_url_enc, onvif_url, username, password_enc, enabled, record_enabled, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         RETURNING id, name, location, main_stream_url_enc, sub_stream_url_enc, onvif_url, username, password_enc, enabled, record_enabled, status, last_seen_at, created_at, updated_at",
    )
    .bind(Uuid::new_v4())
    .bind(request.name.trim())
    .bind(request.location.trim())
    .bind(state.secrets.encrypt(request.main_stream_url.trim())?)
    .bind(
        request
            .sub_stream_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| state.secrets.encrypt(value.trim()))
            .transpose()?,
    )
    .bind(clean_optional(request.onvif_url))
    .bind(clean_optional(request.username))
    .bind(
        request
            .password
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(|value| state.secrets.encrypt(value))
            .transpose()?,
    )
    .bind(request.enabled)
    .bind(request.record_enabled)
    .bind(user.id)
    .fetch_one(&state.pool)
    .await?;

    let sync_result = sync_camera(&state, &record).await;
    let warning = sync_result.as_ref().err().map(ToString::to_string);
    if warning.is_some() {
        sqlx::query("UPDATE cameras SET status = 'error' WHERE id = $1")
            .bind(record.id)
            .execute(&state.pool)
            .await?;
    }
    write_audit(
        &state,
        Some(user.id),
        "camera.create",
        "camera",
        Some(record.id),
        json!({ "name": record.name }),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(CameraMutationResponse {
            camera: CameraView::from(&record),
            media_synced: sync_result.is_ok(),
            warning,
        }),
    ))
}

async fn update_camera(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(request): Json<UpdateCameraRequest>,
) -> Result<Json<CameraMutationResponse>> {
    user.require_admin()?;
    let existing = load_camera(&state, id).await?;
    let name = request.name.unwrap_or(existing.name.clone());
    let location = request.location.unwrap_or(existing.location.clone());

    let main_stream_url_enc = match request.main_stream_url {
        Some(value) if !value.trim().is_empty() => {
            validate_rtsp(&value)?;
            state.secrets.encrypt(value.trim())?
        }
        _ => existing.main_stream_url_enc,
    };
    let sub_stream_url_enc = if request.clear_sub_stream {
        None
    } else if let Some(value) = request
        .sub_stream_url
        .filter(|value| !value.trim().is_empty())
    {
        validate_rtsp(&value)?;
        Some(state.secrets.encrypt(value.trim())?)
    } else {
        existing.sub_stream_url_enc
    };
    let onvif_url = if request.clear_onvif {
        None
    } else {
        request
            .onvif_url
            .and_then(|value| clean_optional(Some(value)))
            .or(existing.onvif_url)
    };
    if let Some(url) = &onvif_url {
        validate_http(url)?;
    }
    let username = request
        .username
        .and_then(|value| clean_optional(Some(value)))
        .or(existing.username);
    let password_enc = if request.clear_password {
        None
    } else if let Some(password) = request.password.filter(|value| !value.is_empty()) {
        Some(state.secrets.encrypt(&password)?)
    } else {
        existing.password_enc
    };
    let enabled = request.enabled.unwrap_or(existing.enabled);
    let record_enabled = request.record_enabled.unwrap_or(existing.record_enabled);
    if name.trim().is_empty() {
        return Err(AppError::Validation("摄像头名称不能为空".into()));
    }

    let record = sqlx::query_as::<_, CameraRecord>(
        "UPDATE cameras SET name = $2, location = $3, main_stream_url_enc = $4, sub_stream_url_enc = $5, onvif_url = $6, username = $7, password_enc = $8, enabled = $9, record_enabled = $10, status = CASE WHEN $9 THEN 'pending' ELSE 'disabled' END, updated_at = NOW() WHERE id = $1 \
         RETURNING id, name, location, main_stream_url_enc, sub_stream_url_enc, onvif_url, username, password_enc, enabled, record_enabled, status, last_seen_at, created_at, updated_at",
    )
    .bind(id)
    .bind(name.trim())
    .bind(location.trim())
    .bind(main_stream_url_enc)
    .bind(sub_stream_url_enc)
    .bind(onvif_url)
    .bind(username)
    .bind(password_enc)
    .bind(enabled)
    .bind(record_enabled)
    .fetch_one(&state.pool)
    .await?;

    let sync_result = sync_camera(&state, &record).await;
    let warning = sync_result.as_ref().err().map(ToString::to_string);
    if warning.is_some() {
        sqlx::query("UPDATE cameras SET status = 'error' WHERE id = $1")
            .bind(id)
            .execute(&state.pool)
            .await?;
    }
    write_audit(
        &state,
        Some(user.id),
        "camera.update",
        "camera",
        Some(id),
        json!({ "name": record.name }),
    )
    .await;
    Ok(Json(CameraMutationResponse {
        camera: CameraView::from(&record),
        media_synced: sync_result.is_ok(),
        warning,
    }))
}

async fn delete_camera(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    user.require_admin()?;
    let camera = load_camera(&state, id).await?;
    sqlx::query("DELETE FROM cameras WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await?;
    if let Err(error) = state.media.delete_path(&camera_path(id, "main")).await {
        tracing::warn!(%error, camera_id = %id, "main path cleanup failed");
    }
    if let Err(error) = state.media.delete_path(&camera_path(id, "sub")).await {
        tracing::warn!(%error, camera_id = %id, "sub path cleanup failed");
    }
    write_audit(
        &state,
        Some(user.id),
        "camera.delete",
        "camera",
        Some(id),
        json!({ "name": camera.name }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn stream_ticket(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<StreamTicketQuery>,
) -> Result<Json<StreamTicket>> {
    let camera = load_camera(&state, id).await?;
    if !camera.enabled {
        return Err(AppError::Conflict("摄像头已停用".into()));
    }
    let profile = query.profile.as_deref().unwrap_or("main");
    if profile != "main" && profile != "sub" {
        return Err(AppError::Validation("profile只能是main或sub".into()));
    }
    if profile == "sub" && camera.sub_stream_url_enc.is_none() {
        return Err(AppError::Validation("该摄像头没有配置子码流".into()));
    }
    let path = camera_path(id, profile);
    let (token, expires_at) = issue_media_token(
        user.id,
        id,
        path.clone(),
        vec!["read".into()],
        &state.config,
    )?;
    Ok(Json(StreamTicket {
        profile: profile.to_string(),
        whep_url: format!("{}/{}/whep", state.config.public_webrtc_base_url, path),
        hls_url: format!("{}/{}/index.m3u8", state.config.public_hls_base_url, path),
        token,
        expires_at,
    }))
}

async fn discover_onvif(
    user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<onvif::DiscoveredDevice>>> {
    user.require_operator()?;
    Ok(Json(
        onvif::discover(state.config.onvif_discovery_timeout).await?,
    ))
}

async fn ptz(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(request): Json<PtzRequest>,
) -> Result<StatusCode> {
    user.require_operator()?;
    if request.action != "move" && request.action != "stop" {
        return Err(AppError::Validation("PTZ action只能是move或stop".into()));
    }
    let values = (
        request.pan.unwrap_or(0.0),
        request.tilt.unwrap_or(0.0),
        request.zoom.unwrap_or(0.0),
    );
    if [values.0, values.1, values.2]
        .iter()
        .any(|value| !(-1.0..=1.0).contains(value))
    {
        return Err(AppError::Validation("PTZ速度必须在-1到1之间".into()));
    }
    let camera = load_camera(&state, id).await?;
    let onvif_url = camera
        .onvif_url
        .as_deref()
        .ok_or_else(|| AppError::Validation("摄像头没有配置ONVIF地址".into()))?;
    let password = camera
        .password_enc
        .as_deref()
        .map(|value| state.secrets.decrypt(value))
        .transpose()?;
    onvif::ptz(
        &state.http,
        onvif_url,
        camera.username.as_deref(),
        password.as_deref(),
        &request.action,
        values.0,
        values.1,
        values.2,
    )
    .await?;
    write_audit(
        &state,
        Some(user.id),
        "camera.ptz",
        "camera",
        Some(id),
        json!({ "action": request.action, "pan": values.0, "tilt": values.1, "zoom": values.2 }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct RecordingQuery {
    camera_id: Uuid,
    profile: Option<String>,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
}

async fn list_recordings(
    user: CurrentUser,
    State(state): State<AppState>,
    Query(query): Query<RecordingQuery>,
) -> Result<Json<Vec<crate::mediamtx::RecordingSpan>>> {
    let camera = load_camera(&state, query.camera_id).await?;
    let profile = query.profile.as_deref().unwrap_or("main");
    if profile == "sub" && camera.sub_stream_url_enc.is_none() {
        return Err(AppError::Validation("该摄像头没有配置子码流".into()));
    }
    if profile != "main" && profile != "sub" {
        return Err(AppError::Validation("profile只能是main或sub".into()));
    }
    let path = camera_path(camera.id, profile);
    let (token, _) = issue_media_token(
        user.id,
        camera.id,
        path.clone(),
        vec!["playback".into()],
        &state.config,
    )?;
    Ok(Json(
        state
            .media
            .recordings(&path, query.start, query.end, &token)
            .await?,
    ))
}

#[derive(Deserialize)]
struct PlayRecordingQuery {
    camera_id: Uuid,
    start: DateTime<Utc>,
    duration: f64,
    format: Option<String>,
}

async fn play_recording(
    user: CurrentUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PlayRecordingQuery>,
) -> Result<Response<Body>> {
    if !(0.1..=21_600.0).contains(&query.duration) {
        return Err(AppError::Validation(
            "单次回放时长必须在0.1秒到6小时之间".into(),
        ));
    }
    let format = query.format.as_deref().unwrap_or("mp4");
    if format != "mp4" && format != "fmp4" {
        return Err(AppError::Validation("回放格式只能是mp4或fmp4".into()));
    }
    let camera = load_camera(&state, query.camera_id).await?;
    let path = camera_path(camera.id, "main");
    let (token, _) = issue_media_token(
        user.id,
        camera.id,
        path.clone(),
        vec!["playback".into()],
        &state.config,
    )?;
    let range = headers.get(RANGE).and_then(|value| value.to_str().ok());
    let response = state
        .media
        .recording_stream(&path, query.start, query.duration, format, &token, range)
        .await?;
    if !response.status().is_success() {
        return Err(AppError::Upstream(format!(
            "recording playback returned {}",
            response.status()
        )));
    }
    let status = response.status();
    let upstream_headers = response.headers().clone();
    let mut builder = Response::builder().status(status);
    for name in [
        CONTENT_TYPE,
        CONTENT_LENGTH,
        CONTENT_RANGE,
        ACCEPT_RANGES,
        CONTENT_DISPOSITION,
    ] {
        if let Some(value) = upstream_headers.get(&name) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(Body::from_stream(response.bytes_stream()))
        .map_err(|error| AppError::Internal(format!("playback response failed: {error}")))
}

async fn list_events(
    _user: CurrentUser,
    State(state): State<AppState>,
    Query(query): Query<EventQuery>,
) -> Result<Json<Vec<EventRecord>>> {
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let events = sqlx::query_as::<_, EventRecord>(
        "SELECT id, camera_id, kind, severity, message, details, acknowledged_at, acknowledged_by, created_at \
         FROM events WHERE ($1::uuid IS NULL OR camera_id = $1) \
         AND ($2::boolean = FALSE OR acknowledged_at IS NULL) ORDER BY created_at DESC LIMIT $3",
    )
    .bind(query.camera_id)
    .bind(query.unacknowledged.unwrap_or(false))
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(events))
}

async fn ack_event(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    user.require_operator()?;
    let result = sqlx::query(
        "UPDATE events SET acknowledged_at = NOW(), acknowledged_by = $2 WHERE id = $1",
    )
    .bind(id)
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("事件不存在".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn event_stream(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let mut receiver = state.events.subscribe();
    let output = stream! {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    if let Ok(payload) = Event::default().event("system-event").json_data(event) {
                        yield Ok(payload);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(output).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

#[derive(Deserialize)]
struct AuditQuery {
    limit: Option<i64>,
}

async fn list_audit(
    user: CurrentUser,
    State(state): State<AppState>,
    Query(query): Query<AuditQuery>,
) -> Result<Json<Vec<AuditRecord>>> {
    user.require_admin()?;
    let rows = sqlx::query_as::<_, AuditRecord>(
        "SELECT id, user_id, action, entity_type, entity_id, details, created_at FROM audit_logs ORDER BY created_at DESC LIMIT $1",
    )
    .bind(query.limit.unwrap_or(100).clamp(1, 500))
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows))
}

async fn system_status(_user: CurrentUser, State(state): State<AppState>) -> Result<Json<Value>> {
    let (total, online, recording): (i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COUNT(*) FILTER (WHERE status = 'online'), COUNT(*) FILTER (WHERE record_enabled AND enabled) FROM cameras",
    )
    .fetch_one(&state.pool)
    .await?;
    Ok(Json(json!({
        "service": "sentinel-monitor",
        "version": env!("CARGO_PKG_VERSION"),
        "database": "ok",
        "media_service": if state.media.health().await { "ok" } else { "unavailable" },
        "cameras": { "total": total, "online": online, "recording": recording },
        "server_time": Utc::now()
    })))
}

#[derive(Deserialize)]
struct MediaAuthRequest {
    #[serde(default)]
    token: String,
    action: String,
    #[serde(default)]
    path: String,
}

async fn media_auth(
    State(state): State<AppState>,
    Json(request): Json<MediaAuthRequest>,
) -> StatusCode {
    let Ok(claims) = decode_media_token(&request.token, &state.config) else {
        return StatusCode::UNAUTHORIZED;
    };
    if claims.path != request.path
        || !claims
            .actions
            .iter()
            .any(|action| action == &request.action)
    {
        return StatusCode::FORBIDDEN;
    }
    StatusCode::OK
}

async fn live() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn ready(State(state): State<AppState>) -> Response {
    let database = sarmg_platform_postgres::ready(&state.pool).await;
    let media = state.media.health().await;
    let status = if database && media {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({ "database": database, "media_service": media })),
    )
        .into_response()
}

async fn load_camera(state: &AppState, id: Uuid) -> Result<CameraRecord> {
    sqlx::query_as::<_, CameraRecord>(&format!("{CAMERA_SELECT} WHERE id = $1"))
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::NotFound("摄像头不存在".into()))
}

async fn load_user(state: &AppState, id: Uuid) -> Result<UserRecord> {
    sqlx::query_as::<_, UserRecord>(&format!("{USER_SELECT} WHERE id = $1"))
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::NotFound("用户不存在".into()))
}

async fn ensure_another_admin(state: &AppState, excluded: Uuid) -> Result<()> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users WHERE role = 'admin' AND active AND id <> $1",
    )
    .bind(excluded)
    .fetch_one(&state.pool)
    .await?;
    if count == 0 {
        return Err(AppError::Conflict("系统必须保留至少一个可用管理员".into()));
    }
    Ok(())
}

fn validate_role(role: &str) -> Result<()> {
    if matches!(role, "admin" | "operator" | "viewer") {
        Ok(())
    } else {
        Err(AppError::Validation(
            "角色只能是admin、operator或viewer".into(),
        ))
    }
}

fn validate_email(email: &str) -> Result<()> {
    let email = email.trim();
    if email.len() >= 5 && email.contains('@') && !email.contains(char::is_whitespace) {
        Ok(())
    } else {
        Err(AppError::Validation("邮箱格式无效".into()))
    }
}

fn validate_camera_values(
    name: &str,
    main: &str,
    sub: Option<&str>,
    onvif: Option<&str>,
) -> Result<()> {
    if name.trim().is_empty() {
        return Err(AppError::Validation("摄像头名称不能为空".into()));
    }
    validate_rtsp(main)?;
    if let Some(sub) = sub.filter(|value| !value.trim().is_empty()) {
        validate_rtsp(sub)?;
    }
    if let Some(onvif) = onvif.filter(|value| !value.trim().is_empty()) {
        validate_http(onvif)?;
    }
    Ok(())
}

fn validate_rtsp(value: &str) -> Result<()> {
    let url =
        Url::parse(value.trim()).map_err(|_| AppError::Validation("RTSP地址格式无效".into()))?;
    if matches!(url.scheme(), "rtsp" | "rtsps") && url.host().is_some() {
        Ok(())
    } else {
        Err(AppError::Validation(
            "流地址必须使用rtsp://或rtsps://".into(),
        ))
    }
}

fn validate_http(value: &str) -> Result<()> {
    let url =
        Url::parse(value.trim()).map_err(|_| AppError::Validation("ONVIF地址格式无效".into()))?;
    if matches!(url.scheme(), "http" | "https") && url.host().is_some() {
        Ok(())
    } else {
        Err(AppError::Validation(
            "ONVIF地址必须使用http://或https://".into(),
        ))
    }
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn map_unique_email(error: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(database) = &error {
        if database.constraint() == Some("users_email_lower_idx") {
            return AppError::Conflict("该邮箱已经存在".into());
        }
    }
    AppError::Database(error)
}

async fn write_audit(
    state: &AppState,
    user_id: Option<Uuid>,
    action: &str,
    entity_type: &str,
    entity_id: Option<Uuid>,
    details: Value,
) {
    if let Err(error) = sqlx::query(
        "INSERT INTO audit_logs (id, user_id, action, entity_type, entity_id, details) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(action)
    .bind(entity_type)
    .bind(entity_id)
    .bind(details)
    .execute(&state.pool)
    .await
    {
        tracing::warn!(%error, "audit log write failed");
    }
}
