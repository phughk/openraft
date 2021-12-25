use std::collections::BTreeMap;

use tokio::sync::oneshot;
use tracing_futures::Instrument;

use crate::config::SnapshotPolicy;
use crate::core::LeaderState;
use crate::core::ReplicationState;
use crate::core::SnapshotState;
use crate::core::State;
use crate::core::UpdateCurrentLeader;
use crate::error::AddNonVoterError;
use crate::error::RaftResult;
use crate::raft::AddNonVoterResponse;
use crate::raft::RaftRespTx;
use crate::replication::RaftEvent;
use crate::replication::ReplicaEvent;
use crate::replication::ReplicationStream;
use crate::storage::Snapshot;
use crate::summary::MessageSummary;
use crate::AppData;
use crate::AppDataResponse;
use crate::LogId;
use crate::NodeId;
use crate::RaftNetwork;
use crate::RaftStorage;
use crate::ReplicationMetrics;

impl<'a, D: AppData, R: AppDataResponse, N: RaftNetwork<D>, S: RaftStorage<D, R>> LeaderState<'a, D, R, N, S> {
    /// Spawn a new replication stream returning its replication state handle.
    #[tracing::instrument(level = "debug", skip(self, caller_tx))]
    pub(super) fn spawn_replication_stream(
        &self,
        target: NodeId,
        caller_tx: Option<RaftRespTx<AddNonVoterResponse, AddNonVoterError>>,
    ) -> ReplicationState<D> {
        let replstream = ReplicationStream::new(
            self.core.id,
            target,
            self.core.current_term,
            self.core.config.clone(),
            self.core.last_log_id,
            self.core.commit_index,
            self.core.network.clone(),
            self.core.storage.clone(),
            self.replication_tx.clone(),
        );
        ReplicationState {
            matched: LogId { term: 0, index: 0 },
            repl_stream: replstream,
            remove_since: None,
            tx: caller_tx,
        }
    }

    /// Handle a replication event coming from one of the replication streams.
    #[tracing::instrument(level = "trace", skip(self, event), fields(event=%event.summary()))]
    pub(super) async fn handle_replica_event(&mut self, event: ReplicaEvent<S::SnapshotData>) {
        let res = match event {
            ReplicaEvent::RevertToFollower { target, term } => self.handle_revert_to_follower(target, term).await,
            ReplicaEvent::UpdateMatched { target, matched } => self.handle_update_matched(target, matched).await,
            ReplicaEvent::NeedsSnapshot { target, tx } => self.handle_needs_snapshot(target, tx).await,
            ReplicaEvent::Shutdown => {
                self.core.set_target_state(State::Shutdown);
                return;
            }
        };

        if let Err(err) = res {
            tracing::error!({error=%err}, "error while processing event from replication stream");
        }
    }

