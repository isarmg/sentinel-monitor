use crate::{
    background::camera_path,
    error::{AppError, Result},
    mediamtx::{source_digest, PathConfigSnapshot, PathSnapshot},
    models::CameraRecord,
    AppState,
};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::json;
use sqlx::{Sqlite, SqlitePool, Transaction};
use std::time::Duration as StdDuration;
use url::Url;
use uuid::Uuid;

const GLOBAL_LEASE_REQUEST_BUDGET: u64 = 6;
const OPERATION_LEASE_REQUEST_BUDGET: u64 = 4;
const LEASE_SAFETY_MARGIN_SECONDS: u64 = 30;

const CAMERA_SELECT_INTERNAL: &str = "SELECT id, name, location, main_stream_url_enc, \
    sub_stream_url_enc, onvif_url, username, password_enc, enabled, record_enabled, status, \
    last_seen_at, created_at, updated_at FROM cameras";
const OPERATION_SELECT: &str = "SELECT id, camera_id, generation, kind, state, reason, \
    requested_by, attempt, created_at, started_at, finished_at, retry_at, error_code, \
    error_message, lease_owner, lease_expires_at FROM media_operations";

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct MediaOperationView {
    pub id: String,
    pub camera_id: Uuid,
    pub generation: i64,
    pub kind: String,
    pub state: String,
    pub reason: String,
    pub requested_by: Option<Uuid>,
    pub attempt: i64,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub retry_at: Option<DateTime<Utc>>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    #[serde(skip_serializing)]
    pub lease_owner: Option<String>,
    #[serde(skip_serializing)]
    pub lease_expires_at: Option<DateTime<Utc>>,
}

#[derive(Clone, sqlx::FromRow)]
struct DesiredState {
    camera_id: Uuid,
    generation: i64,
    desired_present: bool,
    main_path: String,
    sub_path: Option<String>,
    record_enabled: bool,
}

pub async fn queue_camera_change(
    transaction: &mut Transaction<'_, Sqlite>,
    camera: &CameraRecord,
    desired_present: bool,
    requested_by: Uuid,
    reason: &'static str,
) -> Result<MediaOperationView> {
    let current_generation = sqlx::query_scalar::<_, i64>(
        "SELECT generation FROM media_desired_states WHERE camera_id = ?",
    )
    .bind(camera.id)
    .fetch_optional(&mut **transaction)
    .await?
    .unwrap_or(0);
    let generation = current_generation + 1;
    let now = Utc::now();
    let main_path = camera_path(camera.id, "main");
    let sub_path = camera
        .sub_stream_url_enc
        .as_ref()
        .map(|_| camera_path(camera.id, "sub"));

    sqlx::query(
        "INSERT INTO media_desired_states (camera_id, generation, desired_present, main_path, \
         sub_path, record_enabled, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(camera_id) DO UPDATE SET generation = excluded.generation, \
         desired_present = excluded.desired_present, main_path = excluded.main_path, \
         sub_path = excluded.sub_path, record_enabled = excluded.record_enabled, \
         updated_at = excluded.updated_at",
    )
    .bind(camera.id)
    .bind(generation)
    .bind(desired_present)
    .bind(&main_path)
    .bind(&sub_path)
    .bind(camera.record_enabled)
    .bind(now)
    .execute(&mut **transaction)
    .await?;

    sqlx::query(
        "UPDATE media_operations SET state = 'failed', finished_at = ?, retry_at = NULL, \
         error_code = 'superseded', error_message = 'A newer desired generation replaced this operation', \
         lease_owner = NULL, lease_expires_at = NULL \
         WHERE camera_id = ? AND generation < ? AND state = 'pending'",
    )
    .bind(now)
    .bind(camera.id)
    .bind(generation)
    .execute(&mut **transaction)
    .await?;

    let id = Uuid::new_v4().to_string();
    sqlx::query_as::<_, MediaOperationView>(&format!(
        "INSERT INTO media_operations (id, camera_id, generation, kind, state, reason, \
         requested_by, attempt, created_at, retry_at) \
         VALUES (?, ?, ?, 'reconcile_camera', 'pending', ?, ?, 0, ?, ?) \
         RETURNING {OPERATION_SELECT_FIELDS}",
        OPERATION_SELECT_FIELDS = operation_select_fields()
    ))
    .bind(&id)
    .bind(camera.id)
    .bind(generation)
    .bind(reason)
    .bind(requested_by)
    .bind(now)
    .bind(now)
    .fetch_one(&mut **transaction)
    .await
    .map_err(AppError::from)
}

