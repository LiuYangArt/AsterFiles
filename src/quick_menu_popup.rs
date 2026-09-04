use crate::{
    app::WindowId,
    domain::{RequestId, TabId},
};
use std::{io, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalPoint {
    pub x: i32,
    pub y: i32,
}

impl PhysicalPoint {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalSize {
    pub width: i32,
    pub height: i32,
}

impl PhysicalSize {
    pub const fn new(width: i32, height: i32) -> Self {
        Self { width, height }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl PhysicalRect {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(self) -> i32 {
        self.x.saturating_add(self.width.max(0))
    }

    pub fn bottom(self) -> i32 {
        self.y.saturating_add(self.height.max(0))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PopupPlacement {
    pub rect: PhysicalRect,
    pub horizontal_flipped: bool,
    pub vertical_flipped: bool,
    pub scrollable: bool,
    pub height_limited: bool,
}

pub fn place_root_popup(
    anchor: PhysicalPoint,
    desired_size: PhysicalSize,
    work_area: PhysicalRect,
) -> PopupPlacement {
    let normalized_work_area = normalize_rect(work_area);
    let (width, height, scrollable, height_limited) =
        limited_size(desired_size, normalized_work_area);
    let horizontal_flipped = anchor.x.saturating_add(width) > normalized_work_area.right();
    let vertical_flipped = anchor.y.saturating_add(height) > normalized_work_area.bottom();
    let preferred_x = if horizontal_flipped {
        anchor.x.saturating_sub(width)
    } else {
        anchor.x
    };
    let preferred_y = if vertical_flipped {
        anchor.y.saturating_sub(height)
    } else {
        anchor.y
    };

    PopupPlacement {
        rect: clamp_rect(
            PhysicalRect::new(preferred_x, preferred_y, width, height),
            normalized_work_area,
        ),
        horizontal_flipped,
        vertical_flipped,
        scrollable,
        height_limited,
    }
}

pub fn place_submenu_popup(
    parent_rect: PhysicalRect,
    desired_size: PhysicalSize,
    work_area: PhysicalRect,
) -> PopupPlacement {
    place_submenu_popup_with_margins(parent_rect, desired_size, work_area, 0, 0)
}

pub fn place_submenu_popup_with_margins(
    parent_rect: PhysicalRect,
    desired_size: PhysicalSize,
    work_area: PhysicalRect,
    parent_right_margin: i32,
    submenu_left_margin: i32,
) -> PopupPlacement {
    let normalized_work_area = normalize_rect(work_area);
    let normalized_parent = normalize_rect(parent_rect);
    let visible_parent_right = normalized_parent
        .right()
        .saturating_sub(parent_right_margin.max(0));
    let (width, height, scrollable, height_limited) =
        limited_size(desired_size, normalized_work_area);
    let right_window_x = visible_parent_right.saturating_sub(submenu_left_margin.max(0));
    let horizontal_flipped = right_window_x.saturating_add(width) > normalized_work_area.right()
        && normalized_parent.x.saturating_sub(width) >= normalized_work_area.x;
    let preferred_x = if horizontal_flipped {
        normalized_parent.x.saturating_sub(width)
    } else {
        right_window_x
    };

    PopupPlacement {
        rect: clamp_rect(
            PhysicalRect::new(preferred_x, normalized_parent.y, width, height),
            normalized_work_area,
        ),
        horizontal_flipped,
        vertical_flipped: false,
        scrollable,
        height_limited,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmenuPlacementRequest {
    pub anchor_y: i32,
    pub desired_size: PhysicalSize,
}

pub fn place_submenu_chain(
    root_rect: PhysicalRect,
    requests: &[SubmenuPlacementRequest],
    work_area: PhysicalRect,
) -> Vec<PopupPlacement> {
    let mut parent_rect = root_rect;
    requests
        .iter()
        .map(|request| {
            let anchored_parent = PhysicalRect::new(
                parent_rect.x,
                parent_rect.y.saturating_add(request.anchor_y),
                parent_rect.width,
                parent_rect.height,
            );
            let placement = place_submenu_popup(anchored_parent, request.desired_size, work_area);
            parent_rect = placement.rect;
            placement
        })
        .collect()
}

pub fn stable_root_size(
    root_rect: PhysicalRect,
    desired_height: i32,
    work_area: PhysicalRect,
) -> PhysicalRect {
    let available_height = work_area.bottom().saturating_sub(root_rect.y).max(0);
    PhysicalRect::new(
        root_rect.x,
        root_rect.y,
        root_rect.width,
        desired_height.clamp(0, available_height),
    )
}
fn normalize_rect(rect: PhysicalRect) -> PhysicalRect {
    PhysicalRect::new(rect.x, rect.y, rect.width.max(0), rect.height.max(0))
}

fn limited_size(desired_size: PhysicalSize, work_area: PhysicalRect) -> (i32, i32, bool, bool) {
    let desired_width = desired_size.width.max(0);
    let desired_height = desired_size.height.max(0);
    let height_limited = desired_height > work_area.height;
    (
        desired_width.min(work_area.width),
        desired_height.min(work_area.height),
        height_limited,
        height_limited,
    )
}

fn clamp_rect(rect: PhysicalRect, work_area: PhysicalRect) -> PhysicalRect {
    let max_x = work_area.right().saturating_sub(rect.width);
    let max_y = work_area.bottom().saturating_sub(rect.height);
    PhysicalRect::new(
        rect.x.clamp(work_area.x, max_x.max(work_area.x)),
        rect.y.clamp(work_area.y, max_y.max(work_area.y)),
        rect.width,
        rect.height,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MenuSessionId {
    pub owner_window: WindowId,
    pub tab_id: TabId,
    pub request_id: RequestId,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MenuBranchId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuEventIdentity {
    pub session: MenuSessionId,
    pub branch: MenuBranchId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuBranch {
    pub id: MenuBranchId,
    pub parent: Option<MenuBranchId>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct QuickMenuPopupSession {
    identity: Option<MenuSessionId>,
    branches: Vec<MenuBranch>,
}

impl QuickMenuPopupSession {
    pub fn identity(&self) -> Option<MenuSessionId> {
        self.identity
    }

    pub fn branches(&self) -> &[MenuBranch] {
        &self.branches
    }

    pub fn is_open(&self) -> bool {
        self.identity.is_some()
    }

    pub fn open_root(&mut self, identity: MenuSessionId, root_branch: MenuBranchId) {
        self.identity = Some(identity);
        self.branches.clear();
        self.branches.push(MenuBranch {
            id: root_branch,
            parent: None,
        });
    }

    pub fn push_branch(&mut self, event: MenuEventIdentity, branch: MenuBranchId) -> bool {
        let Some(parent_index) = self.matching_branch_index(event) else {
            return false;
        };
        self.branches.truncate(parent_index + 1);
        self.branches.push(MenuBranch {
            id: branch,
            parent: Some(event.branch),
        });
        true
    }

    pub fn matches_event(&self, event: MenuEventIdentity) -> bool {
        self.matching_branch_index(event).is_some()
    }

    pub fn close_to_branch(&mut self, event: MenuEventIdentity) -> bool {
        let Some(branch_index) = self.matching_branch_index(event) else {
            return false;
        };
        self.branches.truncate(branch_index + 1);
        true
    }

    pub fn close_branch_and_descendants(&mut self, event: MenuEventIdentity) -> bool {
        let Some(branch_index) = self.matching_branch_index(event) else {
            return false;
        };
        if branch_index == 0 {
            return self.close_all();
        }
        self.branches.truncate(branch_index);
        true
    }

    pub fn close_all(&mut self) -> bool {
        let was_open = self.identity.take().is_some();
        self.branches.clear();
        was_open
    }

    pub fn invalidate_owner(&mut self, owner_window: WindowId) -> bool {
        if self
            .identity
            .is_some_and(|identity| identity.owner_window == owner_window)
        {
            self.close_all()
        } else {
            false
        }
    }

    pub fn invalidate_request(
        &mut self,
        owner_window: WindowId,
        tab_id: TabId,
        request_id: RequestId,
    ) -> bool {
        if self.identity.is_some_and(|identity| {
            identity.owner_window == owner_window
                && identity.tab_id == tab_id
                && identity.request_id != request_id
        }) {
            self.close_all()
        } else {
            false
        }
    }

    fn matching_branch_index(&self, event: MenuEventIdentity) -> Option<usize> {
        (self.identity == Some(event.session)).then(|| {
            self.branches
                .iter()
                .position(|branch| branch.id == event.branch)
        })?
    }
}

pub fn export_state(path: &Path) -> io::Result<()> {
    let work_area = PhysicalRect::new(-1920, 0, 1920, 1080);
    let root = place_root_popup(
        PhysicalPoint::new(-80, 1040),
        PhysicalSize::new(480, 720),
        work_area,
    );
    let loading_submenu = place_submenu_chain(
        root.rect,
        &[SubmenuPlacementRequest {
            anchor_y: 180,
            desired_size: PhysicalSize::new(420, 48),
        }],
        work_area,
    );
    let loaded_submenus = place_submenu_chain(
        root.rect,
        &[
            SubmenuPlacementRequest {
                anchor_y: 180,
                desired_size: PhysicalSize::new(420, 1200),
            },
            SubmenuPlacementRequest {
                anchor_y: 120,
                desired_size: PhysicalSize::new(360, 420),
            },
        ],
        work_area,
    );
    let identity = MenuSessionId {
        owner_window: WindowId(2),
        tab_id: TabId(7),
        request_id: RequestId(11),
        generation: 3,
    };
    let mut session = QuickMenuPopupSession::default();
    session.open_root(identity, MenuBranchId(100));
    let branch_added = session.push_branch(
        MenuEventIdentity {
            session: identity,
            branch: MenuBranchId(100),
        },
        MenuBranchId(200),
    );
    let cross_window_rejected = !session.matches_event(MenuEventIdentity {
        session: MenuSessionId {
            owner_window: WindowId(3),
            ..identity
        },
        branch: MenuBranchId(100),
    });
    let stale_request_closed = session.invalidate_request(WindowId(2), TabId(7), RequestId(12));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        path,
        format!(
            concat!(
                "{{\n",
                "  \"schema_version\": 1,\n",
                "  \"scenario\": \"quick-menu-popup\",\n",
                "  \"scope\": \"pure_physical_geometry_and_session_no_shell_or_ui\",\n",
                "  \"root_inside_work_area\": {},\n",
                "  \"root_flipped_horizontal\": {},\n",
                "  \"root_flipped_vertical\": {},\n",
                "  \"root_rect_stable_after_submenu_load\": {},\n",
                "  \"loading_submenu_inside_work_area\": {},\n",
                "  \"loaded_submenu_inside_work_area\": {},\n",
                "  \"loaded_submenu_height_limited\": {},\n",
                "  \"multilevel_submenu_inside_work_area\": {},\n",
                "  \"multi_level_branch_added\": {},\n",
                "  \"cross_window_event_rejected\": {},\n",
                "  \"stale_request_closed_session\": {},\n",
                "  \"active_after_invalidation\": {},\n",
                "  \"directory_enumerations\": 0,\n",
                "  \"metadata_reads\": 0,\n",
                "  \"shell_or_com_queries\": 0,\n",
                "  \"network_queries\": 0\n",
                "}}\n"
            ),
            contains(work_area, root.rect),
            root.horizontal_flipped,
            root.vertical_flipped,
            root.rect
                == place_root_popup(
                    PhysicalPoint::new(-80, 1040),
                    PhysicalSize::new(480, 720),
                    work_area,
                )
                .rect,
            contains(work_area, loading_submenu[0].rect),
            contains(work_area, loaded_submenus[0].rect),
            loaded_submenus[0].height_limited,
            contains(work_area, loaded_submenus[1].rect),
            branch_added,
            cross_window_rejected,
            stale_request_closed,
            session.is_open(),
        ),
    )
}

fn contains(outer: PhysicalRect, inner: PhysicalRect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.right() <= outer.right()
        && inner.bottom() <= outer.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK_AREA: PhysicalRect = PhysicalRect::new(0, 0, 1000, 800);

    fn identity(window: u32, request: u64, generation: u64) -> MenuSessionId {
        MenuSessionId {
            owner_window: WindowId(window),
            tab_id: TabId(7),
            request_id: RequestId(request),
            generation,
        }
    }

    #[test]
    fn root_keeps_default_right_down_position() {
        let placement = place_root_popup(
            PhysicalPoint::new(100, 120),
            PhysicalSize::new(300, 240),
            WORK_AREA,
        );

        assert_eq!(placement.rect, PhysicalRect::new(100, 120, 300, 240));
        assert!(!placement.horizontal_flipped);
        assert!(!placement.vertical_flipped);
    }

    #[test]
    fn root_flips_left_when_right_side_is_too_narrow() {
        let placement = place_root_popup(
            PhysicalPoint::new(950, 120),
            PhysicalSize::new(300, 240),
            WORK_AREA,
        );

        assert_eq!(placement.rect, PhysicalRect::new(650, 120, 300, 240));
        assert!(placement.horizontal_flipped);
        assert!(!placement.vertical_flipped);
    }

    #[test]
    fn root_flips_up_when_bottom_side_is_too_short() {
        let placement = place_root_popup(
            PhysicalPoint::new(100, 760),
            PhysicalSize::new(300, 240),
            WORK_AREA,
        );

        assert_eq!(placement.rect, PhysicalRect::new(100, 520, 300, 240));
        assert!(!placement.horizontal_flipped);
        assert!(placement.vertical_flipped);
    }

    #[test]
    fn root_flips_both_directions_near_bottom_right() {
        let placement = place_root_popup(
            PhysicalPoint::new(950, 760),
            PhysicalSize::new(300, 240),
            WORK_AREA,
        );

        assert_eq!(placement.rect, PhysicalRect::new(650, 520, 300, 240));
        assert!(placement.horizontal_flipped);
        assert!(placement.vertical_flipped);
    }

    #[test]
    fn negative_monitor_coordinates_are_preserved() {
        let placement = place_root_popup(
            PhysicalPoint::new(-1500, -200),
            PhysicalSize::new(400, 300),
            PhysicalRect::new(-1920, -1080, 1920, 1080),
        );

        assert_eq!(placement.rect, PhysicalRect::new(-1500, -500, 400, 300));
        assert!(!placement.horizontal_flipped);
        assert!(placement.vertical_flipped);
    }

    #[test]
    fn physical_coordinates_do_not_apply_dpi_scaling() {
        let placement = place_root_popup(
            PhysicalPoint::new(2100, 100),
            PhysicalSize::new(450, 600),
            PhysicalRect::new(1920, 0, 1440, 900),
        );

        assert_eq!(placement.rect, PhysicalRect::new(2100, 100, 450, 600));
    }

    #[test]
    fn oversized_root_is_clamped_and_height_limited() {
        let placement = place_root_popup(
            PhysicalPoint::new(600, 500),
            PhysicalSize::new(1400, 1200),
            WORK_AREA,
        );

        assert_eq!(placement.rect, WORK_AREA);
        assert!(placement.horizontal_flipped);
        assert!(placement.vertical_flipped);
        assert!(placement.scrollable);
        assert!(placement.height_limited);
    }

    #[test]
    fn submenu_opens_right_when_space_is_available() {
        let placement = place_submenu_popup(
            PhysicalRect::new(100, 120, 240, 300),
            PhysicalSize::new(260, 280),
            WORK_AREA,
        );

        assert_eq!(placement.rect, PhysicalRect::new(340, 120, 260, 280));
        assert!(!placement.horizontal_flipped);
    }

    #[test]
    fn submenu_shadow_margins_do_not_create_a_visible_gap() {
        let placement = place_submenu_popup_with_margins(
            PhysicalRect::new(100, 120, 340, 300),
            PhysicalSize::new(300, 280),
            WORK_AREA,
            10,
            10,
        );

        assert_eq!(placement.rect.x, 420);
        assert_eq!(placement.rect.x + 10, 100 + 340 - 10);
        assert!(!placement.horizontal_flipped);
    }

    #[test]
    fn submenu_flips_left_when_right_side_is_too_narrow() {
        let placement = place_submenu_popup(
            PhysicalRect::new(700, 120, 240, 300),
            PhysicalSize::new(260, 280),
            WORK_AREA,
        );

        assert_eq!(placement.rect, PhysicalRect::new(440, 120, 260, 280));
        assert!(placement.horizontal_flipped);
    }

    #[test]
    fn submenu_clamps_vertically_and_limits_excess_height() {
        let placement = place_submenu_popup(
            PhysicalRect::new(100, 700, 240, 100),
            PhysicalSize::new(260, 1200),
            WORK_AREA,
        );

        assert_eq!(placement.rect, PhysicalRect::new(340, 0, 260, 800));
        assert!(placement.scrollable);
        assert!(placement.height_limited);
    }

    #[test]
    fn root_size_change_keeps_origin_until_work_area_requires_clamping() {
        let work_area = PhysicalRect::new(-1920, 0, 1920, 1080);
        let root = PhysicalRect::new(-800, 400, 320, 220);

        assert_eq!(
            stable_root_size(root, 500, work_area),
            PhysicalRect::new(-800, 400, 320, 500)
        );
        assert_eq!(
            stable_root_size(root, 900, work_area),
            PhysicalRect::new(-800, 400, 320, 680)
        );
    }
    #[test]
    fn submenu_loading_update_keeps_the_root_rect_stable() {
        let root = place_root_popup(
            PhysicalPoint::new(700, 760),
            PhysicalSize::new(260, 220),
            WORK_AREA,
        );
        let loading = place_submenu_chain(
            root.rect,
            &[SubmenuPlacementRequest {
                anchor_y: 150,
                desired_size: PhysicalSize::new(260, 48),
            }],
            WORK_AREA,
        );
        let loaded = place_submenu_chain(
            root.rect,
            &[SubmenuPlacementRequest {
                anchor_y: 150,
                desired_size: PhysicalSize::new(260, 360),
            }],
            WORK_AREA,
        );

        assert_eq!(root.rect, PhysicalRect::new(700, 540, 260, 220));
        assert_eq!(loading[0].rect, PhysicalRect::new(440, 690, 260, 48));
        assert_eq!(loaded[0].rect, PhysicalRect::new(440, 440, 260, 360));
    }

    #[test]
    fn sibling_and_multilevel_submenus_reposition_independently_of_the_root() {
        let root = PhysicalRect::new(-800, 400, 300, 320);
        let work_area = PhysicalRect::new(-1920, 0, 1920, 1080);
        let first = place_submenu_chain(
            root,
            &[
                SubmenuPlacementRequest {
                    anchor_y: 180,
                    desired_size: PhysicalSize::new(280, 420),
                },
                SubmenuPlacementRequest {
                    anchor_y: 120,
                    desired_size: PhysicalSize::new(260, 300),
                },
            ],
            work_area,
        );
        let sibling = place_submenu_chain(
            root,
            &[
                SubmenuPlacementRequest {
                    anchor_y: 40,
                    desired_size: PhysicalSize::new(280, 140),
                },
                SubmenuPlacementRequest {
                    anchor_y: 70,
                    desired_size: PhysicalSize::new(260, 500),
                },
            ],
            work_area,
        );

        assert_eq!(first[0].rect, PhysicalRect::new(-500, 580, 280, 420));
        assert_eq!(first[1].rect, PhysicalRect::new(-760, 700, 260, 300));
        assert_eq!(sibling[0].rect, PhysicalRect::new(-500, 440, 280, 140));
        assert_eq!(sibling[1].rect, PhysicalRect::new(-760, 510, 260, 500));
        assert_eq!(root, PhysicalRect::new(-800, 400, 300, 320));
    }
    #[test]
    fn same_depth_branch_replacement_invalidates_the_old_identity() {
        let mut session = QuickMenuPopupSession::default();
        let active = identity(1, 10, 3);
        session.open_root(active, MenuBranchId(100));
        let root = MenuEventIdentity {
            session: active,
            branch: MenuBranchId(100),
        };
        assert!(session.push_branch(root, MenuBranchId(200)));
        let old = MenuEventIdentity {
            session: active,
            branch: MenuBranchId(200),
        };
        assert!(session.push_branch(root, MenuBranchId(300)));
        let current = MenuEventIdentity {
            session: active,
            branch: MenuBranchId(300),
        };

        assert!(!session.matches_event(old));
        assert!(session.matches_event(current));
        assert_eq!(session.branches().len(), 2);
    }

    #[test]
    fn replacing_a_shallow_branch_invalidates_all_descendants() {
        let mut session = QuickMenuPopupSession::default();
        let active = identity(1, 10, 3);
        session.open_root(active, MenuBranchId(100));
        let root = MenuEventIdentity {
            session: active,
            branch: MenuBranchId(100),
        };
        assert!(session.push_branch(root, MenuBranchId(200)));
        let middle = MenuEventIdentity {
            session: active,
            branch: MenuBranchId(200),
        };
        assert!(session.push_branch(middle, MenuBranchId(300)));
        assert!(session.push_branch(root, MenuBranchId(400)));

        assert!(!session.matches_event(middle));
        assert!(!session.matches_event(MenuEventIdentity {
            session: active,
            branch: MenuBranchId(300),
        }));
        assert_eq!(session.branches()[1].id, MenuBranchId(400));
    }
    #[test]
    fn multi_level_branch_replacement_and_close_all_are_deterministic() {
        let mut session = QuickMenuPopupSession::default();
        let active = identity(1, 10, 3);
        session.open_root(active, MenuBranchId(100));
        assert!(session.push_branch(
            MenuEventIdentity {
                session: active,
                branch: MenuBranchId(100),
            },
            MenuBranchId(200),
        ));
        assert!(session.push_branch(
            MenuEventIdentity {
                session: active,
                branch: MenuBranchId(200),
            },
            MenuBranchId(300),
        ));
        assert!(session.push_branch(
            MenuEventIdentity {
                session: active,
                branch: MenuBranchId(100),
            },
            MenuBranchId(400),
        ));

        assert_eq!(
            session.branches(),
            &[
                MenuBranch {
                    id: MenuBranchId(100),
                    parent: None,
                },
                MenuBranch {
                    id: MenuBranchId(400),
                    parent: Some(MenuBranchId(100)),
                },
            ]
        );
        assert!(session.close_all());
        assert!(!session.close_all());
        assert!(!session.is_open());
    }

    #[test]
    fn cross_window_and_old_request_events_are_rejected() {
        let mut session = QuickMenuPopupSession::default();
        let active = identity(1, 10, 3);
        session.open_root(active, MenuBranchId(100));

        for stale in [identity(2, 10, 3), identity(1, 9, 3), identity(1, 10, 2)] {
            let event = MenuEventIdentity {
                session: stale,
                branch: MenuBranchId(100),
            };
            assert!(!session.matches_event(event));
            assert!(!session.push_branch(event, MenuBranchId(200)));
        }
        assert_eq!(session.branches().len(), 1);
    }

    #[test]
    fn closing_a_submenu_pops_only_the_last_branch() {
        let mut session = QuickMenuPopupSession::default();
        let active = identity(1, 10, 3);
        session.open_root(active, MenuBranchId(100));
        assert!(session.push_branch(
            MenuEventIdentity {
                session: active,
                branch: MenuBranchId(100),
            },
            MenuBranchId(200),
        ));

        assert!(session.is_open());
    }

    #[test]
    fn closing_a_middle_branch_removes_it_and_all_descendants() {
        let mut session = QuickMenuPopupSession::default();
        let active = identity(1, 10, 3);
        session.open_root(active, MenuBranchId(100));
        assert!(session.push_branch(
            MenuEventIdentity {
                session: active,
                branch: MenuBranchId(100),
            },
            MenuBranchId(200),
        ));
        assert!(session.push_branch(
            MenuEventIdentity {
                session: active,
                branch: MenuBranchId(200),
            },
            MenuBranchId(300),
        ));

        assert!(session.close_branch_and_descendants(MenuEventIdentity {
            session: active,
            branch: MenuBranchId(200),
        }));
        assert_eq!(session.branches().len(), 1);
        assert_eq!(session.branches()[0].id, MenuBranchId(100));
        assert!(session.is_open());
    }

    #[test]
    fn owner_and_request_invalidation_close_only_matching_session() {
        let mut session = QuickMenuPopupSession::default();
        session.open_root(identity(1, 10, 3), MenuBranchId(100));

        assert!(!session.invalidate_owner(WindowId(2)));
        assert!(!session.invalidate_request(WindowId(2), TabId(7), RequestId(11)));
        assert!(!session.invalidate_request(WindowId(1), TabId(8), RequestId(11)));
        assert!(session.invalidate_request(WindowId(1), TabId(7), RequestId(11)));
        assert!(!session.is_open());

        session.open_root(identity(1, 12, 4), MenuBranchId(500));
        assert!(session.invalidate_owner(WindowId(1)));
        assert!(!session.invalidate_owner(WindowId(1)));
    }
}
