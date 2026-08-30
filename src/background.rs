use crate::{
    error::Result,
    models::{CameraRecord, EventRecord},
    reconciliation, AppState,
};
use serde_json::{json, Value};
use tokio::time::{self, MissedTickBehavior};
use uuid::Uuid;

const CAMERA_SELECT: &str = "SELECT id, name, location, main_stream_url_enc, sub_stream_url_enc, \
    onvif_url, username, password_enc, enabled, record_enabled, status, last_seen_at, created_at, updated_at \
    FROM cameras WHERE deleted_at IS NULL";

pub fn spawn(state: AppState) {
    let reconcile_state = state.clone();
    tokio::spawn(async move {
        let mut interval = time::interval(reconcile_state.config.reconcile_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(error) = reconciliation::reconcile_available(&reconcile_state).await {
                tracing::warn!(error = %error, "camera reconciliation cycle failed");
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

pub async fn emit_event(
    state: &AppState,
    camera_id: Option<Uuid>,
    kind: &str,
    severity: &str,
    message: &str,
    details: Value,
) -> Result<EventRecord> {
    let now = chrono::Utc::now();
    let event = sqlx::query_as::<_, EventRecord>(
        "INSERT INTO events (id, camera_id, kind, severity, message, details, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?) \
         RETURNING id, camera_id, kind, severity, message, details, acknowledged_at, acknowledged_by, created_at",
    )
    .bind(Uuid::new_v4())
    .bind(camera_id)
    .bind(kind)
    .bind(severity)
    .bind(message)
    .bind(details)
    .bind(now)
    .fetch_one(&state.pool)
    .await?;
    let _ = state.events.send(event.clone());
    Ok(event)
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
