use super::*;
use crate::context::ContextManager;
use crate::event::VoidListener;
use rio_backend::event::WindowId;
use taffy::FlexDirection;

fn manager() -> ContextManager<VoidListener> {
    let mut manager =
        ContextManager::start_with_capacity(5, VoidListener {}, WindowId::from(0))
            .unwrap();
    manager.add_context(false, 1);
    manager.add_context(false, 2);
    manager.set_current(0);
    manager
}

// Real layout nodes and terminal identities, without a GPU or live PTYs.
fn split(manager: &mut ContextManager<VoidListener>, tab: usize) -> usize {
    let mut source =
        ContextManager::start_with_capacity(1, VoidListener {}, WindowId::from(0))
            .unwrap();
    let context = source
        .current_grid_mut()
        .contexts_mut()
        .drain()
        .next()
        .unwrap()
        .1
        .val;
    let route_id = context.route_id;
    let grid = &mut manager.contexts_mut()[tab];
    let node = grid.split_panel(FlexDirection::Row).unwrap();
    grid.inner.insert(node, ContextGridItem::new(context));
    route_id
}

#[test]
fn select_route_finds_background_split_and_updates_current_route_and_title() {
    let mut manager = manager();
    let original = manager.current_route();
    let target = split(&mut manager, 0);
    manager.config.title.content = "{{ columns }}".into();
    manager.get_by_route_id(target).unwrap().val.title_dirty = true;
    manager.set_current(2);

    assert!(manager.select_route(target));
    assert_eq!(manager.current_index(), 0);
    assert_eq!(manager.current_grid_len(), 2);
    assert_eq!(manager.current_route(), target);
    assert_eq!(manager.current().route_id, target);
    assert!(!manager.current().title_dirty);
    assert_eq!(
        manager.current().title.content,
        manager.current().dimension.columns.to_string()
    );

    // Selecting another split in the already-active tab must work too.
    assert!(manager.select_route(original));
    assert_eq!(manager.current_route(), original);
    assert!(manager.select_route(target));
    assert_eq!(manager.current_route(), target);
}

#[test]
fn select_route_tracks_reordered_tabs_and_does_not_reuse_closed_panes() {
    let mut manager = manager();
    let target = split(&mut manager, 1);
    manager.contexts_mut().swap(0, 1);
    manager.set_current(2);
    assert!(manager.select_route(target));
    assert_eq!(manager.current_index(), 0);
    assert_eq!(manager.current_route(), target);

    manager.contexts_mut().remove(0);
    manager.set_current(0);
    manager.add_context(true, 3);
    let current = manager.current_route();
    assert_ne!(current, target);
    assert!(!manager.select_route(target));
    assert_eq!(manager.current_index(), 2);
    assert_eq!(manager.current_route(), current);
}

#[test]
fn select_route_unknown_id_leaves_every_tabs_selected_split_unchanged() {
    let mut manager = manager();
    for tab in 0..3 {
        split(&mut manager, tab);
    }
    manager.set_current(1);
    let current = manager.current_route();
    let selected: Vec<_> = manager
        .contexts_mut()
        .iter()
        .map(|grid| grid.current)
        .collect();
    assert!(!manager.select_route(usize::MAX));
    assert_eq!(manager.current_index(), 1);
    assert_eq!(manager.current_route(), current);
    assert_eq!(
        selected,
        manager
            .contexts_mut()
            .iter()
            .map(|grid| grid.current)
            .collect::<Vec<_>>()
    );
}
