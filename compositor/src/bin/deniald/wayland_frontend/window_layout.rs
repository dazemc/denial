//! Smithay adapter for pluggable window-layout algorithms.

use std::collections::HashMap;

use denial_core::topology::OutputId;
use smithay::desktop::Window;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::seat::WaylandFocus;
use tracing::info;

use super::super::window_grab::constrain_dimension;
use super::super::window_layout::{
    DEFAULT_SCROLLING_COLUMN_FRACTION, LayoutAxis, LayoutDirection, LayoutInsertion,
    LayoutPlacement, LayoutResizeEdges, LayoutResizeRequest, LayoutSpace, WindowLayoutKind,
    create_window_layout, directional_neighbor,
};
use super::managed_window::ManagedWindow;
#[cfg(feature = "flutter")]
use super::shell_content_geometry;
use super::{WaylandFrontend, WindowGeometryAuthority};

#[cfg(feature = "flutter")]
pub(crate) struct HorizontalLayoutScrollFrame {
    pub(crate) selected: Option<Window>,
    pub(crate) placements: Vec<(Window, Rectangle<i32, Logical>)>,
    pub(crate) monitor_geometry: Rectangle<i32, Logical>,
}

#[cfg(feature = "flutter")]
const TOUCHPAD_SCROLLING_TILE_SWIPE_DISTANCE: f64 = 125.0;

#[cfg(feature = "flutter")]
fn touchpad_scrolling_layout_delta(
    delta_x: f64,
    work_extent: i32,
    gap: i32,
    swipe_speed_factor: f64,
) -> f64 {
    // Libinput swipe deltas describe gesture travel, not logical scene pixels.
    // Normalize only this scrolling-layout route so 125 units of touchpad
    // travel track one default tile stride without changing device or
    // shortcut sensitivity anywhere else.
    let default_tile_stride =
        f64::from(work_extent.max(1)) * DEFAULT_SCROLLING_COLUMN_FRACTION + f64::from(gap.max(0));
    delta_x * default_tile_stride * swipe_speed_factor / TOUCHPAD_SCROLLING_TILE_SWIPE_DISTANCE
}

fn scrolling_layout_axis(transform: super::OutputTransform) -> LayoutAxis {
    if transform.swaps_axes() {
        LayoutAxis::Vertical
    } else {
        LayoutAxis::Horizontal
    }
}

impl WaylandFrontend {
    pub(super) fn window_layout_manages_geometry(&self) -> bool {
        self.window_layout.manages_geometry()
    }

    pub(crate) fn window_is_layout_managed(&self, window: &Window) -> bool {
        self.window_root_surface(window)
            .is_some_and(|surface| self.window_layout.contains(&surface.id()))
    }

    /// Follow an activated window in layouts with a movable viewport.
    pub(crate) fn activate_layout_window(&mut self, window: &Window) -> bool {
        let Some(window_id) = self.window_root_surface(window).map(|surface| surface.id()) else {
            return false;
        };
        if !self.window_layout.activate(&window_id) {
            return false;
        }
        self.arrange_layout_windows()
    }

    #[cfg(feature = "flutter")]
    pub(crate) fn can_scroll_layout_horizontally(&self) -> bool {
        self.window_layout.kind() == WindowLayoutKind::Scrolling
            && self.horizontal_layout_scroll_context().is_some()
    }

    #[cfg(feature = "flutter")]
    pub(crate) fn scroll_layout_horizontally(
        &mut self,
        delta_x: f64,
    ) -> Option<HorizontalLayoutScrollFrame> {
        let (layout_space, work_area, gap, monitor_geometry, axis) =
            self.horizontal_layout_scroll_context()?;
        let swipe_speed_factor = self.settings.touchpad().scrolling_layout_swipe_speed_factor;
        let delta_x = touchpad_scrolling_layout_delta(
            delta_x,
            axis.main_extent(work_area),
            gap,
            swipe_speed_factor,
        );
        if !self
            .window_layout
            .scroll_horizontally(layout_space, work_area, gap, axis, delta_x)
        {
            return None;
        }
        self.arrange_layout_windows();
        Some(self.horizontal_layout_scroll_frame(
            layout_space,
            work_area,
            gap,
            monitor_geometry,
            None,
        ))
    }

