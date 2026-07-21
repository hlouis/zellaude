use std::collections::HashMap;
use zellij_tile::prelude::*;

/// pane_id -> (tab_index, tab_name)
pub type PaneToTab = HashMap<u32, (usize, String)>;
/// pane_id -> (x, y) screen position
pub type PanePos = HashMap<u32, (usize, usize)>;

/// Build two pane_id-keyed maps from one pass over the manifest:
///  - `pane_to_tab`: pane_id -> (tab_index, tab_name)
///  - `pane_pos`:    pane_id -> (x, y) screen coordinates
///
/// The position map lets the bar draw a tab's status icons in the panes'
/// physical left-to-right order rather than by pane_id (creation order), which
/// drifts from the layout once panes are split or moved.
///
/// Uses PaneManifest (keyed by tab_index) cross-referenced with TabInfo list.
pub fn build_pane_to_tab_map(
    tabs: &[TabInfo],
    manifest: &PaneManifest,
) -> (PaneToTab, PanePos) {
    let tab_name_by_position: HashMap<usize, String> = tabs
        .iter()
        .map(|t| (t.position, t.name.clone()))
        .collect();

    let mut map = HashMap::new();
    let mut pos = HashMap::new();
    for (&tab_index, panes) in &manifest.panes {
        let tab_name = tab_name_by_position
            .get(&tab_index)
            .cloned()
            .unwrap_or_default();
        for pane in panes {
            if !pane.is_plugin {
                map.insert(pane.id, (tab_index, tab_name.clone()));
                pos.insert(pane.id, (pane.pane_x, pane.pane_y));
            }
        }
    }
    (map, pos)
}
