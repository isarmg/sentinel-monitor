use crate::{
    background::camera_path,
    error::{AppError, Result},
    mediamtx::{source_digest, PathConfigSnapshot, PathSnapshot},
    models::CameraRecord,
    sqlite::{self, GlobalLeaseState},
    AppState,
};
use chrono::{DateTime, Duration, Utc};
use sarmg_operations::{
    EnqueueOutcome, NewOperation, OperationState, SqliteOperationStore, StoredOperation, Transition,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Executor, Sqlite, SqlitePool, Transaction};
use std::time::Duration as StdDuration;
use url::Url;
use uuid::Uuid;

const GLOBAL_LEASE_REQUEST_BUDGET: u64 = 6;
const OPERATION_LEASE_REQUEST_BUDGET: u64 = 4;
const LEASE_SAFETY_MARGIN_SECONDS: u64 = 30;
const OPERATION_NAMESPACE: &str = "sentinel.media-reconciliation";

const CAMERA_SELECT_INTERNAL: &str = "SELECT id, name, location, main_stream_url_enc, \
    sub_stream_url_enc, onvif_url, username_enc, password_enc, enabled, record_enabled, status, \
    last_seen_at, created_at, updated_at FROM cameras";
#[derive(Clone, Debug, Serialize)]
pub struct MediaOperationView {
    pub id: String,
    pub camera_id: Uuid,
    pub generation: i64,
    pub kind: String,
    pub state: String,
    pub reason: String,
    pub requested_by: Option<String>,
    pub attempt: i64,
    pub max_attempts: i64,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MediaOperationRequest {
    camera_id: Uuid,
    generation: i64,
    reason: String,
    requested_by: Option<String>,
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

#[derive(sqlx::FromRow)]
struct GlobalLeaseRaw {
    table_sql: String,
    singleton_storage: String,
    singleton: i64,
    owner_storage: String,
    lease_owner: Option<String>,
    expiry_storage: String,
    lease_expires_at: Option<String>,
    updated_storage: String,
    updated_at: String,
}

pub async fn queue_camera_change(
    transaction: &mut Transaction<'_, Sqlite>,
    camera: &CameraRecord,
    desired_present: bool,
    requested_by: &str,
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

    enqueue_operation_in(
        transaction,
        MediaOperationRequest {
            camera_id: camera.id,
            generation,
            reason: reason.to_owned(),
            requested_by: Some(requested_by.to_owned()),
        },
        now,
    )
    .await
}

pub async fn get_operation(pool: &SqlitePool, id: &str) -> Result<MediaOperationView> {
    SqliteOperationStore::new(pool.clone())
        .get(id)
        .await
        .map_err(operation_error)?
        .map(operation_view)
        .transpose()?
        .ok_or_else(|| AppError::NotFound("媒体操作不存在".into()))
}

pub async fn recover_interrupted_operations(pool: &SqlitePool) -> Result<u64> {
    SqliteOperationStore::new(pool.clone())
        .recover_running(
            OPERATION_NAMESPACE,
            "worker_interrupted",
            Utc::now().timestamp_micros(),
        )
        .await
        .map_err(operation_error)
}

pub async fn reconcile_once(state: &AppState) -> Result<bool> {
    load_global_lease_state(&state.pool).await?;
    validate_stored_camera_credentials(state).await?;
    let Some(lease_owner) = acquire_reconciler_lease(state).await? else {
        return Ok(false);
    };
    let result = async { reconcile_once_with_lease(state, &lease_owner).await }.await;
    let release = release_reconciler_lease(&state.pool, &lease_owner).await;
    match (result, release) {
        (Ok(processed), Ok(_)) => Ok(processed),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

pub async fn validate_stored_camera_credentials(state: &AppState) -> Result<()> {
    let cameras = sqlx::query_as::<_, CameraRecord>(CAMERA_SELECT_INTERNAL)
        .fetch_all(&state.pool)
        .await?;
    for camera in cameras {
        camera.decrypt_credentials(&state.secrets)?;
    }
    Ok(())
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
    let mut transaction = state.pool.begin_with("BEGIN IMMEDIATE").await?;
    let current = load_global_lease_state(&mut *transaction).await?;
    let now = Utc::now();
    if current
        .lease_expires_at
        .is_some_and(|expires_at| expires_at > now)
    {
        transaction.commit().await?;
        return Ok(None);
    }
    let lease_expires_at = lease_deadline(
        now,
        state.config.request_timeout,
        GLOBAL_LEASE_REQUEST_BUDGET,
    );
    let owner = Uuid::new_v4().to_string();
    let result = sqlx::query(
        "UPDATE media_reconciler_leases SET lease_owner = ?, lease_expires_at = ?, updated_at = ? \
         WHERE singleton = 1",
    )
    .bind(&owner)
    .bind(lease_expires_at)
    .bind(now)
    .execute(&mut *transaction)
    .await?;
    if result.rows_affected() != 1 {
        return Err(corrupt_global_lease());
    }
    transaction.commit().await?;
    Ok(Some(owner))
}

async fn renew_global_lease(state: &AppState, owner: &str) -> Result<()> {
    let mut transaction = state.pool.begin_with("BEGIN IMMEDIATE").await?;
    let current = load_global_lease_state(&mut *transaction).await?;
    let now = Utc::now();
    if current.owner.as_deref() != Some(owner)
        || current
            .lease_expires_at
            .is_none_or(|expires_at| expires_at <= now)
    {
        return Err(AppError::Conflict(
            "媒体协调器租约已由其他执行器接管".into(),
        ));
    }
    let deadline = lease_deadline(
        now,
        state.config.request_timeout,
        GLOBAL_LEASE_REQUEST_BUDGET,
    );
    let result = sqlx::query(
        "UPDATE media_reconciler_leases SET lease_expires_at = ?, updated_at = ? \
         WHERE singleton = 1 AND lease_owner = ? AND lease_expires_at > ?",
    )
    .bind(deadline)
    .bind(now)
    .bind(owner)
    .bind(now)
    .execute(&mut *transaction)
    .await?;
    if result.rows_affected() != 1 {
        return Err(corrupt_global_lease());
    }
    transaction.commit().await?;
    Ok(())
}

async fn release_reconciler_lease(pool: &SqlitePool, owner: &str) -> Result<bool> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let current = load_global_lease_state(&mut *transaction).await?;
    if current.owner.as_deref() != Some(owner) {
        transaction.commit().await?;
        return Ok(false);
    }
    let result = sqlx::query(
        "UPDATE media_reconciler_leases SET lease_owner = NULL, lease_expires_at = NULL, \
         updated_at = ? WHERE singleton = 1 AND lease_owner = ?",
    )
    .bind(Utc::now())
    .bind(owner)
    .execute(&mut *transaction)
    .await?;
    if result.rows_affected() != 1 {
        return Err(corrupt_global_lease());
    }
    transaction.commit().await?;
    Ok(true)
}

async fn load_global_lease_state<'e, E>(executor: E) -> Result<GlobalLeaseState>
where
    E: Executor<'e, Database = Sqlite>,
{
    let rows = sqlx::query_as::<_, GlobalLeaseRaw>(
        "SELECT (SELECT sql FROM sqlite_schema
                    WHERE type = 'table' AND name = 'media_reconciler_leases') AS table_sql,
                typeof(singleton) AS singleton_storage, singleton,
                typeof(lease_owner) AS owner_storage, lease_owner,
                typeof(lease_expires_at) AS expiry_storage, lease_expires_at,
                typeof(updated_at) AS updated_storage, updated_at
         FROM media_reconciler_leases ORDER BY singleton",
    )
    .fetch_all(executor)
    .await
    .map_err(|_| corrupt_global_lease())?;
    if rows.len() != 1 {
        return Err(corrupt_global_lease());
    }
    let row = rows.into_iter().next().expect("one lease row was required");
    sqlite::validate_global_lease_schema_sql(&row.table_sql).map_err(|_| corrupt_global_lease())?;
    sqlite::validate_global_lease_values(
        &row.singleton_storage,
        row.singleton,
        &row.owner_storage,
        row.lease_owner.as_deref(),
        &row.expiry_storage,
        row.lease_expires_at.as_deref(),
        &row.updated_storage,
        &row.updated_at,
    )
    .map_err(|_| corrupt_global_lease())
}

fn corrupt_global_lease() -> AppError {
    AppError::Internal("global lease state is not exactly the Sentinel 0.2 contract".into())
}

pub async fn reconcile_available(state: &AppState) -> Result<()> {
    SqliteOperationStore::new(state.pool.clone())
        .recover_expired(OPERATION_NAMESPACE, Utc::now().timestamp_micros())
        .await
        .map_err(operation_error)?;
    for _ in 0..64 {
        if !reconcile_once(state).await? {
            break;
        }
    }
    Ok(())
}

/// The outbox event ID is also the product audit ID, making replay idempotent.
/// Only safe product metadata is projected; request and result payloads are not logged.
pub async fn flush_operation_audit(pool: &SqlitePool) -> Result<usize> {
    let store = SqliteOperationStore::new(pool.clone());
    let events = store
        .pending_audit_events(128)
        .await
        .map_err(operation_error)?;
    let count = events.len();
    for event in events {
        let operation = store
            .get(&event.operation_id)
            .await
            .map_err(operation_error)?
            .ok_or_else(|| AppError::Internal("审计操作不存在".into()))?;
        let operation = operation_view(operation)?;
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("INSERT INTO audit_logs (id, user_id, action, entity_type, entity_id, details, created_at) \
            VALUES (?, (SELECT administrator_id FROM _sarmg_administrators WHERE administrator_id = ?), \
            'media.operation.transition', 'camera', ?, ?, ?) ON CONFLICT(id) DO NOTHING")
            .bind(Uuid::parse_str(&event.event_id).map_err(|_| AppError::Internal("审计事件 ID 无效".into()))?)
            .bind(operation.requested_by)
            .bind(operation.camera_id)
            .bind(json!({ "operation_id": event.operation_id, "generation": operation.generation,
                "from_state": event.from_state, "to_state": event.to_state }))
            .bind(timestamp(event.created_at_micros)?)
            .execute(&mut *transaction).await?;
        SqliteOperationStore::mark_audit_delivered_in(
            &mut transaction,
            &event.event_id,
            Utc::now().timestamp_micros(),
        )
        .await
        .map_err(operation_error)?;
        transaction.commit().await?;
    }
    Ok(count)
}

async fn claim_next_operation(
    pool: &SqlitePool,
    global_owner: &str,
    request_timeout: StdDuration,
) -> Result<Option<MediaOperationView>> {
    let now = Utc::now();
    let lease_expires_at = lease_deadline(now, request_timeout, OPERATION_LEASE_REQUEST_BUDGET);
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let lease = load_global_lease_state(&mut *transaction).await?;
    if lease.owner.as_deref() != Some(global_owner)
        || lease.lease_expires_at.is_none_or(|expiry| expiry <= now)
    {
        return Err(AppError::Conflict(
            "媒体协调器租约已由其他执行器接管".into(),
        ));
    }
    let claimed = SqliteOperationStore::claim_next_in(
        &mut transaction,
        OPERATION_NAMESPACE,
        global_owner,
        now.timestamp_micros(),
        lease_expires_at.timestamp_micros(),
    )
    .await
    .map_err(operation_error)?;
    transaction.commit().await?;
    claimed.map(operation_view).transpose()
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
    let mut transaction = state.pool.begin_with("BEGIN IMMEDIATE").await?;
    let current = load_global_lease_state(&mut *transaction).await?;
    let now = Utc::now();
    if current.owner.as_deref() != Some(owner)
        || current
            .lease_expires_at
            .is_none_or(|expires_at| expires_at <= now)
    {
        return Err(AppError::Conflict("媒体操作租约已由其他执行器接管".into()));
    }
    let global_deadline = lease_deadline(
        now,
        state.config.request_timeout,
        GLOBAL_LEASE_REQUEST_BUDGET,
    );
    let global = sqlx::query(
        "UPDATE media_reconciler_leases SET lease_expires_at = ?, updated_at = ? \
         WHERE singleton = 1 AND lease_owner = ? AND lease_expires_at > ?",
    )
    .bind(global_deadline)
    .bind(now)
    .bind(owner)
    .bind(now)
    .execute(&mut *transaction)
    .await?;
    if global.rows_affected() != 1 {
        return Err(corrupt_global_lease());
    }
    transaction.commit().await?;
    let current = SqliteOperationStore::new(state.pool.clone())
        .get(&operation.id)
        .await
        .map_err(operation_error)?
        .ok_or_else(|| AppError::Conflict("媒体操作已不存在".into()))?;
    if current.operation.state != OperationState::Running
        || current.operation.lease_owner.as_deref() != Some(owner)
        || current
            .operation
            .lease_expiry_micros
            .is_none_or(|expiry| expiry <= now.timestamp_micros())
    {
        return Err(AppError::Conflict("媒体操作租约已由其他执行器接管".into()));
    }
    Ok(())
}

fn operation_owner(operation: &MediaOperationView) -> Result<&str> {
    match (operation.lease_owner.as_deref(), operation.lease_expires_at) {
        (Some(owner), Some(_)) => Ok(owner),
        _ => Err(AppError::Conflict("媒体操作没有有效租约所有者".into())),
    }
}

fn operation_error(error: sarmg_operations::Error) -> AppError {
    if matches!(error, sarmg_operations::Error::ConcurrentModification) {
        return AppError::Conflict("媒体操作租约已由其他执行器接管".into());
    }
    tracing::error!(error = %error, "Foundation rejected media operation state transition");
    AppError::Internal("媒体操作状态不符合当前 Foundation 合同".into())
}

async fn enqueue_operation_in(
    transaction: &mut Transaction<'_, Sqlite>,
    request: MediaOperationRequest,
    now: DateTime<Utc>,
) -> Result<MediaOperationView> {
    let request_payload = serde_json::to_vec(&request)
        .map_err(|_| AppError::Internal("媒体操作请求无法编码".into()))?;
    let request_fingerprint: [u8; 32] = Sha256::digest(&request_payload).into();
    let mut idempotency = Sha256::new();
    for value in [
        OPERATION_NAMESPACE.as_bytes(),
        request.camera_id.hyphenated().to_string().as_bytes(),
        request.generation.to_string().as_bytes(),
        request.reason.as_bytes(),
        now.timestamp_micros().to_string().as_bytes(),
    ] {
        idempotency.update((value.len() as u64).to_be_bytes());
        idempotency.update(value);
    }
    let idempotency_digest: [u8; 32] = idempotency.finalize().into();
    let value = NewOperation {
        operation_id: Uuid::new_v4().to_string(),
        namespace: OPERATION_NAMESPACE.into(),
        target_key: request.camera_id.hyphenated().to_string(),
        action: "reconcile_camera".into(),
        idempotency_digest,
        request_fingerprint,
        request_payload,
        max_attempts: 8,
        not_before_micros: now.timestamp_micros(),
        created_at_micros: now.timestamp_micros(),
    };
    let stored = match SqliteOperationStore::enqueue_in(transaction, value)
        .await
        .map_err(operation_error)?
    {
        EnqueueOutcome::Created(stored) | EnqueueOutcome::Existing(stored) => stored,
    };
    operation_view(stored)
}

fn operation_view(stored: StoredOperation) -> Result<MediaOperationView> {
    if stored.operation.namespace != OPERATION_NAMESPACE || stored.action != "reconcile_camera" {
        return Err(AppError::NotFound("媒体操作不存在".into()));
    }
    let request: MediaOperationRequest = serde_json::from_slice(&stored.request_payload)
        .map_err(|_| AppError::Internal("媒体操作请求损坏".into()))?;
    if stored.operation.target_key != request.camera_id.hyphenated().to_string() {
        return Err(AppError::Internal("媒体操作目标与请求不一致".into()));
    }
    let created_at = timestamp(stored.created_at_micros)?;
    let updated_at = timestamp(stored.updated_at_micros)?;
    let state = stored.operation.state;
    let retry_at = (state == OperationState::Pending)
        .then(|| timestamp(stored.operation.not_before_micros))
        .transpose()?;
    let finished_at = matches!(
        state,
        OperationState::Succeeded
            | OperationState::Failed
            | OperationState::Unknown
            | OperationState::DeadLetter
            | OperationState::Resolved
    )
    .then_some(updated_at);
    Ok(MediaOperationView {
        id: stored.operation.operation_id,
        camera_id: request.camera_id,
        generation: request.generation,
        kind: stored.action,
        state: state.as_str().into(),
        reason: request.reason,
        requested_by: request.requested_by,
        attempt: i64::from(stored.operation.attempt),
        max_attempts: i64::from(stored.operation.max_attempts),
        created_at,
        started_at: (state == OperationState::Running).then_some(updated_at),
        finished_at,
        retry_at,
        error_code: stored.operation.error_code,
        error_message: None,
        lease_owner: stored.operation.lease_owner,
        lease_expires_at: stored
            .operation
            .lease_expiry_micros
            .map(timestamp)
            .transpose()?,
    })
}

fn timestamp(micros: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_micros(micros)
        .ok_or_else(|| AppError::Internal("媒体操作时间戳无效".into()))
}

async fn complete_owned(
    pool: &SqlitePool,
    operation: &MediaOperationView,
    event: Transition,
    result: Option<serde_json::Value>,
) -> Result<()> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    complete_owned_in(&mut transaction, operation, event, result).await?;
    transaction.commit().await?;
    Ok(())
}

async fn complete_owned_in(
    transaction: &mut Transaction<'_, Sqlite>,
    operation: &MediaOperationView,
    event: Transition,
    result: Option<serde_json::Value>,
) -> Result<()> {
    let owner = operation_owner(operation)?;
    let now = Utc::now();
    let lease = load_global_lease_state(&mut **transaction).await?;
    if lease.owner.as_deref() != Some(owner)
        || lease.lease_expires_at.is_none_or(|expiry| expiry <= now)
    {
        return Err(AppError::Conflict(
            "媒体协调器租约已由其他执行器接管".into(),
        ));
    }
    let payload = result
        .map(|value| serde_json::to_vec(&value))
        .transpose()
        .map_err(|_| AppError::Internal("媒体操作结果无法编码".into()))?;
    SqliteOperationStore::apply_transition_owned_in(
        transaction,
        &operation.id,
        owner,
        event,
        payload.as_deref(),
        now.timestamp_micros(),
    )
    .await
    .map(|_| ())
    .map_err(operation_error)
}

async fn apply_claimed_operation(state: &AppState, operation: MediaOperationView) -> Result<()> {
    let store = SqliteOperationStore::new(state.pool.clone());
    let claim = store
        .get(&operation.id)
        .await
        .map_err(operation_error)?
        .ok_or_else(|| AppError::NotFound("媒体操作不存在".into()))?;
    if claim.operation.lease_owner != operation.lease_owner
        || i64::from(claim.operation.attempt) != operation.attempt
        || claim.operation.lease_expiry_micros
            != operation
                .lease_expires_at
                .map(|value| value.timestamp_micros())
    {
        return Err(AppError::Conflict("媒体操作租约已由其他执行器接管".into()));
    }
    let result = apply_claimed_operation_inner(state, operation).await;
    if result.is_err() {
        // Local completion failure after remote effects is ambiguous, not retryable.
        // Captured owner/attempt/expiry prevent this marker touching another claim.
        match store
            .abandon_claim(
                &claim.operation,
                "completion_persistence_uncertain",
                Utc::now().timestamp_micros(),
            )
            .await
        {
            Ok(_) | Err(sarmg_operations::Error::ConcurrentModification) => {}
            Err(error) => return Err(operation_error(error)),
        }
    }
    result
}

async fn apply_claimed_operation_inner(
    state: &AppState,
    operation: MediaOperationView,
) -> Result<()> {
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
        Ok(applied) => finish_success(&state.pool, &operation, &desired, &applied).await,
        Err(error) => finish_failure(&state.pool, &operation, &error).await,
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
    let credentials = camera.decrypt_credentials(&state.secrets)?;
    let standard_sub_path = camera_path(camera.id, "sub");
    if !desired.desired_present {
        state.media.delete_path(&desired.main_path).await?;
        state.media.delete_path(&standard_sub_path).await?;
        return Ok(AppliedSources {
            main_digest: None,
            sub_digest: None,
        });
    }

    let main_source = source_with_credentials(
        &credentials.main_stream_url,
        credentials.username.as_deref(),
        credentials.password.as_deref(),
    )?;
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
    match (&desired.sub_path, &credentials.sub_stream_url) {
        (Some(sub_path), Some(sub_url)) => {
            let sub_source = source_with_credentials(
                sub_url,
                credentials.username.as_deref(),
                credentials.password.as_deref(),
            )?;
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
    pool: &SqlitePool,
    operation: &MediaOperationView,
    desired: &DesiredState,
    applied: &AppliedSources,
) -> Result<()> {
    let now = Utc::now();
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let current_generation = sqlx::query_scalar::<_, i64>(
        "SELECT generation FROM media_desired_states WHERE camera_id = ?",
    )
    .bind(desired.camera_id)
    .fetch_one(&mut *transaction)
    .await?;
    if current_generation != desired.generation {
        complete_owned_in(
            &mut transaction,
            operation,
            Transition::Succeed,
            Some(json!({ "converged": false, "superseded_after_apply": true })),
        )
        .await?;
        transaction.commit().await?;
        return Ok(());
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
    complete_owned_in(
        &mut transaction,
        operation,
        Transition::Succeed,
        Some(json!({ "generation": desired.generation, "converged": true })),
    )
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
    pool: &SqlitePool,
    operation: &MediaOperationView,
    error: &AppError,
) -> Result<()> {
    let (kind, error_code, error_message) = sanitized_failure(error);
    let now = Utc::now();
    let retry_at = (kind == FailureKind::Retryable).then(|| now + retry_delay(operation.attempt));
    let event = match kind {
        FailureKind::Indeterminate => Transition::MarkIndeterminate {
            code: error_code.into(),
        },
        FailureKind::Retryable | FailureKind::Definitive => Transition::Fail {
            code: error_code.into(),
            retryable: kind == FailureKind::Retryable,
            retry_not_before_micros: retry_at.map_or(0, |value| value.timestamp_micros()),
        },
    };
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE cameras SET status = 'error', updated_at = ? WHERE id = ? \
        AND EXISTS (SELECT 1 FROM media_desired_states WHERE camera_id = cameras.id AND generation = ?)")
        .bind(now)
        .bind(operation.camera_id)
        .bind(operation.generation)
        .execute(&mut *transaction)
        .await?;
    complete_owned_in(
        &mut transaction,
        operation,
        event,
        Some(json!({ "error": error_message })),
    )
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailureKind {
    Retryable,
    Definitive,
    Indeterminate,
}

fn sanitized_failure(error: &AppError) -> (FailureKind, &'static str, &'static str) {
    match error {
        AppError::UpstreamUnknown(_) => (
            FailureKind::Indeterminate,
            "media_outcome_unknown",
            "The media service outcome could not be determined",
        ),
        AppError::Upstream(_) => (
            FailureKind::Retryable,
            "media_request_failed",
            "The media service rejected or could not process the desired state",
        ),
        AppError::Validation(_) => (
            FailureKind::Definitive,
            "invalid_stored_camera_configuration",
            "The stored camera configuration is invalid",
        ),
        _ => (
            FailureKind::Definitive,
            "media_reconciliation_internal",
            "The desired media state could not be prepared",
        ),
    }
}

fn retry_delay(attempt: i64) -> Duration {
    let exponent = u32::try_from(attempt.clamp(0, 8)).unwrap_or(8);
    Duration::seconds((1_i64 << exponent).min(300))
}

async fn finish_superseded(pool: &SqlitePool, operation: &MediaOperationView) -> Result<()> {
    complete_owned(
        pool,
        operation,
        Transition::Succeed,
        Some(json!({ "converged": false, "superseded": true })),
    )
    .await
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
    let credentials = camera.decrypt_credentials(&state.secrets)?;
    if !desired.desired_present {
        return Ok(ExpectedConfigs {
            main: None,
            sub: None,
        });
    }
    let main_source = source_with_credentials(
        &credentials.main_stream_url,
        credentials.username.as_deref(),
        credentials.password.as_deref(),
    )?;
    let main = Some(PathConfigSnapshot {
        source_digest: Some(source_digest(&main_source)),
        source_on_demand: !desired.record_enabled,
        record: desired.record_enabled,
    });
    let sub = match (&desired.sub_path, &credentials.sub_stream_url) {
        (Some(_), Some(sub_url)) => {
            let source = source_with_credentials(
                sub_url,
                credentials.username.as_deref(),
                credentials.password.as_deref(),
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
    let store = SqliteOperationStore::new(pool.clone());
    if let Some(latest) = store
        .latest_for_target(
            OPERATION_NAMESPACE,
            &desired.camera_id.hyphenated().to_string(),
        )
        .await
        .map_err(operation_error)?
    {
        let request: MediaOperationRequest = serde_json::from_slice(&latest.request_payload)
            .map_err(|_| AppError::Internal("媒体操作请求损坏".into()))?;
        let blocks_new = request.generation == desired.generation
            && (matches!(
                latest.operation.state,
                OperationState::Pending
                    | OperationState::Running
                    | OperationState::Unknown
                    | OperationState::Failed
                    | OperationState::DeadLetter
            ) || (latest.operation.state == OperationState::Succeeded
                && latest.updated_at_micros >= observation_started_at.timestamp_micros()));
        if blocks_new {
            return Ok(());
        }
    }
    let mut transaction = pool.begin().await?;
    enqueue_operation_in(
        &mut transaction,
        MediaOperationRequest {
            camera_id: desired.camera_id,
            generation: desired.generation,
            reason: "drift_detected".into(),
            requested_by: None,
        },
        now,
    )
    .await?;
    transaction.commit().await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use sarmg_admin_core::AdministratorStore as _;

    async fn lease_test_database() -> (tempfile::TempDir, SqlitePool, String, String) {
        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("leases.sqlite3");
        let database_url = format!("sqlite://{}", database.display());
        let pool = crate::sqlite::open_pool(&database_url).await.unwrap();
        let now = Utc::now();
        let now_micros = now.timestamp_micros();
        let camera = Uuid::new_v4();
        let operation = Uuid::new_v4().to_string();
        let administrator = sarmg_admin_core::AdministratorService::new(
            sarmg_admin_sqlite::SqliteAdministratorStore::new(pool.clone()),
        );
        assert!(administrator
            .bootstrap_administrator("lease-admin", "lease-admin-password", now_micros as u64)
            .await
            .unwrap());
        let user = administrator
            .store()
            .administrator_by_username("lease-admin")
            .await
            .unwrap()
            .unwrap()
            .administrator_id
            .to_string();
        assert!(
            Uuid::parse_str(&user).is_err(),
            "Foundation administrator IDs are opaque, not product UUIDs"
        );
        sqlx::query(
            "INSERT INTO cameras (id, name, main_stream_url_enc, created_by, created_at, updated_at) \
             VALUES (?, 'Lease Camera', ?, ?, ?, ?)",
        )
        .bind(camera)
        .bind(vec![1u8; 32])
        .bind(user.to_string())
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
        let request = MediaOperationRequest {
            camera_id: camera,
            generation: 1,
            reason: "drift_detected".into(),
            requested_by: Some(user.to_string()),
        };
        let payload = serde_json::to_vec(&request).unwrap();
        SqliteOperationStore::new(pool.clone())
            .enqueue(NewOperation {
                operation_id: operation.clone(),
                namespace: OPERATION_NAMESPACE.into(),
                target_key: camera.hyphenated().to_string(),
                action: "reconcile_camera".into(),
                idempotency_digest: Sha256::digest(operation.as_bytes()).into(),
                request_fingerprint: Sha256::digest(&payload).into(),
                request_payload: payload,
                max_attempts: 8,
                not_before_micros: now_micros,
                created_at_micros: now_micros,
            })
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

    async fn claim_for_test(pool: &SqlitePool) -> MediaOperationView {
        let owner = Uuid::new_v4().to_string();
        sqlx::query("UPDATE media_reconciler_leases SET lease_owner = ?, lease_expires_at = ?, updated_at = ? WHERE singleton = 1")
            .bind(&owner).bind(Utc::now() + Duration::minutes(5)).bind(Utc::now())
            .execute(pool).await.unwrap();
        claim_next_operation(pool, &owner, StdDuration::from_secs(1))
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn success_and_audit_failure_cannot_publish_partial_camera_results() {
        let (_directory, pool, id, _) = lease_test_database().await;
        let operation = claim_for_test(&pool).await;
        let desired = load_desired(&pool, operation.camera_id).await.unwrap();
        let applied = AppliedSources {
            main_digest: Some([42; 32]),
            sub_digest: None,
        };
        sqlx::raw_sql("CREATE TRIGGER reject_operation_audit BEFORE INSERT ON _sarmg_operation_audit_outbox BEGIN SELECT RAISE(FAIL, 'injected'); END;")
            .execute(&pool).await.unwrap();
        assert!(finish_success(&pool, &operation, &desired, &applied)
            .await
            .is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media_actual_paths")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        let store = SqliteOperationStore::new(pool.clone());
        assert_eq!(
            store.get(&id).await.unwrap().unwrap().operation.state,
            OperationState::Running
        );
        sqlx::query("DROP TRIGGER reject_operation_audit")
            .execute(&pool)
            .await
            .unwrap();
        finish_success(&pool, &operation, &desired, &applied)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media_actual_paths")
                .fetch_one(&pool)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            store.get(&id).await.unwrap().unwrap().operation.state,
            OperationState::Succeeded
        );
        assert_eq!(store.pending_audit_count().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn superseded_global_owner_cannot_publish_success_or_failure() {
        let (_directory, pool, id, _) = lease_test_database().await;
        let operation = claim_for_test(&pool).await;
        let desired = load_desired(&pool, operation.camera_id).await.unwrap();
        let initial_status: String = sqlx::query_scalar("SELECT status FROM cameras WHERE id = ?")
            .bind(operation.camera_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE media_reconciler_leases SET lease_owner = ? WHERE singleton = 1")
            .bind(Uuid::new_v4().to_string())
            .execute(&pool)
            .await
            .unwrap();
        assert!(finish_success(
            &pool,
            &operation,
            &desired,
            &AppliedSources {
                main_digest: None,
                sub_digest: None
            }
        )
        .await
        .is_err());
        assert!(
            finish_failure(&pool, &operation, &AppError::Upstream("rejected".into()))
                .await
                .is_err()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media_actual_paths")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT status FROM cameras WHERE id = ?")
                .bind(operation.camera_id)
                .fetch_one(&pool)
                .await
                .unwrap(),
            initial_status
        );
        assert_eq!(
            SqliteOperationStore::new(pool)
                .get(&id)
                .await
                .unwrap()
                .unwrap()
                .operation
                .state,
            OperationState::Running
        );
    }

    #[tokio::test]
    async fn operation_audit_delivery_is_atomic_and_idempotent() {
        let (_directory, pool, _, _) = lease_test_database().await;
        let store = SqliteOperationStore::new(pool.clone());
        sqlx::raw_sql("CREATE TRIGGER reject_ack BEFORE UPDATE OF delivered_at_micros ON _sarmg_operation_audit_outbox BEGIN SELECT RAISE(FAIL, 'injected'); END;")
            .execute(&pool).await.unwrap();
        assert!(flush_operation_audit(&pool).await.is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM audit_logs")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(store.pending_audit_count().await.unwrap(), 1);
        sqlx::query("DROP TRIGGER reject_ack")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(flush_operation_audit(&pool).await.unwrap(), 1);
        assert_eq!(flush_operation_audit(&pool).await.unwrap(), 0);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM audit_logs")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        assert_eq!(store.pending_audit_count().await.unwrap(), 0);
        let record = sqlx::query_as::<_, crate::models::AuditRecord>(
            "SELECT id, user_id, action, entity_type, entity_id, details, created_at FROM audit_logs",
        ).fetch_one(&pool).await.unwrap();
        assert_eq!(record.user_id.as_ref().map(String::len), Some(43));
        assert!(record.entity_id.is_some());
        let event_id = Uuid::new_v4();
        sqlx::query("INSERT INTO events (id, kind, severity, message, acknowledged_by, created_at) VALUES (?, 'test', 'info', 'test', ?, ?)")
            .bind(event_id).bind(record.user_id).bind(Utc::now()).execute(&pool).await.unwrap();
        let event = sqlx::query_as::<_, crate::models::EventRecord>(
            "SELECT id, camera_id, kind, severity, message, details, acknowledged_at, acknowledged_by, created_at FROM events WHERE id = ?",
        ).bind(event_id).fetch_one(&pool).await.unwrap();
        assert_eq!(event.acknowledged_by.as_ref().map(String::len), Some(43));
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
        let (_, code, message) = sanitized_failure(&error);
        assert_eq!(code, "media_request_failed");
        assert!(!message.contains("super-secret"));
        assert!(!message.contains("camera.invalid"));
    }

    #[tokio::test]
    async fn expired_operation_is_fenced_and_a_new_owner_can_take_over() {
        let (_temporary, pool, operation_id, _database_url) = lease_test_database().await;
        let active_until = Utc::now() + Duration::minutes(5);
        let new_owner = Uuid::new_v4().to_string();
        sqlx::query(
            "UPDATE media_reconciler_leases SET lease_owner = ?, lease_expires_at = ?, \
             updated_at = ? WHERE singleton = 1",
        )
        .bind(&new_owner)
        .bind(active_until)
        .bind(Utc::now())
        .execute(&pool)
        .await
        .unwrap();
        let store = SqliteOperationStore::new(pool.clone());
        let claimed = store
            .claim_next(
                OPERATION_NAMESPACE,
                &new_owner,
                Utc::now().timestamp_micros(),
                active_until.timestamp_micros(),
            )
            .await
            .unwrap()
            .unwrap();
        let mut old_operation = operation_view(claimed).unwrap();
        old_operation.lease_owner = Some("old-owner".into());
        assert!(finish_superseded(&pool, &old_operation).await.is_err());
        let fenced_state: String =
            sqlx::query_scalar("SELECT state FROM _sarmg_operations WHERE operation_id = ?")
                .bind(&operation_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(fenced_state, "running");

        let expired = (Utc::now() - Duration::seconds(1)).timestamp_micros();
        sqlx::query("UPDATE _sarmg_operations SET lease_expiry_micros = ? WHERE operation_id = ?")
            .bind(expired)
            .bind(&operation_id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(recover_interrupted_operations(&pool).await.unwrap(), 1);
        assert!(
            claim_next_operation(&pool, &new_owner, StdDuration::from_secs(1))
                .await
                .unwrap()
                .is_none(),
            "Unknown operations are never automatically claimed"
        );
        let still_unknown: (String, Option<String>) = sqlx::query_as(
            "SELECT state, lease_owner FROM _sarmg_operations WHERE operation_id = ?",
        )
        .bind(&operation_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(still_unknown, ("unknown".into(), None));

        assert!(!release_reconciler_lease(&pool, "old-owner").await.unwrap());
        let global_owner: Option<String> = sqlx::query_scalar(
            "SELECT lease_owner FROM media_reconciler_leases WHERE singleton = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(global_owner.as_deref(), Some(new_owner.as_str()));

        assert!(release_reconciler_lease(&pool, &new_owner).await.unwrap());
        pool.close().await;
    }

    #[tokio::test]
    async fn startup_recovery_marks_previous_running_work_unknown() {
        let (_temporary, first, operation_id, database_url) = lease_test_database().await;
        let active_until = Utc::now() + Duration::minutes(5);
        let healthy_owner = Uuid::new_v4().to_string();
        sqlx::query(
            "UPDATE media_reconciler_leases SET lease_owner = ?, \
             lease_expires_at = ?, updated_at = ? WHERE singleton = 1",
        )
        .bind(&healthy_owner)
        .bind(active_until)
        .bind(Utc::now())
        .execute(&first)
        .await
        .unwrap();
        SqliteOperationStore::new(first.clone())
            .claim_next(
                OPERATION_NAMESPACE,
                &healthy_owner,
                Utc::now().timestamp_micros(),
                active_until.timestamp_micros(),
            )
            .await
            .unwrap()
            .unwrap();

        let second = crate::sqlite::open_pool(&database_url).await.unwrap();
        assert_eq!(recover_interrupted_operations(&second).await.unwrap(), 1);
        let operation: (String, Option<String>) = sqlx::query_as(
            "SELECT state, lease_owner FROM _sarmg_operations WHERE operation_id = ?",
        )
        .bind(&operation_id)
        .fetch_one(&first)
        .await
        .unwrap();
        assert_eq!(operation, ("unknown".into(), None));
        let global: Option<String> = sqlx::query_scalar(
            "SELECT lease_owner FROM media_reconciler_leases WHERE singleton = 1",
        )
        .fetch_one(&first)
        .await
        .unwrap();
        assert_eq!(global.as_deref(), Some(healthy_owner.as_str()));
        second.close().await;
        first.close().await;
    }
}
