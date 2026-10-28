//! Bounded, opt-in API body observations, stored separately from trace summaries.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{AdminState, TraceStore};

pub const BODY_LIMIT: usize = 256 * 1024;
const RECORD_LIMIT: usize = 500;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservationConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub key_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureLabel {
    pub name: String,
    pub heuristic: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservationSummary {
    pub id: String,
    pub ts: String,
    pub key_id: u64,
    pub path: String,
    pub model: Option<String>,
    pub labels: Vec<FeatureLabel>,
    pub state: String,
    pub status_code: Option<u16>,
    pub content_type: Option<String>,
    pub duration_ms: u64,
    pub request_bytes: usize,
    pub request_truncated: bool,
    pub response_bytes: usize,
    pub response_truncated: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservationDetail {
    #[serde(flatten)]
    pub summary: ObservationSummary,
    pub request_body: String,
    pub response_body: String,
}

pub(super) fn init_observations(conn: &Connection) -> rusqlite::Result<ObservationConfig> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS observation_settings (id INTEGER PRIMARY KEY, config TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS request_observations (
           id TEXT PRIMARY KEY, ts_epoch INTEGER NOT NULL, key_id INTEGER NOT NULL,
           metadata TEXT NOT NULL, request_body TEXT NOT NULL, response_body TEXT NOT NULL DEFAULT ''
         );
         CREATE INDEX IF NOT EXISTS idx_observations_time ON request_observations(ts_epoch);",
    )?;
    // A process restart cannot leave old requests labelled as still running.
    let unfinished = {
        let mut statement = conn.prepare("SELECT metadata FROM request_observations")?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for metadata in unfinished {
        if let Ok(mut summary) = serde_json::from_str::<ObservationSummary>(&metadata) {
            if summary.state == "running" {
                summary.state = "interrupted".to_owned();
                conn.execute(
                    "UPDATE request_observations SET metadata=?1 WHERE id=?2",
                    rusqlite::params![json!(summary).to_string(), summary.id],
                )?;
            }
        }
    }
    let saved: Option<String> = conn
        .query_row(
            "SELECT config FROM observation_settings WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(saved
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default())
}

fn parse_metadata(value: &str) -> rusqlite::Result<ObservationSummary> {
    serde_json::from_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

impl TraceStore {
    pub fn observation_config(&self) -> ObservationConfig {
        self.observation_config.read().clone()
    }

    pub fn set_observation_config(&self, config: ObservationConfig) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO observation_settings(id,config) VALUES(1,?1)
             ON CONFLICT(id) DO UPDATE SET config=excluded.config",
            [json!(config).to_string()],
        )?;
        *self.observation_config.write() = config;
        Ok(())
    }

    pub fn begin_observation(
        &self,
        summary: &ObservationSummary,
        body: &str,
    ) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO request_observations(id,ts_epoch,key_id,metadata,request_body)
             VALUES(?1,?2,?3,?4,?5)",
            rusqlite::params![
                summary.id,
                chrono::Utc::now().timestamp(),
                summary.key_id,
                json!(summary).to_string(),
                body
            ],
        )?;
        tx.execute(
            "DELETE FROM request_observations WHERE ts_epoch < ?1",
            [chrono::Utc::now().timestamp() - 86400],
        )?;
        tx.execute(
            "DELETE FROM request_observations WHERE rowid IN (
               SELECT rowid FROM request_observations ORDER BY rowid DESC LIMIT -1 OFFSET ?1
             )",
            [RECORD_LIMIT as i64],
        )?;
        tx.commit()
    }

    pub fn finish_observation(
        &self,
        summary: &ObservationSummary,
        body: &str,
    ) -> rusqlite::Result<()> {
        self.conn.lock().execute(
            "UPDATE request_observations SET metadata=?1,response_body=?2 WHERE id=?3",
            rusqlite::params![json!(summary).to_string(), body, summary.id],
        )?;
        Ok(())
    }

    pub fn list_observations(
        &self,
        key_id: Option<u64>,
        limit: usize,
        offset: usize,
    ) -> rusqlite::Result<(Vec<ObservationSummary>, usize)> {
        let conn = self.conn.lock();
        let cutoff = chrono::Utc::now().timestamp() - 86400;
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM request_observations WHERE (?1 IS NULL OR key_id=?1) AND ts_epoch>=?2",
            rusqlite::params![key_id, cutoff], |row| row.get(0),
        )?;
        let mut statement = conn.prepare(
            "SELECT metadata FROM request_observations WHERE (?1 IS NULL OR key_id=?1) AND ts_epoch>=?2
             ORDER BY rowid DESC LIMIT ?3 OFFSET ?4",
        )?;
        let records = statement
            .query_map(
                rusqlite::params![
                    key_id,
                    cutoff,
                    limit.clamp(1, 100) as i64,
                    offset.min(10000) as i64
                ],
                |row| parse_metadata(&row.get::<_, String>(0)?),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((records, total as usize))
    }

    pub fn observation_detail(&self, id: &str) -> rusqlite::Result<Option<ObservationDetail>> {
        self.conn
            .lock()
            .query_row(
                "SELECT metadata,request_body,response_body FROM request_observations
             WHERE id=?1 AND ts_epoch>=?2",
                rusqlite::params![id, chrono::Utc::now().timestamp() - 86400],
                |row| {
                    Ok(ObservationDetail {
                        summary: parse_metadata(&row.get::<_, String>(0)?)?,
                        request_body: row.get(1)?,
                        response_body: row.get(2)?,
                    })
                },
            )
            .optional()
    }
}

