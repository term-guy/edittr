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
        const ITEMS: [Option<Command>; 9] = [
            Some(Command::GoToDefinition),
            Some(Command::FindReferences),
            Some(Command::ShowCallHierarchy),
            None, // separator
            Some(Command::RenameSymbol),
            Some(Command::CodeActions),
            None,
            Some(Command::FileHistory),
            Some(Command::SelectionHistory),
        ];
        let selected = st.sels.iter().any(|s| !s.is_empty());
        for item in ITEMS {
            let Some(id) = item else {
                ui.separator();
                continue;
            };
            let entry = commands::entry(id);
            let label = match id {
                Command::GoToDefinition => "Go to definition",
                Command::FindReferences => "Find references",
                Command::ShowCallHierarchy => "Show call hierarchy",
                Command::RenameSymbol => "Rename symbol…",
                Command::CodeActions => "Quick fix…",
                Command::FileHistory => "File history",
                Command::SelectionHistory => "File history for selection",
                _ => entry.title,
            };
            let button = egui::Button::new(label).shortcut_text(entry.shortcut_text(ui.ctx()));
            let enabled = target.is_some() && (id != Command::SelectionHistory || selected);
            if ui.add_enabled(enabled, button).clicked() {
                command = Some(id);
                ui.close();
            }
        }
    });
    // A click inside the selection keeps it (a quick fix for the selected code).
    if let (Some(_), Some((_, byte))) = (command, target)
        && !st.sels.iter().any(|s| !s.is_empty() && s.range().contains(&byte))
    {
        st.sels = vec![Sel::caret(byte)];
        st.preferred_x = None;
        st.history.seal();
    }
    if command.is_some() {
        resp.request_focus();
    }
    command
}