    #[cfg(feature = "flutter")]
    pub(crate) fn finish_layout_horizontal_scroll(
        &mut self,
        cancelled: bool,
        projected_delta_x: Option<f64>,
    ) -> Option<HorizontalLayoutScrollFrame> {
        let (layout_space, work_area, gap, monitor_geometry, axis) =
            self.horizontal_layout_scroll_context()?;
        let swipe_speed_factor = self.settings.touchpad().scrolling_layout_swipe_speed_factor;
        let projected_delta_x = projected_delta_x.map(|delta_x| {
            touchpad_scrolling_layout_delta(
                delta_x,
                axis.main_extent(work_area),
                gap,
                swipe_speed_factor,
            )
        });
        let selected = self.window_layout.finish_horizontal_scroll(
            layout_space,
            work_area,
            gap,
            axis,
            cancelled,
            projected_delta_x,
        )?;
        self.arrange_layout_windows();
        let selected = self.window_for_layout_id(&selected);
        Some(self.horizontal_layout_scroll_frame(
            layout_space,
            work_area,
            gap,
            monitor_geometry,
            selected,
        ))
    }

    #[cfg(feature = "flutter")]
    fn horizontal_layout_scroll_context(
        &self,
    ) -> Option<(
        LayoutSpace,
        Rectangle<i32, Logical>,
        i32,
        Rectangle<i32, Logical>,
        LayoutAxis,
    )> {
        let focused = self.focused_layout_window()?;
        if !self.window_layout.contains(&focused) {
            return None;
        }
        let window = self.window_for_layout_id(&focused)?;
        if self.window_has_constrained_state(&window) {
            return None;
        }
        let layout_space = self.window_layout.space_for(&focused)?;
        let output = self
            .outputs
            .iter()
            .find(|output| output.id == layout_space.output)?;
        let monitor_geometry = output.logical_geometry;
        let work_area = self.maximize_work_area(Some(&output.output), monitor_geometry);
        Some((
            layout_space,
            work_area,
            self.layout_gap(),
            monitor_geometry,
            scrolling_layout_axis(output.transform),
        ))
    }

    #[cfg(feature = "flutter")]
    fn horizontal_layout_scroll_frame(
        &self,
        layout_space: LayoutSpace,
        work_area: Rectangle<i32, Logical>,
        gap: i32,
        monitor_geometry: Rectangle<i32, Logical>,
        selected: Option<Window>,
    ) -> HorizontalLayoutScrollFrame {
        let placements = self
            .window_layout
            .arrange(layout_space, work_area, gap)
            .into_iter()
            .filter_map(|placement| {
                let window = self.window_for_layout_id(&placement.window)?;
                (!self.window_has_constrained_state(&window)).then(|| {
                    let geometry = self.window_geometry_target(&window);
                    (window, geometry)
                })
            })
            .collect();
        HorizontalLayoutScrollFrame {
            selected,
            placements,
            monitor_geometry,
        }
    }

    fn layout_swap_target_at(&self, location: Point<f64, Logical>) -> Option<Window> {
        if !location.x.is_finite() || !location.y.is_finite() {
            return None;
        }
        let point = Point::<i32, Logical>::from((
            location
                .x
                .floor()
                .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32,
            location
                .y
                .floor()
                .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32,
        ));
        let output = self
            .outputs
            .iter()
            .find(|output| output.logical_geometry.contains(point))?
            .id;
        let workspace = self.active_workspace(output);
        self.space
            .elements()
            .rev()
            .find(|window| {
                self.managed_layout_space(window) == Some(LayoutSpace::new(output, workspace))
                    && !self.window_has_constrained_state(window)
                    && self.window_geometry_target(window).contains(point)
            })
            .cloned()
    }

