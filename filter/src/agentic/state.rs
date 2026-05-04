// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Praxis Contributors

//! Local in-memory state store for agentic protocol sessions.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

// -----------------------------------------------------------------------------
// Tool Catalog Entry
// -----------------------------------------------------------------------------

/// Entry in the tool catalog.
#[derive(Debug, Clone)]
pub(crate) struct ToolEntry {
    /// Exposed (prefixed) tool name visible to clients.
    pub exposed_name: String,
    /// Original tool name on the backend.
    pub original_name: String,
    /// Backend server name from config.
    pub server_name: String,
    /// Tool schema from backend `tools/list`.
    pub schema: serde_json::Value,
    /// Optional description.
    pub description: Option<String>,
}

// -----------------------------------------------------------------------------
// Gateway Session
// -----------------------------------------------------------------------------

/// MCP gateway session info.
#[derive(Debug, Clone)]
pub(crate) struct GatewaySession {
    /// Gateway-issued session ID.
    pub session_id: String,
    /// When the session was created.
    pub created_at: Instant,
    /// When the session was last used.
    pub last_used: Instant,
    /// Protocol version negotiated.
    pub protocol_version: Option<String>,
}

// -----------------------------------------------------------------------------
// Backend Session
// -----------------------------------------------------------------------------

/// Backend session mapping.
#[derive(Debug, Clone)]
pub(crate) struct BackendSession {
    /// Backend-issued `Mcp-Session-Id`.
    pub backend_session_id: String,
    /// When this backend session was created.
    pub created_at: Instant,
}

// -----------------------------------------------------------------------------
// Task Route
// -----------------------------------------------------------------------------

/// A2A task routing entry.
#[derive(Debug, Clone)]
pub(crate) struct TaskRoute {
    /// Backend/cluster that owns this task.
    pub backend: String,
    /// Context ID if known.
    pub context_id: Option<String>,
    /// When this mapping was created.
    pub created_at: Instant,
}

// -----------------------------------------------------------------------------
// Local State Store
// -----------------------------------------------------------------------------

/// Local in-memory state store for gateway sessions, backend session
/// mappings, and A2A task routing.
#[derive(Debug, Clone)]
pub(crate) struct LocalStateStore {
    /// Gateway sessions: `session_id` -> [`GatewaySession`].
    gateway_sessions: Arc<RwLock<HashMap<String, GatewaySession>>>,
    /// Backend session map: `(gateway_session_id, server_name)` -> [`BackendSession`].
    backend_sessions: Arc<RwLock<HashMap<(String, String), BackendSession>>>,
    /// A2A task routing: `task_id` -> [`TaskRoute`].
    task_routes: Arc<RwLock<HashMap<String, TaskRoute>>>,
}

