use crate::{
    auth::{
        csrf_cookie, decode_media_token, enforce_browser_security, expired_csrf_cookie,
        expired_session_cookie, hash_password, issue_media_token, issue_session, revoke_session,
        session_cookie, CurrentUser,
    },
    background::camera_path,
    error::{AppError, Result},
    models::*,
    onvif, reconciliation, AppState,
};
use async_stream::stream;
use axum::{
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Path, Query, State},
    http::{
        header::{
            ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE,
            SET_COOKIE,
        },
        HeaderMap, HeaderValue, StatusCode,
    },
    middleware,
    response::{sse::Event, IntoResponse, Response, Sse},
    routing::{get, post, put},
    Json, Router,
};
use chrono::{DateTime, Utc};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{convert::Infallible, net::SocketAddr, time::Duration};
use tower_http::{compression::CompressionLayer, services::ServeDir, trace::TraceLayer};
use url::Url;
use uuid::Uuid;

const USER_SELECT: &str = "SELECT id, email, password_hash, role, active, session_version, last_login_at, created_at, updated_at FROM users";
const CAMERA_SELECT: &str = "SELECT id, name, location, main_stream_url_enc, sub_stream_url_enc, onvif_url, username, password_enc, enabled, record_enabled, status, last_seen_at, created_at, updated_at FROM cameras";

