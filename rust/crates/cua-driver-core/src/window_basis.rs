//! Screenshot coordinate frames and dispatch effect reporting.
use crate::element_token::{format_snapshot_id, parse_snapshot_handle};
use crate::protocol::ToolResult;
use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct WindowBasis {
    pub window_id: u64,
    pub pid: i32,
    pub snapshot_id: String,
    pub x: i64,
    pub y: i64,
    pub width: u32,
    pub height: u32,
    pub scale_x: f64,
    pub scale_y: f64,
    pub capture_width: u32,
    pub capture_height: u32,
    pub display_scale: Option<f64>,
    pub captured_at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LiveFrame {
    pub x: i64,
    pub y: i64,
    pub width: u32,
    pub height: u32,
    pub display_scale: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BasisRefusal {
    pub code: &'static str,
    pub reason: &'static str,
}

impl LiveFrame {
    pub fn to_json(&self) -> Value {
        json!({"x":self.x,"y":self.y,"width":self.width,"height":self.height,"display_scale":self.display_scale})
    }
}
impl WindowBasis {
    pub fn to_json(&self) -> Value {
        json!({"window_id":self.window_id,"pid":self.pid,"snapshot_id":self.snapshot_id,
            "x":self.x,"y":self.y,"width":self.width,"height":self.height,
            "scale_x":self.scale_x,"scale_y":self.scale_y,"capture_width":self.capture_width,
            "capture_height":self.capture_height,"display_scale":self.display_scale,"captured_at_ms":self.captured_at_ms})
    }
    pub fn check(&self, live: &LiveFrame) -> Result<(), BasisRefusal> {
        let reason = if self.x != live.x || self.y != live.y {
            Some("window_moved")
        } else if self.width != live.width || self.height != live.height {
            Some("window_resized")
        } else if self.display_scale != live.display_scale {
            Some("scale_changed")
        } else {
            None
        };
        match reason {
            Some(reason) => Err(BasisRefusal {
                code: "stale_basis",
                reason,
            }),
            None => Ok(()),
        }
    }
    pub fn check_and_map(
        &self,
        live: &LiveFrame,
        x: f64,
        y: f64,
    ) -> Result<(i64, i64), BasisRefusal> {
        self.check(live)?;
        self.to_screen(x, y)
    }

    pub fn to_screen(&self, x: f64, y: f64) -> Result<(i64, i64), BasisRefusal> {
        if !x.is_finite()
            || !y.is_finite()
            || !self.scale_x.is_finite()
            || !self.scale_y.is_finite()
            || self.scale_x <= 0.0
            || self.scale_y <= 0.0
        {
            return Err(BasisRefusal {
                code: "invalid",
                reason: "invalid_coordinates",
            });
        }
        if x < 0.0
            || y < 0.0
            || x >= f64::from(self.capture_width)
            || y >= f64::from(self.capture_height)
        {
            return Err(BasisRefusal {
                code: "point_outside_window",
                reason: "point_outside_window",
            });
        }
        let dx = (x / self.scale_x).floor();
        let dy = (y / self.scale_y).floor();
        if dx >= f64::from(self.width) || dy >= f64::from(self.height) {
            return Err(BasisRefusal {
                code: "point_outside_window",
                reason: "point_outside_window",
            });
        }
        let (Some(sx), Some(sy)) = (self.x.checked_add(dx as i64), self.y.checked_add(dy as i64))
        else {
            return Err(BasisRefusal {
                code: "invalid",
                reason: "invalid_coordinates",
            });
        };
        Ok((sx, sy))
    }
    pub fn refusal(&self, code: &str, reason: &str, live: Option<&LiveFrame>) -> ToolResult {
        basis_refusal(code, reason, Some(self), live)
    }
    pub(crate) fn same_frame(&self, other: &Self) -> bool {
        self.x == other.x
            && self.y == other.y
            && self.width == other.width
            && self.height == other.height
            && self.scale_x == other.scale_x
            && self.scale_y == other.scale_y
            && self.capture_width == other.capture_width
            && self.capture_height == other.capture_height
            && self.display_scale == other.display_scale
    }
}
pub fn basis_refusal(
    code: &str,
    reason: &str,
    basis: Option<&WindowBasis>,
    live: Option<&LiveFrame>,
) -> ToolResult {
    ToolResult::error(format!("{code}: {reason}")).with_structured(json!({"code":code,"reason":reason,"effect":"refused","nothing_sent":true,"prepared_side_effects":[],"window_basis":basis.map(WindowBasis::to_json),"live_frame":live.map(LiveFrame::to_json)}))
}
pub fn format_basis_id(snapshot_id: u32, generation: u32) -> String {
    let id = format_snapshot_id(snapshot_id);
    if generation == 0 {
        id
    } else {
        format!("{id}.f{generation}")
    }
}
pub fn parse_basis_id(id: &str) -> Option<(u32, u32)> {
    match id.split_once(".f") {
        None => Some((parse_snapshot_handle(id)?, 0)),
        Some((sid, suffix)) => {
            let generation: u32 = suffix.parse().ok()?;
            if generation == 0 || generation.to_string() != suffix {
                return None;
            }
            Some((parse_snapshot_handle(sid)?, generation))
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchEffect {
    Refused,
    Partial,
    Uncertain,
    Unverifiable,
}
#[derive(Clone, Debug, PartialEq)]
pub struct DispatchOutcome {
    pub events_planned: u32,
    pub events_sent: u32,
    pub prepared_side_effects: Vec<String>,
    pub effect: DispatchEffect,
    pub cleanup_results: Vec<Value>,
}
impl DispatchOutcome {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.effect == DispatchEffect::Refused && self.events_sent != 0 {
            return Err("refused dispatch cannot have sent events");
        }
        Ok(())
    }

    pub fn to_json(&self) -> Value {
        // A "refused" outcome that nevertheless sent events is contradictory.
        // Report it as uncertain rather than claiming nothing was sent, and
        // never panic in the tool path.
        let contradictory = self.validate().is_err();
        let effect = match self.effect {
            DispatchEffect::Refused if contradictory => "uncertain",
            DispatchEffect::Refused => "refused",
            DispatchEffect::Partial => "partial",
            DispatchEffect::Uncertain => "uncertain",
            DispatchEffect::Unverifiable => "unverifiable",
        };
        let mut result = json!({"events_planned":self.events_planned,"events_sent":self.events_sent,"prepared_side_effects":self.prepared_side_effects,"effect":effect,"cleanup_results":self.cleanup_results});
        if self.effect == DispatchEffect::Refused && self.events_sent == 0 {
            result["nothing_sent"] = json!(true);
        }
        // A cleanup step that could not undo a visible side effect carries a
        // short "warning"; surface it at the top level so it reaches the
        // caller even when the cleanup details are not shown.
        let warnings: Vec<&str> = self
            .cleanup_results
            .iter()
            .filter_map(|entry| entry.get("warning").and_then(Value::as_str))
            .collect();
        if !warnings.is_empty() {
            result["warning"] = json!(warnings.join("; "));
        }
        result
    }
}
