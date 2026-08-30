use crate::error::{AppError, Result};
use chrono::{DateTime, Utc};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

#[derive(Clone)]
pub struct MediaMtxClient {
    client: Client,
    api_url: String,
    playback_url: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PathSnapshot {
    pub ready: bool,
    pub readers: usize,
    pub tracks: usize,
}

#[derive(Clone, Debug, Default)]
pub struct PathConfigSnapshot {
    pub source_digest: Option<[u8; 32]>,
    pub source_on_demand: bool,
    pub record: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordingSpan {
    pub start: String,
    pub duration: f64,
    #[serde(default)]
    pub url: Option<String>,
}

impl MediaMtxClient {
    pub fn new(client: Client, api_url: String, playback_url: String) -> Self {
        Self {
            client,
            api_url,
            playback_url,
        }
    }

    pub async fn health(&self) -> bool {
        self.client
            .get(format!("{}/v3/info", self.api_url))
            .send()
            .await
            .map(|response| response.status().is_success())
            .unwrap_or(false)
    }

    pub async fn upsert_path(
        &self,
        path: &str,
        source: &str,
        source_on_demand: bool,
        record: bool,
    ) -> Result<()> {
        let get_url = format!("{}/v3/config/paths/get/{path}", self.api_url);
        let status = self
            .client
            .get(&get_url)
            .send()
            .await
            .map_err(|_| AppError::UpstreamUnknown("path lookup did not complete".into()))?
            .status();
        let exists = if status.is_success() {
            true
        } else if status == reqwest::StatusCode::NOT_FOUND {
            false
        } else {
            return Err(AppError::Upstream(format!("path lookup returned {status}")));
        };

        let payload = json!({
            "source": source,
            "sourceOnDemand": source_on_demand,
            "rtspTransport": "tcp",
            "record": record
        });
        let url = if exists {
            format!("{}/v3/config/paths/patch/{path}", self.api_url)
        } else {
            format!("{}/v3/config/paths/add/{path}", self.api_url)
        };
        let request = if exists {
            self.client.patch(url)
        } else {
            self.client.post(url)
        };
        let response = request
            .json(&payload)
            .send()
            .await
            .map_err(|_| AppError::UpstreamUnknown("path update did not complete".into()))?;
        if !response.status().is_success() {
            let status = response.status();
            return Err(AppError::Upstream(format!("path update returned {status}")));
        }
        Ok(())
    }

    pub async fn delete_path(&self, path: &str) -> Result<()> {
        let response = self
            .client
            .delete(format!("{}/v3/config/paths/delete/{path}", self.api_url))
            .send()
            .await
            .map_err(|_| AppError::UpstreamUnknown("path deletion did not complete".into()))?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(AppError::Upstream(format!(
            "path deletion returned {}",
            response.status()
        )))
    }

    pub async fn paths(&self) -> Result<HashMap<String, PathSnapshot>> {
        let response = self
            .client
            .get(format!("{}/v3/paths/list", self.api_url))
            .send()
            .await
            .map_err(|_| AppError::UpstreamUnknown("path inventory did not complete".into()))?;
        if !response.status().is_success() {
            return Err(AppError::Upstream(format!(
                "path list returned {}",
                response.status()
            )));
        }
        let value: Value = response
            .json()
            .await
            .map_err(|_| AppError::Upstream("path inventory response was invalid".into()))?;
        let mut paths = HashMap::new();
        for item in value
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = item.get("name").and_then(Value::as_str) else {
                continue;
            };
            paths.insert(
                name.to_string(),
                PathSnapshot {
                    ready: item.get("ready").and_then(Value::as_bool).unwrap_or(false),
                    readers: item
                        .get("readers")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or(0),
                    tracks: item
                        .get("tracks")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or(0),
                },
            );
        }
        Ok(paths)
    }

    pub async fn path_configs(&self) -> Result<HashMap<String, PathConfigSnapshot>> {
        let response = self
            .client
            .get(format!("{}/v3/config/paths/list", self.api_url))
            .send()
            .await
            .map_err(|_| {
                AppError::UpstreamUnknown("path configuration inventory did not complete".into())
            })?;
        if !response.status().is_success() {
            return Err(AppError::Upstream(format!(
                "path configuration inventory returned {}",
                response.status()
            )));
        }
        let value: Value = response.json().await.map_err(|_| {
            AppError::Upstream("path configuration inventory response was invalid".into())
        })?;
        let mut paths = HashMap::new();
        for item in value
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = item.get("name").and_then(Value::as_str) else {
                continue;
            };
            let source_digest = item
                .get("source")
                .and_then(Value::as_str)
                .map(source_digest);
            paths.insert(
                name.to_string(),
                PathConfigSnapshot {
                    source_digest,
                    source_on_demand: item
                        .get("sourceOnDemand")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    record: item.get("record").and_then(Value::as_bool).unwrap_or(false),
                },
            );
        }
        Ok(paths)
    }

    pub async fn recordings(
        &self,
        path: &str,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        token: &str,
    ) -> Result<Vec<RecordingSpan>> {
        let mut query = vec![("path", path.to_string())];
        if let Some(start) = start {
            query.push(("start", start.to_rfc3339()));
        }
        if let Some(end) = end {
            query.push(("end", end.to_rfc3339()));
        }
        let response = self
            .client
            .get(format!("{}/list", self.playback_url))
            .bearer_auth(token)
            .query(&query)
            .send()
            .await
            .map_err(|_| AppError::Upstream("recording list request failed".into()))?;
        if !response.status().is_success() {
            return Err(AppError::Upstream(format!(
                "recording list returned {}",
                response.status()
            )));
        }
        response
            .json()
            .await
            .map_err(|_| AppError::Upstream("recording list response was invalid".into()))
    }

    pub async fn recording_stream(
        &self,
        path: &str,
        start: DateTime<Utc>,
        duration: f64,
        format: &str,
        token: &str,
        range: Option<&str>,
    ) -> Result<Response> {
        let mut request = self
            .client
            .get(format!("{}/get", self.playback_url))
            .bearer_auth(token)
            .query(&[
                ("path", path.to_string()),
                ("start", start.to_rfc3339()),
                ("duration", duration.to_string()),
                ("format", format.to_string()),
            ]);
        if let Some(range) = range {
            request = request.header(reqwest::header::RANGE, range);
        }
        request
            .send()
            .await
            .map_err(|_| AppError::Upstream("recording stream request failed".into()))
    }
}

pub fn source_digest(source: &str) -> [u8; 32] {
    Sha256::digest(source.as_bytes()).into()
}