impl LocalStateStore {
    /// Create a new empty local state store.
    pub(crate) fn new() -> Self {
        Self {
            gateway_sessions: Arc::new(RwLock::new(HashMap::new())),
            backend_sessions: Arc::new(RwLock::new(HashMap::new())),
            task_routes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Store a gateway session.
    pub(crate) fn put_gateway_session(&self, session: GatewaySession) {
        let mut sessions = self.gateway_sessions.write().expect("lock poisoned");
        sessions.insert(session.session_id.clone(), session);
    }

    /// Get a gateway session by ID.
    pub(crate) fn get_gateway_session(&self, session_id: &str) -> Option<GatewaySession> {
        let sessions = self.gateway_sessions.read().expect("lock poisoned");
        sessions.get(session_id).cloned()
    }

    /// Touch a gateway session (update `last_used`).
    pub(crate) fn touch_gateway_session(&self, session_id: &str) {
        let mut sessions = self.gateway_sessions.write().expect("lock poisoned");
        if let Some(session) = sessions.get_mut(session_id) {
            session.last_used = Instant::now();
        }
    }

    /// Delete a gateway session and its backend session mappings.
    pub(crate) fn delete_gateway_session(&self, session_id: &str) {
        {
            let mut sessions = self.gateway_sessions.write().expect("lock poisoned");
            sessions.remove(session_id);
        }
        {
            let mut backend = self.backend_sessions.write().expect("lock poisoned");
            backend.retain(|(gw_sid, _), _| gw_sid != session_id);
        }
    }

    /// Store a backend session mapping.
    pub(crate) fn put_backend_session(
        &self,
        gateway_session_id: &str,
        server_name: &str,
        backend_session_id: &str,
    ) {
        let mut sessions = self.backend_sessions.write().expect("lock poisoned");
        sessions.insert(
            (gateway_session_id.to_owned(), server_name.to_owned()),
            BackendSession {
                backend_session_id: backend_session_id.to_owned(),
                created_at: Instant::now(),
            },
        );
    }

    /// Get a backend session ID for the given gateway session and server.
    pub(crate) fn get_backend_session(
        &self,
        gateway_session_id: &str,
        server_name: &str,
    ) -> Option<String> {
        let sessions = self.backend_sessions.read().expect("lock poisoned");
        sessions
            .get(&(gateway_session_id.to_owned(), server_name.to_owned()))
            .map(|s| s.backend_session_id.clone())
    }

    /// Remove a backend session mapping (e.g., on 404).
    pub(crate) fn remove_backend_session(
        &self,
        gateway_session_id: &str,
        server_name: &str,
    ) {
        let mut sessions = self.backend_sessions.write().expect("lock poisoned");
        sessions.remove(&(gateway_session_id.to_owned(), server_name.to_owned()));
    }

    /// Store a task route.
    pub(crate) fn put_task_route(&self, task_id: &str, backend: &str, context_id: Option<&str>) {
        let mut routes = self.task_routes.write().expect("lock poisoned");
        routes.insert(
            task_id.to_owned(),
            TaskRoute {
                backend: backend.to_owned(),
                context_id: context_id.map(str::to_owned),
                created_at: Instant::now(),
            },
        );
    }

    /// Look up a task route.
    pub(crate) fn get_task_route(&self, task_id: &str) -> Option<TaskRoute> {
        let routes = self.task_routes.read().expect("lock poisoned");
        routes.get(task_id).cloned()
    }

    /// Remove a task route.
    pub(crate) fn remove_task_route(&self, task_id: &str) {
        let mut routes = self.task_routes.write().expect("lock poisoned");
        routes.remove(task_id);
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests"
)]
mod tests {
    use super::*;

    #[test]
    fn gateway_session_roundtrip() {
        let store = LocalStateStore::new();
        let session = GatewaySession {
            session_id: "gw-abc".to_owned(),
            created_at: Instant::now(),
            last_used: Instant::now(),
            protocol_version: Some("2025-03-26".to_owned()),
        };

        store.put_gateway_session(session);
        let got = store.get_gateway_session("gw-abc").unwrap();
        assert_eq!(got.session_id, "gw-abc");
        assert_eq!(got.protocol_version.as_deref(), Some("2025-03-26"));
    }

    #[test]
    fn gateway_session_not_found() {
        let store = LocalStateStore::new();
        assert!(store.get_gateway_session("missing").is_none());
    }

    #[test]
    fn delete_gateway_session_removes_backend_sessions() {
        let store = LocalStateStore::new();
        store.put_gateway_session(GatewaySession {
            session_id: "gw-1".to_owned(),
            created_at: Instant::now(),
            last_used: Instant::now(),
            protocol_version: None,
        });
        store.put_backend_session("gw-1", "weather", "be-111");
        store.put_backend_session("gw-1", "calendar", "be-222");

        store.delete_gateway_session("gw-1");

        assert!(store.get_gateway_session("gw-1").is_none());
        assert!(store.get_backend_session("gw-1", "weather").is_none());
        assert!(store.get_backend_session("gw-1", "calendar").is_none());
    }

    #[test]
    fn backend_session_roundtrip() {
        let store = LocalStateStore::new();
        store.put_backend_session("gw-1", "weather", "be-abc");

        let got = store.get_backend_session("gw-1", "weather").unwrap();
        assert_eq!(got, "be-abc");
    }

    #[test]
    fn remove_backend_session() {
        let store = LocalStateStore::new();
        store.put_backend_session("gw-1", "weather", "be-abc");
        store.remove_backend_session("gw-1", "weather");

        assert!(store.get_backend_session("gw-1", "weather").is_none());
    }

    #[test]
    fn task_route_roundtrip() {
        let store = LocalStateStore::new();
        store.put_task_route("task-1", "backend-a", Some("ctx-1"));

        let route = store.get_task_route("task-1").unwrap();
        assert_eq!(route.backend, "backend-a");
        assert_eq!(route.context_id.as_deref(), Some("ctx-1"));
    }

    #[test]
    fn task_route_without_context() {
        let store = LocalStateStore::new();
        store.put_task_route("task-2", "backend-b", None);

        let route = store.get_task_route("task-2").unwrap();
        assert_eq!(route.backend, "backend-b");
        assert!(route.context_id.is_none());
    }

    #[test]
    fn remove_task_route() {
        let store = LocalStateStore::new();
        store.put_task_route("task-1", "backend-a", None);
        store.remove_task_route("task-1");

        assert!(store.get_task_route("task-1").is_none());
    }

    #[test]
    fn touch_gateway_session_updates_last_used() {
        let store = LocalStateStore::new();
        let now = Instant::now();
        store.put_gateway_session(GatewaySession {
            session_id: "gw-touch".to_owned(),
            created_at: now,
            last_used: now,
            protocol_version: None,
        });

        store.touch_gateway_session("gw-touch");

        let got = store.get_gateway_session("gw-touch").unwrap();
        assert!(got.last_used >= now);
    }

    #[test]
    fn touch_nonexistent_session_is_noop() {
        let store = LocalStateStore::new();
        store.touch_gateway_session("nonexistent");
    }
}
