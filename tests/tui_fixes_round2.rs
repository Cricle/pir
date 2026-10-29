//! Winpty 交互验收第一轮修复的回归测试（P1-1/P1-2/P2-1/P2-2/P2-3/P3①）。
//!
//! 纯函数缝：键位归一（BackTab ↔ shift+tab）、自然触发 vs Tab 强制路径
//! 补全、命令补全的精确命中与默认高亮、settings 菜单的精确写回路径提示。
//! UI 线程 notice flush（P1-1）与退出序列去重（P3②）锚定在
//! `terminal.rs` 的事件循环里（crossterm-bound），由 ConPTY 驱动脚本验收。

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use pir::config::settings::SettingsManager;
use pir::tui::autocomplete::{builtin_commands, AutocompletePopup, CombinedAutocompleteProvider};
use pir::tui::keybindings::{Keybindings, TuiAction};
use pir::tui::settings_menu::{apply_settings_action, SettingsAction};

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

// ---------------------------------------------------------------------------
// P1-2: shift+tab（ThinkingCycle）跨平台触发
// ---------------------------------------------------------------------------

/// crossterm 在 Windows（VK_TAB+SHIFT）与 Unix（`\x1b[Z`）上都把
/// Shift+Tab 归一为 `BackTab`（无 SHIFT 修饰符）：默认绑定必须命中。
#[test]
fn thinking_cycle_matches_backtab_event() {
    let kb = Keybindings::new();
    let backtab = key(KeyCode::BackTab, KeyModifiers::NONE);
    assert!(
        kb.matches(TuiAction::ThinkingCycle, &backtab),
        "default shift+tab binding must match the normalized BackTab event"
    );
    // 带 SHIFT 修饰的 BackTab（某些终端会上报）同样命中。
    let backtab_shift = key(KeyCode::BackTab, KeyModifiers::SHIFT);
    assert!(kb.matches(TuiAction::ThinkingCycle, &backtab_shift));
    // 字面 shift+tab 事件（Tab + SHIFT）也命中。
    let shift_tab = key(KeyCode::Tab, KeyModifiers::SHIFT);
    assert!(kb.matches(TuiAction::ThinkingCycle, &shift_tab));
}

/// 普通 Tab 决不能触发 thinking 切换。
#[test]
fn plain_tab_does_not_trigger_thinking_cycle() {
    let kb = Keybindings::new();
    let tab = key(KeyCode::Tab, KeyModifiers::NONE);
    assert!(!kb.matches(TuiAction::ThinkingCycle, &tab));
    assert!(kb.matches(TuiAction::AutocompleteAccept, &tab));
}

/// 用户自定义 "shift+tab"（Tab + SHIFT 组合）也能命中 BackTab 事件
///（matches() 归一化同时覆盖自定义绑定）。
#[test]
fn user_shift_tab_override_matches_backtab_event() {
    let value = serde_json::json!({ "thinkingCycle": "shift+tab" });
    let kb = Keybindings::from_settings(Some(&value)).expect("settings");
    let backtab = key(KeyCode::BackTab, KeyModifiers::NONE);
    assert!(kb.matches(TuiAction::ThinkingCycle, &backtab));
    // 默认的第二个绑定 backtab 也在，且 "backtab" 显式绑定同样命中。
    let explicit = serde_json::json!({ "thinkingCycle": "backtab" });
    let kb = Keybindings::from_settings(Some(&explicit)).expect("settings");
    assert!(kb.matches(TuiAction::ThinkingCycle, &backtab));
}

#[test]
fn at_completion_lists_directories_and_files() {
    let dir = seeded_dir("atdirs");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src").join("main.rs"), b"fn main() {}\n").unwrap();
    let provider = CombinedAutocompleteProvider::with_builtin_commands(dir.clone());
    let line = "@".to_string();
    let suggestions = provider
        .suggestions(std::slice::from_ref(&line), 0, 1, false)
        .expect("@ must offer file completions");
    let values: Vec<&str> = suggestions
        .items
        .iter()
        .map(|item| item.value.as_str())
        .collect();
    assert!(
        values.contains(&"@sessions/"),
        "the `sessions/` directory must be offered with its trailing slash: {values:?}"
    );
    assert!(
        values.contains(&"@src/"),
        "a project subdirectory must be offered with its trailing slash: {values:?}"
    );
    assert!(
        values.contains(&"@models.json"),
        "files stay in the same list: {values:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// P2-2: 普通输入“词+空格”不弹全 cwd 文件补全
// ---------------------------------------------------------------------------

fn seeded_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pir-tui-fixes-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("models.json"), b"{}").unwrap();
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    dir
}

