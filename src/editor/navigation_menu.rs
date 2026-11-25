use super::{Layout, Lines, State, buffer::Sel};
use crate::commands::{self, Command};
use eframe::egui::{self, Response};

pub(super) fn show(resp: &Response, st: &mut State, lines: &Lines<'_>, lay: &Layout<'_>, version: u64) -> Option<Command> {
    let key = resp.id.with("navigation_menu_position");
    if resp.secondary_clicked()
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let row = ((pos.y - lay.top) / lay.row_h).floor().max(0.0) as usize;
        let byte = lines.byte_at_x(lay.view.line(row), pos.x - lay.text_x);
        resp.ctx.data_mut(|data| data.insert_temp(key, (version, byte)));
    }
    let target = resp
        .ctx
        .data(|data| data.get_temp::<(u64, usize)>(key))
        .filter(|&(v, _)| v == version);
    let mut command = None;
    resp.context_menu(|ui| {
        for id in [Command::FindReferences, Command::ShowCallHierarchy] {
            let entry = commands::entry(id);
            let label = if id == Command::FindReferences {
                "Find references"
            } else {
                entry.title
            };
            let button = egui::Button::new(label).shortcut_text(entry.shortcut_text(ui.ctx()));
            if ui.add_enabled(target.is_some(), button).clicked() {
                command = Some(id);
                ui.close();
            }
        }
    });
    if let (Some(_), Some((_, byte))) = (command, target) {
        st.sels = vec![Sel::caret(byte)];
        st.preferred_x = None;
        st.history.seal();
        resp.request_focus();
    }
    command
}
