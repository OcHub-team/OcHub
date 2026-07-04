//! Shared axum server state: a handle to the `routedeck-core` `AppState`.

use std::sync::Arc;

use routedeck_core::db::Database;
use routedeck_core::AppError;

/// Cloneable state handed to every axum handler.
#[derive(Clone)]
pub struct ServerState {
    pub app: Arc<routedeck_core::app_state::AppState>,
}

impl ServerState {
    /// Build state by initializing the SQLite store at the standard location.
    pub fn init() -> Result<Self, AppError> {
        let db = Arc::new(Database::init()?);
        let app = Arc::new(routedeck_core::app_state::AppState::new(db));
        app.bootstrap();
        Ok(Self { app })
    }

    /// Build state from an existing `AppState` (used when the GPUI app hosts the
    /// server in-process and already owns the state).
    pub fn from_app(app: Arc<routedeck_core::app_state::AppState>) -> Self {
        Self { app }
    }
}