pub async fn get_config(State(state): State<AdminState>) -> Json<ObservationConfig> {
    Json(state.trace_store.observation_config())
}

pub async fn set_config(
    State(state): State<AdminState>,
    Json(config): Json<ObservationConfig>,
) -> Response {
    if config.key_id == Some(0)
        || (config.enabled && config.key_id.is_none())
        || config
            .key_id
            .is_some_and(|id| !state.client_keys.list().iter().any(|key| key.id == id))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "请先选择检测专用客户端 Key"})),
        )
            .into_response();
    }
    match state.trace_store.set_observation_config(config.clone()) {
        Ok(()) => Json(config).into_response(),
        Err(error) => {
            tracing::warn!("保存内容观察配置失败: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"保存观察配置失败"})),
            )
                .into_response()
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservationQuery {
    key_id: Option<u64>,
    limit: Option<usize>,
    offset: Option<usize>,
}

pub async fn list(
    State(state): State<AdminState>,
    Query(query): Query<ObservationQuery>,
) -> Response {
    match state.trace_store.list_observations(
        query.key_id,
        query.limit.unwrap_or(25),
        query.offset.unwrap_or(0),
    ) {
        Ok((records, total)) => Json(json!({"records": records, "total": total})).into_response(),
        Err(error) => {
            tracing::warn!("查询内容观察失败: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"读取观察记录失败"})),
            )
                .into_response()
        }
    }
}

pub async fn detail(State(state): State<AdminState>, Path(id): Path<String>) -> Response {
    match state.trace_store.observation_detail(&id) {
        Ok(Some(record)) => Json(record).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"记录不存在或已过期"})),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!("读取内容观察详情失败: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"读取观察详情失败"})),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: usize, key_id: u64) -> ObservationSummary {
        ObservationSummary {
            id: id.to_string(),
            ts: chrono::Utc::now().to_rfc3339(),
            key_id,
            path: "/v1/messages".into(),
            model: Some("test".into()),
            labels: vec![],
            state: "running".into(),
            status_code: None,
            content_type: None,
            duration_ms: 0,
            request_bytes: 2,
            request_truncated: false,
            response_bytes: 0,
            response_truncated: false,
        }
    }

    #[test]
    fn observation_store_bounds_and_filters_records_without_returning_bodies_in_list() {
        let store = TraceStore::open_in_memory().unwrap();
        assert!(!store.observation_config().enabled);
        for id in 0..502 {
            store
                .begin_observation(&summary(id, (id % 2 + 1) as u64), "{\"secret\":true}")
                .unwrap();
        }
        let (records, total) = store.list_observations(None, 9999, 0).unwrap();
        assert_eq!(total, 500);
        assert_eq!(records.len(), 100);
        assert_eq!(records[0].id, "501");
        assert!(!json!(records).to_string().contains("secret"));
        let (filtered, total) = store.list_observations(Some(2), 10, 0).unwrap();
        assert_eq!(total, 250);
        assert!(filtered.iter().all(|item| item.key_id == 2));
        assert!(store.observation_detail("0").unwrap().is_none());
        assert_eq!(
            store
                .observation_detail("501")
                .unwrap()
                .unwrap()
                .request_body,
            "{\"secret\":true}"
        );
        assert!(store.observation_detail("' OR 1=1 --").unwrap().is_none());
    }

    #[test]
    fn observation_settings_persist_and_old_records_expire() {
        let conn = Connection::open_in_memory().unwrap();
        init_observations(&conn).unwrap();
        let config = ObservationConfig {
            enabled: true,
            key_id: Some(7),
        };
        conn.execute(
            "INSERT INTO observation_settings(id,config) VALUES(1,?1)",
            [json!(config).to_string()],
        )
        .unwrap();
        let restored = init_observations(&conn).unwrap();
        assert!(restored.enabled);
        assert_eq!(restored.key_id, Some(7));
        let store = TraceStore::open_in_memory().unwrap();
        store.begin_observation(&summary(1, 7), "{}").unwrap();
        store
            .conn
            .lock()
            .execute(
                "UPDATE request_observations SET ts_epoch=?1",
                [chrono::Utc::now().timestamp() - 90000],
            )
            .unwrap();
        assert_eq!(store.list_observations(None, 10, 0).unwrap().1, 0);
        assert!(store.observation_detail("1").unwrap().is_none());
        store.cleanup();
        let rows: i64 = store
            .conn
            .lock()
            .query_row("SELECT COUNT(*) FROM request_observations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0);
    }
}
