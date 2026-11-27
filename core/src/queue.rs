//! Messages the person lines up while a turn runs; each is sent as a turn
//! of its own once the turn before it ends (docs/evidence/message-queue.md).

use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::store::{Queued, SessionId};
use crate::{turn, unknown_thread, Activity, Claim, Core, ErrorInfo};

fn no_such_message(id: i64) -> ErrorInfo {
    ErrorInfo::new(
        "invalid_params",
        format!("no message {id} is queued in this thread"),
    )
}

impl Core {
    /// `thread/queue/list`: the messages queued in `thread`, in the order
    /// they are sent.
    pub async fn queue(&self, thread: SessionId) -> Result<Vec<Queued>, ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        store.queue(thread).await
    }

    /// `thread/queue/add`: lines `prompt` up as the last queued message of
    /// `thread`, and sends the first one at once when no turn runs. Its id
    /// and the queue as it then is.
    pub async fn add_to_queue(
        self: &Arc<Self>,
        thread: SessionId,
        prompt: String,
    ) -> Result<(i64, Vec<Queued>), ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        let (id, queue) = {
            let _held = self.queue_lock.lock().await;
            let id = store.add_to_queue(thread, prompt).await?;
            (id, self.announce_queue(thread).await?)
        };
        // The message stays queued when it cannot be sent now; the next
        // `thread/queue/start` reports why.
        let _ = self.send_queued(thread).await;
        Ok((id, queue))
    }

    /// `thread/queue/update`: writes the queued message `id` anew.
    pub async fn update_queued(
        &self,
        thread: SessionId,
        id: i64,
        prompt: String,
    ) -> Result<Vec<Queued>, ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        let _held = self.queue_lock.lock().await;
        if !store.update_queued(thread, id, prompt).await? {
            return Err(no_such_message(id));
        }
        self.announce_queue(thread).await
    }

    /// `thread/queue/delete`: takes the queued message `id` out.
    pub async fn delete_queued(
        &self,
        thread: SessionId,
        id: i64,
    ) -> Result<Vec<Queued>, ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        let _held = self.queue_lock.lock().await;
        if !store.delete_queued(thread, id).await? {
            return Err(no_such_message(id));
        }
        self.announce_queue(thread).await
    }

    /// `thread/queue/reorder`: sends the queued messages in the order of
    /// `ids`, which must be exactly the ids queued.
    pub async fn reorder_queue(
        &self,
        thread: SessionId,
        ids: Vec<i64>,
    ) -> Result<Vec<Queued>, ErrorInfo> {
        let store = self.store()?;
        if !store.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        let _held = self.queue_lock.lock().await;
        if !store.reorder_queue(thread, ids).await? {
            return Err(ErrorInfo::new(
                "invalid_params",
                "\"queue_ids\" must name every queued message of this thread exactly once",
            ));
        }
        self.announce_queue(thread).await
    }

    /// `thread/queue/start`: sends the first queued message of `thread` as
    /// a turn now; `false` when nothing is queued or a turn runs.
    pub async fn start_queue(self: &Arc<Self>, thread: SessionId) -> Result<bool, ErrorInfo> {
        if !self.store()?.session_exists(thread).await? {
            return Err(unknown_thread());
        }
        self.send_queued(thread).await
    }

    /// Called once a turn of `thread` has ended and [`Self::end_turn`]
    /// released it: a turn that completed or stopped at the length limit
    /// is followed by the first queued message. After an interrupted or
    /// failed turn the queue waits for `thread/queue/start` or the next
    /// `thread/queue/add`.
    pub fn after_turn(self: &Arc<Self>, thread: SessionId, ending: &turn::Ending) {
        if matches!(ending, turn::Ending::Completed | turn::Ending::Length) {
            let core = Arc::clone(self);
            tokio::spawn(async move {
                let _ = core.send_queued(thread).await;
            });
        }
    }

    /// The queue of `thread`, sent to those watching it.
    async fn announce_queue(&self, thread: SessionId) -> Result<Vec<Queued>, ErrorInfo> {
        let queue = self.store()?.queue(thread).await?;
        let _ = self.activity.send((thread, Activity::Queue(queue.clone())));
        Ok(queue)
    }

    /// Claims `thread` and takes its first queued message to run as a turn
    /// in the background, all under the queue's lock: a message is taken
    /// only once the claim holds, so a running turn or a concurrent call
    /// leaves it queued.
    async fn send_queued(self: &Arc<Self>, thread: SessionId) -> Result<bool, ErrorInfo> {
        let store = self.store()?;
        let _held = self.queue_lock.lock().await;
        let Some(first) = store.queue(thread).await?.into_iter().next() else {
            return Ok(false);
        };
        if self.turn_running(thread) {
            return Ok(false);
        }
        let approvals = turn::Approvals {
            mode: self.thread_mode(thread).await?.unwrap_or_default(),
        };
        let claim = match self.begin_turn(thread).await {
            Ok(claim) => claim,
            Err(error) if error.code == "turn_running" => return Ok(false),
            Err(error) => return Err(error),
        };
        if let Err(error) = store.delete_queued(thread, first.queue_id).await {
            self.end_turn(thread);
            return Err(error);
        }
        if let Err(error) = self.announce_queue(thread).await {
            self.end_turn(thread);
            return Err(error);
        }
        self.run_queued(thread, first.prompt, approvals, claim);
        Ok(true)
    }

    /// Runs a queued message as a turn no connection asked for: its events
    /// reach the screens through `thread/subscribe` only.
    fn run_queued(
        self: &Arc<Self>,
        thread: SessionId,
        prompt: String,
        approvals: turn::Approvals,
        claim: Claim,
    ) {
        let core = Arc::clone(self);
        tokio::spawn(async move {
            // No connection can cancel it: the sender is dropped, so only
            // `turn/interrupt` stops it. Its events are only teed to the
            // watchers, so nothing reads `events`.
            let (_, cancel) = watch::channel(false);
            let (events, _) = mpsc::channel(1);
            let outcome = core
                .run_turn(thread, prompt, None, approvals, claim, cancel, events)
                .await;
            core.end_turn(thread);
            core.after_turn(thread, &outcome.ending);
        });
    }
}
