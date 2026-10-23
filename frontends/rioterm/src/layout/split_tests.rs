use super::*;
use crate::context::ContextManager;
use crate::event::VoidListener;
use rio_backend::event::WindowId;
use taffy::FlexDirection;

// The test manager creates dead PTYs, so these exercise the real layout tree
// without starting shells or initializing a GPU renderer.
fn context() -> Context<VoidListener> {
    let mut manager =
        ContextManager::start_with_capacity(1, VoidListener {}, WindowId::from(0))
            .unwrap();
    let context = manager
        .current_grid_mut()
        .contexts_mut()
        .drain()
        .next()
        .unwrap()
        .1
        .val;
    context
}

fn grid(width: f32, height: f32) -> ContextGrid<VoidListener> {
    let mut context = context();
    context.dimension.width = width;
    context.dimension.height = height;
    ContextGrid::new(
        context,
        Margin::all(0.0),
        [1.0; 4],
        [1.0; 4],
        rio_backend::config::layout::Panel {
            padding: Margin::all(0.0),
            margin: Margin::all(0.0),
            ..Default::default()
        },
    )
}

fn split(grid: &mut ContextGrid<VoidListener>, direction: FlexDirection) -> NodeId {
    let node = grid.split_panel(direction).unwrap();
    grid.inner.insert(node, ContextGridItem::new(context()));
    grid.current = node;
    grid.compute_layout().unwrap();
    node
}

fn equalize(grid: &mut ContextGrid<VoidListener>) {
    grid.equalize_node(grid.root_node).unwrap();
    grid.compute_layout().unwrap();
}

fn assert_size(grid: &ContextGrid<VoidListener>, node: NodeId, width: f32, height: f32) {
    let size = grid.tree.layout(node).unwrap().size;
    assert!(
        (size.width - width).abs() <= 1.0,
        "{size:?}, expected width {width}"
    );
    assert!(
        (size.height - height).abs() <= 1.0,
        "{size:?}, expected height {height}"
    );
}

#[test]
fn auto_split_uses_physical_aspect_ratio() {
    assert!(!grid(1000.0, 600.0).auto_split_down());
    assert!(grid(600.0, 1000.0).auto_split_down());
    assert!(grid(600.0, 600.0).auto_split_down());
}

#[test]
fn auto_split_uses_focused_pane_not_window() {
    let mut grid = grid(1200.0, 800.0);
    assert!(!grid.auto_split_down());
    let left = grid.current;
    let right = split(&mut grid, FlexDirection::Row);
    assert!(grid.auto_split_down());
    let bottom_right = split(&mut grid, FlexDirection::Column);
    assert!(!grid.auto_split_down());
    grid.current = left;
    assert!(grid.auto_split_down());
    grid.current = right;
    assert!(!grid.auto_split_down());
    grid.current = bottom_right;
    assert!(!grid.auto_split_down());
}

#[test]
fn auto_split_tracks_panel_resize() {
    let mut grid = grid(1200.0, 800.0);
    let left = grid.current;
    let right = split(&mut grid, FlexDirection::Row);
    grid.set_panel_size(left, Some(200.0), None).unwrap();
    grid.set_panel_size(right, Some(1000.0), None).unwrap();
    grid.compute_layout().unwrap();
    assert!(!grid.auto_split_down());
    grid.current = left;
    assert!(grid.auto_split_down());
}