    /// Handle events from replication streams for when this node needs to revert to follower state.
    #[tracing::instrument(level = "trace", skip(self, term))]
    async fn handle_revert_to_follower(&mut self, _: NodeId, term: u64) -> RaftResult<()> {
        if term > self.core.current_term {
            self.core.update_current_term(term, None);
            self.core.save_hard_state().await?;
            self.core.update_current_leader(UpdateCurrentLeader::Unknown);
            self.core.set_target_state(State::Follower);
        }
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn handle_update_matched(&mut self, target: NodeId, matched: LogId) -> RaftResult<()> {
        // Update target's match index & check if it is awaiting removal.

        if let Some(state) = self.nodes.get_mut(&target) {
            tracing::debug!("state.matched: {}, update to matched: {}", state.matched, matched);

            assert!(matched >= state.matched, "the matched increments monotonically");

            state.matched = matched;

            // Issue a response on the non-voters response channel if needed.
            if state.is_line_rate(&self.core.last_log_id, &self.core.config) {
                // This replication became line rate.

                // When adding a non-voter, it blocks until the replication becomes line-rate.
                if let Some(tx) = state.tx.take() {
                    // TODO(xp): define a specific response type for non-voter matched event.
                    let x = AddNonVoterResponse { matched: state.matched };
                    let _ = tx.send(Ok(x));
                }
            }
        } else {
            return Ok(());
        }

        // Drop replication stream if needed.
        if self.try_remove_replication(target) {
            // nothing to do
        } else {
            self.update_leader_metrics(target, matched);
        }

        if matched.index <= self.core.commit_index {
            self.leader_report_metrics();
            return Ok(());
        }

        let commit_index = self.calc_commit_index();

        // Determine if we have a new commit index, accounting for joint consensus.
        // If a new commit index has been established, then update a few needed elements.

        if commit_index > self.core.commit_index {
            self.core.commit_index = commit_index;

            // Update all replication streams based on new commit index.
            for node in self.nodes.values() {
                let _ = node.repl_stream.repl_tx.send((
                    RaftEvent::UpdateCommitIndex {
                        commit_index: self.core.commit_index,
                    },
                    tracing::debug_span!("CH"),
                ));
            }

            // Check if there are any pending requests which need to be processed.
            let filter = self
                .awaiting_committed
                .iter()
                .enumerate()
                .take_while(|(_idx, elem)| elem.entry.log_id.index <= self.core.commit_index)
                .last()
                .map(|(idx, _)| idx);

            if let Some(offset) = filter {
                // Build a new ApplyLogsTask from each of the given client requests.

                for request in self.awaiting_committed.drain(..=offset).collect::<Vec<_>>() {
                    self.client_request_post_commit(request).await;
                }
            }
        }

        // TODO(xp): does this update too frequently?
        self.leader_report_metrics();
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn update_leader_metrics(&mut self, target: NodeId, matched: LogId) {
        self.leader_metrics.replication.insert(target, ReplicationMetrics { matched });
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn calc_commit_index(&self) -> u64 {
        let repl_indexes = self.get_match_log_indexes();
        let committed = self.core.membership.membership.greatest_majority_value(&repl_indexes);
        *committed.unwrap_or(&self.core.commit_index)
    }

    fn get_match_log_indexes(&self) -> BTreeMap<NodeId, u64> {
        let node_ids = self.core.membership.membership.all_nodes();

        let mut res = BTreeMap::new();

        for id in node_ids.iter() {
            // this node is me, the leader
            let matched = if *id == self.core.id {
                self.core.last_log_id
            } else {
                let repl_state = self.nodes.get(id);
                if let Some(x) = repl_state {
                    x.matched
                } else {
                    LogId::new(0, 0)
                }
            };

            if matched.term == self.core.current_term {
                res.insert(*id, matched.index);
            }
        }

        res
    }

    /// Handle events from replication streams requesting for snapshot info.
    #[tracing::instrument(level = "trace", skip(self, tx))]
    async fn handle_needs_snapshot(
        &mut self,
        _: NodeId,
        tx: oneshot::Sender<Snapshot<S::SnapshotData>>,
    ) -> RaftResult<()> {
        // Ensure snapshotting is configured, else do nothing.
        let threshold = match &self.core.config.snapshot_policy {
            SnapshotPolicy::LogsSinceLast(threshold) => *threshold,
        };

        // Check for existence of current snapshot.
        let current_snapshot_opt =
            self.core.storage.get_current_snapshot().await.map_err(|err| self.core.map_storage_error(err))?;

        if let Some(snapshot) = current_snapshot_opt {
            // If snapshot exists, ensure its distance from the leader's last log index is <= half
            // of the configured snapshot threshold, else create a new snapshot.
            if snapshot_is_within_half_of_threshold(
                &snapshot.meta.last_log_id.index,
                &self.core.last_log_id.index,
                &threshold,
            ) {
                let _ = tx.send(snapshot);
                return Ok(());
            }
        }

        // Check if snapshot creation is already in progress. If so, we spawn a task to await its
        // completion (or cancellation), and respond to the replication stream. The repl stream
        // will wait for the completion and will then send another request to fetch the finished snapshot.
        // Else we just drop any other state and continue. Leaders never enter `Streaming` state.
        if let Some(SnapshotState::Snapshotting { handle, sender }) = self.core.snapshot_state.take() {
            let mut chan = sender.subscribe();
            tokio::spawn(
                async move {
                    let _ = chan.recv().await;
                    // TODO(xp): send another ReplicaEvent::NeedSnapshot to raft core
                    drop(tx);
                }
                .instrument(tracing::debug_span!("spawn-recv-and-drop")),
            );
            self.core.snapshot_state = Some(SnapshotState::Snapshotting { handle, sender });
            return Ok(());
        }

        // At this point, we just attempt to request a snapshot. Under normal circumstances, the
        // leader will always be keeping up-to-date with its snapshotting, and the latest snapshot
        // will always be found and this block will never even be executed.
        //
        // If this block is executed, and a snapshot is needed, the repl stream will submit another
        // request here shortly, and will hit the above logic where it will await the snapshot completion.
        //
        // If snapshot is too old, i.e., the distance from last_log_index is greater than half of snapshot threshold,
        // always force a snapshot creation.
        self.core.trigger_log_compaction_if_needed(true);
        Ok(())
    }
}

/// Check if the given snapshot data is within half of the configured threshold.
fn snapshot_is_within_half_of_threshold(snapshot_last_index: &u64, last_log_index: &u64, threshold: &u64) -> bool {
    // Calculate distance from actor's last log index.
    let distance_from_line = last_log_index.saturating_sub(*snapshot_last_index);

    distance_from_line <= threshold / 2
}

//////////////////////////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod tests {
    use super::*;

    //////////////////////////////////////////////////////////////////////////
    // snapshot_is_within_half_of_threshold //////////////////////////////////

    mod snapshot_is_within_half_of_threshold {
        use super::*;

        macro_rules! test_snapshot_is_within_half_of_threshold {
            ({test=>$name:ident, snapshot_last_index=>$snapshot_last_index:expr, last_log_index=>$last_log:expr, threshold=>$thresh:expr, expected=>$exp:literal}) => {
                #[test]
                fn $name() {
                    let res = snapshot_is_within_half_of_threshold($snapshot_last_index, $last_log, $thresh);
                    assert_eq!(res, $exp)
                }
            };
        }

        test_snapshot_is_within_half_of_threshold!({
            test=>happy_path_true_when_within_half_threshold,
            snapshot_last_index=>&50, last_log_index=>&100, threshold=>&500, expected=>true
        });

        test_snapshot_is_within_half_of_threshold!({
            test=>happy_path_false_when_above_half_threshold,
            snapshot_last_index=>&1, last_log_index=>&500, threshold=>&100, expected=>false
        });

        test_snapshot_is_within_half_of_threshold!({
            test=>guards_against_underflow,
            snapshot_last_index=>&200, last_log_index=>&100, threshold=>&500, expected=>true
        });
    }
}
