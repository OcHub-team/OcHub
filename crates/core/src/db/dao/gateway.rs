//! Gateway (local relay) DAO: channels, local API keys, and settings blob.

use crate::db::{lock_conn, to_json_string, Database};
use crate::error::AppError;
use crate::gateway::types::{Dialect, GatewayChannel, GatewayConfig, GatewayKey};
use rusqlite::params;

const GATEWAY_CONFIG_KEY: &str = "gateway_config";

fn row_to_channel(row: &rusqlite::Row<'_>) -> rusqlite::Result<GatewayChannel> {
    let dialect_str: String = row.get(2)?;
    let models_str: String = row.get(6)?;
    let extra_headers_str: String = row.get(11)?;
    Ok(GatewayChannel {
        id: row.get(0)?,
        name: row.get(1)?,
        dialect: Dialect::parse(&dialect_str).unwrap_or(Dialect::Messages),
        base_url: row.get(3)?,
        api_key: row.get(4)?,
        path_override: row.get(5)?,
        models: serde_json::from_str(&models_str).unwrap_or_default(),
        model_override: row.get(7)?,
        priority: row.get(8)?,
        weight: row.get::<_, i64>(9)?.max(1) as u32,
        enabled: row.get(10)?,
        extra_headers: serde_json::from_str(&extra_headers_str).unwrap_or_default(),
    })
}

const CHANNEL_COLUMNS: &str = "id, name, dialect, base_url, api_key, path_override, models, \
     model_override, priority, weight, enabled, extra_headers";

impl Database {
    // -- settings blob ------------------------------------------------------

    pub fn get_gateway_config(&self) -> Result<GatewayConfig, AppError> {
        match self.get_setting(GATEWAY_CONFIG_KEY)? {
            Some(raw) => Ok(serde_json::from_str(&raw).unwrap_or_default()),
            None => Ok(GatewayConfig::default()),
        }
    }

    pub fn set_gateway_config(&self, config: &GatewayConfig) -> Result<(), AppError> {
        self.set_setting(GATEWAY_CONFIG_KEY, &to_json_string(config)?)
    }

    // -- channels -----------------------------------------------------------

