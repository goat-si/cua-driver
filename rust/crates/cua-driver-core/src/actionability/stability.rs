use super::{Clock, Evidence, Rect};
use std::time::Duration;

pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(17);
pub fn geometry_matches(a: Rect, b: Rect) -> bool {
    [
        (a.x, b.x),
        (a.y, b.y),
        (a.width, b.width),
        (a.height, b.height),
    ]
    .into_iter()
    .all(|(x, y)| x.is_finite() && y.is_finite() && (x - y).abs() <= 0.5)
}
#[derive(Debug, Default)]
pub struct StabilitySampler {
    previous: Option<Rect>,
    since: Duration,
    last_sample: Option<Duration>,
    count: u32,
}
impl StabilitySampler {
    pub fn observe(&mut self, clock: &impl Clock, bounds: Evidence<Rect>) -> Evidence<bool> {
        let now = clock.now();
        let bounds = match bounds {
            Evidence::Known(r) if [r.x, r.y, r.width, r.height].iter().all(|v| v.is_finite()) => r,
            Evidence::Known(_) => {
                self.reset();
                return Evidence::Unknown("invalid geometry".into());
            }
            Evidence::Absent => {
                self.reset();
                return Evidence::Absent;
            }
            Evidence::Unknown(reason) => {
                self.reset();
                return Evidence::Unknown(reason);
            }
        };
        if self
            .last_sample
            .is_some_and(|last| now.saturating_sub(last) < SAMPLE_INTERVAL)
        {
            if self
                .previous
                .is_some_and(|previous| !geometry_matches(previous, bounds))
            {
                self.previous = Some(bounds);
                self.since = now;
                self.last_sample = Some(now);
                self.count = 1;
            }
            return Evidence::Known(false);
        }
        if self
            .previous
            .is_some_and(|prev| geometry_matches(prev, bounds))
        {
            self.count += 1;
        } else {
            self.count = 1;
            self.since = now;
        }
        self.last_sample = Some(now);
        self.previous = Some(bounds);
        Evidence::Known(
            self.count >= 3 && now.saturating_sub(self.since) >= Duration::from_millis(34),
        )
    }
    pub fn revalidate(&self, bounds: Evidence<Rect>) -> Evidence<bool> {
        match bounds {
            Evidence::Known(r) => Evidence::Known(
                self.count >= 3 && self.previous.is_some_and(|p| geometry_matches(p, r)),
            ),
            Evidence::Absent => Evidence::Absent,
            Evidence::Unknown(reason) => Evidence::Unknown(reason),
        }
    }
    pub fn samples(&self) -> u32 {
        self.count
    }
    fn reset(&mut self) {
        self.previous = None;
        self.last_sample = None;
        self.count = 0;
    }
}
