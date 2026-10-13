use rusqlite::{Connection, OptionalExtension, params};

use crate::application::{
    ApplicationError, ApplicationResult, EditorDraft, EditorDraftInput, EditorDraftSummary,
    provider_revision_hash,
};
use crate::db::{Database, lock_conn};
use crate::{AppError, Provider};

pub(crate) fn create_tables(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS editor_drafts (
            app_id TEXT NOT NULL, id TEXT NOT NULL, revision INTEGER NOT NULL,
            updated_at INTEGER NOT NULL, input_json TEXT NOT NULL,
            PRIMARY KEY(app_id, id)
        );
        CREATE TABLE IF NOT EXISTS config_apply_heads (
            app_id TEXT NOT NULL, target_id TEXT NOT NULL, receipt_id TEXT NOT NULL,
            PRIMARY KEY(app_id, target_id)
        );",
    )
    .map_err(|e| AppError::Database(e.to_string()))
}

fn read_draft(conn: &Connection, app: &str, id: &str) -> Result<Option<EditorDraft>, AppError> {
    let row = conn
        .query_row(
            "SELECT revision, updated_at, input_json FROM editor_drafts WHERE app_id=?1 AND id=?2",
            params![app, id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|e| AppError::Database(e.to_string()))?;
    row.map(|(revision, updated_at, json)| {
        let input = serde_json::from_str(&json).map_err(|e| AppError::Database(e.to_string()))?;
        Ok(EditorDraft {
            app: app.into(),
            id: id.into(),
            revision,
            updated_at,
            input,
        })
    })
    .transpose()
}

pub(crate) fn write_provider(
    conn: &Connection,
    app: &str,
    provider: &Provider,
) -> Result<(), AppError> {
    let mut meta = provider.meta.clone().unwrap_or_default();
    let endpoints = std::mem::take(&mut meta.custom_endpoints);
    let settings = serde_json::to_string(&provider.settings_config)
        .map_err(|e| AppError::JsonSerialize { source: e })?;
    let meta = serde_json::to_string(&meta).map_err(|e| AppError::JsonSerialize { source: e })?;
    conn.execute(
        "INSERT INTO providers (id, app_type, name, settings_config, website_url, category,
          created_at, sort_index, notes, icon, icon_color, meta, is_current)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,0)
         ON CONFLICT(id,app_type) DO UPDATE SET name=excluded.name, settings_config=excluded.settings_config,
          website_url=excluded.website_url,category=excluded.category,created_at=excluded.created_at,
          sort_index=excluded.sort_index,notes=excluded.notes,icon=excluded.icon,
          icon_color=excluded.icon_color,meta=excluded.meta",
        params![provider.id, app, provider.name, settings, provider.website_url, provider.category,
            provider.created_at, provider.sort_index.map(|v|v as i64), provider.notes, provider.icon,
            provider.icon_color, meta],
    ).map_err(|e| AppError::Database(e.to_string()))?;
    conn.execute(
        "DELETE FROM provider_endpoints WHERE app_type=?1 AND provider_id=?2",
        params![app, provider.id],
    )
    .map_err(|e| AppError::Database(e.to_string()))?;
    for (url, endpoint) in endpoints {
        conn.execute("INSERT INTO provider_endpoints (provider_id,app_type,url,added_at) VALUES (?1,?2,?3,?4)",
            params![provider.id, app, url, endpoint.added_at]).map_err(|e|AppError::Database(e.to_string()))?;
    }
    Ok(())
}

