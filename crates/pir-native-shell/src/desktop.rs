use std::sync::Arc;

use nsh_engine::HostChannel;
use nsh_platform_gpui::{AppOptions, AppSource};
use pir_core::sdk::{AgentBuilder, Endpoint};
use serde_json::json;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, watch};

use crate::gui_core::{GuiCommand, GuiEvent, NativeShellBridge};

/// Minimal desktop Native Shell host.
///
/// The session engine remains in `pir-core`; Native Shell only owns the window
/// and forwards JSON commands/events through `nsh/host`.
pub fn run() -> anyhow::Result<()> {
    let (commands, mut command_rx) = mpsc::channel::<GuiCommand>(64);
    let (bridge_events, bridge_events_rx) = watch::channel(GuiEvent::State {
        state: crate::gui_core::GuiState::default(),
    });
    let host = HostChannel::new();
    let command_host = host.clone();
    command_host.handle("command", move |call| {
        let data = call.data().clone();
        let Some(reply) = call.into_reply() else {
            return;
        };
        let commands = commands.clone();
        std::thread::spawn(move || {
            let command = match crate::gui_core::GuiCodec::decode_command(&data) {
                Ok(command) => command,
                Err(error) => {
                    reply.reject(format!("invalid pir command: {error}"));
                    return;
                }
            };
            if commands.blocking_send(command).is_err() {
                reply.reject("pir bridge is closed");
            } else {
                reply.resolve(json!({ "accepted": true }));
            }
        });
    });

    let endpoint_url = std::env::var("PIR_NATIVE_SHELL_URL")
        .unwrap_or_else(|_| "http://localhost:11434/v1".to_owned());
    let model = std::env::var("PIR_NATIVE_SHELL_MODEL").unwrap_or_else(|_| "default".to_owned());
    let endpoint = Endpoint::new(
        &endpoint_url,
        pir_core::sdk::ApiKind::OpenAiCompletions,
        model,
    );
    let bridge_events_for_engine = bridge_events.clone();
    std::thread::spawn(move || {
        // A runtime that cannot be built means this thread has no bridge to
        // drive, so the UI would sit on the default state forever. Report it
        // through the same channel every other bridge failure uses instead of
        // panicking on a detached thread (a panic there is invisible to the
        // window).
        let runtime = match Runtime::new() {
            Ok(runtime) => runtime,
            Err(error) => {
                bridge_events_for_engine.send_replace(GuiEvent::Error {
                    message: format!("Native Shell bridge runtime: {error}"),
                });
                return;
            }
        };
        let mut bridge =
            match runtime.block_on(build_bridge(endpoint, bridge_events_for_engine.clone())) {
                Ok(bridge) => bridge,
                Err(error) => {
                    bridge_events_for_engine.send_replace(GuiEvent::Error {
                        message: error.to_string(),
                    });
                    return;
                }
            };
        runtime.block_on(async move {
            while let Some(command) = command_rx.recv().await {
                bridge.command(command).await;
            }
        });
    });

    let event_host = host.clone();
    std::thread::spawn(move || {
        // This thread only forwards watch updates to the host, and it OWNS the
        // receiver, so there is no event channel to report through. Without a
        // runtime it cannot deliver anything; log the reason instead of
        // panicking on a detached thread.
        let Ok(runtime) = Runtime::new() else {
            pir_core::logging::ext_warn(
                "native-shell: could not start the event runtime; UI events will not be delivered",
            );
            return;
        };
        let mut events = bridge_events_rx;
        while runtime.block_on(events.changed()).is_ok() {
            let event = events.borrow_and_update().clone();
            if let Ok(value) = crate::gui_core::GuiCodec::encode_event(&event) {
                let _ = event_host.send("state", value);
            }
        }
    });

    nsh_platform_gpui::set_ui_sets(vec![nsh_base::registry()]);
    let source = AppSource::Bundle(nsh_platform_gpui::Bundle::source(
        include_str!("../assets/app.js").to_owned(),
    ));
    let status = nsh_platform_gpui::run_app(
        source,
        AppOptions {
            title: "pir Native Shell".to_owned(),
            app_id: "com.cricle.pir.native-shell".to_owned(),
            host,
            ..Default::default()
        },
    );
    if status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("Native Shell window failed to start"))
    }
}

/// Create the GUI-side host configuration and the agent bridge.
///
/// Kept separate from `run` so headless embedding/tests can construct the
/// bridge without opening a window.
pub async fn build_bridge(
    endpoint: Endpoint,
    events: watch::Sender<crate::gui_core::GuiEvent>,
) -> Result<NativeShellBridge, pir_core::sdk::SdkError> {
    let agent = AgentBuilder::new()
        .endpoint(endpoint)
        .no_session()
        .build()
        .await?;
    Ok(NativeShellBridge::new(agent, events))
}

/// Encode a host message sent by the Native Shell JS app.
pub fn command_message(command: &GuiCommand) -> serde_json::Value {
    match serde_json::to_value(command) {
        Ok(value) => value,
        Err(_) => json!({ "type": "invalid_command" }),
    }
}

pub type SharedBridge = Arc<tokio::sync::Mutex<NativeShellBridge>>;
