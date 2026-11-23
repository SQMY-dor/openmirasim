//! Session orchestration (W0-4).
//!
//! The [`Orchestrator`] owns one actor per live session (see `actor.rs`), the per-session
//! broadcast hub, and the harness registry. It is the API the REST / WebSocket layer
//! (W0-5) calls; it never exposes harness internals.
//!
//! Invariants:
//! - Every event of a live session is appended by its actor: persisted first, then
//!   broadcast. Subscribers therefore see a gapless stream (see [`Subscription`]).
//! - Approvals are resolved through the actor with a first-writer-wins update, so two
//!   members answering at once get exactly one success.
//! - Phase 0: one prompt at a time per session; a prompt while a turn runs is refused
//!   with a conflict. Queue & Steer arrives in Phase 2 (W2-3).

mod actor;
mod hub;

pub use hub::Subscription;

use crate::db::Db;
use actor::{ActorSpec, Command};
use hub::EventHub;
use om_core::approval::{ApprovalDecision, ApprovalRecord};
use om_core::content::Prompt;
use om_core::event::EventPayload;
use om_core::harness::Harness;
use om_core::ids::{ApprovalId, MemberId, SessionId, SpaceId, TurnId};
use om_core::sandbox::{DirectSandbox, Sandbox, SandboxMode};
use om_core::session::{SessionConfig, SessionStatus, SessionSummary};
use om_core::{CoreError, CoreResult};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

/// Registered harnesses, keyed by [`Harness::id`].
#[derive(Default, Clone)]
pub struct HarnessRegistry {
    harnesses: HashMap<String, Arc<dyn Harness>>,
}

impl HarnessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers (or replaces) a harness under its id.
    pub fn register(&mut self, harness: Arc<dyn Harness>) -> &mut Self {
        self.harnesses.insert(harness.id().to_owned(), harness);
        self
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Harness>> {
        self.harnesses.get(id).cloned()
    }

    /// Like [`HarnessRegistry::get`] but `NotFound` when missing.
    pub fn require(&self, id: &str) -> CoreResult<Arc<dyn Harness>> {
        self.get(id)
            .ok_or_else(|| CoreError::NotFound(format!("harness `{id}`")))
    }

    /// Registered ids, sorted.
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<_> = self.harnesses.keys().cloned().collect();
        ids.sort();
        ids
    }
}

/// Orchestrator settings.
#[derive(Debug, Clone)]
pub struct OrchestratorConfig {
    /// Permit `SandboxMode::Direct` (no isolation). Only for single-developer setups
    /// bound to loopback; the server binary must refuse it otherwise.
    pub allow_direct_sandbox: bool,
    /// Per-session broadcast buffer; slower subscribers replay from SQLite.
    pub broadcast_capacity: usize,
    /// Per-session command queue depth.
    pub command_capacity: usize,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            allow_direct_sandbox: false,
            broadcast_capacity: 1024,
            command_capacity: 64,
        }
    }
}

type Live = Arc<Mutex<HashMap<SessionId, mpsc::Sender<Command>>>>;

/// Entry point for session lifecycle, prompts, approvals and subscriptions.
#[derive(Clone)]
pub struct Orchestrator {
    db: Db,
    hub: Arc<EventHub>,
    registry: HarnessRegistry,
    config: OrchestratorConfig,
    live: Live,
    bwrap: Option<Arc<dyn Sandbox>>,
}

impl Orchestrator {
    pub fn new(db: Db, registry: HarnessRegistry, config: OrchestratorConfig) -> Self {
        Self {
            db,
            hub: Arc::new(EventHub::new(config.broadcast_capacity)),
            registry,
            config,
            live: Live::default(),
            bwrap: None,
        }
    }