#[test]
fn equalize_resets_resized_panes_in_both_axes() {
    for direction in [FlexDirection::Row, FlexDirection::Column] {
        let mut grid = grid(1000.0, 1000.0);
        let first = grid.current;
        let second = split(&mut grid, direction);
        grid.set_panel_size(first, Some(200.0), None).unwrap();
        grid.set_panel_size(second, Some(800.0), None).unwrap();
        grid.compute_layout().unwrap();
        let current = grid.current;
        equalize(&mut grid);
        assert_eq!(grid.current, current);
        match direction {
            FlexDirection::Row => {
                assert_size(&grid, first, 500.0, 1000.0);
                assert_size(&grid, second, 500.0, 1000.0);
            }
            FlexDirection::Column => {
                assert_size(&grid, first, 1000.0, 500.0);
                assert_size(&grid, second, 1000.0, 500.0);
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn equalize_weights_nested_same_axis_by_pane_count() {
    for direction in [FlexDirection::Row, FlexDirection::Column] {
        let mut grid = grid(1200.0, 900.0);
        let first = grid.current;
        let second = split(&mut grid, direction);
        let third = split(&mut grid, direction);
        equalize(&mut grid);
        for node in [first, second, third] {
            if direction == FlexDirection::Row {
                assert_size(&grid, node, 400.0, 900.0);
            } else {
                assert_size(&grid, node, 1200.0, 300.0);
            }
        }

        // Equal sizes remain proportional after a window resize.
        grid.try_update_size(1800.0, 600.0).unwrap();
        grid.compute_layout().unwrap();
        for node in [first, second, third] {
            if direction == FlexDirection::Row {
                assert_size(&grid, node, 600.0, 600.0);
            } else {
                assert_size(&grid, node, 1800.0, 200.0);
            }
        }
    }
}

#[test]
fn equalize_counts_perpendicular_subtrees_as_one_unit() {
    let mut grid = grid(1200.0, 900.0);
    let left = grid.current;
    let top_right = split(&mut grid, FlexDirection::Row);
    let bottom_right = split(&mut grid, FlexDirection::Column);
    equalize(&mut grid);
    assert_size(&grid, left, 600.0, 900.0);
    assert_size(&grid, top_right, 600.0, 450.0);
    assert_size(&grid, bottom_right, 600.0, 450.0);
}

#[test]
fn equalize_stops_weighting_at_first_perpendicular_split() {
    // H(V(H(A, B), C), D): A/B do not increase the root's left weight.
    let mut grid = grid(1200.0, 900.0);
    let a = grid.current;
    let d = split(&mut grid, FlexDirection::Row);
    grid.current = a;
    let c = split(&mut grid, FlexDirection::Column);
    grid.current = a;
    let b = split(&mut grid, FlexDirection::Row);
    equalize(&mut grid);
    assert_size(&grid, a, 300.0, 450.0);
    assert_size(&grid, b, 300.0, 450.0);
    assert_size(&grid, c, 600.0, 450.0);
    assert_size(&grid, d, 600.0, 900.0);
}

#[test]
fn equalize_preserves_spacing_and_is_idempotent() {
    let mut grid = grid(1200.0, 900.0);
    grid.scaled_margin = Margin::all(10.0);
    grid.panel_config.column_gap = 8.0;
    grid.panel_config.row_gap = 6.0;
    grid.panel_config.padding = Margin::all(7.0);
    grid.panel_config.margin = Margin::all(3.0);
    grid.tree
        .set_style(grid.current, grid.create_panel_style())
        .unwrap();
    grid.try_update_size(1200.0, 900.0).unwrap();
    let first = grid.current;
    let second = split(&mut grid, FlexDirection::Row);
    let third = split(&mut grid, FlexDirection::Column);
    let nodes = [grid.root_node, first, second, third];
    let styles: Vec<_> = nodes
        .iter()
        .map(|&node| grid.tree.style(node).unwrap().clone())
        .collect();
    equalize(&mut grid);
    let rects: Vec<_> = nodes
        .iter()
        .map(|&node| *grid.tree.layout(node).unwrap())
        .collect();
    equalize(&mut grid);
    for ((node, style), rect) in nodes.into_iter().zip(styles).zip(rects) {
        let actual = grid.tree.style(node).unwrap();
        assert_eq!(actual.padding, style.padding);
        assert_eq!(actual.margin, style.margin);
        assert_eq!(actual.gap, style.gap);
        assert_eq!(*grid.tree.layout(node).unwrap(), rect);
    }
    let root = grid.tree.layout(grid.root_node).unwrap().size;
    assert_eq!(root.width, 1180.0);
    assert_eq!(root.height, 880.0);
}

#[test]
fn equalize_single_pane_preserves_window_size() {
    let mut grid = grid(1200.0, 900.0);
    let current = grid.current;
    equalize(&mut grid);
    assert_eq!(grid.current, current);
    assert_size(&grid, current, 1200.0, 900.0);
}
