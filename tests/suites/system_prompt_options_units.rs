//! Unit coverage moved out of `src/extensions/system_prompt_options.rs`;
//! the source-hygiene gate forbids inline tests in `src/`.

use std::path::PathBuf;

use pir::extensions::system_prompt_options::{
    SkillOption, SystemPromptOptions, DEFAULT_SELECTED_TOOLS,
};
use pir::skills::SkillInfo;

#[test]
fn selected_tools_default_matches_upstream() {
    let options = SystemPromptOptions::from_json(&serde_json::json!({ "cwd": "/tmp" }));
    assert_eq!(options.selected_tools, DEFAULT_SELECTED_TOOLS);
    let empty = SystemPromptOptions::from_json(&serde_json::json!({
        "cwd": "/tmp",
        "selectedTools": [],
    }));
    assert!(empty.selected_tools.is_empty());
}

#[test]
fn mutating_sections_changes_the_rendered_prompt() {
    let mut options = SystemPromptOptions::from_json(&serde_json::json!({
        "cwd": "/work",
        "selectedTools": ["read"],
        "toolSnippets": { "read": "Read a file" },
    }));
    let before = options.render(None);
    assert!(before.contains("<tools>"));
    options.sections.insert(
        "custom".to_owned(),
        serde_json::Value::String("extra".to_owned()),
    );
    let after = options.render(None);
    assert!(after.contains("<custom>\nextra\n</custom>"));
}

#[test]
fn force_system_prompt_wins() {
    let options = SystemPromptOptions::from_json(&serde_json::json!({
        "cwd": "/work",
        "forceSystemPrompt": "forced",
    }));
    assert_eq!(options.render(None), "forced");
}

#[test]
fn skill_option_round_trips_and_survives_renames() {
    let skill = SkillInfo {
        name: "demo".to_owned(),
        description: "A demo".to_owned(),
        path: PathBuf::from("/skills/demo/SKILL.md"),
        disable_model_invocation: false,
    };
    let option = SkillOption::from_skill(&skill);
    assert_eq!(option.file_path, "/skills/demo/SKILL.md");
    assert_eq!(option.base_dir.replace('\\', "/"), "/skills/demo");
    let mut renamed = option.clone();
    renamed.name = "renamed".to_owned();
    assert_eq!(renamed.to_skill().unwrap().name, "renamed");
    let mut pathless = SkillOption::default();
    pathless.source_info.path = "/x.md".to_owned();
    assert_eq!(pathless.to_skill().unwrap().path, PathBuf::from("/x.md"));
    assert!(SkillOption::default().to_skill().is_none());
}