pub async fn get_operation(pool: &SqlitePool, id: &str) -> Result<MediaOperationView> {
    sqlx::query_as::<_, MediaOperationView>(&format!("{OPERATION_SELECT} WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("媒体操作不存在".into()))
}

pub async fn recover_interrupted_operations(pool: &SqlitePool) -> Result<u64> {
    recover_expired_operation_leases(pool).await
}

async fn recover_expired_operation_leases(pool: &SqlitePool) -> Result<u64> {
    recover_expired_operation_leases_at(pool, Utc::now()).await
}

async fn recover_expired_operation_leases_at(pool: &SqlitePool, now: DateTime<Utc>) -> Result<u64> {
    let result = sqlx::query(
        "UPDATE media_operations SET state = 'unknown', finished_at = NULL, retry_at = ?, \
         lease_owner = NULL, lease_expires_at = NULL, error_code = 'worker_lease_expired', \
         error_message = 'The worker lease expired before the outcome was recorded' \
         WHERE state = 'running' AND (lease_expires_at IS NULL OR lease_expires_at <= ?)",
    )
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

pub async fn reconcile_once(state: &AppState) -> Result<bool> {
    let Some(lease_owner) = acquire_reconciler_lease(state).await? else {
        return Ok(false);
    };
    let result = async {
        recover_expired_operation_leases(&state.pool).await?;
        reconcile_once_with_lease(state, &lease_owner).await
    }
    .await;
    let release = release_reconciler_lease(&state.pool, &lease_owner).await;
    match (result, release) {
        (Ok(processed), Ok(_)) => Ok(processed),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

async fn reconcile_once_with_lease(state: &AppState, lease_owner: &str) -> Result<bool> {
    renew_global_lease(state, lease_owner).await?;
    if let Some(operation) =
        claim_next_operation(&state.pool, lease_owner, state.config.request_timeout).await?
    {
        apply_claimed_operation(state, operation).await?;
        return Ok(true);
    }

    observe_and_schedule_drift(state).await?;
    renew_global_lease(state, lease_owner).await?;
    if let Some(operation) =
        claim_next_operation(&state.pool, lease_owner, state.config.request_timeout).await?
    {
        apply_claimed_operation(state, operation).await?;
        return Ok(true);
    }
    Ok(false)
}

async fn acquire_reconciler_lease(state: &AppState) -> Result<Option<String>> {
    let now = Utc::now();
    let lease_expires_at = lease_deadline(
        now,
        state.config.request_timeout,
        GLOBAL_LEASE_REQUEST_BUDGET,
    );
    let owner = Uuid::new_v4().to_string();
    sqlx::query_scalar::<_, String>(
        "UPDATE media_reconciler_leases SET lease_owner = ?, lease_expires_at = ?, updated_at = ? \
         WHERE scope = 'global' AND (lease_owner IS NULL OR lease_expires_at IS NULL \
             OR lease_expires_at <= ?) \
         RETURNING lease_owner",
    )
    .bind(&owner)
    .bind(lease_expires_at)
    .bind(now)
    .bind(now)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::from)
}

async fn renew_global_lease(state: &AppState, owner: &str) -> Result<()> {
    let now = Utc::now();
    let deadline = lease_deadline(
        now,
        state.config.request_timeout,
        GLOBAL_LEASE_REQUEST_BUDGET,
    );
    let result = sqlx::query(
        "UPDATE media_reconciler_leases SET lease_expires_at = ?, updated_at = ? \
         WHERE scope = 'global' AND lease_owner = ? AND lease_expires_at > ?",
    )
    .bind(deadline)
    .bind(now)
    .bind(owner)
    .bind(now)
    .execute(&state.pool)
    .await?;
    if result.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "媒体协调器租约已由其他执行器接管".into(),
        ));
    }
    Ok(())
}

async fn release_reconciler_lease(pool: &SqlitePool, owner: &str) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE media_reconciler_leases SET lease_owner = NULL, lease_expires_at = NULL, \
         updated_at = ? WHERE scope = 'global' AND lease_owner = ?",
    )
    .bind(Utc::now())
    .bind(owner)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn reconcile_available(state: &AppState) -> Result<()> {
    for _ in 0..64 {
        if !reconcile_once(state).await? {
            break;
        }
    }
    Ok(())
}