    pub(crate) fn layout_drop_target_at(
        &self,
        window: &Window,
        location: Point<i32, Logical>,
    ) -> Option<Window> {
        if !self.window_is_layout_managed(window) {
            return None;
        }
        let output = self
            .outputs
            .iter()
            .find(|output| output.logical_geometry.contains(location))?
            .id;
        self.layout_swap_target_at(location.to_f64()).or_else(|| {
            self.space
                .elements()
                .filter(|candidate| {
                    self.managed_layout_space(candidate).is_some_and(|space| {
                        space.output == output && space.workspace == self.active_workspace(output)
                    }) && !self.window_has_constrained_state(candidate)
                })
                .min_by(|left, right| {
                    layout_drop_distance(self.window_geometry_target(left), location).total_cmp(
                        &layout_drop_distance(self.window_geometry_target(right), location),
                    )
                })
                .cloned()
        })
    }

    pub(crate) fn layout_neighbor_window(
        &self,
        window: &Window,
        direction: LayoutDirection,
    ) -> Option<Window> {
        let focused = self.window_root_surface(window)?.id();
        let layout_space = self.window_layout.space_for(&focused)?;
        let physical_output = self
            .outputs
            .iter()
            .find(|output| output.id == layout_space.output)?;
        let work_area = self.maximize_work_area(
            Some(&physical_output.output),
            physical_output.logical_geometry,
        );
        let placements = self
            .window_layout
            .arrange(layout_space, work_area, self.layout_gap());
        let neighbor = directional_neighbor(&focused, &placements, direction)?;
        self.window_for_layout_id(&neighbor)
    }

    pub(crate) fn resize_layout_window(
        &mut self,
        window: &Window,
        edges: LayoutResizeEdges,
        delta_x: f64,
        delta_y: f64,
    ) -> Vec<(Window, Rectangle<i32, Logical>)> {
        let Some(window_id) = self.window_root_surface(window).map(|root| root.id()) else {
            return Vec::new();
        };
        let Some(layout_space) = self.window_layout.space_for(&window_id) else {
            return Vec::new();
        };
        let Some(output) = self
            .outputs
            .iter()
            .find(|output| output.id == layout_space.output)
        else {
            return Vec::new();
        };
        let output_handle = output.output.clone();
        let output_geometry = output.logical_geometry;
        let work_area = self.maximize_work_area(Some(&output_handle), output_geometry);
        let gap = self.layout_gap();
        let before = self
            .current_layout_placements()
            .into_iter()
            .map(|placement| (placement.window, placement.geometry))
            .collect::<HashMap<_, _>>();
        if !self.window_layout.resize(LayoutResizeRequest {
            window: window_id,
            work_area,
            gap,
            delta_x,
            delta_y,
            edges,
        }) {
            return Vec::new();
        }
        self.arrange_layout_windows();
        self.current_layout_placements()
            .into_iter()
            .filter(|placement| {
                before.get(&placement.window).copied() != Some(placement.geometry)
                    && self.window_layout.space_for(&placement.window) == Some(layout_space)
            })
            .filter_map(|placement| {
                self.window_for_layout_id(&placement.window).map(|window| {
                    let geometry = self.window_geometry_target(&window);
                    (window, geometry)
                })
            })
            .collect()
    }

    pub(crate) fn swap_layout_windows(&mut self, first: &Window, second: &Window) -> bool {
        let Some(first) = self.window_root_surface(first).map(|root| root.id()) else {
            return false;
        };
        let Some(second) = self.window_root_surface(second).map(|root| root.id()) else {
            return false;
        };
        if !self.window_layout.swap(&first, &second) {
            return false;
        }
        // A swap exchanges leaves, including across layout spaces. The
        // dragged/focused leaf becomes active in its destination so scrolling
        // rows never retain an active id that moved to another row.
        self.window_layout.activate(&first);
        self.arrange_layout_windows();
        true
    }

    /// Resolve a shell overview drop without surrendering geometry ownership.
    /// A populated destination exchanges leaves; an empty output receives the
    /// existing leaf as a normal layout insertion. Returning false means the
    /// window is floating and the caller should apply ordinary placement.
    pub(crate) fn apply_layout_drop(
        &mut self,
        window: &Window,
        location: Point<i32, Logical>,
    ) -> bool {
        if !self.window_is_layout_managed(window) {
            return false;
        }
        let Some(physical_output) = self
            .outputs
            .iter()
            .find(|output| output.logical_geometry.contains(location))
            .map(|output| output.id)
        else {
            return true;
        };
        let Some(window_id) = self.window_root_surface(window).map(|root| root.id()) else {
            return true;
        };
        let target = self.layout_drop_target_at(window, location);
        if let Some(target) = target {
            if target != *window {
                self.swap_layout_windows(window, &target);
            }
            return true;
        }

        let space = LayoutSpace::new(physical_output, self.active_workspace(physical_output));
        self.window_layout.insert(LayoutInsertion {
            window: window_id,
            space,
            anchor: None,
        });
        self.arrange_layout_windows();
        true
    }

