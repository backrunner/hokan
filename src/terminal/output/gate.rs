use std::io::Write;

use super::super::{BoundaryId, RenderBoundaryEvent, RenderReadiness, ScreenEpoch, ScreenRevision};
use super::{OutputError, RenderGateRequest, actor::OutputActor};

pub(super) const RECENT_BOUNDARY_LIMIT: usize = 8;

#[derive(Clone, Copy, Debug)]
pub(super) struct ObservedBoundary {
    event: RenderBoundaryEvent,
    screen_revision: ScreenRevision,
    screen_epoch: ScreenEpoch,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct RedisplayConvergence {
    boundary_id: BoundaryId,
    screen_revision: ScreenRevision,
    screen_epoch: ScreenEpoch,
}

impl<W: Write> OutputActor<W> {
    pub(super) fn arm_prompt_gate(&mut self, boundary_id: BoundaryId) -> Result<(), OutputError> {
        let observed_position = self.recent_boundaries.iter().rposition(|observed| {
            matches!(
                observed.event,
                RenderBoundaryEvent::PromptRendered { boundary_id: observed_id }
                    if observed_id == boundary_id
            )
        });
        self.model.invalidate()?;
        let new_epoch = self.model.screen_epoch();
        if let Some(position) = observed_position {
            for observed in self.recent_boundaries.iter_mut().skip(position) {
                observed.screen_epoch = new_epoch;
            }
            let prompt_revision = self.recent_boundaries[position].screen_revision;
            for convergence in &mut self.recent_convergences {
                if convergence.screen_revision >= prompt_revision {
                    convergence.screen_epoch = new_epoch;
                }
            }
            for (_, _, epoch, _) in &mut self.pending_redisplays {
                *epoch = new_epoch;
            }
        }
        self.compositor.invalidate();
        self.latest_frame = None;
        self.expected_prompt = Some(boundary_id);
        self.cursor_probe_ready = observed_position.is_some();
        self.cursor_probe_revision = None;
        self.readiness = RenderReadiness::AwaitingPromptMarker { boundary_id };
        if observed_position.is_some() {
            self.scanner.reset_at_trusted_boundary();
        }
        Ok(())
    }

    pub(super) fn arm_gate(&mut self, request: RenderGateRequest) {
        self.buffer_revision = request.buffer_revision;
        self.readiness = RenderReadiness::AwaitingRedisplay {
            buffer_revision: request.buffer_revision,
            boundary_id: request.boundary_id,
            deadline: request.deadline,
        };
        if let Some(observed) = self.recent_convergences.iter().rev().find(|observed| {
            observed.boundary_id == request.boundary_id
                && observed.screen_revision == self.model.screen_revision()
                && observed.screen_epoch == self.model.screen_epoch()
        }) {
            self.readiness = RenderReadiness::Ready {
                buffer_revision: request.buffer_revision,
                screen_revision: observed.screen_revision,
            };
        }
    }

    pub(super) fn observe_boundary(
        &mut self,
        event: RenderBoundaryEvent,
        read_cycle: u64,
        _drained: bool,
    ) {
        if let RenderBoundaryEvent::PromptRendered { boundary_id } = event
            && self.expected_prompt == Some(boundary_id)
        {
            self.cursor_probe_ready = true;
            self.scanner.reset_at_trusted_boundary();
        }
        let observed = ObservedBoundary {
            event,
            screen_revision: self.model.screen_revision(),
            screen_epoch: self.model.screen_epoch(),
        };
        if self.recent_boundaries.len() == RECENT_BOUNDARY_LIMIT {
            self.recent_boundaries.pop_front();
        }
        self.recent_boundaries.push_back(observed);
        self.report.consumed_boundaries = self.report.consumed_boundaries.saturating_add(1);

        if let RenderBoundaryEvent::PostRedisplay { boundary_id } = event {
            if self.pending_redisplays.len() == RECENT_BOUNDARY_LIMIT {
                self.pending_redisplays.pop_front();
            }
            self.pending_redisplays.push_back((
                boundary_id,
                read_cycle,
                self.model.screen_epoch(),
                self.model.screen_revision(),
            ));
        }
    }

    pub(super) fn observe_drain(&mut self, read_cycle: u64) {
        while let Some((boundary_id, marker_cycle, epoch, marker_revision)) =
            self.pending_redisplays.front().copied()
        {
            if marker_cycle > read_cycle {
                break;
            }
            if epoch != self.model.screen_epoch() || self.model.alternate_screen() {
                self.pending_redisplays.pop_front();
                continue;
            }
            if !self.scanner.is_safe() {
                // The marker raced one of hokan's own frames (a sync-output
                // transaction or an erase): the redraw bytes are still being
                // consumed, so defer rather than drop. The drain is re-attempted
                // after every commit and pump batch, and the convergence is
                // recorded as soon as the scanner is safe again.
                break;
            }
            if self.model.screen_revision() <= marker_revision {
                break;
            }
            self.pending_redisplays.pop_front();
            let convergence = RedisplayConvergence {
                boundary_id,
                screen_revision: self.model.screen_revision(),
                screen_epoch: self.model.screen_epoch(),
            };
            if self.recent_convergences.len() == RECENT_BOUNDARY_LIMIT {
                self.recent_convergences.pop_front();
            }
            self.recent_convergences.push_back(convergence);
            if let RenderReadiness::AwaitingRedisplay {
                buffer_revision,
                boundary_id: expected,
                ..
            } = self.readiness
                && expected == boundary_id
            {
                self.readiness = RenderReadiness::Ready {
                    buffer_revision,
                    screen_revision: convergence.screen_revision,
                };
            }
        }
    }
}
