use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AssignmentResolution {
    Unique(String),
    Ambiguous,
    Missing,
}

struct MaximumMatching {
    left_ids: Vec<String>,
    right_ids: Vec<String>,
    candidate_rights: Vec<Vec<usize>>,
    left_to_right: Vec<Option<usize>>,
    right_to_left: Vec<Option<usize>>,
}

pub(super) fn resolve_assignments(
    candidate_edges: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeMap<String, AssignmentResolution> {
    let matching = maximum_matching(candidate_edges);
    let left_count = matching.left_ids.len();
    let node_count = left_count + matching.right_ids.len();
    let mut alternating = vec![Vec::new(); node_count];
    let mut reverse_alternating = vec![Vec::new(); node_count];
    for (left, candidates) in matching.candidate_rights.iter().enumerate() {
        for right in candidates {
            let right_node = left_count + right;
            let (from, to) = if matching.left_to_right[left] == Some(*right) {
                (right_node, left)
            } else {
                (left, right_node)
            };
            alternating[from].push(to);
            reverse_alternating[to].push(from);
        }
    }

    let unmatched_lefts = matching
        .left_to_right
        .iter()
        .enumerate()
        .filter_map(|(left, right)| right.is_none().then_some(left));
    let reachable_from_unmatched_left = reachable(unmatched_lefts, &alternating);
    let unmatched_rights = matching
        .right_to_left
        .iter()
        .enumerate()
        .filter_map(|(right, left)| left.is_none().then_some(left_count + right));
    let can_reach_unmatched_right = reachable(unmatched_rights, &reverse_alternating);
    let components = strongly_connected_components(&alternating, &reverse_alternating);

    matching
        .left_ids
        .iter()
        .enumerate()
        .map(|(left, component_id)| {
            let resolution = match matching.left_to_right[left] {
                None if matching.candidate_rights[left].is_empty() => AssignmentResolution::Missing,
                None => AssignmentResolution::Ambiguous,
                Some(right) => {
                    let right_node = left_count + right;
                    let is_forced = !reachable_from_unmatched_left[left]
                        && !can_reach_unmatched_right[left]
                        && components[left] != components[right_node];
                    if is_forced {
                        AssignmentResolution::Unique(matching.right_ids[right].clone())
                    } else {
                        AssignmentResolution::Ambiguous
                    }
                }
            };
            (component_id.clone(), resolution)
        })
        .collect()
}

fn maximum_matching(candidate_edges: &BTreeMap<String, BTreeSet<String>>) -> MaximumMatching {
    let left_ids = candidate_edges.keys().cloned().collect::<Vec<_>>();
    let right_ids = candidate_edges
        .values()
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let right_indexes = right_ids
        .iter()
        .enumerate()
        .map(|(index, component_id)| (component_id.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    let candidate_rights = left_ids
        .iter()
        .map(|component_id| {
            candidate_edges
                .get(component_id)
                .into_iter()
                .flatten()
                .filter_map(|candidate_id| right_indexes.get(candidate_id.as_str()).copied())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut left_to_right = vec![None; left_ids.len()];
    let mut right_to_left = vec![None; right_ids.len()];
    for left in 0..left_ids.len() {
        augment(
            left,
            &candidate_rights,
            &mut left_to_right,
            &mut right_to_left,
        );
    }
    MaximumMatching {
        left_ids,
        right_ids,
        candidate_rights,
        left_to_right,
        right_to_left,
    }
}

fn augment(
    start_left: usize,
    candidate_rights: &[Vec<usize>],
    left_to_right: &mut [Option<usize>],
    right_to_left: &mut [Option<usize>],
) -> bool {
    let mut queue = VecDeque::from([start_left]);
    let mut visited_left = vec![false; left_to_right.len()];
    let mut predecessor_left = vec![None; right_to_left.len()];
    visited_left[start_left] = true;
    let mut unmatched_right = None;
    while let Some(left) = queue.pop_front() {
        for right in &candidate_rights[left] {
            if predecessor_left[*right].is_some() {
                continue;
            }
            predecessor_left[*right] = Some(left);
            match right_to_left[*right] {
                None => {
                    unmatched_right = Some(*right);
                    break;
                }
                Some(next_left) if !visited_left[next_left] => {
                    visited_left[next_left] = true;
                    queue.push_back(next_left);
                }
                Some(_) => {}
            }
        }
        if unmatched_right.is_some() {
            break;
        }
    }
    let Some(mut right) = unmatched_right else {
        return false;
    };
    loop {
        let Some(left) = predecessor_left[right] else {
            return false;
        };
        let previous_right = left_to_right[left].replace(right);
        right_to_left[right] = Some(left);
        let Some(previous_right) = previous_right else {
            break;
        };
        right_to_left[previous_right] = None;
        right = previous_right;
    }
    true
}

fn reachable(starts: impl IntoIterator<Item = usize>, graph: &[Vec<usize>]) -> Vec<bool> {
    let mut reached = vec![false; graph.len()];
    let mut queue = VecDeque::new();
    for start in starts {
        if !reached[start] {
            reached[start] = true;
            queue.push_back(start);
        }
    }
    while let Some(node) = queue.pop_front() {
        for next in &graph[node] {
            if !reached[*next] {
                reached[*next] = true;
                queue.push_back(*next);
            }
        }
    }
    reached
}

fn strongly_connected_components(graph: &[Vec<usize>], reverse: &[Vec<usize>]) -> Vec<usize> {
    let mut visited = vec![false; graph.len()];
    let mut finish_order = Vec::with_capacity(graph.len());
    for start in 0..graph.len() {
        if visited[start] {
            continue;
        }
        let mut stack = vec![(start, false)];
        while let Some((node, expanded)) = stack.pop() {
            if expanded {
                finish_order.push(node);
            } else if !visited[node] {
                visited[node] = true;
                stack.push((node, true));
                for next in graph[node].iter().rev() {
                    if !visited[*next] {
                        stack.push((*next, false));
                    }
                }
            }
        }
    }
    let mut components = vec![usize::MAX; graph.len()];
    let mut component = 0;
    for start in finish_order.into_iter().rev() {
        if components[start] != usize::MAX {
            continue;
        }
        components[start] = component;
        let mut stack = vec![start];
        while let Some(node) = stack.pop() {
            for next in &reverse[node] {
                if components[*next] == usize::MAX {
                    components[*next] = component;
                    stack.push(*next);
                }
            }
        }
        component += 1;
    }
    components
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edges(entries: &[(&str, &[&str])]) -> BTreeMap<String, BTreeSet<String>> {
        entries
            .iter()
            .map(|(left, right)| {
                (
                    (*left).to_owned(),
                    right.iter().map(|value| (*value).to_owned()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn forced_edges_are_unique_across_the_global_matching() {
        let resolutions = resolve_assignments(&edges(&[("a", &["x", "y"]), ("b", &["x"])]));
        assert_eq!(
            resolutions.get("a"),
            Some(&AssignmentResolution::Unique("y".to_owned()))
        );
        assert_eq!(
            resolutions.get("b"),
            Some(&AssignmentResolution::Unique("x".to_owned()))
        );
    }

    #[test]
    fn free_right_and_alternating_cycle_assignments_are_ambiguous() {
        let single = resolve_assignments(&edges(&[("a", &["x", "y"])]));
        assert_eq!(single.get("a"), Some(&AssignmentResolution::Ambiguous));

        let cycle = resolve_assignments(&edges(&[("a", &["x", "y"]), ("b", &["x", "y"])]));
        assert_eq!(cycle.get("a"), Some(&AssignmentResolution::Ambiguous));
        assert_eq!(cycle.get("b"), Some(&AssignmentResolution::Ambiguous));
    }

    #[test]
    fn unmatched_left_makes_a_shared_assignment_ambiguous() {
        let resolutions = resolve_assignments(&edges(&[("a", &["x"]), ("b", &["x"])]));
        assert_eq!(resolutions.get("a"), Some(&AssignmentResolution::Ambiguous));
        assert_eq!(resolutions.get("b"), Some(&AssignmentResolution::Ambiguous));
    }

    #[test]
    fn a_claim_without_candidates_is_missing() {
        let resolutions = resolve_assignments(&edges(&[("a", &[])]));
        assert_eq!(resolutions.get("a"), Some(&AssignmentResolution::Missing));
    }

    #[test]
    fn dense_graph_uses_bounded_iterative_search() {
        let candidate_ids = (0..128).map(|index| format!("r{index:03}"));
        let all_candidates = candidate_ids.collect::<BTreeSet<_>>();
        let graph = (0..128)
            .map(|index| (format!("l{index:03}"), all_candidates.clone()))
            .collect::<BTreeMap<_, _>>();

        let resolutions = resolve_assignments(&graph);

        assert_eq!(resolutions.len(), 128);
        assert!(resolutions
            .values()
            .all(|resolution| *resolution == AssignmentResolution::Ambiguous));
    }
}