    /// Switch algorithms without exposing protocol or lifecycle details to the
    /// implementation. Floating rectangles survive managed-layout switches and
    /// are restored only when returning to stacking.
    pub(crate) fn set_window_layout_kind(&mut self, kind: WindowLayoutKind) -> bool {
        if self.window_layout.kind() == kind {
            return false;
        }

        self.layout_insertion_anchors.clear();
        let previously_managed = self.window_layout.manages_geometry();
        let next = create_window_layout(kind);
        let next_managed = next.manages_geometry();
        if previously_managed && !next_managed {
            self.restore_stacking_geometries();
        }
        self.window_layout = next;
        if next_managed {
            self.rebuild_window_layout();
        }
        info!(
            layout = kind.settings_name(),
            "changed desktop window layout"
        );
        true
    }

    /// Reconcile a mapped window after its role and size hints are final.
    /// Layout algorithms only ever see regular, resizable toplevels; protocol
    /// policy for dialogs and auxiliary surfaces stays isolated in this adapter.
    pub(super) fn reconcile_window_layout(&mut self, window: &Window) -> bool {
        #[cfg(feature = "flutter")]
        if self.mobile_shell {
            return false;
        }
        if !self.window_layout.manages_geometry() {
            return false;
        }
        let Some(root) = self.window_root_surface(window) else {
            return false;
        };
        let window_id = root.id();
        let remembered_anchor = self.layout_insertion_anchors.remove(&window_id);
        if !self.window_layout_eligible(window) {
            let removed = self.window_layout.remove(&window_id);
            if removed {
                self.restore_detached_window_geometry(window, &window_id);
                self.arrange_layout_windows();
            } else {
                self.layout_restore_geometries.remove(&window_id);
            }
            return removed;
        }
        if self.window_layout.contains(&window_id) {
            return false;
        }
        let geometry = self.window_geometry_target(window);
        let restore = self
            .layout_restore_geometries
            .get(&window_id)
            .copied()
            .filter(|geometry| has_visible_size(*geometry))
            .unwrap_or_else(|| self.stacking_geometry_for_layout(window, geometry));
        let Some(physical_output) = self
            .output_for_geometry(restore)
            .map(|output| output.id)
            .or(self.ticker_output)
            .or_else(|| self.outputs.first().map(|output| output.id))
        else {
            return false;
        };
        #[cfg(feature = "flutter")]
        if let Some(stable_id) = self.surface_ids.get(&window_id).copied() {
            self.reconcile_workspace_assignment(stable_id, physical_output, false);
        }
        let space = self.layout_space_for_window(window, physical_output);
        if has_visible_size(restore) {
            self.layout_restore_geometries
                .entry(window_id.clone())
                .or_insert(restore);
        }
        let anchor = remembered_anchor
            .or_else(|| self.focused_layout_window())
            .filter(|anchor| self.window_layout.contains(anchor));
        self.window_layout.insert(LayoutInsertion {
            window: window_id,
            space,
            anchor,
        });
        self.arrange_layout_windows();
        true
    }

    /// XDG activates a new toplevel before its initial commit finalizes parent
    /// metadata. Remember the previously focused leaf so insertion still
    /// follows the user's focus, as Dwindle does, once the window is eligible.
    pub(super) fn remember_layout_insertion_anchor(&mut self, window: &Window) {
        if !self.window_layout.manages_geometry() {
            return;
        }
        let Some(window_id) = self.window_root_surface(window).map(|root| root.id()) else {
            return;
        };
        let Some(anchor) = self
            .focused_layout_window()
            .filter(|anchor| self.window_layout.contains(anchor))
        else {
            return;
        };
        self.layout_insertion_anchors.insert(window_id, anchor);
    }