async fn claim_next_operation(
    pool: &SqlitePool,
    global_owner: &str,
    request_timeout: StdDuration,
) -> Result<Option<MediaOperationView>> {
    let now = Utc::now();
    let lease_expires_at = lease_deadline(now, request_timeout, OPERATION_LEASE_REQUEST_BUDGET);
    let sql = format!(
        "UPDATE media_operations SET state = 'running', attempt = attempt + 1, started_at = ?, \
         finished_at = NULL, lease_owner = ?, lease_expires_at = ?, error_code = NULL, \
         error_message = NULL WHERE id = ( \
             SELECT candidate.id FROM media_operations candidate \
             WHERE (candidate.state = 'pending' OR (candidate.state IN ('failed', 'unknown') \
                    AND candidate.retry_at IS NOT NULL AND candidate.retry_at <= ?)) \
               AND NOT EXISTS (SELECT 1 FROM media_operations active \
                   WHERE active.camera_id = candidate.camera_id AND active.state = 'running') \
             ORDER BY candidate.created_at, candidate.attempt, candidate.id LIMIT 1 \
         ) AND (state = 'pending' OR (state IN ('failed', 'unknown') AND retry_at IS NOT NULL \
         AND retry_at <= ?)) AND EXISTS (SELECT 1 FROM media_reconciler_leases \
             WHERE scope = 'global' AND lease_owner = ? AND lease_expires_at > ?) \
         RETURNING {fields}",
        fields = operation_select_fields()
    );
    sqlx::query_as::<_, MediaOperationView>(&sql)
        .bind(now)
        .bind(global_owner)
        .bind(lease_expires_at)
        .bind(now)
        .bind(now)
        .bind(global_owner)
        .bind(now)
        .fetch_optional(pool)
        .await
        .map_err(AppError::from)
}

