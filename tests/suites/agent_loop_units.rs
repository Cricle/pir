//! `pir::agent::agent_loop` unit tests moved out of the source module's
//! inline `#[cfg(test)] mod r33_prepare_next_turn_contract`: the
//! `lastCompletedTurn` snapshot the loop records for the `prepare_next_turn`
//! hook (R33).
//!
//! The two items it drives are `#[doc(hidden)] pub` seams: the private
//! `CompletedTurn` payload and `record_completed_turn` (a no-op without the
//! hook, which is why the production path never observes `None` there).
//! Everything else is the public hook surface.

use std::sync::Arc;

use pir::agent::agent_loop::{
    record_completed_turn, CompletedTurn, FinishTurnContext, PrepareNextTurnHook,
};
use pir::ai::types::{AssistantMessage, ContentBlock, TextContent};

fn test_hook() -> PrepareNextTurnHook {
    Arc::new(|_context: Arc<FinishTurnContext>| Box::pin(async { None }))
}

#[test]
fn needs_completed_false_still_records_an_empty_marker() {
    let hook = test_hook();
    let mut completed: Option<CompletedTurn> = None;
    let message = AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: "payload".to_owned(),
            text_signature: None,
        })],
        ..AssistantMessage::default()
    };
    record_completed_turn(Some(&hook), false, &mut completed, 3, &message, &[]);
    let completed = completed.expect("the completion marker must exist");
    assert_eq!(completed.turn_index, 3);
    assert_eq!(
        completed.message,
        AssistantMessage::default(),
        "needs_completed=false must not clone the assistant payload"
    );
    assert!(completed.tool_results.is_empty());
}

#[test]
fn needs_completed_true_copies_the_snapshot() {
    let hook = test_hook();
    let mut completed: Option<CompletedTurn> = None;
    let message = AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: "payload".to_owned(),
            text_signature: None,
        })],
        ..AssistantMessage::default()
    };
    record_completed_turn(Some(&hook), true, &mut completed, 7, &message, &[]);
    let completed = completed.expect("the completion marker must exist");
    assert_eq!(completed.turn_index, 7);
    assert_eq!(
        completed.message, message,
        "the payload is copied when needed"
    );
}

#[test]
fn no_hook_records_nothing() {
    let mut completed: Option<CompletedTurn> = None;
    record_completed_turn(
        None,
        true,
        &mut completed,
        1,
        &AssistantMessage::default(),
        &[],
    );
    assert!(completed.is_none());
}