/// 自然触发（非 force）下 `XY ` 后缀为空 prefix：不得弹出全目录文件列表。
#[test]
fn trailing_space_does_not_pop_file_list_naturally() {
    let dir = seeded_dir("space");
    let provider = CombinedAutocompleteProvider::with_builtin_commands(dir.clone());
    let suggestions = provider.suggestions(&["XY ".to_string()], 0, 3, false);
    assert!(
        suggestions.is_none(),
        "plain word+space must not trigger the whole-cwd file listing"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 显式 Tab（force）仍可从空 prefix 补全文件。
#[test]
fn forced_tab_still_lists_files() {
    let dir = seeded_dir("force");
    let provider = CombinedAutocompleteProvider::with_builtin_commands(dir.clone());
    let suggestions = provider
        .suggestions(&["XY ".to_string()], 0, 3, true)
        .expect("forced Tab must offer file completions");
    assert!(suggestions
        .items
        .iter()
        .any(|item| item.value.contains("models.json")));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// P2-3: /bug + Enter 执行 /bug（精确命令命中优先，不被模糊邻项改写）
// ---------------------------------------------------------------------------

/// pir 实现了 `/bug`（slash.rs `SlashAction::Bug`），它必须出现在补全命令表
/// 里（上游 `BUILTIN_SLASH_COMMANDS` 同样收录），否则 `/bug` 只剩模糊邻项
/// `/debug`。
#[test]
fn bug_command_is_completable() {
    let commands = builtin_commands();
    let names: Vec<&str> = commands
        .iter()
        .map(|command| command.name.as_str())
        .collect();
    for expected in ["bug", "share", "import"] {
        assert!(
            names.contains(&expected),
            "builtin commands must contain /{expected}: {names:?}"
        );
    }
}

/// 输入 `/bug`：补全列表的第一项（默认高亮）就是精确命中的 `bug`，
/// 接受后编辑器内容是 `/bug `（上游 applyCompletion 的命令语义），
/// 回车提交执行 /bug 而非 /debug。
#[test]
fn bug_input_commits_bug_not_debug() {
    let dir = seeded_dir("bug");
    let provider = CombinedAutocompleteProvider::with_builtin_commands(dir.clone());
    let line = "/bug".to_string();
    let suggestions = provider
        .suggestions(std::slice::from_ref(&line), 0, 4, false)
        .expect("/bug must offer command completions");
    // 精确命中排在子序列匹配（debug）之前。
    assert_eq!(
        suggestions.items.first().map(|item| item.value.as_str()),
        Some("bug"),
        "exact command match must rank before the fuzzy neighbour: {:?}",
        suggestions
            .items
            .iter()
            .map(|i| &i.value)
            .collect::<Vec<_>>()
    );

    // 默认高亮（上游 getBestAutocompleteMatchIndex）：精确命中被预选。
    let mut popup = AutocompletePopup::new(suggestions);
    popup.select_best_match();
    assert_eq!(
        popup.selected_item().map(|item| item.value.as_str()),
        Some("bug")
    );

    // Enter：接受高亮项 → 编辑器变为 `/bug ` → 提交执行 /bug。
    let (lines, _cursor_line, _cursor_col) =
        provider.apply_completion(&[line], 0, 4, popup.selected_item().unwrap(), "/bug");
    assert_eq!(lines[0], "/bug ");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 默认高亮规则（上游 editor.ts getBestAutocompleteMatchIndex）：
/// 值精确命中 > 值前缀命中 > 保持 index 0（模糊排序的最佳项）。
/// 注意上游对命令项比较的是完整 prefix（含 `/`），命令场景靠
/// fuzzyFilter 的精确匹配加分把精确命中排到 index 0。
#[test]
fn select_best_match_prefers_exact_then_prefix() {
    use pir::tui::autocomplete::{AutocompleteItem, AutocompleteSuggestions};

    // 文件路径场景：值精确命中胜出。
    let mut popup = AutocompletePopup::new(AutocompleteSuggestions {
        items: vec![
            AutocompleteItem::new("src/main.rs.bak", "main.rs.bak"),
            AutocompleteItem::new("src/main.rs", "main.rs"),
        ],
        prefix: "src/main.rs".to_string(),
    });
    popup.select_best_match();
    assert_eq!(popup.selected, 1, "exact value match must win");

    // 值前缀命中胜过保持 index 0。
    let mut popup = AutocompletePopup::new(AutocompleteSuggestions {
        items: vec![
            AutocompleteItem::new("other.txt", "other.txt"),
            AutocompleteItem::new("src/ma", "ma"),
        ],
        prefix: "src/ma".to_string(),
    });
    popup.select_best_match();
    assert_eq!(popup.selected, 1, "prefix match must win over index 0");

    // 命令场景（值不含 `/`）：无值级命中 → 保持 index 0，即 fuzzyFilter
    // 排序后的最佳项（精确命中的 bug）。
    let mut popup = AutocompletePopup::new(AutocompleteSuggestions {
        items: vec![
            AutocompleteItem::new("bug", "bug"),
            AutocompleteItem::new("debug", "debug"),
        ],
        prefix: "/bug".to_string(),
    });
    popup.select_best_match();
    assert_eq!(popup.selected, 0);
    assert_eq!(
        popup.selected_item().map(|item| item.value.as_str()),
        Some("bug")
    );
}

// ---------------------------------------------------------------------------
// P3①: settings 菜单提示精确写回路径
// ---------------------------------------------------------------------------

#[test]
fn saved_setting_message_reports_exact_path() {
    let dir = std::env::temp_dir().join(format!("pir-settings-path-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, false);

    let message = apply_settings_action(
        &mut manager,
        &SettingsAction::Set {
            id: "image-width-cells",
            value: serde_json::json!(48),
        },
    )
    .expect("image-width-cells must map to a settings write");
    assert_eq!(message, "Saved setting: terminal.imageWidthCells");

    let message = apply_settings_action(
        &mut manager,
        &SettingsAction::Set {
            id: "steering-mode",
            value: serde_json::json!("all"),
        },
    )
    .expect("steering-mode must map to a settings write");
    assert_eq!(message, "Saved setting: steeringMode");
    let _ = std::fs::remove_dir_all(&dir);
}
