use rio_window::event::{MouseScrollDelta, TouchPhase};

/// Accumulate smooth scrolling separately from terminal scrollback. A touchpad
/// needs deliberate motion, rather than switching tabs for every pixel event.
#[derive(Default)]
pub(super) struct TabBarScroll {
    remainder: f64,
    pixels: bool,
}

impl TabBarScroll {
    pub(super) fn steps(
        &mut self,
        delta: MouseScrollDelta,
        phase: TouchPhase,
        scale_factor: f64,
    ) -> i32 {
        let (units, pixels) = match delta {
            MouseScrollDelta::LineDelta(_, y) => (f64::from(y), false),
            MouseScrollDelta::PixelDelta(position) => match phase {
                TouchPhase::Moved => (position.y / scale_factor / 40.0, true),
                _ => {
                    *self = Self::default();
                    return 0;
                }
            },
        };
        if !units.is_finite() {
            return 0;
        }
        if self.pixels != pixels {
            self.remainder = 0.0;
            self.pixels = pixels;
        }
        self.remainder += units;
        let steps = self.remainder.trunc() as i32;
        self.remainder = self.remainder.fract();
        steps
    }
}

/// Hit-test tab slots without treating the strip's blank tail as a tab.
/// A single centred title is measured separately by the renderer.
pub(super) fn tab_index_at_x(
    layout: &crate::renderer::island::TabStripLayout,
    count: usize,
    x: f32,
) -> Option<usize> {
    let x = x - layout.left_margin;
    if count == 0 || layout.tab_width <= 0.0 || x < 0.0 || x >= layout.tabs_width {
        return None;
    }
    Some(((x / layout.tab_width) as usize).min(count - 1))
}

/// Positive wheel motion is up/previous; negative motion is down/next.
pub(super) fn tab_index_after_scroll(current: usize, count: usize, steps: i32) -> usize {
    if count == 0 {
        return 0;
    }
    (current as i64 - i64::from(steps)).rem_euclid(count as i64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_window::dpi::PhysicalPosition;

    #[test]
    fn middle_click_targets_the_hovered_tab_not_blank_space() {
        let layout = crate::renderer::island::TabStripLayout {
            left_margin: 76.0,
            tab_width: 200.0,
            tabs_width: 600.0,
        };
        assert_eq!(tab_index_at_x(&layout, 3, 75.0), None);
        assert_eq!(tab_index_at_x(&layout, 3, 76.0), Some(0));
        assert_eq!(tab_index_at_x(&layout, 3, 300.0), Some(1));
        assert_eq!(tab_index_at_x(&layout, 3, 675.0), Some(2));
        assert_eq!(tab_index_at_x(&layout, 3, 676.0), None);
        assert_eq!(tab_index_at_x(&layout, 3, 900.0), None);
    }

    #[test]
    fn wheel_direction_and_wrapping() {
        assert_eq!(tab_index_after_scroll(1, 3, 1), 0);
        assert_eq!(tab_index_after_scroll(1, 3, -1), 2);
        assert_eq!(tab_index_after_scroll(0, 3, 1), 2);
        assert_eq!(tab_index_after_scroll(2, 3, -1), 0);
        assert_eq!(tab_index_after_scroll(1, 3, 7), 0);
        assert_eq!(tab_index_after_scroll(0, 1, -1), 0);
    }

    #[test]
    fn wheel_preserves_fractional_notches_and_ignores_horizontal_motion() {
        let mut scroll = TabBarScroll::default();
        let phase = TouchPhase::Moved;
        assert_eq!(
            scroll.steps(MouseScrollDelta::LineDelta(1.0, 0.0), phase, 1.0),
            0
        );
        assert_eq!(
            scroll.steps(MouseScrollDelta::LineDelta(0.0, 0.5), phase, 1.0),
            0
        );
        assert_eq!(
            scroll.steps(MouseScrollDelta::LineDelta(0.0, 0.5), phase, 1.0),
            1
        );
        assert_eq!(
            scroll.steps(MouseScrollDelta::LineDelta(0.0, -2.0), phase, 1.0),
            -2
        );
    }

    #[test]
    fn touchpad_accumulates_logical_pixels_and_resets_between_gestures() {
        let mut scroll = TabBarScroll::default();
        let delta = MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 40.0));
        assert_eq!(scroll.steps(delta, TouchPhase::Started, 2.0), 0);
        assert_eq!(scroll.steps(delta, TouchPhase::Moved, 2.0), 0);
        assert_eq!(scroll.steps(delta, TouchPhase::Moved, 2.0), 1);
        assert_eq!(scroll.steps(delta, TouchPhase::Moved, 2.0), 0);
        assert_eq!(scroll.steps(delta, TouchPhase::Ended, 2.0), 0);
        assert_eq!(scroll.steps(delta, TouchPhase::Moved, 2.0), 0);
    }

    #[test]
    fn changing_input_sources_discards_the_previous_remainder() {
        let mut scroll = TabBarScroll::default();
        assert_eq!(
            scroll.steps(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 20.0)),
                TouchPhase::Moved,
                1.0,
            ),
            0
        );
        assert_eq!(
            scroll.steps(
                MouseScrollDelta::LineDelta(0.0, 0.5),
                TouchPhase::Moved,
                1.0,
            ),
            0
        );
    }
}
