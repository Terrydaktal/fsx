//! Collision-policy parsing and source/destination relation decisions.

use crate::domain::{
    ChangeKind, CollisionCombineMode, CollisionPredicates, CollisionWinner, FileRelationBreakdown,
    MergeCollisionPolicy,
};
use std::time::SystemTime;

pub(crate) fn parse_merge_collision_policy(raw: &str) -> Result<MergeCollisionPolicy, String> {
    let (winner_raw, expr_raw) = raw
        .split_once(':')
        .ok_or_else(|| "expected winner:rule".to_string())?;
    let winner = match winner_raw {
        "source" => CollisionWinner::Source,
        "dest" => CollisionWinner::Dest,
        _ => return Err("winner must be 'source' or 'dest'".to_string()),
    };
    if expr_raw.is_empty() {
        return Err("collision rule cannot be empty".to_string());
    }
    if expr_raw.contains(',') && expr_raw.contains('+') {
        return Err(
            "use either ',' (or) or '+' (and), not both, in one --collision rule".to_string(),
        );
    }
    let combine = if expr_raw.contains('+') {
        CollisionCombineMode::All
    } else {
        CollisionCombineMode::Any
    };
    let splitter = if matches!(combine, CollisionCombineMode::All) {
        '+'
    } else {
        ','
    };
    let mut predicates = CollisionPredicates::default();
    for token in expr_raw.split(splitter) {
        match token.trim() {
            "always" => predicates.always = true,
            "newer" => predicates.newer = true,
            "larger" => predicates.larger = true,
            "size-differs" => predicates.size_differs = true,
            "" => return Err("collision rule contains an empty condition".to_string()),
            other => {
                return Err(format!(
                    "unknown collision condition '{other}'; expected always, newer, larger, or size-differs"
                ))
            }
        }
    }
    if !predicates.always && !predicates.newer && !predicates.larger && !predicates.size_differs {
        return Err("collision rule must contain at least one condition".to_string());
    }
    Ok(MergeCollisionPolicy {
        winner,
        combine,
        predicates,
    })
}

pub(crate) fn regular_file_collision_change(
    policy: MergeCollisionPolicy,
    src_size: u64,
    src_mtime: Option<SystemTime>,
    dst_exists: bool,
    dst_size: Option<u64>,
    dst_mtime: Option<SystemTime>,
) -> Option<ChangeKind> {
    if !dst_exists {
        return Some(ChangeKind::NewFile);
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PredicateResult {
        True,
        False,
        Unknown,
    }

    let eval_always = if policy.predicates.always {
        Some(PredicateResult::True)
    } else {
        None
    };
    let eval_newer = if policy.predicates.newer {
        Some(match (src_mtime, dst_mtime) {
            (Some(src), Some(dst)) => {
                if src > dst {
                    PredicateResult::True
                } else {
                    PredicateResult::False
                }
            }
            _ => PredicateResult::Unknown,
        })
    } else {
        None
    };
    let eval_larger = if policy.predicates.larger {
        Some(match dst_size {
            Some(dst) => match policy.winner {
                CollisionWinner::Source => {
                    if src_size > dst {
                        PredicateResult::True
                    } else {
                        PredicateResult::False
                    }
                }
                CollisionWinner::Dest => {
                    if dst > src_size {
                        PredicateResult::True
                    } else {
                        PredicateResult::False
                    }
                }
            },
            None => PredicateResult::Unknown,
        })
    } else {
        None
    };
    let eval_size_differs = if policy.predicates.size_differs {
        Some(match dst_size {
            Some(dst) => {
                if src_size != dst {
                    PredicateResult::True
                } else {
                    PredicateResult::False
                }
            }
            None => PredicateResult::Unknown,
        })
    } else {
        None
    };

    let predicate_results = [eval_always, eval_newer, eval_larger, eval_size_differs];
    let match_selected_winner = match policy.combine {
        CollisionCombineMode::Any => {
            if predicate_results
                .iter()
                .flatten()
                .any(|r| matches!(r, PredicateResult::True))
            {
                true
            } else {
                predicate_results
                    .iter()
                    .flatten()
                    .any(|r| matches!(r, PredicateResult::Unknown))
            }
        }
        CollisionCombineMode::All => !predicate_results
            .iter()
            .flatten()
            .any(|r| matches!(r, PredicateResult::False)),
    };

    let source_wins = match_selected_winner == matches!(policy.winner, CollisionWinner::Source);
    if source_wins {
        Some(ChangeKind::ModFile)
    } else {
        None
    }
}

pub(crate) fn sync_regular_file_change(
    src_size: u64,
    src_mtime: Option<SystemTime>,
    dst_is_regular_file: bool,
    dst_size: Option<u64>,
    dst_mtime: Option<SystemTime>,
) -> Option<ChangeKind> {
    if !dst_is_regular_file {
        return Some(ChangeKind::ModFile);
    }

    let same_size = dst_size == Some(src_size);
    let same_mtime = matches!((src_mtime, dst_mtime), (Some(src), Some(dst)) if src == dst);
    if same_size && same_mtime {
        None
    } else {
        Some(ChangeKind::ModFile)
    }
}

pub(crate) fn classify_file_relation(
    src_size: u64,
    src_mtime: Option<SystemTime>,
    dst_size: Option<u64>,
    dst_mtime: Option<SystemTime>,
) -> Option<FileRelationBreakdown> {
    let dst_size = dst_size?;
    let src_mtime = src_mtime?;
    let dst_mtime = dst_mtime?;
    let mut out = FileRelationBreakdown::default();
    match src_mtime.cmp(&dst_mtime) {
        std::cmp::Ordering::Equal => match src_size.cmp(&dst_size) {
            std::cmp::Ordering::Equal => out.same_time_same_size = 1,
            std::cmp::Ordering::Greater => out.same_time_source_larger = 1,
            std::cmp::Ordering::Less => out.same_time_source_smaller = 1,
        },
        std::cmp::Ordering::Greater => match src_size.cmp(&dst_size) {
            std::cmp::Ordering::Equal => out.same_size_source_newer = 1,
            std::cmp::Ordering::Greater => out.source_newer_larger = 1,
            std::cmp::Ordering::Less => out.source_newer_smaller = 1,
        },
        std::cmp::Ordering::Less => match src_size.cmp(&dst_size) {
            std::cmp::Ordering::Equal => out.same_size_source_older = 1,
            std::cmp::Ordering::Greater => out.source_older_larger = 1,
            std::cmp::Ordering::Less => out.source_older_smaller = 1,
        },
    }
    Some(out)
}