pub(crate) fn set_current(conn: &Connection, app: &str, id: Option<&str>) -> Result<(), AppError> {
    conn.execute(
        "UPDATE providers SET is_current=CASE WHEN id=?2 THEN 1 ELSE 0 END WHERE app_type=?1",
        params![app, id],
    )
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

fn read_provider(conn: &Connection, app: &str, id: &str) -> Result<Option<Provider>, AppError> {
    let provider=conn.query_row("SELECT name,settings_config,website_url,category,created_at,sort_index,notes,icon,icon_color,meta FROM providers WHERE app_type=?1 AND id=?2",params![app,id],|row|{
        let settings:String=row.get(1)?;let meta:String=row.get(9)?;
        Ok(Provider{id:id.into(),name:row.get(0)?,settings_config:serde_json::from_str(&settings).unwrap_or_default(),website_url:row.get(2)?,category:row.get(3)?,created_at:row.get(4)?,sort_index:row.get::<_,Option<i64>>(5)?.map(|n|n as usize),notes:row.get(6)?,icon:row.get(7)?,icon_color:row.get(8)?,meta:Some(serde_json::from_str(&meta).unwrap_or_default())})
    }).optional().map_err(|e|AppError::Database(e.to_string()))?;
    let Some(mut provider) = provider else {
        return Ok(None);
    };
    let mut stmt = conn
        .prepare("SELECT url,added_at FROM provider_endpoints WHERE app_type=?1 AND provider_id=?2")
        .map_err(|e| AppError::Database(e.to_string()))?;
    let rows = stmt
        .query_map(params![app, id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })
        .map_err(|e| AppError::Database(e.to_string()))?;
    for row in rows {
        let (url, added_at) = row.map_err(|e| AppError::Database(e.to_string()))?;
        provider
            .meta
            .as_mut()
            .expect("meta populated")
            .custom_endpoints
            .insert(
                url.clone(),
                crate::settings::CustomEndpoint {
                    url,
                    added_at: added_at.unwrap_or(0),
                    last_used: None,
                },
            );
    }
    Ok(Some(provider))
}

fn write_common(
    conn: &Connection,
    app: &str,
    snippet: Option<&str>,
    cleared: bool,
) -> Result<(), AppError> {
    let key = format!("common_config_{app}");
    match snippet {
        Some(value) => {
            conn.execute(
                "INSERT OR REPLACE INTO settings(key,value) VALUES (?1,?2)",
                params![key, value],
            )?;
        }
        None => {
            conn.execute("DELETE FROM settings WHERE key=?1", [key])?;
        }
    }
    let key = format!("common_config_{app}_cleared");
    if cleared {
        conn.execute(
            "INSERT OR REPLACE INTO settings(key,value) VALUES (?1,'true')",
            [key],
        )?;
    } else {
        conn.execute("DELETE FROM settings WHERE key=?1", [key])?;
    }
    Ok(())
}

impl Database {
    pub fn list_editor_drafts(&self, app: &str) -> Result<Vec<EditorDraftSummary>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare("SELECT id FROM editor_drafts WHERE app_id=?1 ORDER BY updated_at DESC, id")
            .map_err(|e| AppError::Database(e.to_string()))?;
        let ids = stmt
            .query_map([app], |row| row.get::<_, String>(0))
            .map_err(|e| AppError::Database(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(e.to_string()))?;
        ids.into_iter()
            .map(|id| {
                read_draft(&conn, app, &id)?
                    .map(|draft| EditorDraftSummary::from(&draft))
                    .ok_or_else(|| AppError::Database("draft disappeared while reading".into()))
            })
            .collect()
    }

    pub fn get_editor_draft(&self, app: &str, id: &str) -> Result<Option<EditorDraft>, AppError> {
        let conn = lock_conn!(self.conn);
        read_draft(&conn, app, id)
    }

    pub fn upsert_editor_draft(
        &self,
        app: &str,
        id: &str,
        expected: Option<u64>,
        mut input: EditorDraftInput,
    ) -> ApplicationResult<EditorDraft> {
        // Capture before taking the connection lock, since provider reads use that same lock.
        if expected.is_none()
            && input.base_provider_hash.is_none()
            && let Some(original) = input.original_provider_id.as_deref()
        {
            input.base_provider_hash = self
                .get_provider_by_id(original, app)?
                .as_ref()
                .map(provider_revision_hash);
        }
        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|e| AppError::Database(e.to_string()))?;
        let previous = read_draft(&tx, app, id)?;
        if previous.as_ref().map(|d| d.revision) != expected {
            return Err(ApplicationError::ResourceConflict(
                "draft changed; reopen it before saving".into(),
            ));
        }
        if let Some(previous) = previous.as_ref()
            && (previous.input.original_provider_id != input.original_provider_id
                || previous.input.base_provider_hash != input.base_provider_hash)
        {
            return Err(ApplicationError::ResourceConflict(
                "draft base changed; reopen the source connection".into(),
            ));
        }
        let json =
            serde_json::to_string(&input).map_err(|e| AppError::JsonSerialize { source: e })?;
        if json.len() > 16 * 1024 * 1024 {
            return Err(ApplicationError::InvalidInput(
                "draft exceeds 16 MiB".into(),
            ));
        }
        let revision = expected
            .unwrap_or(0)
            .checked_add(1)
            .filter(|v| *v <= i64::MAX as u64)
            .ok_or_else(|| ApplicationError::InvalidInput("draft revision overflow".into()))?;
        let updated_at = chrono::Utc::now().timestamp_millis();
        tx.execute("INSERT INTO editor_drafts (app_id,id,revision,updated_at,input_json) VALUES (?1,?2,?3,?4,?5)
            ON CONFLICT(app_id,id) DO UPDATE SET revision=excluded.revision,updated_at=excluded.updated_at,input_json=excluded.input_json",
            params![app,id,revision as i64,updated_at,json]).map_err(|e|AppError::Database(e.to_string()))?;
        tx.commit().map_err(|e| AppError::Database(e.to_string()))?;
        Ok(EditorDraft {
            app: app.into(),
            id: id.into(),
            revision,
            updated_at,
            input,
        })
    }

    pub fn delete_editor_draft(&self, app: &str, id: &str, revision: u64) -> ApplicationResult<()> {
        let conn = lock_conn!(self.conn);
        let count = conn
            .execute(
                "DELETE FROM editor_drafts WHERE app_id=?1 AND id=?2 AND revision=?3",
                params![app, id, revision as i64],
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        if count != 1 {
            return Err(ApplicationError::ResourceConflict(
                "draft changed; refusing to discard newer edits".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn config_apply_head(
        &self,
        app: &str,
        target: &str,
    ) -> Result<Option<String>, AppError> {
        let conn = lock_conn!(self.conn);
        conn.query_row(
            "SELECT receipt_id FROM config_apply_heads WHERE app_id=?1 AND target_id=?2",
            params![app, target],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| AppError::Database(e.to_string()))
    }

    pub(crate) fn commit_editor_application(
        &self,
        draft: &EditorDraft,
        provider: &Provider,
        receipt: &str,
        switch: bool,
        updates: &[Provider],
        baselines: &[Provider],
    ) -> ApplicationResult<()> {
        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|e| AppError::Database(e.to_string()))?;
        if read_draft(&tx, &draft.app, &draft.id)?
            .as_ref()
            .map(|d| d.revision)
            != Some(draft.revision)
        {
            return Err(ApplicationError::ResourceConflict(
                "draft changed while applying".into(),
            ));
        }
        let original = draft
            .input
            .original_provider_id
            .as_deref()
            .map(|id| read_provider(&tx, &draft.app, id))
            .transpose()?
            .flatten();
        if original.as_ref().map(provider_revision_hash) != draft.input.base_provider_hash {
            return Err(ApplicationError::ResourceConflict(
                "source connection changed while applying".into(),
            ));
        }
        for baseline in baselines {
            if read_provider(&tx, &draft.app, &baseline.id)?
                .as_ref()
                .map(provider_revision_hash)
                != Some(provider_revision_hash(baseline))
            {
                return Err(ApplicationError::ResourceConflict(
                    "a related connection changed while applying".into(),
                ));
            }
        }
        if draft.input.original_provider_id.as_deref() != Some(&provider.id)
            && read_provider(&tx, &draft.app, &provider.id)?.is_some()
        {
            return Err(ApplicationError::AlreadyExists {
                kind: "provider",
                id: provider.id.clone(),
            });
        }
        if let Some(common) = &draft.input.common_config {
            let current: Option<String> = tx
                .query_row(
                    "SELECT value FROM settings WHERE key=?1",
                    [format!("common_config_{}", draft.app)],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| AppError::Database(e.to_string()))?;
            if current != common.base_snippet {
                return Err(ApplicationError::ResourceConflict(
                    "shared configuration changed while applying".into(),
                ));
            }
            write_common(
                &tx,
                &draft.app,
                (!common.snippet.trim().is_empty()).then_some(common.snippet.as_str()),
                common.snippet.trim().is_empty(),
            )?;
        }
        for update in updates {
            if update.id != provider.id {
                write_provider(&tx, &draft.app, update)?;
            }
        }
        write_provider(&tx, &draft.app, provider)?;
        if let Some(old) = draft
            .input
            .original_provider_id
            .as_deref()
            .filter(|old| *old != provider.id)
        {
            tx.execute(
                "DELETE FROM providers WHERE app_type=?1 AND id=?2",
                params![draft.app, old],
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        }
        let omo = draft.app == "opencode"
            && matches!(provider.category.as_deref(), Some("omo" | "omo-slim"));
        if switch || omo {
            set_current(&tx, &draft.app, Some(&provider.id))?;
        }
        let target = if switch {
            "*"
        } else if omo {
            "omo-profiles"
        } else {
            provider.id.as_str()
        };
        tx.execute(
            "INSERT INTO config_apply_heads (app_id,target_id,receipt_id) VALUES (?1,?2,?3)
            ON CONFLICT(app_id,target_id) DO UPDATE SET receipt_id=excluded.receipt_id",
            params![draft.app, target, receipt],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        tx.execute(
            "DELETE FROM editor_drafts WHERE app_id=?1 AND id=?2 AND revision=?3",
            params![draft.app, draft.id, draft.revision as i64],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        tx.commit().map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    pub(crate) fn restore_editor_application(
        &self,
        context: &crate::application::config_commits::ApplyContext,
        receipt: &str,
    ) -> ApplicationResult<()> {
        let mut conn = lock_conn!(self.conn);
        let tx = conn
            .transaction()
            .map_err(|e| AppError::Database(e.to_string()))?;
        let head: Option<String> = tx
            .query_row(
                "SELECT receipt_id FROM config_apply_heads WHERE app_id=?1 AND target_id=?2",
                params![context.app, context.target_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| AppError::Database(e.to_string()))?;
        if head.as_deref() != Some(receipt) {
            return Err(ApplicationError::ResourceConflict(
                "a newer application replaced this receipt".into(),
            ));
        }
        for provider in &context.after_providers {
            if read_provider(&tx, &context.app, &provider.id)?
                .as_ref()
                .map(provider_revision_hash)
                != Some(provider_revision_hash(provider))
            {
                return Err(ApplicationError::ResourceConflict(
                    "connection changed while undoing".into(),
                ));
            }
        }
        for before in &context.before_providers {
            if context.after_providers.iter().any(|p| p.id == before.id) {
                continue;
            }
            let removed = context.draft.input.original_provider_id.as_deref() == Some(&before.id)
                && before.id != context.provider_id;
            let expected = (!removed).then(|| provider_revision_hash(before));
            if read_provider(&tx, &context.app, &before.id)?
                .as_ref()
                .map(provider_revision_hash)
                != expected
            {
                return Err(ApplicationError::ResourceConflict(
                    "a connection that undo would restore has changed".into(),
                ));
            }
        }
        for provider in &context.after_providers {
            if !context.before_providers.iter().any(|p| p.id == provider.id) {
                tx.execute(
                    "DELETE FROM providers WHERE app_type=?1 AND id=?2",
                    params![context.app, provider.id],
                )
                .map_err(|e| AppError::Database(e.to_string()))?;
            }
        }
        for provider in &context.before_providers {
            write_provider(&tx, &context.app, provider)?;
        }
        if context.switch || context.omo {
            set_current(&tx, &context.app, context.before_db_current.as_deref())?;
        }
        if context.draft.input.common_config.is_some() {
            write_common(
                &tx,
                &context.app,
                context.common_before.as_deref(),
                context.common_cleared_before,
            )?;
        }
        match &context.previous_head {
            Some(head) => {
                tx.execute(
                    "UPDATE config_apply_heads SET receipt_id=?3 WHERE app_id=?1 AND target_id=?2",
                    params![context.app, context.target_id, head],
                )
                .map_err(|e| AppError::Database(e.to_string()))?;
            }
            None => {
                tx.execute(
                    "DELETE FROM config_apply_heads WHERE app_id=?1 AND target_id=?2",
                    params![context.app, context.target_id],
                )
                .map_err(|e| AppError::Database(e.to_string()))?;
            }
        }
        // Undo restores the applied draft for further editing without replacing a newer draft.
        let mut restored = context.draft.clone();
        restored.revision += 1;
        restored.updated_at = chrono::Utc::now().timestamp_millis();
        let input = serde_json::to_string(&restored.input)
            .map_err(|source| AppError::JsonSerialize { source })?;
        tx.execute("INSERT OR IGNORE INTO editor_drafts(app_id,id,revision,updated_at,input_json) VALUES (?1,?2,?3,?4,?5)",params![restored.app,restored.id,restored.revision as i64,restored.updated_at,input]).map_err(|e|AppError::Database(e.to_string()))?;
        tx.commit().map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }
}
