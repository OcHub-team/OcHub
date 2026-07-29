//! Gateway (local relay) DAO: channels, local API keys, and settings blob.

use crate::db::{Database, lock_conn, to_json_string};
use crate::error::AppError;
use crate::gateway::types::{
    Dialect, GatewayChannel, GatewayConfig, GatewayKey, GatewayReasoningConfig, GatewayRoute,
};
use rusqlite::params;

const GATEWAY_CONFIG_KEY: &str = "gateway_config";

fn row_to_channel(row: &rusqlite::Row<'_>) -> rusqlite::Result<GatewayChannel> {
    let dialect_str: String = row.get(3)?;
    let models_str: String = row.get(7)?;
    let extra_headers_str: String = row.get(12)?;
    Ok(GatewayChannel {
        id: row.get(0)?,
        endpoint_id: row.get(1)?,
        name: row.get(2)?,
        dialect: Dialect::parse(&dialect_str).unwrap_or(Dialect::Messages),
        base_url: row.get(4)?,
        api_key: row.get(5)?,
        path_override: row.get(6)?,
        models: serde_json::from_str(&models_str).unwrap_or_default(),
        model_override: row.get(8)?,
        priority: row.get(9)?,
        weight: row.get::<_, i64>(10)?.max(1) as u32,
        enabled: row.get(11)?,
        extra_headers: serde_json::from_str(&extra_headers_str).unwrap_or_default(),
        imported_from: row.get(13)?,
    })
}

const CHANNEL_COLUMNS: &str = "id, endpoint_id, name, dialect, base_url, api_key, path_override, models, \
     model_override, priority, weight, enabled, extra_headers, imported_from";

fn row_to_route(row: &rusqlite::Row<'_>) -> rusqlite::Result<GatewayRoute> {
    let channel_ids: String = row.get(4)?;
    let model_rules: String = row.get(6)?;
    let reasoning: String = row.get(7)?;
    Ok(GatewayRoute {
        id: row.get(0)?,
        name: row.get(1)?,
        website_url: row.get(2)?,
        app_type: row.get(3)?,
        channel_ids: serde_json::from_str(&channel_ids).unwrap_or_default(),
        default_model: row.get(5)?,
        model_rules: serde_json::from_str(&model_rules).unwrap_or_default(),
        reasoning: serde_json::from_str(&reasoning)
            .unwrap_or_else(|_| GatewayReasoningConfig::default()),
        enabled: row.get(8)?,
        created_at: row.get(9)?,
    })
}