pub fn router(state: AppState) -> Router {
    let static_dir = state.config.static_dir.clone();
    let api = Router::new()
        .route(
            "/auth/login",
            post(login).layer(DefaultBodyLimit::max(state.config.login_body_limit)),
        )
        .route("/auth/logout", post(logout))
        .route("/me", get(me))
        .route("/users", get(list_users).post(create_user))
        .route("/users/{id}", put(update_user).delete(delete_user))
        .route("/cameras", get(list_cameras).post(create_camera))
        .route("/cameras/{id}", put(update_camera).delete(delete_camera))
        .route("/media/operations/{id}", get(media_operation))
        .route("/cameras/{id}/stream-ticket", get(stream_ticket))
        .route("/cameras/{id}/ptz", post(ptz))
        .route("/discovery/onvif", post(discover_onvif))
        .route("/recordings", get(list_recordings))
        .route("/recordings/play", get(play_recording))
        .route("/events", get(list_events))
        .route("/events/stream", get(event_stream))
        .route("/events/{id}/ack", post(ack_event))
        .route("/audit", get(list_audit))
        .route("/system/status", get(system_status))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            enforce_browser_security,
        ));

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
    ConnectInfo(source): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
    Json(request): Json<LoginRequest>,
) -> Result<impl IntoResponse> {
    state
        .login
        .check_attempt(source.ip(), request.email.trim())?;
    let user =
        sqlx::query_as::<_, UserRecord>(&format!("{USER_SELECT} WHERE LOWER(email) = LOWER(?)"))
            .bind(request.email.trim())
            .fetch_optional(&state.pool)
            .await?;
    let password_hash = user
        .as_ref()
        .map(|user| user.password_hash.clone())
        .unwrap_or_else(|| state.login.dummy_password_hash().to_string());
    let verified = state.login.verify(request.password, password_hash).await?;
    let user = user
        .filter(|user| user.active && verified)
        .ok_or(AppError::Unauthorized)?;

    sqlx::query("UPDATE users SET last_login_at = datetime('now') WHERE id = ?")
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    let session = issue_session(&state, user.id, user.session_version).await?;
    let mut headers = HeaderMap::new();
    headers.append(
        SET_COOKIE,
        HeaderValue::from_str(&session_cookie(&session.token, &state.config))
            .map_err(|_| AppError::Internal("session cookie failed".into()))?,
    );
    headers.append(
        SET_COOKIE,
        HeaderValue::from_str(&csrf_cookie(&session.csrf_token, &state.config))
            .map_err(|_| AppError::Internal("csrf cookie failed".into()))?,
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

async fn logout(user: CurrentUser, State(state): State<AppState>) -> Result<impl IntoResponse> {
    revoke_session(&state, user.session_id).await?;
    let mut headers = HeaderMap::new();
    headers.append(
        SET_COOKIE,
        HeaderValue::from_str(&expired_session_cookie(&state.config))
            .map_err(|_| AppError::Internal("session cookie failed".into()))?,
    );
    headers.append(
        SET_COOKIE,
        HeaderValue::from_str(&expired_csrf_cookie(&state.config))
            .map_err(|_| AppError::Internal("csrf cookie failed".into()))?,
    );
    Ok((headers, StatusCode::NO_CONTENT))
}

async fn me(user: CurrentUser) -> Json<UserView> {
    Json(user.view())
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
    let now = Utc::now();
    let record = sqlx::query_as::<_, UserRecord>(
        "INSERT INTO users (id, email, password_hash, role, active, created_at, updated_at) \
         VALUES (?, LOWER(?), ?, ?, 1, ?, ?) \
         RETURNING id, email, password_hash, role, active, session_version, last_login_at, created_at, updated_at",
    )
    .bind(Uuid::new_v4())
    .bind(request.email.trim())
    .bind(hash_password(&request.password)?)
    .bind(&request.role)
    .bind(now)
    .bind(now)
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
    let (password_hash, password_changed) = match request.password {
        Some(password) if !password.is_empty() => (hash_password(&password)?, true),
        _ => (existing.password_hash, false),
    };
    let invalidate_sessions =
        password_changed || role != existing.role || active != existing.active;
    let record = sqlx::query_as::<_, UserRecord>(
        "UPDATE users SET role = ?, active = ?, password_hash = ?, \
         session_version = session_version + ?, updated_at = datetime('now') WHERE id = ? \
         RETURNING id, email, password_hash, role, active, session_version, last_login_at, created_at, updated_at",
    )
    .bind(&role)
    .bind(active)
    .bind(password_hash)
    .bind(i64::from(invalidate_sessions))
    .bind(id)
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
    sqlx::query("DELETE FROM users WHERE id = ?")
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
    let cameras = sqlx::query_as::<_, CameraRecord>(&format!(
        "{CAMERA_SELECT} WHERE deleted_at IS NULL ORDER BY name"
    ))
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
    let now = Utc::now();
    let mut transaction = state.pool.begin().await?;
    let record = sqlx::query_as::<_, CameraRecord>(
        "INSERT INTO cameras (id, name, location, main_stream_url_enc, sub_stream_url_enc, onvif_url, username, password_enc, enabled, record_enabled, created_by, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
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
    .bind(now)
    .bind(now)
    .fetch_one(&mut *transaction)
    .await?;
    let operation = reconciliation::queue_camera_change(
        &mut transaction,
        &record,
        record.enabled,
        user.id,
        "camera_created",
    )
    .await?;
    transaction.commit().await?;
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
            media_synced: false,
            warning: None,
            operation_id: operation.id,
            operation_state: operation.state,
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

    let updated_at = Utc::now();
    let mut transaction = state.pool.begin().await?;
    let record = sqlx::query_as::<_, CameraRecord>(
        "UPDATE cameras SET name = ?1, location = ?2, main_stream_url_enc = ?3, sub_stream_url_enc = ?4, onvif_url = ?5, username = ?6, password_enc = ?7, enabled = ?8, record_enabled = ?9, status = CASE WHEN ?8 THEN 'pending' ELSE 'disabled' END, updated_at = ?10 WHERE id = ?11 AND deleted_at IS NULL \
         RETURNING id, name, location, main_stream_url_enc, sub_stream_url_enc, onvif_url, username, password_enc, enabled, record_enabled, status, last_seen_at, created_at, updated_at",
    )
    .bind(name.trim())
    .bind(location.trim())
    .bind(main_stream_url_enc)
    .bind(sub_stream_url_enc)
    .bind(onvif_url)
    .bind(username)
    .bind(password_enc)
    .bind(enabled)
    .bind(record_enabled)
    .bind(updated_at)
    .bind(id)
    .fetch_one(&mut *transaction)
    .await?;
    let operation = reconciliation::queue_camera_change(
        &mut transaction,
        &record,
        record.enabled,
        user.id,
        "camera_updated",
    )
    .await?;
    transaction.commit().await?;
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
        media_synced: false,
        warning: None,
        operation_id: operation.id,
        operation_state: operation.state,
    }))
}