    /// Detach a window and collapse its layout node. `forget_restore` is false
    /// for minimization so activation can re-enroll the same floating identity.
    pub(super) fn remove_window_from_layout(
        &mut self,
        window: &Window,
        forget_restore: bool,
    ) -> bool {
        let Some(window_id) = self.window_root_surface(window).map(|root| root.id()) else {
            return false;
        };
        let removed = self.window_layout.remove(&window_id);
        if forget_restore {
            self.layout_restore_geometries.remove(&window_id);
        }
        if removed {
            self.arrange_layout_windows();
        }
        removed
    }

    /// Rebuild output membership after hotplug/rotation while preserving each
    /// window's original stacking rectangle.
    pub(crate) fn rebuild_window_layout(&mut self) -> bool {
        if !self.window_layout.manages_geometry() {
            return false;
        }
        let windows = self
            .space
            .elements()
            .filter_map(|window| {
                if !self.window_layout_eligible(window) {
                    return None;
                }
                let root = self.window_root_surface(window)?;
                let geometry = self.window_geometry_target(window);
                let restore = self.stacking_geometry_for_layout(window, geometry);
                let physical_output = self
                    .output_for_geometry(restore)
                    .map(|output| output.id)
                    .or(self.ticker_output)
                    .or_else(|| self.outputs.first().map(|output| output.id))?;
                let space = self.layout_space_for_window(window, physical_output);
                Some((root.id(), space, restore))
            })
            .collect::<Vec<_>>();
        let mut previous_by_space = HashMap::<LayoutSpace, ObjectId>::new();
        let mut insertions = Vec::with_capacity(windows.len());
        for (window, space, geometry) in windows {
            if has_visible_size(geometry) {
                self.layout_restore_geometries
                    .entry(window.clone())
                    .or_insert(geometry);
            }
            let anchor = previous_by_space.insert(space, window.clone());
            insertions.push(LayoutInsertion {
                window,
                space,
                anchor,
            });
        }
        self.window_layout.rebuild(insertions);
        self.arrange_layout_windows()
    }

    pub(super) fn arrange_layout_windows(&mut self) -> bool {
        #[cfg(feature = "flutter")]
        if self.mobile_shell {
            let windows = self.space.elements().cloned().collect::<Vec<_>>();
            for window in windows {
                self.configure_mobile_window(&window);
            }
            return false;
        }
        if !self.window_layout.manages_geometry() {
            return false;
        }
        let ownership_changed = self.reconcile_layout_workspace_ownership();
        self.prepare_layout_arrangement();
        let placements = self.current_layout_placements();

        let mut changed = ownership_changed;
        for LayoutPlacement {
            window: window_id,
            geometry: frame,
        } in placements
        {
            let window = self.space.elements().find_map(|window| {
                (self.window_root_surface(window).map(|root| root.id()) == Some(window_id.clone()))
                    .then(|| window.clone())
            });
            let Some(window) = window else {
                continue;
            };
            // Fullscreen/maximized windows temporarily overlay their retained
            // tree node. Exiting that state returns them to the next arrangement.
            if self.window_has_constrained_state(&window) {
                continue;
            }
            #[cfg(feature = "flutter")]
            let target = shell_content_geometry(frame, super::shell_draws_server_frame(&window));
            #[cfg(not(feature = "flutter"))]
            let target = frame;
            let previous = self.window_geometry_target(&window);
            if let Some(managed) = ManagedWindow::new(&window) {
                managed.prepare_tiled_geometry(
                    target,
                    layout_resize_required(previous, window.geometry().size, target),
                );
            }
            self.set_window_geometry_target_with_authority(
                &window,
                target,
                WindowGeometryAuthority::Layout,
            );
            changed |= previous != target;
        }
        changed
    }

    fn prepare_layout_arrangement(&mut self) {
        let gap = self.layout_gap();
        let workspace_count = self.layout_workspace_count();
        let contexts = self
            .outputs
            .iter()
            .map(|output| {
                (
                    output.id,
                    self.maximize_work_area(Some(&output.output), output.logical_geometry),
                    scrolling_layout_axis(output.transform),
                )
            })
            .collect::<Vec<_>>();
        for (output, work_area, axis) in contexts {
            for workspace in 1..=workspace_count {
                self.window_layout.prepare_arrange(
                    LayoutSpace::new(output, workspace),
                    work_area,
                    gap,
                    axis,
                );
            }
        }
    }

