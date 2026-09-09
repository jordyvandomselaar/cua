use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::sync::Arc;

use crate::ax::bindings::{
    copy_action_names, element_screen_center, kAXErrorSuccess, perform_action, AXUIElementRef,
};

use super::ToolState;

pub struct DoubleClickTool {
    state: Arc<ToolState>,
}

fn background_action_for_element(
    has_ax_open: bool,
) -> cua_driver_core::background_input::BackgroundAction {
    if has_ax_open {
        cua_driver_core::background_input::BackgroundAction::AxSemantic
    } else {
        cua_driver_core::background_input::BackgroundAction::WindowPointer
    }
}

impl DoubleClickTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "double_click".into(),
        description:
            "Double-click at (x, y) or on a snapshot-bound AX element.\n\n\
             AX path: performs AXOpen when advertised; otherwise double-clicks the element's center.\n\n\
             Pixel path: uses click with count:2, including its exact-window checks, background \
             preparation, foreground HID fallback, cursor restoration, and result reporting."
                .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["pid"],
            "properties": {
                "session": { "type": "string", "description": "Public lifecycle session label." },
                "pid": { "type": "integer" },
                "x": { "type": "number", "description": "Window-local screenshot X coordinate." },
                "y": { "type": "number", "description": "Window-local screenshot Y coordinate." },
                "window_id": { "type": "integer", "description": "CGWindowID. Required with element_index; carried by element_token." },
                "element_index": cua_driver_core::tool_schema::element_index_schema(),
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "snapshot_id": cua_driver_core::tool_schema::snapshot_id_schema(),
                "delivery_mode": cua_driver_core::tool_schema::delivery_mode_schema()
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    })
}

#[async_trait]
impl Tool for DoubleClickTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, mut args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid = match args.require_i32("pid") {
            Ok(pid) => pid,
            Err(error) => return error,
        };
        let foreground =
            super::DeliveryMode::parse(args.opt_str("delivery_mode").as_deref()).is_foreground();
        let window_id = args.opt_u64("window_id").map(|id| id as u32);
        let resolved = match cua_driver_core::element_token::resolve_element_args(
            pid,
            args.opt_u64("element_index").map(|index| index as usize),
            args.opt_str("element_token").as_deref(),
            args.opt_str("snapshot_id").as_deref(),
            window_id,
            "double_click",
        ) {
            Ok(resolved) => resolved,
            Err(error) => return error,
        };

        if let cua_driver_core::element_token::ResolvedElement::Element {
            window_id: Some(wid),
            element_index: index,
            ..
        } = resolved
        {
            // Retain across the action so another snapshot cannot free this target.
            let element_guard = match self
                .state
                .element_cache
                .get_element_retained(pid, wid, index)
            {
                Some(element) => element,
                None => {
                    return ToolResult::error(format!(
                        "Element index {index} not found. Call get_window_state first."
                    ));
                }
            };
            let element_ptr = element_guard.as_ptr();
            let has_ax_open = tokio::task::spawn_blocking(move || unsafe {
                copy_action_names(element_ptr as AXUIElementRef)
                    .iter()
                    .any(|action| action == "AXOpen")
            })
            .await
            .unwrap_or(false);
            let mutation_lease = if !foreground {
                match super::gate_background_window_action(
                    pid,
                    wid,
                    Some(element_ptr),
                    background_action_for_element(has_ax_open),
                )
                .await
                {
                    Ok(lease) => Some(lease),
                    Err(refusal) => return refusal,
                }
            } else {
                None
            };

            if has_ax_open {
                let result = tokio::task::spawn_blocking(move || unsafe {
                    perform_action(element_ptr as AXUIElementRef, "AXOpen")
                })
                .await;
                match result {
                    Ok(error) if error == kAXErrorSuccess => {
                        return ToolResult::text(format!("AXOpen performed on element [{index}]."))
                            .with_structured(serde_json::json!({
                                "path": "ax", "verified": false, "effect": "unverifiable"
                            }));
                    }
                    Ok(error) if !foreground => {
                        return ToolResult::error(format!(
                            "AXOpen returned {error}; background delivery will not fall back to pointer input."
                        ));
                    }
                    Err(error) => return ToolResult::error(format!("AXOpen task failed: {error}")),
                    _ => {}
                }
            }

            let center = tokio::task::spawn_blocking(move || unsafe {
                element_screen_center(element_ptr as AXUIElementRef)
            })
            .await;
            let (x, y) = match center {
                Ok(Some(center)) => center,
                _ => return ToolResult::error("Cannot resolve element's screen center."),
            };
            let frame = match super::px_frame::resolve_or_refuse(wid).await {
                Ok(frame) => frame,
                Err(refusal) => return refusal,
            };
            let ratio = self
                .state
                .resize_registry
                .ratio(pid, Some(wid))
                .unwrap_or(1.0);
            args["x"] = ((x - frame.bounds.x) * frame.scale / ratio).into();
            args["y"] = ((y - frame.bounds.y) * frame.scale / ratio).into();
            args["window_id"] = wid.into();
            for key in ["element_token", "element_index", "snapshot_id"] {
                args.as_object_mut()
                    .expect("tool arguments are an object")
                    .remove(key);
            }
            // click acquires its own exact-window pointer lease.
            drop(mutation_lease);
        }

        args["count"] = 2.into();
        super::click::ClickTool::new(self.state.clone())
            .invoke(args)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_element_route_does_not_guess_across_actuator_classes() {
        assert_eq!(
            background_action_for_element(true),
            cua_driver_core::background_input::BackgroundAction::AxSemantic
        );
        assert_eq!(
            background_action_for_element(false),
            cua_driver_core::background_input::BackgroundAction::WindowPointer
        );
    }
}
