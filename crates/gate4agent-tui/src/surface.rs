use std::collections::BTreeMap;

pub const MAX_SURFACE_LEAVES: usize = 4;
const DEFAULT_SPLIT_RATIO_BPS: u16 = 5_000;

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PaneId(pub u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaneBranch {
    First,
    Second,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PaneSplitPath(pub Vec<PaneBranch>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceDropZone {
    Top,
    Bottom,
    Left,
    Right,
    Center,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutPreset {
    OneByOne,
    TwoByOne,
    OneByTwo,
    ThreeByOne,
    OneByThree,
    TwoPlusOne,
    OnePlusTwo,
    TwoByTwo,
    FourByOne,
    OneByFour,
}

impl LayoutPreset {
    pub const ALL: [Self; 10] = [
        Self::OneByOne,
        Self::TwoByOne,
        Self::OneByTwo,
        Self::ThreeByOne,
        Self::OneByThree,
        Self::TwoPlusOne,
        Self::OnePlusTwo,
        Self::TwoByTwo,
        Self::FourByOne,
        Self::OneByFour,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::OneByOne => "1x1",
            Self::TwoByOne => "2x1",
            Self::OneByTwo => "1x2",
            Self::ThreeByOne => "3x1",
            Self::OneByThree => "1x3",
            Self::TwoPlusOne => "2+1",
            Self::OnePlusTwo => "1+2",
            Self::TwoByTwo => "2x2",
            Self::FourByOne => "4x1",
            Self::OneByFour => "1x4",
        }
    }

    pub fn capacity(self) -> usize {
        match self {
            Self::OneByOne => 1,
            Self::TwoByOne | Self::OneByTwo => 2,
            Self::ThreeByOne | Self::OneByThree | Self::TwoPlusOne | Self::OnePlusTwo => 3,
            Self::TwoByTwo | Self::FourByOne | Self::OneByFour => 4,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PaneNode {
    Leaf(PaneId),
    Split {
        axis: SplitAxis,
        ratio_bps: u16,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

impl PaneNode {
    pub fn leaf_ids(&self) -> Vec<PaneId> {
        let mut leaves = Vec::new();
        self.collect_leaf_ids(&mut leaves);
        leaves
    }

    pub fn contains(&self, pane_id: PaneId) -> bool {
        match self {
            Self::Leaf(candidate) => *candidate == pane_id,
            Self::Split { first, second, .. } => {
                first.contains(pane_id) || second.contains(pane_id)
            }
        }
    }

    fn collect_leaf_ids(&self, leaves: &mut Vec<PaneId>) {
        match self {
            Self::Leaf(pane_id) => leaves.push(*pane_id),
            Self::Split { first, second, .. } => {
                first.collect_leaf_ids(leaves);
                second.collect_leaf_ids(leaves);
            }
        }
    }

    fn split_leaf(
        &mut self,
        target: PaneId,
        incoming: PaneId,
        zone: SurfaceDropZone,
    ) -> bool {
        match self {
            Self::Leaf(pane_id) if *pane_id == target => {
                let target_node = Self::Leaf(target);
                let incoming_node = Self::Leaf(incoming);
                let (axis, incoming_first) = match zone {
                    SurfaceDropZone::Left => (SplitAxis::Horizontal, true),
                    SurfaceDropZone::Right => (SplitAxis::Horizontal, false),
                    SurfaceDropZone::Top => (SplitAxis::Vertical, true),
                    SurfaceDropZone::Bottom => (SplitAxis::Vertical, false),
                    SurfaceDropZone::Center => return false,
                };
                let (first, second) = if incoming_first {
                    (incoming_node, target_node)
                } else {
                    (target_node, incoming_node)
                };
                *self = Self::Split {
                    axis,
                    ratio_bps: DEFAULT_SPLIT_RATIO_BPS,
                    first: Box::new(first),
                    second: Box::new(second),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.split_leaf(target, incoming, zone)
                    || second.split_leaf(target, incoming, zone)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pane<T> {
    pub tabs: Vec<T>,
    pub active: usize,
}

impl<T> Default for Pane<T> {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
        }
    }
}

impl<T> Pane<T> {
    pub fn active_tab(&self) -> Option<&T> {
        self.tabs.get(self.active)
    }

    fn remove_tab(&mut self, index: usize) -> T {
        let removed = self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.active = 0;
        } else if self.active > index {
            self.active -= 1;
        } else {
            self.active = self.active.min(self.tabs.len() - 1);
        }
        removed
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceError {
    PaneNotFound(PaneId),
    MaximumLeavesReached { maximum: usize },
    PresetCapacityExceeded { preset: LayoutPreset, leaves: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceState<T> {
    pub root: PaneNode,
    pub panes: BTreeMap<PaneId, Pane<T>>,
    pub focused: PaneId,
    pub preset: Option<LayoutPreset>,
    next_pane_id: u32,
}

impl<T> Default for SurfaceState<T> {
    fn default() -> Self {
        let pane_id = PaneId(0);
        let mut panes = BTreeMap::new();
        panes.insert(pane_id, Pane::default());
        Self {
            root: PaneNode::Leaf(pane_id),
            panes,
            focused: pane_id,
            preset: Some(LayoutPreset::OneByOne),
            next_pane_id: 1,
        }
    }
}

impl<T: Eq> SurfaceState<T> {
    pub fn leaf_ids(&self) -> Vec<PaneId> {
        self.root.leaf_ids()
    }

    pub fn focused_pane(&self) -> &Pane<T> {
        self.panes
            .get(&self.focused)
            .expect("surface focus must refer to an existing pane")
    }

    pub fn focused_pane_mut(&mut self) -> &mut Pane<T> {
        self.panes
            .get_mut(&self.focused)
            .expect("surface focus must refer to an existing pane")
    }

    pub fn active_tab(&self) -> Option<&T> {
        self.focused_pane().active_tab()
    }

    pub fn all_tabs(&self) -> Vec<&T> {
        self.root
            .leaf_ids()
            .into_iter()
            .filter_map(|pane_id| self.panes.get(&pane_id))
            .flat_map(|pane| pane.tabs.iter())
            .collect()
    }

    pub fn tab_location(&self, tab: &T) -> Option<(PaneId, usize)> {
        self.panes.iter().find_map(|(pane_id, pane)| {
            pane.tabs
                .iter()
                .position(|candidate| candidate == tab)
                .map(|index| (*pane_id, index))
        })
    }

    pub fn set_focused(&mut self, pane_id: PaneId) -> Result<(), SurfaceError> {
        if !self.panes.contains_key(&pane_id) {
            return Err(SurfaceError::PaneNotFound(pane_id));
        }
        self.focused = pane_id;
        Ok(())
    }

    pub fn activate_tab(&mut self, pane_id: PaneId, index: usize) -> bool {
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return false;
        };
        if index >= pane.tabs.len() {
            return false;
        }
        pane.active = index;
        self.focused = pane_id;
        true
    }

    pub fn set_split_ratio(&mut self, path: &PaneSplitPath, ratio_bps: u16) -> bool {
        let Some(node) = split_node_mut(&mut self.root, &path.0) else {
            return false;
        };
        let PaneNode::Split {
            ratio_bps: current,
            ..
        } = node
        else {
            return false;
        };
        *current = ratio_bps.min(10_000);
        self.preset = None;
        true
    }

    pub fn split_child_min_extents(
        &self,
        path: &PaneSplitPath,
        axis: SplitAxis,
    ) -> Option<(u16, u16)> {
        let PaneNode::Split {
            axis: split_axis,
            first,
            second,
            ..
        } = split_node(&self.root, &path.0)?
        else {
            return None;
        };
        if *split_axis != axis {
            return None;
        }
        Some((
            minimum_extent(first, axis),
            minimum_extent(second, axis),
        ))
    }

    pub fn replace_tab(&mut self, previous: &T, replacement: T) -> bool {
        let Some((pane_id, index)) = self.tab_location(previous) else {
            return false;
        };
        self.panes
            .get_mut(&pane_id)
            .expect("tab location must refer to an existing pane")
            .tabs[index] = replacement;
        true
    }

    pub fn apply_layout_preset(&mut self, preset: LayoutPreset) -> Result<(), SurfaceError> {
        let mut leaves = self.root.leaf_ids();
        if leaves.len() > preset.capacity() {
            return Err(SurfaceError::PresetCapacityExceeded {
                preset,
                leaves: leaves.len(),
            });
        }

        let focused_tab_index = leaves
            .iter()
            .scan(0_usize, |offset, pane_id| {
                let pane = self
                    .panes
                    .get(pane_id)
                    .expect("surface tree leaf must refer to an existing pane");
                let start = *offset;
                *offset += pane.tabs.len();
                Some((*pane_id, start, pane.active, pane.tabs.len()))
            })
            .find_map(|(pane_id, start, active, tab_count)| {
                (pane_id == self.focused && active < tab_count).then_some(start + active)
            });
        let tab_count = leaves
            .iter()
            .map(|pane_id| {
                self.panes
                    .get(pane_id)
                    .expect("surface tree leaf must refer to an existing pane")
                    .tabs
                    .len()
            })
            .sum::<usize>();
        let desired_leaves = leaves
            .len()
            .max(tab_count)
            .min(preset.capacity())
            .max(1);
        while leaves.len() < desired_leaves {
            let pane_id = PaneId(self.next_pane_id);
            self.next_pane_id = self.next_pane_id.saturating_add(1);
            self.panes.insert(pane_id, Pane::default());
            leaves.push(pane_id);
        }

        let mut tabs = Vec::with_capacity(tab_count);
        for pane_id in &leaves {
            let pane = self
                .panes
                .get_mut(pane_id)
                .expect("surface tree leaf must refer to an existing pane");
            tabs.append(&mut pane.tabs);
            pane.active = 0;
        }

        let base_tabs_per_pane = tab_count / leaves.len();
        let panes_with_extra_tab = tab_count % leaves.len();
        let mut tabs = tabs.into_iter();
        let mut tab_offset = 0_usize;
        let previous_focus = self.focused;
        for (pane_index, pane_id) in leaves.iter().enumerate() {
            let tabs_in_pane = base_tabs_per_pane + usize::from(pane_index < panes_with_extra_tab);
            let pane = self
                .panes
                .get_mut(pane_id)
                .expect("surface tree leaf must refer to an existing pane");
            pane.tabs.extend(tabs.by_ref().take(tabs_in_pane));
            if let Some(focused_tab_index) = focused_tab_index {
                if (tab_offset..tab_offset + tabs_in_pane).contains(&focused_tab_index) {
                    pane.active = focused_tab_index - tab_offset;
                    self.focused = *pane_id;
                }
            }
            tab_offset += tabs_in_pane;
        }
        if focused_tab_index.is_none() && leaves.contains(&previous_focus) {
            self.focused = previous_focus;
        }

        self.root = build_preset_tree(preset, &leaves);
        self.preset = Some(preset);
        Ok(())
    }

    pub fn open_in_focused(&mut self, tab: T) -> PaneId {
        if let Some((pane_id, index)) = self.tab_location(&tab) {
            self.panes
                .get_mut(&pane_id)
                .expect("tab location must refer to an existing pane")
                .active = index;
            self.focused = pane_id;
            return pane_id;
        }

        let pane = self
            .panes
            .get_mut(&self.focused)
            .expect("surface focus must refer to an existing pane");
        pane.tabs.push(tab);
        pane.active = pane.tabs.len() - 1;
        self.focused
    }

    pub fn drop_tab(
        &mut self,
        tab: T,
        target: PaneId,
        zone: SurfaceDropZone,
    ) -> Result<PaneId, SurfaceError> {
        if !self.panes.contains_key(&target) {
            return Err(SurfaceError::PaneNotFound(target));
        }
        match zone {
            SurfaceDropZone::Center => self.merge_tab(tab, target),
            SurfaceDropZone::Top
            | SurfaceDropZone::Bottom
            | SurfaceDropZone::Left
            | SurfaceDropZone::Right => self.split_tab(tab, target, zone),
        }
    }

    pub fn detach_tab(&mut self, tab: &T) -> bool {
        let Some((pane_id, index)) = self.tab_location(tab) else {
            return false;
        };
        self.panes
            .get_mut(&pane_id)
            .expect("tab location must refer to an existing pane")
            .remove_tab(index);
        self.remove_pane_if_empty(pane_id);
        true
    }

    fn merge_tab(&mut self, tab: T, target: PaneId) -> Result<PaneId, SurfaceError> {
        if let Some((source, index)) = self.tab_location(&tab) {
            if source == target {
                self.panes
                    .get_mut(&target)
                    .expect("target pane must exist")
                    .active = index;
                self.focused = target;
                return Ok(target);
            }
            self.panes
                .get_mut(&source)
                .expect("tab location must refer to an existing pane")
                .remove_tab(index);
            self.remove_pane_if_empty(source);
        }

        let target_pane = self
            .panes
            .get_mut(&target)
            .ok_or(SurfaceError::PaneNotFound(target))?;
        target_pane.tabs.push(tab);
        target_pane.active = target_pane.tabs.len() - 1;
        self.focused = target;
        Ok(target)
    }

    fn split_tab(
        &mut self,
        tab: T,
        target: PaneId,
        zone: SurfaceDropZone,
    ) -> Result<PaneId, SurfaceError> {
        let existing = self.tab_location(&tab);
        if let Some((source, index)) = existing {
            if source == target
                && self
                    .panes
                    .get(&source)
                    .is_some_and(|pane| pane.tabs.len() == 1)
            {
                self.panes
                    .get_mut(&source)
                    .expect("source pane must exist")
                    .active = index;
                self.focused = source;
                return Ok(source);
            }
        }

        let source_will_disappear = existing.is_some_and(|(source, _)| {
            source != target
                && self
                    .panes
                    .get(&source)
                    .is_some_and(|pane| pane.tabs.len() == 1)
        });
        let resulting_leaf_count = self.panes.len() + 1 - usize::from(source_will_disappear);
        if resulting_leaf_count > MAX_SURFACE_LEAVES {
            return Err(SurfaceError::MaximumLeavesReached {
                maximum: MAX_SURFACE_LEAVES,
            });
        }

        if let Some((source, index)) = existing {
            self.panes
                .get_mut(&source)
                .expect("tab location must refer to an existing pane")
                .remove_tab(index);
            self.remove_pane_if_empty(source);
        }

        let pane_id = PaneId(self.next_pane_id);
        self.next_pane_id = self.next_pane_id.saturating_add(1);
        self.panes.insert(
            pane_id,
            Pane {
                tabs: vec![tab],
                active: 0,
            },
        );
        let split = self.root.split_leaf(target, pane_id, zone);
        debug_assert!(split, "validated target pane must exist in the split tree");
        self.preset = None;
        self.focused = pane_id;
        Ok(pane_id)
    }

    fn remove_pane_if_empty(&mut self, pane_id: PaneId) {
        if self.panes.len() == 1
            || !self
                .panes
                .get(&pane_id)
                .is_some_and(|pane| pane.tabs.is_empty())
        {
            return;
        }

        let old_order = self.root.leaf_ids();
        let old_index = old_order
            .iter()
            .position(|candidate| *candidate == pane_id)
            .unwrap_or(0);
        self.panes.remove(&pane_id);
        let fallback = *self
            .panes
            .keys()
            .next()
            .expect("removing a non-final pane must leave a fallback");
        let old_root = std::mem::replace(&mut self.root, PaneNode::Leaf(fallback));
        self.root = remove_leaf(old_root, pane_id).unwrap_or(PaneNode::Leaf(fallback));
        self.preset = match self.panes.len() {
            1 => Some(LayoutPreset::OneByOne),
            _ => None,
        };

        if self.focused == pane_id {
            let new_order = self.root.leaf_ids();
            self.focused = new_order[old_index.min(new_order.len() - 1)];
        }
    }
}

fn split_node_mut<'a>(node: &'a mut PaneNode, path: &[PaneBranch]) -> Option<&'a mut PaneNode> {
    let Some((branch, rest)) = path.split_first() else {
        return Some(node);
    };
    match node {
        PaneNode::Split { first, second, .. } => match branch {
            PaneBranch::First => split_node_mut(first, rest),
            PaneBranch::Second => split_node_mut(second, rest),
        },
        PaneNode::Leaf(_) => None,
    }
}

fn split_node<'a>(node: &'a PaneNode, path: &[PaneBranch]) -> Option<&'a PaneNode> {
    let Some((branch, rest)) = path.split_first() else {
        return Some(node);
    };
    match node {
        PaneNode::Split { first, second, .. } => match branch {
            PaneBranch::First => split_node(first, rest),
            PaneBranch::Second => split_node(second, rest),
        },
        PaneNode::Leaf(_) => None,
    }
}

fn minimum_extent(node: &PaneNode, axis: SplitAxis) -> u16 {
    match node {
        PaneNode::Leaf(_) => 3,
        PaneNode::Split {
            axis: split_axis,
            first,
            second,
            ..
        } if *split_axis == axis => minimum_extent(first, axis)
            .saturating_add(1)
            .saturating_add(minimum_extent(second, axis)),
        PaneNode::Split { first, second, .. } => {
            minimum_extent(first, axis).max(minimum_extent(second, axis))
        }
    }
}

fn build_preset_tree(preset: LayoutPreset, leaves: &[PaneId]) -> PaneNode {
    debug_assert!(!leaves.is_empty());
    if leaves.len() == 1 {
        return PaneNode::Leaf(leaves[0]);
    }
    match preset {
        LayoutPreset::OneByOne => PaneNode::Leaf(leaves[0]),
        LayoutPreset::TwoByOne | LayoutPreset::ThreeByOne | LayoutPreset::FourByOne => {
            build_strip(leaves, SplitAxis::Horizontal)
        }
        LayoutPreset::OneByTwo | LayoutPreset::OneByThree | LayoutPreset::OneByFour => {
            build_strip(leaves, SplitAxis::Vertical)
        }
        LayoutPreset::TwoPlusOne if leaves.len() == 3 => PaneNode::Split {
            axis: SplitAxis::Horizontal,
            ratio_bps: DEFAULT_SPLIT_RATIO_BPS,
            first: Box::new(build_strip(&leaves[..2], SplitAxis::Vertical)),
            second: Box::new(PaneNode::Leaf(leaves[2])),
        },
        LayoutPreset::OnePlusTwo if leaves.len() == 3 => PaneNode::Split {
            axis: SplitAxis::Horizontal,
            ratio_bps: DEFAULT_SPLIT_RATIO_BPS,
            first: Box::new(PaneNode::Leaf(leaves[0])),
            second: Box::new(build_strip(&leaves[1..], SplitAxis::Vertical)),
        },
        LayoutPreset::TwoByTwo if leaves.len() >= 3 => {
            let first_row_len = leaves.len().min(2);
            PaneNode::Split {
                axis: SplitAxis::Vertical,
                ratio_bps: DEFAULT_SPLIT_RATIO_BPS,
                first: Box::new(build_strip(
                    &leaves[..first_row_len],
                    SplitAxis::Horizontal,
                )),
                second: Box::new(build_strip(
                    &leaves[first_row_len..],
                    SplitAxis::Horizontal,
                )),
            }
        }
        LayoutPreset::TwoPlusOne
        | LayoutPreset::OnePlusTwo
        | LayoutPreset::TwoByTwo => build_strip(leaves, SplitAxis::Horizontal),
    }
}

fn build_strip(leaves: &[PaneId], axis: SplitAxis) -> PaneNode {
    if leaves.len() == 1 {
        return PaneNode::Leaf(leaves[0]);
    }
    let ratio_bps = (10_000_u32 / leaves.len() as u32) as u16;
    PaneNode::Split {
        axis,
        ratio_bps,
        first: Box::new(PaneNode::Leaf(leaves[0])),
        second: Box::new(build_strip(&leaves[1..], axis)),
    }
}

fn remove_leaf(node: PaneNode, pane_id: PaneId) -> Option<PaneNode> {
    match node {
        PaneNode::Leaf(candidate) => (candidate != pane_id).then_some(PaneNode::Leaf(candidate)),
        PaneNode::Split {
            axis,
            ratio_bps,
            first,
            second,
        } => {
            let first = remove_leaf(*first, pane_id);
            let second = remove_leaf(*second, pane_id);
            match (first, second) {
                (Some(first), Some(second)) => Some(PaneNode::Split {
                    axis,
                    ratio_bps,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(survivor), None) | (None, Some(survivor)) => Some(survivor),
                (None, None) => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected_split(
        axis: SplitAxis,
        first: PaneId,
        second: PaneId,
    ) -> PaneNode {
        PaneNode::Split {
            axis,
            ratio_bps: DEFAULT_SPLIT_RATIO_BPS,
            first: Box::new(PaneNode::Leaf(first)),
            second: Box::new(PaneNode::Leaf(second)),
        }
    }

    #[test]
    fn default_surface_has_one_empty_focused_leaf() {
        let surface = SurfaceState::<&str>::default();

        assert_eq!(surface.root, PaneNode::Leaf(PaneId(0)));
        assert_eq!(surface.leaf_ids(), vec![PaneId(0)]);
        assert_eq!(surface.focused, PaneId(0));
        assert!(surface.focused_pane().tabs.is_empty());
        assert_eq!(surface.focused_pane().active, 0);
    }

    #[test]
    fn open_adds_tabs_to_the_focused_leaf_and_activates_the_latest() {
        let mut surface = SurfaceState::default();

        assert_eq!(surface.open_in_focused("claude"), PaneId(0));
        assert_eq!(surface.open_in_focused("codex"), PaneId(0));

        assert_eq!(surface.focused_pane().tabs, vec!["claude", "codex"]);
        assert_eq!(surface.focused_pane().active_tab(), Some(&"codex"));
        assert_eq!(surface.leaf_ids(), vec![PaneId(0)]);
    }

    #[test]
    fn center_drop_merges_into_tabs_and_collapses_the_empty_source_leaf() {
        let mut surface = SurfaceState::default();
        surface.open_in_focused("claude");
        let second = surface
            .drop_tab("codex", PaneId(0), SurfaceDropZone::Right)
            .unwrap();

        assert_eq!(surface.drop_tab("codex", PaneId(0), SurfaceDropZone::Center), Ok(PaneId(0)));
        assert_eq!(surface.root, PaneNode::Leaf(PaneId(0)));
        assert_eq!(surface.panes.len(), 1);
        assert!(!surface.panes.contains_key(&second));
        assert_eq!(surface.focused_pane().tabs, vec!["claude", "codex"]);
        assert_eq!(surface.focused_pane().active_tab(), Some(&"codex"));
    }

    #[test]
    fn left_drop_splits_with_the_incoming_leaf_first() {
        let mut surface = SurfaceState::default();
        surface.open_in_focused("claude");
        let incoming = surface
            .drop_tab("codex", PaneId(0), SurfaceDropZone::Left)
            .unwrap();

        assert_eq!(
            surface.root,
            expected_split(SplitAxis::Horizontal, incoming, PaneId(0))
        );
        assert_eq!(surface.focused, incoming);
    }

    #[test]
    fn right_drop_splits_with_the_incoming_leaf_second() {
        let mut surface = SurfaceState::default();
        surface.open_in_focused("claude");
        let incoming = surface
            .drop_tab("codex", PaneId(0), SurfaceDropZone::Right)
            .unwrap();

        assert_eq!(
            surface.root,
            expected_split(SplitAxis::Horizontal, PaneId(0), incoming)
        );
        assert_eq!(surface.focused, incoming);
    }

    #[test]
    fn top_drop_splits_with_the_incoming_leaf_first() {
        let mut surface = SurfaceState::default();
        surface.open_in_focused("claude");
        let incoming = surface
            .drop_tab("codex", PaneId(0), SurfaceDropZone::Top)
            .unwrap();

        assert_eq!(
            surface.root,
            expected_split(SplitAxis::Vertical, incoming, PaneId(0))
        );
        assert_eq!(surface.focused, incoming);
    }

    #[test]
    fn bottom_drop_splits_with_the_incoming_leaf_second() {
        let mut surface = SurfaceState::default();
        surface.open_in_focused("claude");
        let incoming = surface
            .drop_tab("codex", PaneId(0), SurfaceDropZone::Bottom)
            .unwrap();

        assert_eq!(
            surface.root,
            expected_split(SplitAxis::Vertical, PaneId(0), incoming)
        );
        assert_eq!(surface.focused, incoming);
    }

    #[test]
    fn detaching_the_last_tab_collapses_its_branch_and_focuses_the_neighbor() {
        let mut surface = SurfaceState::default();
        surface.open_in_focused("claude");
        let second = surface
            .drop_tab("codex", PaneId(0), SurfaceDropZone::Right)
            .unwrap();
        assert_eq!(surface.focused, second);

        assert!(surface.detach_tab(&"codex"));
        assert_eq!(surface.root, PaneNode::Leaf(PaneId(0)));
        assert_eq!(surface.focused, PaneId(0));
        assert_eq!(surface.panes.len(), 1);

        assert!(surface.detach_tab(&"claude"));
        assert_eq!(surface.root, PaneNode::Leaf(PaneId(0)));
        assert!(surface.focused_pane().tabs.is_empty());
    }

    #[test]
    fn opening_and_center_dropping_existing_tabs_never_duplicates_them() {
        let mut surface = SurfaceState::default();
        surface.open_in_focused("claude");
        surface.open_in_focused("codex");
        surface.open_in_focused("claude");
        assert_eq!(surface.focused_pane().tabs, vec!["claude", "codex"]);
        assert_eq!(surface.focused_pane().active_tab(), Some(&"claude"));

        let second = surface
            .drop_tab("codex", PaneId(0), SurfaceDropZone::Right)
            .unwrap();
        assert_eq!(surface.drop_tab("codex", second, SurfaceDropZone::Center), Ok(second));
        let occurrences = surface
            .panes
            .values()
            .flat_map(|pane| pane.tabs.iter())
            .filter(|tab| **tab == "codex")
            .count();
        assert_eq!(occurrences, 1);
    }

    #[test]
    fn a_fifth_leaf_is_rejected_without_mutating_the_surface() {
        let mut surface = SurfaceState::default();
        surface.open_in_focused("one");
        let two = surface
            .drop_tab("two", PaneId(0), SurfaceDropZone::Right)
            .unwrap();
        let three = surface
            .drop_tab("three", two, SurfaceDropZone::Bottom)
            .unwrap();
        surface
            .drop_tab("four", three, SurfaceDropZone::Left)
            .unwrap();
        let before = surface.clone();

        assert_eq!(
            surface.drop_tab("five", PaneId(0), SurfaceDropZone::Top),
            Err(SurfaceError::MaximumLeavesReached {
                maximum: MAX_SURFACE_LEAVES,
            })
        );
        assert_eq!(surface, before);
    }

    #[test]
    fn three_tabs_expand_three_by_one_into_three_visible_panes() {
        let mut surface = SurfaceState::default();
        surface.open_in_focused("file-one");
        surface.open_in_focused("file-two");
        surface.open_in_focused("git");

        assert_eq!(surface.apply_layout_preset(LayoutPreset::ThreeByOne), Ok(()));

        let leaves = surface.leaf_ids();
        assert_eq!(leaves, vec![PaneId(0), PaneId(1), PaneId(2)]);
        assert_eq!(surface.panes[&leaves[0]].tabs, vec!["file-one"]);
        assert_eq!(surface.panes[&leaves[1]].tabs, vec!["file-two"]);
        assert_eq!(surface.panes[&leaves[2]].tabs, vec!["git"]);
        assert_eq!(surface.focused, leaves[2]);
        assert_eq!(surface.active_tab(), Some(&"git"));
    }

    #[test]
    fn preset_redistributes_mixed_tabs_without_reordering_or_duplication_and_preserves_focus() {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum MixedTab {
            File(u8),
            Pty(u8),
            Git(u8),
        }

        let expected = vec![MixedTab::File(1), MixedTab::Pty(2), MixedTab::Git(3)];
        let mut surface = SurfaceState::default();
        surface.open_in_focused(expected[0]);
        surface.open_in_focused(expected[1]);
        let git_pane = surface
            .drop_tab(expected[2], PaneId(0), SurfaceDropZone::Right)
            .unwrap();
        assert_ne!(git_pane, PaneId(0));
        assert!(surface.activate_tab(PaneId(0), 1));

        assert_eq!(surface.apply_layout_preset(LayoutPreset::ThreeByOne), Ok(()));

        let leaves = surface.leaf_ids();
        let tabs = surface.all_tabs().into_iter().copied().collect::<Vec<_>>();
        assert_eq!(tabs, expected);
        assert_eq!(surface.panes.values().map(|pane| pane.tabs.len()).sum::<usize>(), 3);
        assert!(surface.panes.values().all(|pane| pane.tabs.len() == 1));
        assert_eq!(surface.focused, leaves[1]);
        assert_eq!(surface.active_tab(), Some(&MixedTab::Pty(2)));
    }
}