    pub fn get_gateway_channels(&self) -> Result<Vec<GatewayChannel>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {CHANNEL_COLUMNS} FROM gateway_channels
                 ORDER BY COALESCE(sort_index, 999999), created_at ASC, id ASC"
            ))
            .map_err(|e| AppError::Database(e.to_string()))?;
        let rows = stmt
            .query_map([], row_to_channel)
            .map_err(|e| AppError::Database(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(e.to_string()))
    }

    pub fn upsert_gateway_channel(&self, channel: &GatewayChannel) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT INTO gateway_channels (
                id, name, dialect, base_url, api_key, path_override, models,
                model_override, priority, weight, enabled, extra_headers, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name, dialect = excluded.dialect,
                base_url = excluded.base_url, api_key = excluded.api_key,
                path_override = excluded.path_override, models = excluded.models,
                model_override = excluded.model_override, priority = excluded.priority,
                weight = excluded.weight, enabled = excluded.enabled,
                extra_headers = excluded.extra_headers",
            params![
                channel.id,
                channel.name,
                channel.dialect.as_str(),
                channel.base_url,
                channel.api_key,
                channel.path_override,
                to_json_string(&channel.models)?,
                channel.model_override,
                channel.priority,
                channel.weight as i64,
                channel.enabled,
                to_json_string(&channel.extra_headers)?,
                chrono::Utc::now().timestamp(),
            ],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn delete_gateway_channel(&self, id: &str) -> Result<bool, AppError> {
        let conn = lock_conn!(self.conn);
        let n = conn
            .execute("DELETE FROM gateway_channels WHERE id = ?1", params![id])
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(n > 0)
    }

    // -- keys ---------------------------------------------------------------

    pub fn get_gateway_keys(&self) -> Result<Vec<GatewayKey>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT id, name, key, enabled, created_at FROM gateway_keys
                 ORDER BY created_at ASC, id ASC",
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(GatewayKey {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    key: row.get(2)?,
                    enabled: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })
            .map_err(|e| AppError::Database(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(e.to_string()))
    }

    pub fn upsert_gateway_key(&self, key: &GatewayKey) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT INTO gateway_keys (id, name, key, enabled, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name, key = excluded.key, enabled = excluded.enabled",
            params![key.id, key.name, key.key, key.enabled, key.created_at],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn delete_gateway_key(&self, id: &str) -> Result<bool, AppError> {
        let conn = lock_conn!(self.conn);
        let n = conn
            .execute("DELETE FROM gateway_keys WHERE id = ?1", params![id])
            .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(n > 0)
    }

    /// Find an enabled key row by its secret. Used by the gateway auth layer.
    pub fn find_gateway_key(&self, secret: &str) -> Result<Option<GatewayKey>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT id, name, key, enabled, created_at FROM gateway_keys
                 WHERE key = ?1 AND enabled = 1",
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        let mut rows = stmt
            .query_map(params![secret], |row| {
                Ok(GatewayKey {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    key: row.get(2)?,
                    enabled: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })
            .map_err(|e| AppError::Database(e.to_string()))?;
        match rows.next() {
            Some(Ok(k)) => Ok(Some(k)),
            Some(Err(e)) => Err(AppError::Database(e.to_string())),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::db::Database;
    use crate::gateway::types::{Dialect, GatewayChannel, GatewayConfig, GatewayKey};

    fn channel(id: &str) -> GatewayChannel {
        GatewayChannel {
            id: id.into(),
            name: format!("channel-{id}"),
            dialect: Dialect::Responses,
            base_url: "https://up.example.com".into(),
            api_key: "sk-x".into(),
            path_override: None,
            models: vec!["claude-*".into()],
            model_override: Some("upstream-model".into()),
            priority: 1,
            weight: 3,
            enabled: true,
            extra_headers: vec![("x-extra".into(), "1".into())],
        }
    }

    #[test]
    fn channel_crud_round_trips() {
        let db = Database::memory().unwrap();
        db.upsert_gateway_channel(&channel("a")).unwrap();
        db.upsert_gateway_channel(&channel("b")).unwrap();

        let got = db.get_gateway_channels().unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].dialect, Dialect::Responses);
        assert_eq!(got[0].models, vec!["claude-*".to_string()]);
        assert_eq!(got[0].weight, 3);
        assert_eq!(got[0].extra_headers, vec![("x-extra".into(), "1".into())]);

        let mut updated = channel("a");
        updated.name = "renamed".into();
        updated.enabled = false;
        db.upsert_gateway_channel(&updated).unwrap();
        let got = db.get_gateway_channels().unwrap();
        let a = got.iter().find(|c| c.id == "a").unwrap();
        assert_eq!(a.name, "renamed");
        assert!(!a.enabled);

        assert!(db.delete_gateway_channel("a").unwrap());
        assert!(!db.delete_gateway_channel("a").unwrap());
        assert_eq!(db.get_gateway_channels().unwrap().len(), 1);
    }

    #[test]
    fn key_crud_and_lookup() {
        let db = Database::memory().unwrap();
        let key = GatewayKey {
            id: "k1".into(),
            name: "claude-code".into(),
            key: "rd-secret".into(),
            enabled: true,
            created_at: 1,
        };
        db.upsert_gateway_key(&key).unwrap();
        assert_eq!(db.get_gateway_keys().unwrap().len(), 1);

        let found = db.find_gateway_key("rd-secret").unwrap().unwrap();
        assert_eq!(found.name, "claude-code");
        assert!(db.find_gateway_key("wrong").unwrap().is_none());

        let mut disabled = key.clone();
        disabled.enabled = false;
        db.upsert_gateway_key(&disabled).unwrap();
        assert!(db.find_gateway_key("rd-secret").unwrap().is_none());

        assert!(db.delete_gateway_key("k1").unwrap());
    }

    #[test]
    fn config_round_trips_with_default() {
        let db = Database::memory().unwrap();
        let got = db.get_gateway_config().unwrap();
        assert_eq!(got.port, GatewayConfig::default().port);

        let cfg = GatewayConfig {
            enabled: true,
            port: 5000,
            ..Default::default()
        };
        db.set_gateway_config(&cfg).unwrap();
        let got = db.get_gateway_config().unwrap();
        assert!(got.enabled);
        assert_eq!(got.port, 5000);
    }
}
