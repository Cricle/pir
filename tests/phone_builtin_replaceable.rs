//! `builtin:phone` is declared replaceable in the builtin factory table.
//! The plugin identity must mirror that flag: a non-replaceable extension
//! registering `phone-start` must omit the native phone builtin (upstream
//! `omitReplacedExtensions`) instead of producing a `phone-start:2` command.

#![cfg(feature = "phone")]

use std::path::PathBuf;
use std::sync::Arc;

use pir::extensions::ExtensionConfig;
use pir::extensions::ExtensionHandle;
use pir::plugins::plugin::{AgentPlugin, RegistrationContext};
use pir::plugins::source::PluginInfo;
use pir::plugins::PluginHost;

struct RivalPhoneCommand {
    info: PluginInfo,
}

impl AgentPlugin for RivalPhoneCommand {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn register(&self, ctx: &mut RegistrationContext<'_>) -> anyhow::Result<()> {
        ctx.register_command("phone-start", Some("user replacement"), None);
        Ok(())
    }
}

#[test]
fn phone_builtin_reports_replaceable_and_is_omitted_by_a_user_command() {
    let plugin = pir::plugins::instantiate_builtin("phone").expect("builtin:phone");
    assert!(
        plugin.info().replaceable,
        "builtin:phone must report the factory table's replaceable flag"
    );

    let handle = ExtensionHandle::start_plan_only(ExtensionConfig {
        cwd: std::env::current_dir().unwrap(),
        mode: "print".to_owned(),
        has_ui: false,
        ..Default::default()
    });
    handle.set_path_plan(vec![PathBuf::from("builtin:phone")]);
    let host = PluginHost::build(
        Some(handle),
        vec![Arc::new(RivalPhoneCommand {
            info: PluginInfo::inline("rival-phone"),
        })],
    );
    let paths: Vec<&str> = host
        .plugins()
        .iter()
        .map(|p| p.info().path.as_str())
        .collect();
    assert!(
        !paths.contains(&"builtin:phone"),
        "a user `phone-start` command must omit the replaceable builtin: {paths:?}"
    );
    let commands: Vec<String> = host
        .registry()
        .resolved_commands()
        .iter()
        .map(|command| command.invocation_name.clone())
        .collect();
    assert_eq!(
        commands,
        vec!["phone-start".to_owned()],
        "the builtin must not add a duplicate `phone-start:2`: {commands:?}"
    );
}
