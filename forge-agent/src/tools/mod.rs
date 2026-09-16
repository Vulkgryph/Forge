// SPDX-License-Identifier: Apache-2.0
pub mod custom;
pub mod documents;
mod definitions;
mod executor;
pub mod papers;
pub mod patch;
pub mod refused;
pub mod search;
pub mod scratchpad;
pub mod web;

pub use definitions::{
    ask_question_definition, delegate_task_definition, enter_plan_mode_definition,
};
pub use executor::{terminate_child, SpawnedCommand, ToolExecutor, ToolKind};
pub use scratchpad::Scratchpad;
