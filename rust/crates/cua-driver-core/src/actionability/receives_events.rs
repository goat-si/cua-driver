use super::{Point, Rect};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Occluder {
    pub role: Option<String>,
    pub name: Option<String>,
    pub bounds: Option<Rect>,
}
#[derive(Debug, Clone, PartialEq)]
pub enum Hit {
    ReachesTarget,
    Occluded(Occluder),
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HitEvidence {
    /// Whether this hit test used the target window's own content hierarchy.
    #[serde(default)]
    pub target_window: bool,
    pub attempted: u8,
    pub unknown: u8,
    pub occluded: u8,
    pub verified_point: Option<Point>,
    pub occluder: Option<Occluder>,
}

pub fn candidate_points(r: Rect) -> [Point; 5] {
    [
        (0.5, 0.5),
        (0.25, 0.25),
        (0.75, 0.25),
        (0.25, 0.75),
        (0.75, 0.75),
    ]
    .map(|(x, y)| Point {
        x: r.x + r.width * x,
        y: r.y + r.height * y,
    })
}

/// Stops at the first confirmed point. This does not validate a later action coordinate.
pub fn collect_hits(bounds: Rect, mut hit_test: impl FnMut(Point) -> Hit) -> HitEvidence {
    collect_hits_scoped(bounds, &mut hit_test, false)
}

/// Collect hits from the target window's own hit-test API, without activating it.
pub fn collect_window_hits(bounds: Rect, mut hit_test: impl FnMut(Point) -> Hit) -> HitEvidence {
    collect_hits_scoped(bounds, &mut hit_test, true)
}

fn collect_hits_scoped(
    bounds: Rect,
    hit_test: &mut impl FnMut(Point) -> Hit,
    target_window: bool,
) -> HitEvidence {
    let mut result = HitEvidence {
        target_window,
        attempted: 0,
        unknown: 0,
        occluded: 0,
        verified_point: None,
        occluder: None,
    };
    for point in candidate_points(bounds) {
        result.attempted += 1;
        match hit_test(point) {
            Hit::ReachesTarget => {
                result.verified_point = Some(point);
                break;
            }
            Hit::Unknown => result.unknown += 1,
            Hit::Occluded(occluder) => {
                result.occluded += 1;
                if result.occluder.is_none() {
                    result.occluder = Some(occluder);
                }
            }
        }
    }
    result
}