    fn current_layout_placements(&self) -> Vec<LayoutPlacement<ObjectId>> {
        let gap = self.layout_gap();
        self.outputs
            .iter()
            .flat_map(|output| {
                let work_area =
                    self.maximize_work_area(Some(&output.output), output.logical_geometry);
                (1..=self.layout_workspace_count()).flat_map(move |workspace| {
                    self.window_layout.arrange(
                        LayoutSpace::new(output.id, workspace),
                        work_area,
                        gap,
                    )
                })
            })
            .collect()
    }

    fn layout_gap(&self) -> i32 {
        if self.work_area.maximize_padding.is_finite() {
            self.work_area.maximize_padding.round().max(0.0) as i32
        } else {
            0
        }
    }

    fn window_for_layout_id(&self, window_id: &ObjectId) -> Option<Window> {
        self.space.elements().find_map(|window| {
            (self.window_root_surface(window).map(|root| root.id()) == Some(window_id.clone()))
                .then(|| window.clone())
        })
    }

    fn restore_stacking_geometries(&mut self) {
        self.window_layout.clear();
        let restores = std::mem::take(&mut self.layout_restore_geometries);
        for (window_id, saved_restore) in restores {
            let window = self.space.elements().find_map(|window| {
                (self.window_root_surface(window).map(|root| root.id()) == Some(window_id.clone()))
                    .then(|| window.clone())
            });
            let Some(window) = window else {
                continue;
            };
            let restore =
                visible_stacking_restore(saved_restore, self.window_geometry_target(&window));
            if self.window_has_constrained_state(&window) {
                self.restore_window_geometries
                    .insert(window_id.clone(), restore);
                #[cfg(feature = "flutter")]
                {
                    if let Some(shell_restore) =
                        self.shell_maximize_restore_geometries.get_mut(&window_id)
                    {
                        *shell_restore = restore;
                    }
                    if let Some(shell_restore) =
                        self.shell_fullscreen_restore_geometries.get_mut(&window_id)
                    {
                        *shell_restore = restore;
                    }
                }
                continue;
            }
            if let Some(managed) = ManagedWindow::new(&window) {
                managed.prepare_restore_size(restore.size, false);
            }
            self.set_window_geometry_target(&window, restore);
        }
    }

    fn restore_detached_window_geometry(&mut self, window: &Window, window_id: &ObjectId) {
        let current = self.window_geometry_target(window);
        let mut restore = self
            .layout_restore_geometries
            .remove(window_id)
            .map(|saved| visible_stacking_restore(saved, current))
            .unwrap_or(current);
        let (minimum, maximum) = self.window_size_constraints(window);
        restore.size = Size::from((
            constrain_dimension(restore.size.w, minimum.w, maximum.w),
            constrain_dimension(restore.size.h, minimum.h, maximum.h),
        ));
        if let Some(managed) = ManagedWindow::new(window) {
            managed.prepare_restore_size(restore.size, true);
        }
        self.set_window_geometry_target(window, restore);
    }

    fn window_layout_eligible(&self, window: &Window) -> bool {
        let Some(root) = self.window_root_surface(window) else {
            return false;
        };
        #[cfg(feature = "flutter")]
        let minimized = self.minimized_windows.contains(&root.id());
        #[cfg(not(feature = "flutter"))]
        let minimized = false;
        let Some(facts) = ManagedWindow::new(window).map(|window| window.facts()) else {
            return false;
        };
        LayoutWindowProperties {
            alive: root.is_alive(),
            transient: self.window_has_transient_parent(window),
            auxiliary: facts.auxiliary,
            override_redirect: facts.override_redirect,
            minimized,
        }
        .is_tiling_candidate()
    }

    pub(crate) fn window_size_constraints(
        &self,
        window: &Window,
    ) -> (Size<i32, Logical>, Size<i32, Logical>) {
        ManagedWindow::new(window).map_or_else(
            || (Size::from((0, 0)), Size::from((0, 0))),
            |window| {
                let facts = window.facts();
                (facts.minimum_size, facts.maximum_size)
            },
        )
    }

