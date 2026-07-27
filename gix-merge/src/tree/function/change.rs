use std::convert::Infallible;

use bstr::{BString, ByteSlice};
use gix_diff::{tree::recorder::Location, tree_with_rewrites::Change};
use gix_object::FindExt;

use crate::tree::{
    Error,
    utils::{ChangeList, ChangeListRef, PossibleConflict, TreeNodes, track},
};

pub(super) struct SideState {
    changes: ChangeList,
    tree: TreeNodes,
}

impl SideState {
    fn from_changes(changes: ChangeList) -> Self {
        let mut tree = TreeNodes::new();
        for (idx, change) in changes.iter().enumerate() {
            tree.track_change(&change.inner, idx);
        }
        SideState { changes, tree }
    }

    pub(super) fn parts_mut(&mut self) -> (&mut ChangeList, &mut TreeNodes) {
        (&mut self.changes, &mut self.tree)
    }
}

#[derive(Debug)]
pub(super) enum MatchKind {
    /// A tree is supposed to be superseded by something else.
    EraseTree,
    /// A leaf node is superseded by a tree.
    EraseLeaf,
}

#[expect(clippy::too_many_arguments)]
pub(super) fn collect(
    base_tree: &gix_hash::oid,
    side_tree: &gix_hash::oid,
    base_buf: &[u8],
    side_buf: &mut Vec<u8>,
    objects: &impl gix_object::FindObjectOrHeader,
    diff_resource_cache: &mut gix_diff::blob::Platform,
    diff_state: &mut gix_diff::tree::State,
    rewrites: Option<gix_diff::Rewrites>,
) -> Result<SideState, Error> {
    let mut changes = Vec::new();
    if base_tree != side_tree {
        let side_tree = objects.find_tree_iter(side_tree, side_buf)?;
        gix_diff::tree_with_rewrites(
            gix_object::TreeRefIter::from_bytes(base_buf, base_tree.kind()),
            side_tree,
            diff_resource_cache,
            diff_state,
            objects,
            |change| -> Result<_, Infallible> {
                track(change, &mut changes);
                Ok(std::ops::ControlFlow::Continue(()))
            },
            gix_diff::tree_with_rewrites::Options {
                location: Some(Location::Path),
                rewrites,
            },
        )?;
    }
    Ok(SideState::from_changes(changes))
}

pub(super) fn matching(
    theirs: &Change,
    needs_tree_insertion: Option<Option<usize>>,
    rewritten_location: Option<&(BString, usize)>,
    our_tree: &TreeNodes,
    our_changes: &mut ChangeList,
) -> Option<PossibleConflict> {
    let candidate = our_tree
        .check_conflict(rewritten_location.map_or_else(|| theirs.source_location(), |(location, _)| location.as_bstr()))
        .or_else(|| match theirs {
            Change::Rewrite {
                source_location,
                location,
                ..
            } if source_location != location => our_tree.check_conflict(location.as_bstr()).filter(|candidate| {
                candidate
                    .change_idx()
                    .is_some_and(|idx| matches!(our_changes[idx].inner, Change::Rewrite { .. }))
            }),
            _ => None,
        });

    candidate.filter(|ours| {
        ours.change_idx()
            .zip(needs_tree_insertion.flatten())
            .is_none_or(|(ours_idx, ignore_idx)| ours_idx != ignore_idx)
            && ours.change_idx().is_none_or(|ours_idx| {
                let ours = &mut our_changes[ours_idx];
                if ours.inner == *theirs {
                    // Applying `theirs` also consumes an identical source removal, which must not
                    // run again after descendants have been added.
                    if matches!(theirs, Change::Deletion { .. } | Change::Rewrite { .. }) {
                        ours.mark_applied();
                    }
                    false
                } else {
                    !(ours.was_applied() && matches!(ours.inner, Change::Deletion { .. }))
                }
            })
    })
}

pub(super) fn pair(candidate: &PossibleConflict, our_changes: &ChangeListRef) -> (Option<usize>, Option<MatchKind>) {
    match *candidate {
        PossibleConflict::TreeToNonTree { change_idx: Some(idx) }
            if matches!(
                our_changes[idx].inner,
                Change::Deletion { .. } | Change::Addition { .. } | Change::Rewrite { .. }
            ) =>
        {
            (Some(idx), Some(MatchKind::EraseTree))
        }
        PossibleConflict::NonTreeToTree { change_idx } => (change_idx, Some(MatchKind::EraseLeaf)),
        PossibleConflict::Match { change_idx } => (Some(change_idx), None),
        _ => (None, None),
    }
}
