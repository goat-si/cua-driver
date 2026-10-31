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

#[derive(Debug, Clone, PartialEq)]
pub struct ElementFacts {
    pub visible: Evidence<bool>,
    pub enabled: Evidence<bool>,
    pub editable: Evidence<bool>,
    pub stable: Evidence<bool>,
    pub receives_events: Evidence<HitEvidence>,
    pub bounds: Evidence<Rect>,
    pub affordances: Evidence<Vec<Affordance>>,
    pub role: Evidence<String>,
    /// Whether live state proves visibility without geometry for a nonspatial target.
    pub nonspatial_visible: bool,
}
