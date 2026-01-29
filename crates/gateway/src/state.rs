use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{mpsc, oneshot, RwLock};

use moltis_protocol::ConnectParams;

use crate::auth::ResolvedAuth;
use crate::nodes::NodeRegistry;
use crate::pairing::PairingState;
use crate::services::GatewayServices;

// ── Connected client ─────────────────────────────────────────────────────────

/// A WebSocket client currently connected to the gateway.
#[derive(Debug)]
pub struct ConnectedClient {
    pub conn_id: String,
    pub connect_params: ConnectParams,
    /// Channel for sending serialized frames to this client's write loop.
    pub sender: mpsc::UnboundedSender<String>,
    pub connected_at: Instant,
    pub last_activity: Instant,
}

impl ConnectedClient {
    pub fn role(&self) -> &str {
        self.connect_params.role.as_deref().unwrap_or("operator")
    }

    pub fn scopes(&self) -> Vec<&str> {
        self.connect_params
            .scopes
            .as_ref()
            .map(|s| s.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default()
    }

    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes()
            .iter()
            .any(|s| *s == moltis_protocol::scopes::ADMIN || *s == scope)
    }

    /// Send a serialized JSON frame to this client.
    pub fn send(&self, frame: &str) -> bool {
        self.sender.send(frame.to_string()).is_ok()
    }

    /// Touch the activity timestamp.
    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }
}

// ── Dedupe cache ─────────────────────────────────────────────────────────────

struct DedupeEntry {
    inserted_at: Instant,
}

/// Simple TTL-based idempotency cache.
pub struct DedupeCache {
    entries: HashMap<String, DedupeEntry>,
    ttl: std::time::Duration,
    max_entries: usize,
}

impl Default for DedupeCache {
    fn default() -> Self {
        Self::new()
    }
}

impl DedupeCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            ttl: std::time::Duration::from_millis(moltis_protocol::DEDUPE_TTL_MS),
            max_entries: moltis_protocol::DEDUPE_MAX_ENTRIES,
        }
    }

    /// Returns true if the key is a duplicate (already seen within TTL).
    pub fn check_and_insert(&mut self, key: &str) -> bool {
        self.evict_expired();
        if self.entries.contains_key(key) {
            return true;
        }
        if self.entries.len() >= self.max_entries
            && let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, v)| v.inserted_at)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest_key);
            }
        self.entries.insert(
            key.to_string(),
            DedupeEntry {
                inserted_at: Instant::now(),
            },
        );
        false
    }

    fn evict_expired(&mut self) {
        let cutoff = Instant::now() - self.ttl;
        self.entries.retain(|_, v| v.inserted_at > cutoff);
    }
}

// ── Pending node invoke ─────────────────────────────────────────────────────

/// A pending RPC invocation waiting for a node to respond.
pub struct PendingInvoke {
    pub request_id: String,
    pub sender: oneshot::Sender<serde_json::Value>,
    pub created_at: Instant,
}

// ── Gateway state ────────────────────────────────────────────────────────────

/// Shared gateway runtime state, wrapped in Arc for use across async tasks.
pub struct GatewayState {
    /// All connected WebSocket clients, keyed by conn_id.
    pub clients: RwLock<HashMap<String, ConnectedClient>>,
    /// Monotonically increasing sequence counter for broadcast events.
    pub seq: AtomicU64,
    /// Idempotency cache.
    pub dedupe: RwLock<DedupeCache>,
    /// Server version string.
    pub version: String,
    /// Hostname for HelloOk.
    pub hostname: String,
    /// Auth configuration.
    pub auth: ResolvedAuth,
    /// Connected device nodes.
    pub nodes: RwLock<NodeRegistry>,
    /// Device pairing state.
    pub pairing: RwLock<PairingState>,
    /// Pending node invoke requests awaiting results.
    pub pending_invokes: RwLock<HashMap<String, PendingInvoke>>,
    /// Domain services.
    pub services: GatewayServices,
    /// Late-bound chat service override (for circular init).
    pub chat_override: RwLock<Option<Arc<dyn crate::services::ChatService>>>,
}

impl GatewayState {
    pub fn new(auth: ResolvedAuth, services: GatewayServices) -> Arc<Self> {
        let hostname = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "unknown".into());

        Arc::new(Self {
            clients: RwLock::new(HashMap::new()),
            seq: AtomicU64::new(0),
            dedupe: RwLock::new(DedupeCache::new()),
            version: env!("CARGO_PKG_VERSION").to_string(),
            hostname,
            auth,
            nodes: RwLock::new(NodeRegistry::new()),
            pairing: RwLock::new(PairingState::new()),
            pending_invokes: RwLock::new(HashMap::new()),
            services,
            chat_override: RwLock::new(None),
        })
    }

    /// Set a late-bound chat service (for circular init).
    pub async fn set_chat(&self, chat: Arc<dyn crate::services::ChatService>) {
        *self.chat_override.write().await = Some(chat);
    }

    /// Get the active chat service (override or default).
    pub async fn chat(&self) -> Arc<dyn crate::services::ChatService> {
        if let Some(c) = self.chat_override.read().await.as_ref() {
            return Arc::clone(c);
        }
        Arc::clone(&self.services.chat)
    }

    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Register a new client connection.
    pub async fn register_client(&self, client: ConnectedClient) {
        let conn_id = client.conn_id.clone();
        self.clients.write().await.insert(conn_id, client);
    }

    /// Remove a client by conn_id. Returns the removed client if found.
    pub async fn remove_client(&self, conn_id: &str) -> Option<ConnectedClient> {
        self.clients.write().await.remove(conn_id)
    }

    /// Number of connected clients.
    pub async fn client_count(&self) -> usize {
        self.clients.read().await.len()
    }

    /// Close a client: remove from registry, abort if needed.
    pub async fn close_client(&self, conn_id: &str) -> Option<ConnectedClient> {
        // Also unregister from node registry if it was a node.
        self.nodes.write().await.unregister_by_conn(conn_id);
        self.remove_client(conn_id).await
    }
}