    fn window_has_constrained_state(&self, window: &Window) -> bool {
        #[cfg(feature = "flutter")]
        {
            let root = self.window_root_surface(window);
            if root.as_ref().is_some_and(|root| {
                let id = root.id();
                self.shell_fullscreen_locks.contains(&id)
                    || self.shell_maximize_restore_geometries.contains_key(&id)
                    || self
                        .window_geometry_intents
                        .get(&id)
                        .is_some_and(|intent| intent.authority == WindowGeometryAuthority::Exact)
            }) {
                return true;
            }
        }
        ManagedWindow::new(window).is_some_and(|window| window.facts().client_state.fullscreen)
    }

    fn stacking_geometry_for_layout(
        &self,
        window: &Window,
        fallback: Rectangle<i32, Logical>,
    ) -> Rectangle<i32, Logical> {
        let Some(root) = self.window_root_surface(window) else {
            return fallback;
        };
        #[cfg(feature = "flutter")]
        if let Some(restore) = self
            .shell_maximize_restore_geometries
            .get(&root.id())
            .or_else(|| self.shell_fullscreen_restore_geometries.get(&root.id()))
            .copied()
        {
            return restore;
        }
        self.restore_window_geometries
            .get(&root.id())
            .copied()
            .unwrap_or(fallback)
    }

    fn focused_layout_window(&self) -> Option<ObjectId> {
        let focus = self.seat.get_keyboard()?.current_focus()?;
        let surface = focus.wl_surface()?;
        let root = self
            .owning_toplevel_surface(&surface)
            .unwrap_or_else(|| surface.into_owned());
        Some(root.id())
    }

    fn layout_workspace_count(&self) -> u8 {
        #[cfg(feature = "flutter")]
        {
            if self.workspaces_enabled() {
                return self.workspace_count();
            }
        }
        1
    }

    fn layout_space_for_window(&self, window: &Window, physical_output: OutputId) -> LayoutSpace {
        #[cfg(feature = "flutter")]
        {
            let location = self
                .window_root_surface(window)
                .and_then(|root| self.surface_ids.get(&root.id()).copied())
                .and_then(|stable_id| self.workspace_location(stable_id));
            return location.map_or_else(
                || LayoutSpace::new(physical_output, self.active_workspace(physical_output)),
                |location| LayoutSpace::new(location.output, location.workspace),
            );
        }
        #[cfg(not(feature = "flutter"))]
        {
            LayoutSpace::new(physical_output, 1)
        }
    }

    pub(super) fn managed_layout_space(&self, window: &Window) -> Option<LayoutSpace> {
        let window_id = self.window_root_surface(window)?.id();
        self.window_layout.space_for(&window_id)
    }