fn lease_deadline(
    now: DateTime<Utc>,
    request_timeout: StdDuration,
    request_budget: u64,
) -> DateTime<Utc> {
    let seconds = request_timeout
        .as_secs()
        .saturating_mul(request_budget)
        .saturating_add(LEASE_SAFETY_MARGIN_SECONDS)
        .max(60);
    let seconds = i64::try_from(seconds).unwrap_or(i64::MAX);
    now.checked_add_signed(Duration::seconds(seconds))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

async fn renew_claimed_leases(state: &AppState, operation: &MediaOperationView) -> Result<()> {
    let owner = operation_owner(operation)?;
    let now = Utc::now();
    let global_deadline = lease_deadline(
        now,
        state.config.request_timeout,
        GLOBAL_LEASE_REQUEST_BUDGET,
    );
    let operation_deadline = lease_deadline(
        now,
        state.config.request_timeout,
        OPERATION_LEASE_REQUEST_BUDGET,
    );
    let mut transaction = state.pool.begin().await?;
    let global = sqlx::query(
        "UPDATE media_reconciler_leases SET lease_expires_at = ?, updated_at = ? \
         WHERE scope = 'global' AND lease_owner = ? AND lease_expires_at > ?",
    )
    .bind(global_deadline)
    .bind(now)
    .bind(owner)
    .bind(now)
    .execute(&mut *transaction)
    .await?;
    let claimed = sqlx::query(
        "UPDATE media_operations SET lease_expires_at = ? \
         WHERE id = ? AND state = 'running' AND lease_owner = ? AND lease_expires_at > ?",
    )
    .bind(operation_deadline)
    .bind(&operation.id)
    .bind(owner)
    .bind(now)
    .execute(&mut *transaction)
    .await?;
    if global.rows_affected() != 1 || claimed.rows_affected() != 1 {
        return Err(AppError::Conflict("媒体操作租约已由其他执行器接管".into()));
    }
    transaction.commit().await?;
    Ok(())
}

fn operation_owner(operation: &MediaOperationView) -> Result<&str> {
    match (operation.lease_owner.as_deref(), operation.lease_expires_at) {
        (Some(owner), Some(_)) => Ok(owner),
        _ => Err(AppError::Conflict("媒体操作没有有效租约所有者".into())),
    }
}

async fn apply_claimed_operation(state: &AppState, operation: MediaOperationView) -> Result<()> {
    renew_claimed_leases(state, &operation).await?;
    let desired = load_desired(&state.pool, operation.camera_id).await?;
    if desired.generation != operation.generation {
        finish_superseded(&state.pool, &operation).await?;
        return Ok(());
    }
    let camera =
        sqlx::query_as::<_, CameraRecord>(&format!("{CAMERA_SELECT_INTERNAL} WHERE id = ?"))
            .bind(operation.camera_id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| AppError::Internal("media operation camera is missing".into()))?;

    let result = apply_desired(state, &camera, &desired).await;
    match result {
        Ok(applied) => finish_success(state, &operation, &desired, &applied).await,
        Err(error) => finish_failure(state, &operation, &error).await,
    }
}

struct AppliedSources {
    main_digest: Option<[u8; 32]>,
    sub_digest: Option<[u8; 32]>,
}

async fn apply_desired(
    state: &AppState,
    camera: &CameraRecord,
    desired: &DesiredState,
) -> Result<AppliedSources> {
    let standard_sub_path = camera_path(camera.id, "sub");
    if !desired.desired_present {
        state.media.delete_path(&desired.main_path).await?;
        state.media.delete_path(&standard_sub_path).await?;
        return Ok(AppliedSources {
            main_digest: None,
            sub_digest: None,
        });
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
            &desired.main_path,
            &main_source,
            !desired.record_enabled,
            desired.record_enabled,
        )
        .await?;

    let mut sub_digest = None;
    match (&desired.sub_path, &camera.sub_stream_url_enc) {
        (Some(sub_path), Some(encrypted)) => {
            let sub_url = state.secrets.decrypt(encrypted)?;
            let sub_source =
                source_with_credentials(&sub_url, camera.username.as_deref(), password.as_deref())?;
            state
                .media
                .upsert_path(sub_path, &sub_source, true, false)
                .await?;
            sub_digest = Some(source_digest(&sub_source));
        }
        _ => state.media.delete_path(&standard_sub_path).await?,
    }

    Ok(AppliedSources {
        main_digest: Some(source_digest(&main_source)),
        sub_digest,
    })
}

async fn finish_success(
    state: &AppState,
    operation: &MediaOperationView,
    desired: &DesiredState,
    applied: &AppliedSources,
) -> Result<()> {
    let owner = operation_owner(operation)?;
    let now = Utc::now();
    let mut transaction = state.pool.begin().await?;
    let current_generation = sqlx::query_scalar::<_, i64>(
        "SELECT generation FROM media_desired_states WHERE camera_id = ?",
    )
    .bind(desired.camera_id)
    .fetch_one(&mut *transaction)
    .await?;
    if current_generation != desired.generation {
        let result = sqlx::query(
            "UPDATE media_operations SET state = 'succeeded', finished_at = ?, retry_at = NULL, \
             lease_owner = NULL, lease_expires_at = NULL, result_json = ?, error_code = NULL, \
             error_message = NULL WHERE id = ? AND state = 'running' AND lease_owner = ? \
             AND lease_expires_at > ? AND EXISTS (SELECT 1 FROM media_reconciler_leases \
                 WHERE scope = 'global' AND lease_owner = ? AND lease_expires_at > ?)",
        )
        .bind(now)
        .bind(json!({ "converged": false, "superseded_after_apply": true }))
        .bind(&operation.id)
        .bind(owner)
        .bind(now)
        .bind(owner)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() != 1 {
            return Err(AppError::Conflict("媒体操作租约已由其他执行器接管".into()));
        }
        transaction.commit().await?;
        return Ok(());
    }
    let result = sqlx::query(
        "UPDATE media_operations SET state = 'succeeded', finished_at = ?, retry_at = NULL, \
         lease_owner = NULL, lease_expires_at = NULL, result_json = ?, error_code = NULL, \
         error_message = NULL WHERE id = ? AND state = 'running' AND lease_owner = ? \
         AND lease_expires_at > ? AND EXISTS (SELECT 1 FROM media_reconciler_leases \
             WHERE scope = 'global' AND lease_owner = ? AND lease_expires_at > ?)",
    )
    .bind(now)
    .bind(json!({ "generation": desired.generation, "converged": true }))
    .bind(&operation.id)
    .bind(owner)
    .bind(now)
    .bind(owner)
    .bind(now)
    .execute(&mut *transaction)
    .await?;
    if result.rows_affected() != 1 {
        return Err(AppError::Conflict("媒体操作状态已被其他执行器修改".into()));
    }

    persist_applied_path(
        &mut transaction,
        desired,
        "main",
        &desired.main_path,
        desired.desired_present,
        applied.main_digest,
        !desired.record_enabled,
        desired.record_enabled,
        &operation.id,
        now,
    )
    .await?;
    let sub_path = camera_path(desired.camera_id, "sub");
    let sub_present = desired.desired_present && desired.sub_path.is_some();
    persist_applied_path(
        &mut transaction,
        desired,
        "sub",
        &sub_path,
        sub_present,
        applied.sub_digest,
        true,
        false,
        &operation.id,
        now,
    )
    .await?;
    sqlx::query(
        "UPDATE cameras SET status = CASE WHEN deleted_at IS NOT NULL OR enabled = 0 \
         THEN 'disabled' ELSE 'pending' END, updated_at = ? WHERE id = ?",
    )
    .bind(now)
    .bind(desired.camera_id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn persist_applied_path(
    transaction: &mut Transaction<'_, Sqlite>,
    desired: &DesiredState,
    profile: &str,
    path: &str,
    present: bool,
    digest: Option<[u8; 32]>,
    source_on_demand: bool,
    record: bool,
    operation_id: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO media_actual_paths (path_name, camera_id, profile, present, ready, \
         publisher_active, recording_active, source_digest, source_on_demand, record_configured, \
         applied_generation, last_operation_id, observed_at) \
         VALUES (?, ?, ?, ?, 0, 0, 0, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(path_name) DO UPDATE SET camera_id = excluded.camera_id, \
         profile = excluded.profile, present = excluded.present, ready = excluded.ready, \
         publisher_active = excluded.publisher_active, recording_active = excluded.recording_active, \
         source_digest = excluded.source_digest, source_on_demand = excluded.source_on_demand, \
         record_configured = excluded.record_configured, \
         applied_generation = excluded.applied_generation, \
         last_operation_id = excluded.last_operation_id, observed_at = excluded.observed_at",
    )
    .bind(path)
    .bind(desired.camera_id)
    .bind(profile)
    .bind(present)
    .bind(digest.map(Vec::from))
    .bind(source_on_demand)
    .bind(record)
    .bind(desired.generation)
    .bind(operation_id)
    .bind(now)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn finish_failure(
    state: &AppState,
    operation: &MediaOperationView,
    error: &AppError,
) -> Result<()> {
    let owner = operation_owner(operation)?;
    let (operation_state, error_code, error_message, retryable) = sanitized_failure(error);
    let retry_at = retryable.then(|| Utc::now() + retry_delay(operation.attempt));
    let now = Utc::now();
    let finished_at = (operation_state != "unknown").then_some(now);
    let mut transaction = state.pool.begin().await?;
    let result = sqlx::query(
        "UPDATE media_operations SET state = ?, finished_at = ?, retry_at = ?, \
         lease_owner = NULL, lease_expires_at = NULL, error_code = ?, error_message = ? \
         WHERE id = ? AND state = 'running' AND lease_owner = ? AND lease_expires_at > ? \
         AND EXISTS (SELECT 1 FROM media_reconciler_leases WHERE scope = 'global' \
             AND lease_owner = ? AND lease_expires_at > ?)",
    )
    .bind(operation_state)
    .bind(finished_at)
    .bind(retry_at)
    .bind(error_code)
    .bind(error_message)
    .bind(&operation.id)
    .bind(owner)
    .bind(now)
    .bind(owner)
    .bind(now)
    .execute(&mut *transaction)
    .await?;
    if result.rows_affected() != 1 {
        return Err(AppError::Conflict("媒体操作租约已由其他执行器接管".into()));
    }
    sqlx::query("UPDATE cameras SET status = 'error', updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(operation.camera_id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    tracing::warn!(
        operation_id = %operation.id,
        camera_id = %operation.camera_id,
        error_code,
        "media reconciliation attempt did not converge"
    );
    Ok(())
}

fn sanitized_failure(error: &AppError) -> (&'static str, &'static str, &'static str, bool) {
    match error {
        AppError::UpstreamUnknown(_) => (
            "unknown",
            "media_outcome_unknown",
            "The media service outcome could not be determined",
            true,
        ),
        AppError::Upstream(_) => (
            "failed",
            "media_request_failed",
            "The media service rejected or could not process the desired state",
            true,
        ),
        AppError::Validation(_) => (
            "failed",
            "invalid_stored_camera_configuration",
            "The stored camera configuration is invalid",
            false,
        ),
        _ => (
            "failed",
            "media_reconciliation_internal",
            "The desired media state could not be prepared",
            false,
        ),
    }
}

fn retry_delay(attempt: i64) -> Duration {
    let exponent = u32::try_from(attempt.clamp(0, 8)).unwrap_or(8);
    Duration::seconds((1_i64 << exponent).min(300))
}

async fn finish_superseded(pool: &SqlitePool, operation: &MediaOperationView) -> Result<()> {
    let owner = operation_owner(operation)?;
    let now = Utc::now();
    let result = sqlx::query(
        "UPDATE media_operations SET state = 'succeeded', finished_at = ?, retry_at = NULL, \
         lease_owner = NULL, lease_expires_at = NULL, result_json = ?, error_code = NULL, \
         error_message = NULL WHERE id = ? AND state = 'running' AND lease_owner = ? \
         AND lease_expires_at > ? AND EXISTS (SELECT 1 FROM media_reconciler_leases \
             WHERE scope = 'global' AND lease_owner = ? AND lease_expires_at > ?)",
    )
    .bind(now)
    .bind(json!({ "converged": false, "superseded": true }))
    .bind(&operation.id)
    .bind(owner)
    .bind(now)
    .bind(owner)
    .bind(now)
    .execute(pool)
    .await?;
    if result.rows_affected() != 1 {
        return Err(AppError::Conflict("媒体操作租约已由其他执行器接管".into()));
    }
    Ok(())
}

async fn load_desired(pool: &SqlitePool, camera_id: Uuid) -> Result<DesiredState> {
    sqlx::query_as::<_, DesiredState>(
        "SELECT camera_id, generation, desired_present, main_path, sub_path, record_enabled \
         FROM media_desired_states WHERE camera_id = ?",
    )
    .bind(camera_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::Internal("media desired state is missing".into()))
}

async fn observe_and_schedule_drift(state: &AppState) -> Result<()> {
    let observation_started_at = Utc::now();
    let configs = state.media.path_configs().await?;
    let runtime = state.media.paths().await?;
    let desired_states = sqlx::query_as::<_, DesiredState>(
        "SELECT camera_id, generation, desired_present, main_path, sub_path, record_enabled \
         FROM media_desired_states ORDER BY camera_id",
    )
    .fetch_all(&state.pool)
    .await?;

    for desired in desired_states {
        let camera =
            sqlx::query_as::<_, CameraRecord>(&format!("{CAMERA_SELECT_INTERNAL} WHERE id = ?"))
                .bind(desired.camera_id)
                .fetch_one(&state.pool)
                .await?;
        let expected = expected_configs(state, &camera, &desired)?;
        let main_matches = config_matches(
            configs.get(&desired.main_path),
            expected.main.as_ref(),
            desired.desired_present,
        );
        let sub_path = camera_path(desired.camera_id, "sub");
        let sub_should_exist = desired.desired_present && desired.sub_path.is_some();
        let sub_matches = config_matches(
            configs.get(&sub_path),
            expected.sub.as_ref(),
            sub_should_exist,
        );

        persist_observed_path(
            &state.pool,
            &desired,
            "main",
            &desired.main_path,
            configs.get(&desired.main_path),
            runtime.get(&desired.main_path),
            main_matches.then_some(desired.generation),
            observation_started_at,
        )
        .await?;
        persist_observed_path(
            &state.pool,
            &desired,
            "sub",
            &sub_path,
            configs.get(&sub_path),
            runtime.get(&sub_path),
            sub_matches.then_some(desired.generation),
            observation_started_at,
        )
        .await?;

        if !main_matches || !sub_matches {
            ensure_drift_operation(&state.pool, &desired, observation_started_at).await?;
        }
    }
    Ok(())
}

struct ExpectedConfigs {
    main: Option<PathConfigSnapshot>,
    sub: Option<PathConfigSnapshot>,
}

fn expected_configs(
    state: &AppState,
    camera: &CameraRecord,
    desired: &DesiredState,
) -> Result<ExpectedConfigs> {
    if !desired.desired_present {
        return Ok(ExpectedConfigs {
            main: None,
            sub: None,
        });
    }
    let password = camera
        .password_enc
        .as_deref()
        .map(|value| state.secrets.decrypt(value))
        .transpose()?;
    let main_url = state.secrets.decrypt(&camera.main_stream_url_enc)?;
    let main_source =
        source_with_credentials(&main_url, camera.username.as_deref(), password.as_deref())?;
    let main = Some(PathConfigSnapshot {
        source_digest: Some(source_digest(&main_source)),
        source_on_demand: !desired.record_enabled,
        record: desired.record_enabled,
    });
    let sub = match (&desired.sub_path, &camera.sub_stream_url_enc) {
        (Some(_), Some(encrypted)) => {
            let source = source_with_credentials(
                &state.secrets.decrypt(encrypted)?,
                camera.username.as_deref(),
                password.as_deref(),
            )?;
            Some(PathConfigSnapshot {
                source_digest: Some(source_digest(&source)),
                source_on_demand: true,
                record: false,
            })
        }
        _ => None,
    };
    Ok(ExpectedConfigs { main, sub })
}

fn config_matches(
    actual: Option<&PathConfigSnapshot>,
    expected: Option<&PathConfigSnapshot>,
    should_exist: bool,
) -> bool {
    match (should_exist, actual, expected) {
        (false, None, _) => true,
        (true, Some(actual), Some(expected)) => {
            actual.source_digest == expected.source_digest
                && actual.source_on_demand == expected.source_on_demand
                && actual.record == expected.record
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
async fn persist_observed_path(
    pool: &SqlitePool,
    desired: &DesiredState,
    profile: &str,
    path: &str,
    config: Option<&PathConfigSnapshot>,
    runtime: Option<&PathSnapshot>,
    applied_generation: Option<i64>,
    observed_at: DateTime<Utc>,
) -> Result<()> {
    let ready = runtime.map(|value| value.ready).unwrap_or(false);
    let record = config.map(|value| value.record).unwrap_or(false);
    sqlx::query(
        "INSERT INTO media_actual_paths (path_name, camera_id, profile, present, ready, \
         publisher_active, recording_active, source_digest, source_on_demand, record_configured, \
         applied_generation, observed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(path_name) DO UPDATE SET camera_id = excluded.camera_id, \
         profile = excluded.profile, present = excluded.present, ready = excluded.ready, \
         publisher_active = excluded.publisher_active, recording_active = excluded.recording_active, \
         source_digest = excluded.source_digest, source_on_demand = excluded.source_on_demand, \
         record_configured = excluded.record_configured, \
         applied_generation = excluded.applied_generation, observed_at = excluded.observed_at \
         WHERE media_actual_paths.observed_at <= excluded.observed_at",
    )
    .bind(path)
    .bind(desired.camera_id)
    .bind(profile)
    .bind(config.is_some())
    .bind(ready)
    .bind(ready)
    .bind(ready && record)
    .bind(config.and_then(|value| value.source_digest).map(Vec::from))
    .bind(config.map(|value| value.source_on_demand))
    .bind(config.map(|value| value.record))
    .bind(applied_generation)
    .bind(observed_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn ensure_drift_operation(
    pool: &SqlitePool,
    desired: &DesiredState,
    observation_started_at: DateTime<Utc>,
) -> Result<()> {
    let now = Utc::now();
    sqlx::query(
        "INSERT OR IGNORE INTO media_operations (id, camera_id, generation, kind, state, \
         reason, attempt, created_at, retry_at) \
         SELECT ?, ?, ?, 'reconcile_camera', 'pending', 'drift_detected', 0, ?, ? \
         WHERE NOT EXISTS (SELECT 1 FROM media_operations WHERE camera_id = ? AND generation = ? \
             AND (state IN ('pending', 'running', 'failed', 'unknown') \
                  OR (state = 'succeeded' AND finished_at >= ?)))",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(desired.camera_id)
    .bind(desired.generation)
    .bind(now)
    .bind(now)
    .bind(desired.camera_id)
    .bind(desired.generation)
    .bind(observation_started_at)
    .execute(pool)
    .await?;
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

const fn operation_select_fields() -> &'static str {
    "id, camera_id, generation, kind, state, reason, requested_by, attempt, created_at, \
     started_at, finished_at, retry_at, error_code, error_message, lease_owner, lease_expires_at"
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn lease_test_database() -> (tempfile::TempDir, SqlitePool, String, String) {
        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("leases.sqlite3");
        let database_url = format!("sqlite://{}", database.display());
        let pool = crate::sqlite::open_pool(&database_url).await.unwrap();
        let now = Utc::now();
        let user = Uuid::new_v4();
        let camera = Uuid::new_v4();
        let operation = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO users (id, email, password_hash, role, created_at, updated_at) \
             VALUES (?, 'lease@example.test', 'hash', 'admin', ?, ?)",
        )
        .bind(user)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO cameras (id, name, main_stream_url_enc, created_by, created_at, updated_at) \
             VALUES (?, 'Lease Camera', ?, ?, ?, ?)",
        )
        .bind(camera)
        .bind(vec![1u8; 32])
        .bind(user)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO media_desired_states (camera_id, generation, desired_present, \
             main_path, record_enabled, updated_at) VALUES (?, 1, 1, 'lease_main', 0, ?)",
        )
        .bind(camera)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO media_operations (id, camera_id, generation, kind, state, reason, \
             attempt, created_at, retry_at) VALUES (?, ?, 1, 'reconcile_camera', 'pending', \
             'drift_detected', 0, ?, ?)",
        )
        .bind(&operation)
        .bind(camera)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        (temporary, pool, operation, database_url)
    }

    #[test]
    fn retry_is_exponential_and_bounded() {
        assert_eq!(retry_delay(0), Duration::seconds(1));
        assert_eq!(retry_delay(1), Duration::seconds(2));
        assert_eq!(retry_delay(8), Duration::seconds(256));
        assert_eq!(retry_delay(100), Duration::seconds(256));
    }

    #[test]
    fn lease_deadlines_cover_the_declared_external_request_budget() {
        let now = Utc::now();
        let timeout = StdDuration::from_secs(20);
        assert_eq!(
            lease_deadline(now, timeout, OPERATION_LEASE_REQUEST_BUDGET) - now,
            Duration::seconds(110)
        );
        assert_eq!(
            lease_deadline(now, timeout, GLOBAL_LEASE_REQUEST_BUDGET) - now,
            Duration::seconds(150)
        );
    }

    #[test]
    fn persisted_failures_never_include_upstream_or_camera_details() {
        let secret = "rtsp://admin:super-secret@camera.invalid/live";
        let error = AppError::Upstream(format!("rejected payload containing {secret}"));
        let (_, code, message, _) = sanitized_failure(&error);
        assert_eq!(code, "media_request_failed");
        assert!(!message.contains("super-secret"));
        assert!(!message.contains("camera.invalid"));
    }

    #[tokio::test]
    async fn expired_operation_is_fenced_and_a_new_owner_can_take_over() {
        let (_temporary, pool, operation_id, _database_url) = lease_test_database().await;
        let active_until = Utc::now() + Duration::minutes(5);
        sqlx::query(
            "UPDATE media_operations SET state = 'running', attempt = 1, started_at = ?, \
             lease_owner = 'old-owner', lease_expires_at = ? WHERE id = ?",
        )
        .bind(Utc::now())
        .bind(active_until)
        .bind(&operation_id)
        .execute(&pool)
        .await
        .unwrap();
        let new_owner = "new-owner";
        sqlx::query(
            "UPDATE media_reconciler_leases SET lease_owner = ?, lease_expires_at = ?, \
             updated_at = ? WHERE scope = 'global'",
        )
        .bind(new_owner)
        .bind(active_until)
        .bind(Utc::now())
        .execute(&pool)
        .await
        .unwrap();
        let old_operation =
            sqlx::query_as::<_, MediaOperationView>(&format!("{OPERATION_SELECT} WHERE id = ?"))
                .bind(&operation_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(finish_superseded(&pool, &old_operation).await.is_err());
        let fenced_state: String =
            sqlx::query_scalar("SELECT state FROM media_operations WHERE id = ?")
                .bind(&operation_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(fenced_state, "running");

        let expired = Utc::now() - Duration::seconds(1);
        sqlx::query("UPDATE media_operations SET lease_expires_at = ? WHERE id = ?")
            .bind(expired)
            .bind(&operation_id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            recover_expired_operation_leases_at(&pool, Utc::now())
                .await
                .unwrap(),
            1
        );
        let claimed = claim_next_operation(&pool, new_owner, StdDuration::from_secs(1))
            .await
            .unwrap()
            .expect("new owner claims expired operation");
        assert_eq!(claimed.lease_owner.as_deref(), Some(new_owner));
        assert_eq!(claimed.state, "running");

        let mut stale = claimed.clone();
        stale.lease_owner = Some("old-owner".into());
        stale.lease_expires_at = Some(active_until);
        assert!(finish_superseded(&pool, &stale).await.is_err());
        let still_owned: (String, Option<String>) =
            sqlx::query_as("SELECT state, lease_owner FROM media_operations WHERE id = ?")
                .bind(&operation_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(still_owned, ("running".into(), Some(new_owner.into())));

        assert!(!release_reconciler_lease(&pool, "old-owner").await.unwrap());
        let global_owner: Option<String> = sqlx::query_scalar(
            "SELECT lease_owner FROM media_reconciler_leases WHERE scope = 'global'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(global_owner.as_deref(), Some(new_owner));

        finish_superseded(&pool, &claimed).await.unwrap();
        let state: String = sqlx::query_scalar("SELECT state FROM media_operations WHERE id = ?")
            .bind(&operation_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state, "succeeded");
        assert!(release_reconciler_lease(&pool, new_owner).await.unwrap());
        pool.close().await;
    }

    #[tokio::test]
    async fn startup_recovery_from_another_pool_preserves_active_leases() {
        let (_temporary, first, operation_id, database_url) = lease_test_database().await;
        let active_until = Utc::now() + Duration::minutes(5);
        sqlx::query(
            "UPDATE media_reconciler_leases SET lease_owner = 'healthy-owner', \
             lease_expires_at = ?, updated_at = ? WHERE scope = 'global'",
        )
        .bind(active_until)
        .bind(Utc::now())
        .execute(&first)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE media_operations SET state = 'running', started_at = ?, attempt = 1, \
             lease_owner = 'healthy-owner', lease_expires_at = ? WHERE id = ?",
        )
        .bind(Utc::now())
        .bind(active_until)
        .bind(&operation_id)
        .execute(&first)
        .await
        .unwrap();

        let second = crate::sqlite::open_pool(&database_url).await.unwrap();
        assert_eq!(recover_interrupted_operations(&second).await.unwrap(), 0);
        let operation: (String, Option<String>) =
            sqlx::query_as("SELECT state, lease_owner FROM media_operations WHERE id = ?")
                .bind(&operation_id)
                .fetch_one(&first)
                .await
                .unwrap();
        assert_eq!(operation, ("running".into(), Some("healthy-owner".into())));
        let global: Option<String> = sqlx::query_scalar(
            "SELECT lease_owner FROM media_reconciler_leases WHERE scope = 'global'",
        )
        .fetch_one(&first)
        .await
        .unwrap();
        assert_eq!(global.as_deref(), Some("healthy-owner"));
        second.close().await;
        first.close().await;
    }
}
