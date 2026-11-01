use super::{Affordance, HitEvidence};
use serde::{Deserialize, Serialize};

/// `Absent` is a negative answer; `Unknown` records a failed measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence<T> {
    Known(T),
    Absent,
    Unknown(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
impl Rect {
    pub(crate) fn valid(self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width >= 0.0
            && self.height >= 0.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ElementFacts {
    pub visible: Evidence<bool>,
    /// Live offscreen state: `Absent` means no owning-window state was supplied.
    pub offscreen: Evidence<bool>,
    pub enabled: Evidence<bool>,
    pub editable: Evidence<bool>,
    pub stable: Evidence<bool>,
    pub receives_events: Evidence<HitEvidence>,
    pub bounds: Evidence<Rect>,
    pub affordances: Evidence<Vec<Affordance>>,
    /// Exact platform action names advertised by the target.
    pub advertised_actions: Evidence<Vec<String>>,
    /// Evidence that a text range can be selected on this target.
    pub selectable_text: Evidence<bool>,
    pub role: Evidence<String>,
    /// Whether live state proves visibility without geometry for a nonspatial target.
    pub nonspatial_visible: bool,
}
