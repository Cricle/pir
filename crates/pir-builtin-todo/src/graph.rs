//! Dependency graph helpers: cycle detection and reverse (`blocks`) edges.

use std::collections::BTreeMap;

use crate::types::Task;

/// Whether merging `new_blocked_by` into `task_id`'s edges would close a
/// cycle. Iterative DFS (explicit stack) so a long chain can never overflow
/// the call stack; O(V+E).
pub fn detect_cycle(tasks: &[Task], task_id: u64, new_blocked_by: &[u64]) -> bool {
    let index: BTreeMap<u64, usize> = tasks
        .iter()
        .enumerate()
        .map(|(position, task)| (task.id, position))
        .collect();
    let mut adjacency: Vec<Vec<usize>> = Vec::with_capacity(tasks.len());
    for task in tasks {
        let mut deps: Vec<u64> = task.blocked_by.clone().unwrap_or_default();
        if task.id == task_id {
            for dep in new_blocked_by {
                if !deps.contains(dep) {
                    deps.push(*dep);
                }
            }
        }
        // Edges to ids that are not tasks (dangling) carry no out-edges.
        adjacency.push(
            deps.iter()
                .filter_map(|dep| index.get(dep).copied())
                .collect(),
        );
    }

    // 0 = white (unvisited), 1 = gray (on stack), 2 = black (done).
    let mut color = vec![0_u8; tasks.len()];
    // The loop variable is the DFS root index, not a slice cursor: both
    // `color` and `adjacency` are indexed by it, so the range form is the
    // clearest shape here.
    #[allow(clippy::needless_range_loop)]
    for start in 0..tasks.len() {
        if color[start] != 0 {
            continue;
        }
        color[start] = 1;
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(&(node, cursor)) = stack.last() {
            if cursor < adjacency[node].len() {
                let next = adjacency[node][cursor];
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
                match color[next] {
                    1 => return true,
                    0 => {
                        color[next] = 1;
                        stack.push((next, 0));
                    }
                    _ => {}
                }
            } else {
                color[node] = 2;
                stack.pop();
            }
        }
    }
    false
}

/// Inverse edges: which tasks list a given id in their `blockedBy`.
/// O(edges).
pub fn derive_blocks(tasks: &[Task]) -> BTreeMap<u64, Vec<u64>> {
    let mut blocks: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for task in tasks {
        for dep in task.blocked_by.as_deref().unwrap_or(&[]) {
            blocks.entry(*dep).or_default().push(task.id);
        }
    }
    blocks
}
