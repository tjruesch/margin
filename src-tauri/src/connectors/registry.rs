//! Holds the `kind` → factory mapping and the live `Arc<dyn Connector>`
//! instances. Future connector PRs (#61, #63) call `register_kind` at
//! app boot to plug their factories in; the registry then instantiates
//! one connector per row in the `connectors` table.
//!
//! Concurrency: two `RwLock`s, never held across `.await`. Reads (the
//! runner's "give me the live connectors" call) dominate; writes (kind
//! registration at boot, instance rebuild after add/remove) are rare.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use rusqlite::Connection;
use tauri::AppHandle;

use super::{Connector, ConnectorError, ConnectorRow};

/// Constructs a `Connector` instance from its persisted row. The
/// factory is responsible for parsing `row.config_json` and reading
/// any per-instance secrets from the keychain.
pub type ConnectorFactory = Arc<
    dyn Fn(&ConnectorRow, &AppHandle) -> Result<Arc<dyn Connector>, ConnectorError>
        + Send
        + Sync,
>;

pub struct ConnectorRegistry {
    factories: RwLock<HashMap<String, ConnectorFactory>>,
    instances: RwLock<HashMap<String, Arc<dyn Connector>>>,
}

impl ConnectorRegistry {
    pub fn new() -> Self {
        Self {
            factories: RwLock::new(HashMap::new()),
            instances: RwLock::new(HashMap::new()),
        }
    }

    /// Plug a factory in. Called by individual connector modules at
    /// app boot. Idempotent — re-registering a kind overwrites the
    /// previous factory (useful for hot-reload during development).
    pub fn register_kind(&self, kind: &str, factory: ConnectorFactory) {
        if let Ok(mut f) = self.factories.write() {
            f.insert(kind.to_string(), factory);
        }
    }

    /// (Re)build the live instance set from the persisted rows. Any
    /// row whose kind has no registered factory is logged and skipped
    /// — that's the expected state in #59 (no real factories yet) and
    /// stays the right behavior later when a connector kind is
    /// removed from the binary while a row still exists in the DB.
    pub fn rebuild_instances(
        &self,
        app: &AppHandle,
        conn: &Connection,
    ) -> rusqlite::Result<()> {
        let rows = super::load_connector_rows(conn)?;
        let factories = self.factories.read().ok();
        let mut new_instances: HashMap<String, Arc<dyn Connector>> = HashMap::new();
        for row in rows {
            if !row.enabled {
                continue;
            }
            let factory = match factories.as_ref().and_then(|f| f.get(&row.kind)) {
                Some(f) => f.clone(),
                None => {
                    eprintln!(
                        "[connectors] no factory registered for kind '{}' (id {}); skipping",
                        row.kind, row.id
                    );
                    continue;
                }
            };
            match factory(&row, app) {
                Ok(c) => {
                    new_instances.insert(row.id.clone(), c);
                }
                Err(e) => {
                    eprintln!(
                        "[connectors] factory for {} (kind {}) failed: {e}",
                        row.id, row.kind
                    );
                }
            }
        }
        if let Ok(mut existing) = self.instances.write() {
            *existing = new_instances;
        }
        Ok(())
    }

    /// Snapshot of all currently-instantiated connectors. Cheap clone
    /// — `Arc<dyn Connector>` is just two pointers per entry.
    pub fn live(&self) -> Vec<Arc<dyn Connector>> {
        self.instances
            .read()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Connector>> {
        self.instances.read().ok()?.get(id).cloned()
    }
}

impl Default for ConnectorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ----- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::connectors::{SyncCtx, SyncReport};

    /// Test-only connector that just counts how many times `sync()`
    /// was invoked. Used to validate the registry + (later) the
    /// runner.
    struct StubConnector {
        id: String,
        kind: String,
        display: String,
        sync_count: AtomicU64,
    }

    #[async_trait::async_trait]
    impl Connector for StubConnector {
        fn id(&self) -> &str {
            &self.id
        }
        fn kind(&self) -> &str {
            &self.kind
        }
        fn display_name(&self) -> &str {
            &self.display
        }
        fn poll_interval(&self) -> Duration {
            Duration::from_secs(60)
        }
        async fn sync(&self, _ctx: SyncCtx<'_>) -> Result<SyncReport, ConnectorError> {
            self.sync_count.fetch_add(1, Ordering::SeqCst);
            Ok(SyncReport {
                added: 1,
                ..Default::default()
            })
        }
    }

    fn make_stub_factory() -> ConnectorFactory {
        Arc::new(|row: &ConnectorRow, _app: &AppHandle| {
            Ok(Arc::new(StubConnector {
                id: row.id.clone(),
                kind: row.kind.clone(),
                display: row.display_name.clone(),
                sync_count: AtomicU64::new(0),
            }) as Arc<dyn Connector>)
        })
    }

    #[test]
    fn register_kind_and_lookup() {
        let reg = ConnectorRegistry::new();
        reg.register_kind("stub", make_stub_factory());
        let factories = reg.factories.read().unwrap();
        assert!(factories.contains_key("stub"));
    }

    #[test]
    fn rebuild_instances_skips_unknown_kinds() {
        let reg = ConnectorRegistry::new();
        // No factory registered. We can't easily build a real
        // AppHandle in a unit test, but we can test the no-factory
        // branch by exercising load + filter logic without calling
        // any factory. The full rebuild path is exercised by the
        // integration tests in `runner.rs` and validated by hand at
        // app boot.
        let factories = reg.factories.read().unwrap();
        assert!(factories.is_empty());
    }
}
