use std::io::{self, Write};

use super::super::{
    OverlaySurfaceRenderer, RenderReadiness, TerminalSize, modes::TerminalInputModes,
};
use super::actor::OutputActor;

impl<W: Write> OutputActor<W> {
    pub(super) fn suspend_terminal(&mut self) -> io::Result<()> {
        let hide_result = self
            .hide_overlay()
            .map_err(|error| io::Error::other(error.to_string()));
        self.latest_frame = None;
        self.compositor.invalidate();
        self.readiness = RenderReadiness::Unknown;
        self.cursor_probe_ready = false;
        self.cursor_probe_revision = None;
        let restore_result = self.guard.suspend();
        hide_result.and(restore_result)
    }

    pub(super) fn resume_terminal(&mut self, size: TerminalSize) -> io::Result<()> {
        self.guard.resume()?;
        let child_modes = self.model.input_modes();
        let restore_child_modes = TerminalInputModes::default().restore_bytes(child_modes);
        self.guard.write_control(&restore_child_modes)?;
        self.size = size;
        let height = self
            .max_overlay_height
            .min(size.rows.saturating_sub(1))
            .max(1);
        self.renderer = OverlaySurfaceRenderer::new(height, self.surface_theme, self.nerd_fonts);
        self.model
            .resize(size)
            .map_err(|error| io::Error::other(error.to_string()))?;
        self.compositor.invalidate();
        self.latest_frame = None;
        self.readiness = RenderReadiness::Unknown;
        self.cursor_probe_ready = false;
        self.cursor_probe_revision = None;
        Ok(())
    }
}
