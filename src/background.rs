use crate::{
    error::{AppError, Result},
    models::{CameraRecord, EventRecord},
    AppState,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::time::{self, MissedTickBehavior};
use url::Url;
use uuid::Uuid;

const CAMERA_SELECT: &str = "SELECT id, name, location, main_stream_url_enc, sub_stream_url_enc, \
    onvif_url, username, password_enc, enabled, record_enabled, status, last_seen_at, created_at, updated_at \
    FROM cameras";

pub fn spawn(state: AppState) {
    let reconcile_state = state.clone();
    tokio::spawn(async move {
        let mut interval = time::interval(reconcile_state.config.reconcile_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(error) = reconcile_all(&reconcile_state).await {
                tracing::warn!(%error, "camera reconciliation failed");
            }
        }
    });

    tokio::spawn(async move {
        time::sleep(std::time::Duration::from_secs(2)).await;
        let mut interval = time::interval(state.config.status_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(error) = refresh_statuses(&state).await {
                tracing::warn!(%error, "camera status refresh failed");
            }
        }
    });
}

pub fn camera_path(id: Uuid, profile: &str) -> String {
    format!("cam_{}_{}", id.simple(), profile)
}

pub async fn sync_camera(state: &AppState, camera: &CameraRecord) -> Result<()> {
    let main_path = camera_path(camera.id, "main");
    let sub_path = camera_path(camera.id, "sub");
    if !camera.enabled {
        state.media.delete_path(&main_path).await?;
        state.media.delete_path(&sub_path).await?;
        return Ok(());
    }

    let password = camera
        .password_enc
        .as_deref()
        .map(|value| state.secrets.decrypt(value))
        .transpose()?;
    let main_url = state.secrets.decrypt(&camera.main_stream_url_enc)?;
    let main_source =
        source_with_credentials(&main_url, camera.username.as_deref(), password.as_deref())?;
    state
        .media
        .upsert_path(
            &main_path,
            &main_source,
            !camera.record_enabled,
            camera.record_enabled,
        )
        .await?;

    if let Some(encrypted) = &camera.sub_stream_url_enc {
        let sub_url = state.secrets.decrypt(encrypted)?;
        let sub_source =
            source_with_credentials(&sub_url, camera.username.as_deref(), password.as_deref())?;
        state
            .media
            .upsert_path(&sub_path, &sub_source, true, false)
            .await?;
    } else {
        state.media.delete_path(&sub_path).await?;
    }
    Ok(())
}

pub async fn emit_event(
    state: &AppState,
    camera_id: Option<Uuid>,
    kind: &str,
    severity: &str,
    message: &str,
    details: Value,
) -> Result<EventRecord> {
    let event = sqlx::query_as::<_, EventRecord>(
        "INSERT INTO events (id, camera_id, kind, severity, message, details) \
         VALUES (?, ?, ?, ?, ?, ?) \
         RETURNING id, camera_id, kind, severity, message, details, acknowledged_at, acknowledged_by, created_at",
    )
    .bind(Uuid::new_v4())
    .bind(camera_id)
    .bind(kind)
    .bind(severity)
    .bind(message)
    .bind(details)
    .fetch_one(&state.pool)
    .await?;
    let _ = state.events.send(event.clone());
    Ok(event)
}

async fn reconcile_all(state: &AppState) -> Result<()> {
    let cameras = sqlx::query_as::<_, CameraRecord>(CAMERA_SELECT)
        .fetch_all(&state.pool)
        .await?;
    for camera in cameras {
        if let Err(error) = sync_camera(state, &camera).await {
            tracing::warn!(camera_id = %camera.id, %error, "camera media sync failed");
            sqlx::query(
                "UPDATE cameras SET status = 'error', updated_at = datetime('now') WHERE id = ?",
            )
            .bind(camera.id)
            .execute(&state.pool)
            .await?;
        }
    }
    Ok(())
}

async fn refresh_statuses(state: &AppState) -> Result<()> {
    let paths = state.media.paths().await?;
    let cameras = sqlx::query_as::<_, CameraRecord>(CAMERA_SELECT)
        .fetch_all(&state.pool)
        .await?;
    for camera in cameras {
        let snapshot = paths.get(&camera_path(camera.id, "main"));
        let new_status = if !camera.enabled {
            "disabled"
        } else if snapshot.map(|path| path.ready).unwrap_or(false) {
            "online"
        } else {
            "offline"
        };
        if new_status == camera.status {
            if new_status == "online" {
                sqlx::query("UPDATE cameras SET last_seen_at = datetime('now') WHERE id = ?")
                    .bind(camera.id)
                    .execute(&state.pool)
                    .await?;
            }
            continue;
        }

        sqlx::query(
            "UPDATE cameras SET status = ?, last_seen_at = CASE WHEN ? = 'online' THEN datetime('now') ELSE last_seen_at END, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(new_status)
        .bind(new_status)
        .bind(camera.id)
        .execute(&state.pool)
        .await?;

        if camera.status != "pending" && new_status != "disabled" {
            let (severity, message) = if new_status == "online" {
                ("info", format!("{} 已恢复在线", camera.name))
            } else {
                ("warning", format!("{} 已离线", camera.name))
            };
            emit_event(
                state,
                Some(camera.id),
                &format!("camera.{new_status}"),
                severity,
                &message,
                json!({
                    "previous_status": camera.status,
                    "readers": snapshot.map(|path| path.readers).unwrap_or(0),
                    "tracks": snapshot.map(|path| path.tracks).unwrap_or(0)
                }),
            )
            .await?;
        }
    }
    Ok(())
}

fn source_with_credentials(
    source: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<String> {
    let mut url =
        Url::parse(source).map_err(|_| AppError::Validation("RTSP地址格式无效".into()))?;
    if !matches!(url.scheme(), "rtsp" | "rtsps") {
        return Err(AppError::Validation(
            "流地址必须使用rtsp://或rtsps://".into(),
        ));
    }
    if url.username().is_empty() {
        if let Some(username) = username.filter(|value| !value.is_empty()) {
            url.set_username(username)
                .map_err(|_| AppError::Validation("摄像头用户名无效".into()))?;
        }
    }
    if url.password().is_none() {
        if let Some(password) = password {
            url.set_password(Some(password))
                .map_err(|_| AppError::Validation("摄像头密码无效".into()))?;
        }
    }
    Ok(url.to_string())
}