    /// The layout tree is the sole owner of a tiled leaf's output/workspace.
    /// The shell cache is a projection used by visibility, focus, and wire
    /// publication; refresh it as part of the same arrangement transaction.
    fn reconcile_layout_workspace_ownership(&mut self) -> bool {
        #[cfg(feature = "flutter")]
        {
            let assignments = self
                .space
                .elements()
                .filter_map(|window| {
                    let root = self.window_root_surface(window)?;
                    let stable_id = self.surface_ids.get(&root.id()).copied()?;
                    let space = self.window_layout.space_for(&root.id())?;
                    Some((stable_id, space))
                })
                .collect::<Vec<_>>();
            let mut changed = false;
            for (stable_id, space) in assignments {
                let location = super::workspace::WorkspaceLocation {
                    output: space.output,
                    workspace: space.workspace,
                };
                self.minimized_window_outputs.remove(&stable_id);
                changed |= self.window_workspaces.insert(stable_id, location) != Some(location);
            }
            return changed;
        }
        #[cfg(not(feature = "flutter"))]
        {
            false
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LayoutWindowProperties {
    alive: bool,
    transient: bool,
    auxiliary: bool,
    override_redirect: bool,
    minimized: bool,
}

impl LayoutWindowProperties {
    fn is_tiling_candidate(self) -> bool {
        self.alive
            && !self.transient
            && !self.auxiliary
            && !self.override_redirect
            && !self.minimized
    }
}

fn layout_drop_distance(geometry: Rectangle<i32, Logical>, location: Point<i32, Logical>) -> f64 {
    let center_x = f64::from(geometry.loc.x) + f64::from(geometry.size.w) / 2.0;
    let center_y = f64::from(geometry.loc.y) + f64::from(geometry.size.h) / 2.0;
    (center_x - f64::from(location.x)).powi(2) + (center_y - f64::from(location.y)).powi(2)
}

fn has_visible_size(geometry: Rectangle<i32, Logical>) -> bool {
    geometry.size.w > 1 && geometry.size.h > 1
}

fn visible_stacking_restore(
    mut saved: Rectangle<i32, Logical>,
    current: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    if !has_visible_size(saved) && has_visible_size(current) {
        // XDG toplevels can enter the layout before their first natural size
        // exists. Keep the intended floating location, but never restore the
        // resulting 0x0 placeholder; the live tile is a safe visible size.
        saved.size = current.size;
    }
    saved
}

fn layout_resize_required(
    previous_target: Rectangle<i32, Logical>,
    committed_size: smithay::utils::Size<i32, Logical>,
    next_target: Rectangle<i32, Logical>,
) -> bool {
    previous_target.size != next_target.size || committed_size != next_target.size
}

#[cfg(test)]
mod tests {
    use smithay::utils::{Point, Size};

    use super::*;

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((x, y)), Size::from((width, height)))
    }

    #[test]
    fn stacking_restore_keeps_the_location_but_never_restores_an_unknown_size() {
        assert_eq!(
            visible_stacking_restore(rect(80, 60, 0, 0), rect(10, 10, 900, 700)),
            rect(80, 60, 900, 700)
        );
        assert_eq!(
            visible_stacking_restore(rect(80, 60, 640, 480), rect(10, 10, 900, 700)),
            rect(80, 60, 640, 480)
        );
    }

    #[test]
    fn layout_resize_uses_committed_client_size_even_when_the_target_cache_is_current() {
        let target = rect(400, 0, 800, 900);
        assert!(layout_resize_required(
            target,
            Size::from((600, 900)),
            target
        ));
        assert!(!layout_resize_required(
            rect(20, 30, 800, 900),
            Size::from((800, 900)),
            target
        ));
    }

    #[cfg(feature = "flutter")]
    #[test]
    fn touchpad_scrolling_delta_applies_the_configured_travel_scale() {
        assert_eq!(
            touchpad_scrolling_layout_delta(125.0, 1_000, 10, 1.0),
            610.0
        );
        assert_eq!(
            touchpad_scrolling_layout_delta(-62.5, 1_000, 10, 0.5),
            -152.5
        );
        assert_eq!(
            touchpad_scrolling_layout_delta(100.0, 1_000, 10, 2.0),
            976.0
        );
    }

    #[test]
    fn quarter_turned_outputs_use_the_vertical_scrolling_axis() {
        assert_eq!(
            scrolling_layout_axis(super::super::OutputTransform::Normal),
            LayoutAxis::Horizontal
        );
        assert_eq!(
            scrolling_layout_axis(super::super::OutputTransform::Rotate180),
            LayoutAxis::Horizontal
        );
        for transform in [
            super::super::OutputTransform::Rotate90,
            super::super::OutputTransform::Rotate270,
            super::super::OutputTransform::Flipped90,
            super::super::OutputTransform::Flipped270,
        ] {
            assert_eq!(scrolling_layout_axis(transform), LayoutAxis::Vertical);
        }
    }

    #[test]
    fn every_regular_toplevel_uses_the_managed_layout_path() {
        let regular = LayoutWindowProperties {
            alive: true,
            transient: false,
            auxiliary: false,
            override_redirect: false,
            minimized: false,
        };
        assert!(regular.is_tiling_candidate());
        assert!(
            !LayoutWindowProperties {
                auxiliary: true,
                ..regular
            }
            .is_tiling_candidate()
        );
        assert!(
            !LayoutWindowProperties {
                transient: true,
                ..regular
            }
            .is_tiling_candidate()
        );
    }
}