const ROUTE_COLUMNS: &str = "id, name, website_url, app_type, channel_ids, default_model, model_rules, reasoning, enabled, created_at";

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
                id, endpoint_id, name, dialect, base_url, api_key, path_override, models,
                model_override, priority, weight, enabled, extra_headers, created_at,
                imported_from
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
            ON CONFLICT(id) DO UPDATE SET
                endpoint_id = excluded.endpoint_id,
                name = excluded.name, dialect = excluded.dialect,
                base_url = excluded.base_url, api_key = excluded.api_key,
                path_override = excluded.path_override, models = excluded.models,
                model_override = excluded.model_override, priority = excluded.priority,
                weight = excluded.weight, enabled = excluded.enabled,
                extra_headers = excluded.extra_headers,
                imported_from = excluded.imported_from",
            params![
                channel.id,
                channel.endpoint_id,
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
                channel.imported_from,
            ],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    /// Persist every channel and its route as one Station transaction.
    ///
    /// `stale_channel_ids` belongs to the same edited Station and is deleted
    /// only after all replacements have been written successfully.
    pub fn save_gateway_station(
        &self,
        channels: &[GatewayChannel],
        route: &GatewayRoute,
        stale_channel_ids: &[String],
    ) -> Result<(), AppError> {
        route.validate().map_err(AppError::InvalidInput)?;
        let known_ids = channels
            .iter()
            .map(|channel| channel.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        if route
            .channel_ids
            .iter()
            .any(|channel_id| !known_ids.contains(channel_id.as_str()))
        {
            return Err(AppError::InvalidInput(
                "Station route references a channel that is not part of the save".to_string(),
            ));
        }

        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|error| AppError::Database(error.to_string()))?;
        for channel in channels {
            tx.execute(
                "INSERT INTO gateway_channels (
                    id, endpoint_id, name, dialect, base_url, api_key, path_override, models,
                    model_override, priority, weight, enabled, extra_headers, created_at,
                    imported_from
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                ON CONFLICT(id) DO UPDATE SET
                    endpoint_id = excluded.endpoint_id,
                    name = excluded.name, dialect = excluded.dialect,
                    base_url = excluded.base_url, api_key = excluded.api_key,
                    path_override = excluded.path_override, models = excluded.models,
                    model_override = excluded.model_override, priority = excluded.priority,
                    weight = excluded.weight, enabled = excluded.enabled,
                    extra_headers = excluded.extra_headers,
                    imported_from = excluded.imported_from",
                params![
                    channel.id,
                    channel.endpoint_id,
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
                    channel.imported_from,
                ],
            )
            .map_err(|error| AppError::Database(error.to_string()))?;
        }
        tx.execute(
            "INSERT INTO gateway_routes (
                id, name, website_url, app_type, channel_ids, default_model, model_rules,
                reasoning, enabled, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name, website_url = excluded.website_url,
                app_type = excluded.app_type,
                channel_ids = excluded.channel_ids,
                default_model = excluded.default_model,
                model_rules = excluded.model_rules,
                reasoning = excluded.reasoning,
                enabled = excluded.enabled",
            params![
                route.id,
                route.name,
                route.website_url,
                route.app_type,
                to_json_string(&route.channel_ids)?,
                route.default_model,
                to_json_string(&route.model_rules)?,
                to_json_string(&route.reasoning)?,
                route.enabled,
                route.created_at,
            ],
        )
        .map_err(|error| AppError::Database(error.to_string()))?;
        for channel_id in stale_channel_ids {
            tx.execute(
                "DELETE FROM gateway_channels WHERE id = ?1",
                params![channel_id],
            )
            .map_err(|error| AppError::Database(error.to_string()))?;
        }
        tx.commit()
            .map_err(|error| AppError::Database(error.to_string()))
    }

    pub fn delete_gateway_channel(&self, id: &str) -> Result<bool, AppError> {
        let mut affected_routes = Vec::new();
        for mut route in self.get_gateway_routes()? {
            let before_channels = route.channel_ids.len();
            let before_rules = route.model_rules.len();
            route.channel_ids.retain(|channel_id| channel_id != id);
            route
                .model_rules
                .retain(|rule| rule.channel_id.as_deref() != Some(id));
            if route.channel_ids.len() != before_channels || route.model_rules.len() != before_rules
            {
                route.validate().map_err(AppError::InvalidInput)?;
                affected_routes.push(route);
            }
        }
        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|e| AppError::Database(e.to_string()))?;
        for route in affected_routes {
            tx.execute(
                "UPDATE gateway_routes
                 SET channel_ids = ?1, model_rules = ?2
                 WHERE id = ?3",
                params![
                    to_json_string(&route.channel_ids)?,
                    to_json_string(&route.model_rules)?,
                    route.id
                ],
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        }
        let n = tx
            .execute("DELETE FROM gateway_channels WHERE id = ?1", params![id])
            .map_err(|e| AppError::Database(e.to_string()))?;
        tx.commit().map_err(|e| AppError::Database(e.to_string()))?;
        Ok(n > 0)
    }

    // -- routes -------------------------------------------------------------

    pub fn get_gateway_routes(&self) -> Result<Vec<GatewayRoute>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {ROUTE_COLUMNS} FROM gateway_routes
                 ORDER BY COALESCE(sort_index, 999999), created_at ASC, id ASC"
            ))
            .map_err(|e| AppError::Database(e.to_string()))?;
        let rows = stmt
            .query_map([], row_to_route)
            .map_err(|e| AppError::Database(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(e.to_string()))
    }

    pub fn get_gateway_route_by_id(&self, id: &str) -> Result<Option<GatewayRoute>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {ROUTE_COLUMNS} FROM gateway_routes WHERE id = ?1"
            ))
            .map_err(|e| AppError::Database(e.to_string()))?;
        let mut rows = stmt
            .query_map(params![id], row_to_route)
            .map_err(|e| AppError::Database(e.to_string()))?;
        match rows.next() {
            Some(Ok(route)) => Ok(Some(route)),
            Some(Err(err)) => Err(AppError::Database(err.to_string())),
            None => Ok(None),
        }
    }

    pub fn get_gateway_route_for_app(
        &self,
        app_type: &str,
    ) -> Result<Option<GatewayRoute>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {ROUTE_COLUMNS} FROM gateway_routes
                 WHERE app_type = ?1 AND enabled = 1
                 ORDER BY COALESCE(sort_index, 999999), created_at ASC, id ASC
                 LIMIT 1"
            ))
            .map_err(|e| AppError::Database(e.to_string()))?;
        let mut rows = stmt
            .query_map(params![app_type], row_to_route)
            .map_err(|e| AppError::Database(e.to_string()))?;
        match rows.next() {
            Some(Ok(route)) => Ok(Some(route)),
            Some(Err(err)) => Err(AppError::Database(err.to_string())),
            None => Ok(None),
        }
    }

    pub fn upsert_gateway_route(&self, route: &GatewayRoute) -> Result<(), AppError> {
        route.validate().map_err(AppError::InvalidInput)?;
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT INTO gateway_routes (
                id, name, website_url, app_type, channel_ids, default_model, model_rules,
                reasoning, enabled, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name, website_url = excluded.website_url,
                app_type = excluded.app_type,
                channel_ids = excluded.channel_ids,
                default_model = excluded.default_model,
                model_rules = excluded.model_rules,
                reasoning = excluded.reasoning,
                enabled = excluded.enabled",
            params![
                route.id,
                route.name,
                route.website_url,
                route.app_type,
                to_json_string(&route.channel_ids)?,
                route.default_model,
                to_json_string(&route.model_rules)?,
                to_json_string(&route.reasoning)?,
                route.enabled,
                route.created_at,
            ],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn delete_gateway_route(&self, id: &str) -> Result<bool, AppError> {
        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|e| AppError::Database(e.to_string()))?;
        tx.execute(
            "UPDATE gateway_keys SET route_id = NULL WHERE route_id = ?1",
            params![id],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        let n = tx
            .execute("DELETE FROM gateway_routes WHERE id = ?1", params![id])
            .map_err(|e| AppError::Database(e.to_string()))?;
        tx.commit().map_err(|e| AppError::Database(e.to_string()))?;
        Ok(n > 0)
    }

    // -- keys ---------------------------------------------------------------

    pub fn get_gateway_keys(&self) -> Result<Vec<GatewayKey>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT id, name, key, route_id, model_policy, enabled, created_at FROM gateway_keys
                 ORDER BY created_at ASC, id ASC",
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                let model_policy: Option<String> = row.get(4)?;
                Ok(GatewayKey {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    key: row.get(2)?,
                    route_id: row.get(3)?,
                    model_policy: model_policy
                        .as_deref()
                        .and_then(|policy| serde_json::from_str(policy).ok()),
                    enabled: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })
            .map_err(|e| AppError::Database(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(e.to_string()))
    }

    pub fn upsert_gateway_key(&self, key: &GatewayKey) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT INTO gateway_keys (
                id, name, key, route_id, model_policy, enabled, created_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name, key = excluded.key,
                route_id = excluded.route_id,
                model_policy = excluded.model_policy,
                enabled = excluded.enabled",
            params![
                key.id,
                key.name,
                key.key,
                key.route_id,
                key.model_policy.as_ref().map(to_json_string).transpose()?,
                key.enabled,
                key.created_at
            ],
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
                "SELECT id, name, key, route_id, model_policy, enabled, created_at FROM gateway_keys
                 WHERE key = ?1 AND enabled = 1",
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        let mut rows = stmt
            .query_map(params![secret], |row| {
                let model_policy: Option<String> = row.get(4)?;
                Ok(GatewayKey {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    key: row.get(2)?,
                    route_id: row.get(3)?,
                    model_policy: model_policy
                        .as_deref()
                        .and_then(|policy| serde_json::from_str(policy).ok()),
                    enabled: row.get(5)?,
                    created_at: row.get(6)?,
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
    use crate::gateway::types::{
        Dialect, GatewayAppModelPolicy, GatewayChannel, GatewayConfig, GatewayKey,
        GatewayModelRule, GatewayReasoningConfig, GatewayRoute,
    };

    fn channel(id: &str) -> GatewayChannel {
        GatewayChannel {
            id: id.into(),
            endpoint_id: Some(format!("endpoint-{id}")),
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
            imported_from: Some("claude:openrouter".into()),
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
    fn station_save_is_atomic_and_round_trips() {
        let db = Database::memory().unwrap();
        let channel = channel("station-a");
        let route = GatewayRoute {
            id: "station-route:test".into(),
            name: "Imported station".into(),
            website_url: Some("https://example.com".into()),
            app_type: None,
            channel_ids: vec![channel.id.clone()],
            default_model: None,
            model_rules: Vec::new(),
            reasoning: GatewayReasoningConfig::default(),
            enabled: true,
            created_at: 1,
        };
        db.save_gateway_station(std::slice::from_ref(&channel), &route, &[])
            .unwrap();
        assert_eq!(db.get_gateway_channels().unwrap().len(), 1);
        assert_eq!(
            db.get_gateway_route_by_id(&route.id)
                .unwrap()
                .unwrap()
                .reasoning
                .mode,
            crate::gateway::types::GatewayReasoningMode::Passthrough
        );

        let mut invalid = route.clone();
        invalid.id = "station-route:invalid".into();
        invalid.channel_ids = vec!["missing".into()];
        assert!(
            db.save_gateway_station(std::slice::from_ref(&channel), &invalid, &[])
                .is_err()
        );
        assert!(
            db.get_gateway_route_by_id("station-route:invalid")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn deleting_channel_atomically_cleans_route_references() {
        let db = Database::memory().unwrap();
        db.upsert_gateway_channel(&channel("a")).unwrap();
        db.upsert_gateway_channel(&channel("b")).unwrap();
        db.upsert_gateway_route(&GatewayRoute {
            id: "route".into(),
            name: "route".into(),
            website_url: None,
            app_type: Some("claude".into()),
            channel_ids: vec!["a".into(), "b".into()],
            default_model: None,
            model_rules: vec![
                GatewayModelRule {
                    model: "a".into(),
                    upstream_model: "model-a".into(),
                    channel_id: Some("a".into()),
                    dialect: None,
                },
                GatewayModelRule {
                    model: "b".into(),
                    upstream_model: "model-b".into(),
                    channel_id: Some("b".into()),
                    dialect: None,
                },
            ],
            reasoning: GatewayReasoningConfig::default(),
            enabled: true,
            created_at: 1,
        })
        .unwrap();

        assert!(db.delete_gateway_channel("a").unwrap());
        let route = db.get_gateway_route_by_id("route").unwrap().unwrap();
        assert_eq!(route.channel_ids, vec!["b"]);
        assert_eq!(route.model_rules.len(), 1);
        assert_eq!(route.model_rules[0].channel_id.as_deref(), Some("b"));
    }

    #[test]
    fn key_crud_and_lookup() {
        let db = Database::memory().unwrap();
        let key = GatewayKey {
            id: "k1".into(),
            name: "claude-code".into(),
            key: "rd-secret".into(),
            route_id: None,
            model_policy: Some(GatewayAppModelPolicy {
                models: vec!["grok-4.5".into()],
                preferred_model: None,
                fallback_model: None,
                model_rules: vec![GatewayModelRule {
                    model: "claude-opus-5".into(),
                    upstream_model: "grok-4.5".into(),
                    channel_id: None,
                    dialect: Some(Dialect::Responses),
                }],
            }),
            enabled: true,
            created_at: 1,
        };
        db.upsert_gateway_key(&key).unwrap();
        assert_eq!(db.get_gateway_keys().unwrap().len(), 1);

        let found = db.find_gateway_key("rd-secret").unwrap().unwrap();
        assert_eq!(found.name, "claude-code");
        assert_eq!(found.model_policy, key.model_policy);
        assert!(db.find_gateway_key("wrong").unwrap().is_none());

        let mut disabled = key.clone();
        disabled.enabled = false;
        db.upsert_gateway_key(&disabled).unwrap();
        assert!(db.find_gateway_key("rd-secret").unwrap().is_none());

        assert!(db.delete_gateway_key("k1").unwrap());
    }

    #[test]
    fn route_crud_round_trips_and_clears_bound_keys() {
        let db = Database::memory().unwrap();
        let route = GatewayRoute {
            id: "route-claude".into(),
            name: "Claude Code 默认路由".into(),
            website_url: Some("https://relay.example.com".into()),
            app_type: Some("claude".into()),
            channel_ids: vec!["a".into()],
            default_model: Some("sonnet".into()),
            model_rules: vec![GatewayModelRule {
                model: "sonnet".into(),
                upstream_model: "claude-sonnet-4-6".into(),
                channel_id: Some("a".into()),
                dialect: Some(Dialect::Responses),
            }],
            reasoning: GatewayReasoningConfig::default(),
            enabled: true,
            created_at: 1,
        };
        db.upsert_gateway_route(&route).unwrap();

        let got = db.get_gateway_route_for_app("claude").unwrap().unwrap();
        assert_eq!(got.id, "route-claude");
        assert_eq!(
            got.website_url.as_deref(),
            Some("https://relay.example.com")
        );
        assert_eq!(got.model_rules, route.model_rules);

        let key = GatewayKey {
            id: "k-route".into(),
            name: "claude".into(),
            key: "rd-route".into(),
            route_id: Some(route.id.clone()),
            model_policy: None,
            enabled: true,
            created_at: 1,
        };
        db.upsert_gateway_key(&key).unwrap();
        assert!(db.delete_gateway_route(&route.id).unwrap());
        let key = db.get_gateway_keys().unwrap().remove(0);
        assert!(key.route_id.is_none());
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
