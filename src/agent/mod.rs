//! Agent loop, tool execution, and orchestration.
//!
//! This module is the Rust equivalent of the upstream `packages/agent/src`.

pub mod agent_loop;
pub mod commit;
pub mod nested_tool_calls;
pub mod tool_batch;
pub mod tool_dependencies;
pub mod tool_execution;
pub mod tool_scheduler;

pub use commit::CommitTracker;
pub use nested_tool_calls::{
    combine_usage, NestedCallLimits, NestedCallRecorder, NestedCallSummary, NestedEventSink,
    NestedToolEvent, NestedToolExecutor, NestedToolOutcome, MAX_NESTED_DEPTH,
};
pub use tool_batch::*;
pub use tool_dependencies::{MutationQueues, ResourceKey};
pub use tool_execution::{AgentTool, ToolContext, ToolHooks, ToolRegistry};
pub use tool_scheduler::{ToolScheduler, ToolSchedulerConfig};