    /// Enables `SandboxMode::Bwrap` sessions with the given (probed) sandbox, e.g.
    /// `BwrapSandbox::default().probe().await?`.
    pub fn with_bwrap_sandbox(mut self, sandbox: Arc<dyn Sandbox>) -> Self {
        debug_assert_eq!(sandbox.mode(), SandboxMode::Bwrap);
        self.bwrap = Some(sandbox);
        self
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// The harness registry (for listing available harnesses).
    pub fn registry(&self) -> &HarnessRegistry {
        &self.registry
    }

    /// Registered harness ids, sorted.
    pub fn harness_ids(&self) -> Vec<String> {
        self.registry.ids()
    }

    /// Marks sessions left live by a previous process as `Failed`. Call once at startup,
    /// before accepting requests.
    pub async fn recover(&self) -> CoreResult<Vec<SessionId>> {
        self.db.recover_after_restart().await
    }

    fn sandbox_for(&self, mode: SandboxMode) -> CoreResult<Arc<dyn Sandbox>> {
        match mode {
            SandboxMode::Direct if self.config.allow_direct_sandbox => Ok(Arc::new(DirectSandbox)),
            SandboxMode::Direct => Err(CoreError::InvalidInput(
                "sandbox mode `direct` is disabled on this server".into(),
            )),
            SandboxMode::Bwrap => self.bwrap.clone().ok_or_else(|| CoreError::Unsupported {
                harness: "server".into(),
                capability: "bwrap sandbox (not configured on this server)",
            }),
        }
    }

    fn harness_for(&self, id: &str) -> CoreResult<Arc<dyn Harness>> {
        self.registry.require(id)
    }

    fn spawn_actor(&self, spec: ActorSpec) {
        let (tx, rx) = mpsc::channel(self.config.command_capacity);
        let id = spec.session_id.clone();
        self.live
            .lock()
            .expect("live poisoned")
            .insert(id.clone(), tx);
        // Weak: once every Orchestrator clone is dropped, the command senders go away and
        // actors shut their agents down instead of being kept alive by this callback.
        let live = Arc::downgrade(&self.live);
        let on_exit = move || {
            if let Some(live) = live.upgrade()
                && let Ok(mut map) = live.lock()
            {
                map.remove(&id);
            }
        };
        tokio::spawn(actor::run(
            self.db.clone(),
            self.hub.clone(),
            spec,
            rx,
            on_exit,
        ));
    }

    fn live_sender(&self, id: &SessionId) -> Option<mpsc::Sender<Command>> {
        self.live.lock().ok()?.get(id).cloned()
    }

    async fn send<T>(
        &self,
        id: &SessionId,
        make: impl FnOnce(oneshot::Sender<CoreResult<T>>) -> Command,
    ) -> CoreResult<T> {
        let tx = match self.live_sender(id) {
            Some(tx) => tx,
            None => {
                let s = self.db.get_session(id).await?;
                return Err(CoreError::InvalidTransition {
                    from: s.status,
                    to: SessionStatus::Running,
                });
            }
        };
        let (reply, rx) = oneshot::channel();
        tx.send(make(reply))
            .await
            .map_err(|_| CoreError::InvalidInput("session is no longer running".into()))?;
        rx.await
            .map_err(|_| CoreError::InvalidInput("session is no longer running".into()))?
    }

    /// Creates a session and starts its harness in the background. The returned summary
    /// is in `Starting`; watch the stream for `HarnessReady` / `Idle` or `Failed`.
    pub async fn create_session(
        &self,
        space_id: SpaceId,
        config: SessionConfig,
        created_by: Option<MemberId>,
    ) -> CoreResult<SessionSummary> {
        let harness = self.harness_for(&config.harness_id)?;
        let sandbox = self.sandbox_for(config.sandbox)?;
        let (summary, event) = self
            .db
            .create_session(space_id, config.clone(), created_by)
            .await?;
        self.hub.publish(std::slice::from_ref(&event));
        self.spawn_actor(ActorSpec {
            session_id: summary.id.clone(),
            config,
            status: SessionStatus::Starting,
            resume_native_session_id: None,
            harness,
            sandbox,
        });
        Ok(summary)
    }

    /// Restarts a `Failed` session, resuming the agent's native session when possible.
    pub async fn resume_session(&self, id: &SessionId) -> CoreResult<SessionSummary> {
        if self.live_sender(id).is_some() {
            let s = self.db.get_session(id).await?;
            return Err(CoreError::InvalidTransition {
                from: s.status,
                to: SessionStatus::Starting,
            });
        }
        let summary = self.db.get_session(id).await?;
        summary.status.transition(SessionStatus::Starting)?;
        let harness = self.harness_for(&summary.config.harness_id)?;
        let sandbox = self.sandbox_for(summary.config.sandbox)?;
        let resume = summary
            .native_session_id
            .clone()
            .filter(|_| harness.capabilities().resume);
        self.spawn_actor(ActorSpec {
            session_id: summary.id.clone(),
            config: summary.config.clone(),
            status: summary.status,
            resume_native_session_id: resume,
            harness,
            sandbox,
        });
        Ok(summary)
    }

    /// Starts a turn. Fails with a conflict when the session is not `Idle`.
    pub async fn submit_prompt(
        &self,
        id: &SessionId,
        member: Option<MemberId>,
        prompt: Prompt,
    ) -> CoreResult<TurnId> {
        self.send(id, |reply| Command::Prompt {
            member,
            prompt,
            reply,
        })
        .await
    }

    /// Cancels the running turn; `TurnEnded { cancelled }` follows on the stream.
    pub async fn cancel_turn(&self, id: &SessionId) -> CoreResult<()> {
        self.send(id, |reply| Command::Cancel { reply }).await
    }

    /// Resolves a pending approval. Exactly one concurrent caller succeeds; the others
    /// get [`CoreError::ApprovalAlreadyResolved`].
    pub async fn resolve_approval(
        &self,
        approval_id: &ApprovalId,
        decision: ApprovalDecision,
        member: Option<MemberId>,
    ) -> CoreResult<ApprovalRecord> {
        let stored = self.db.get_approval(approval_id).await?;
        if stored.record.status != om_core::approval::ApprovalStatus::Pending {
            return Err(CoreError::ApprovalAlreadyResolved(approval_id.to_string()));
        }
        let approval_id = approval_id.clone();
        self.send(&stored.record.session_id, |reply| {
            Command::ResolveApproval {
                approval_id,
                decision,
                member,
                reply,
            }
        })
        .await
    }

    /// Closes a session (terminal). Works for live and failed sessions.
    pub async fn close_session(&self, id: &SessionId) -> CoreResult<()> {
        if self.live_sender(id).is_some() {
            return self.send(id, |reply| Command::Close { reply }).await;
        }
        let s = self.db.get_session(id).await?;
        let events = self
            .db
            .append_events(
                id,
                vec![EventPayload::StatusChanged {
                    from: s.status,
                    to: s.status.transition(SessionStatus::Closed)?,
                }],
            )
            .await?;
        self.hub.publish(&events);
        Ok(())
    }

    /// Opens a gapless subscription to events with `seq > last_seq`.
    pub async fn subscribe(&self, id: &SessionId, last_seq: u64) -> CoreResult<Subscription> {
        // Fail fast for unknown sessions.
        self.db.get_session(id).await?;
        Subscription::open(self.db.clone(), &self.hub, id.clone(), last_seq).await
    }

    /// Whether the session currently has a running actor.
    pub fn is_live(&self, id: &SessionId) -> bool {
        self.live_sender(id).is_some()
    }
}
