// Integration tests (kept out of src/).

use pir_builtin_todo::*;

fn task(id: u64, blocked_by: Option<Vec<u64>>) -> Task {
    Task {
        id,
        subject: format!("task {id}"),
        blocked_by,
        ..Task::default()
    }
}

#[test]
fn detects_a_simple_cycle() {
    // 1 is blocked by 2; 2 and 3 are independent.
    let tasks = vec![task(1, Some(vec![2])), task(2, None), task(3, None)];
    // Making 2 depend on 1 closes 2 → 1 → 2.
    assert!(detect_cycle(&tasks, 2, &[1]));
    // A dependency among independent tasks is not a cycle.
    assert!(!detect_cycle(&tasks, 3, &[1]));
    assert!(!detect_cycle(&tasks, 1, &[3]));
}

#[test]
fn detects_long_chain_without_recursion() {
    // ids 1..=5000; id k > 1 depends on id k-1.
    let tasks: Vec<Task> = (1..=5000)
        .map(|id| {
            let blocked_by = (id > 1).then(|| vec![id - 1]);
            task(id, blocked_by)
        })
        .collect();
    // Closing the chain.
    assert!(detect_cycle(&tasks, 1, &[5000]));
    // An edge to an already reachable earlier task adds no path back.
    assert!(!detect_cycle(&tasks, 3, &[1]));
    // Task 3 is inside the chain, so depending on 500 closes a loop.
    assert!(detect_cycle(&tasks, 3, &[500]));
}

#[test]
fn dangling_edges_carry_no_out_edges() {
    let tasks = vec![task(1, Some(vec![99]))];
    assert!(!detect_cycle(&tasks, 2, &[1]));
    // A dangling id in the new edge set cannot close a cycle.
    assert!(!detect_cycle(&tasks, 1, &[42]));
}

#[test]
fn derive_blocks_reports_reverse_edges() {
    let tasks = vec![
        task(1, None),
        task(2, Some(vec![1])),
        task(3, Some(vec![1, 2])),
    ];
    let blocks = derive_blocks(&tasks);
    assert_eq!(blocks.get(&1), Some(&vec![2, 3]));
    assert_eq!(blocks.get(&2), Some(&vec![3]));
    assert_eq!(blocks.get(&3), None);
    assert_eq!(blocks.get(&42), None);
}