async fn delete_camera(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<(StatusCode, Json<reconciliation::MediaOperationView>)> {
    user.require_admin()?;
    let camera = load_camera(&state, id).await?;
    let mut transaction = state.pool.begin().await?;
    let now = Utc::now();
    sqlx::query(
        "UPDATE cameras SET deleted_at = ?, status = 'disabled', updated_at = ? \
         WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(now)
    .bind(now)
    .bind(id)
    .execute(&mut *transaction)
    .await?;
    let operation = reconciliation::queue_camera_change(
        &mut transaction,
        &camera,
        false,
        user.id,
        "camera_deleted",
    )
    .await?;
    transaction.commit().await?;
    write_audit(
        &state,
        Some(user.id),
        "camera.delete",
        "camera",
        Some(id),
        json!({ "name": camera.name }),
    )
    .await;
    Ok((StatusCode::ACCEPTED, Json(operation)))
}

async fn media_operation(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<reconciliation::MediaOperationView>> {
    Ok(Json(reconciliation::get_operation(&state.pool, &id).await?))
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
        onvif_url,
        camera.username.as_deref(),
        password.as_deref(),
        onvif::PtzCommand {
            action: &request.action,
            pan: values.0,
            tilt: values.1,
            zoom: values.2,
        },
        &state.config.onvif_xaddr_allowlist,
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
    let mut builder = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT id, camera_id, kind, severity, message, details, acknowledged_at, acknowledged_by, created_at FROM events WHERE 1 = 1",
    );
    if let Some(camera_id) = query.camera_id {
        builder.push(" AND camera_id = ").push_bind(camera_id);
    }
    if query.unacknowledged.unwrap_or(false) {
        builder.push(" AND acknowledged_at IS NULL");
    }
    builder
        .push(" ORDER BY created_at DESC LIMIT ")
        .push_bind(limit);
    let events = builder
        .build_query_as::<EventRecord>()
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
    let acknowledged_at = Utc::now();
    let result =
        sqlx::query("UPDATE events SET acknowledged_at = ?, acknowledged_by = ? WHERE id = ?")
            .bind(acknowledged_at)
            .bind(user.id)
            .bind(id)
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
        "SELECT id, user_id, action, entity_type, entity_id, details, created_at FROM audit_logs ORDER BY created_at DESC LIMIT ?",
    )
    .bind(query.limit.unwrap_or(100).clamp(1, 500))
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows))
}

async fn system_status(_user: CurrentUser, State(state): State<AppState>) -> Result<Json<Value>> {
    let (total, online, recording): (i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COUNT(*) FILTER (WHERE status = 'online'), \
         COUNT(*) FILTER (WHERE record_enabled AND enabled) \
         FROM cameras WHERE deleted_at IS NULL",
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
    let database = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.pool)
        .await
        .is_ok();
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
    sqlx::query_as::<_, CameraRecord>(&format!(
        "{CAMERA_SELECT} WHERE id = ? AND deleted_at IS NULL"
    ))
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| AppError::NotFound("摄像头不存在".into()))
}

async fn load_user(state: &AppState, id: Uuid) -> Result<UserRecord> {
    sqlx::query_as::<_, UserRecord>(&format!("{USER_SELECT} WHERE id = ?"))
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::NotFound("用户不存在".into()))
}

async fn ensure_another_admin(state: &AppState, excluded: Uuid) -> Result<()> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users WHERE role = 'admin' AND active AND id <> ?",
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
    onvif::validate_configured_url(value.trim())
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
    let now = Utc::now();
    if let Err(error) = sqlx::query(
        "INSERT INTO audit_logs (id, user_id, action, entity_type, entity_id, details, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(action)
    .bind(entity_type)
    .bind(entity_id)
    .bind(details)
    .bind(now)
    .execute(&state.pool)
    .await
    {
        tracing::warn!(%error, "audit log write failed");
    }
}
