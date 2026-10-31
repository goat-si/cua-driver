//! Side-effect-free planning for EIS input.
use std::collections::HashMap;

pub(crate) type DeviceKey = usize;
pub(crate) type KeymapChars = HashMap<char, (u32, bool)>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Region {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}
impl Region {
    fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}
#[derive(Debug)]
pub(crate) struct Device {
    pub key: DeviceKey,
    pub resumed: bool,
    pub absolute: bool,
    pub regions: Vec<Region>,
}
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RouteRefusal {
    #[error("non-finite absolute coordinates")]
    NonFinite,
    #[error("resumed absolute EIS device announced no regions (EIS bug)")]
    NoRegions,
    #[error("coordinates outside EIS regions")]
    OutsideRegions,
    #[error("drag does not remain inside one EIS region")]
    CrossRegion,
}
fn point(x: f64, y: f64) -> Result<(f32, f32), RouteRefusal> {
    let (x, y) = (x as f32, y as f32);
    if !x.is_finite() || !y.is_finite() {
        return Err(RouteRefusal::NonFinite);
    }
    Ok((x, y))
}
fn eligible(devices: &[Device]) -> impl Iterator<Item = &Device> {
    devices.iter().filter(|d| d.resumed && d.absolute)
}
pub(crate) fn route_absolute(
    devices: &[Device],
    x: f64,
    y: f64,
) -> Result<(DeviceKey, f32, f32), RouteRefusal> {
    let (x, y) = point(x, y)?;
    for device in eligible(devices) {
        if device.regions.iter().any(|r| r.contains(x, y)) {
            return Ok((device.key, x, y));
        }
    }
    if eligible(devices).any(|d| d.regions.is_empty()) {
        Err(RouteRefusal::NoRegions)
    } else {
        Err(RouteRefusal::OutsideRegions)
    }
}
pub(crate) fn plan_drag(
    devices: &[Device],
    from: (f64, f64),
    to: (f64, f64),
    steps: u32,
) -> Result<(DeviceKey, Vec<(f32, f32)>), RouteRefusal> {
    let (x0, y0) = point(from.0, from.1)?;
    let (x1, y1) = point(to.0, to.1)?;
    let n = steps.clamp(1, 500);
    let mut points = Vec::with_capacity(n as usize + 1);
    points.push((x0, y0));
    for step in 1..=n {
        let t = step as f32 / n as f32;
        points.push(if step == n {
            (x1, y1)
        } else {
            (x0 + (x1 - x0) * t, y0 + (y1 - y0) * t)
        });
    }
    if points.iter().any(|(x, y)| !x.is_finite() || !y.is_finite()) {
        return Err(RouteRefusal::NonFinite);
    }
    for device in eligible(devices) {
        if device
            .regions
            .iter()
            .any(|r| points.iter().all(|&(x, y)| r.contains(x, y)))
        {
            return Ok((device.key, points));
        }
    }
    route_absolute(devices, from.0, from.1)?;
    route_absolute(devices, to.0, to.1)?;
    Err(RouteRefusal::CrossRegion)
}
/// With several regions, `current` must identify the current output.
pub(crate) fn coordinate_free_centre(
    regions: &[Region],
    current: Option<(f32, f32)>,
) -> Option<(f32, f32)> {
    let region = if regions.len() == 1 {
        regions.first()?
    } else {
        let (x, y) = current?;
        regions.iter().find(|r| r.contains(x, y))?
    };
    Some((
        region.x + region.width / 2.0,
        region.y + region.height / 2.0,
    ))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyStroke {
    pub code: u32,
    pub shift: Option<u32>,
}
#[derive(Debug, thiserror::Error)]
#[error("keycode typing refused {count} character(s); categories: {categories:?}")]
pub(crate) struct KeyRefusal {
    pub count: usize,
    pub categories: Vec<unicode_general_category::GeneralCategory>,
}
pub(crate) fn plan_keycodes(
    text: &str,
    keymap: Option<&KeymapChars>,
    shift: Option<u32>,
) -> Result<Vec<KeyStroke>, KeyRefusal> {
    let mut strokes = Vec::new();
    let mut categories = Vec::new();
    for ch in text.chars() {
        let mapped = match keymap {
            Some(map) => map.get(&ch).copied(),
            None => char_to_evdev(ch),
        };
        let shift_code = if keymap.is_some() { shift } else { Some(42) };
        match mapped {
            Some((code, needs_shift)) if !needs_shift || shift_code.is_some() => {
                strokes.push(KeyStroke {
                    code,
                    shift: if needs_shift { shift_code } else { None },
                })
            }
            _ => categories.push(unicode_general_category::get_general_category(ch)),
        }
    }
    if categories.is_empty() {
        Ok(strokes)
    } else {
        Err(KeyRefusal {
            count: categories.len(),
            categories,
        })
    }
}
fn char_to_evdev(c: char) -> Option<(u32, bool)> {
    // Letter codes follow the QWERTY scancode layout (input-event-codes.h).
    fn letter(c: char) -> u32 {
        // KEY_A..KEY_Z follow the QWERTY scancode layout, not the alphabet.
        match c {
            'a' => 30,
            'b' => 48,
            'c' => 46,
            'd' => 32,
            'e' => 18,
            'f' => 33,
            'g' => 34,
            'h' => 35,
            'i' => 23,
            'j' => 36,
            'k' => 37,
            'l' => 38,
            'm' => 50,
            'n' => 49,
            'o' => 24,
            'p' => 25,
            'q' => 16,
            'r' => 19,
            's' => 31,
            't' => 20,
            'u' => 22,
            'v' => 47,
            'w' => 17,
            'x' => 45,
            'y' => 21,
            'z' => 44,
            _ => 0,
        }
    }
    // (keycode, needs_shift). Digit/symbol codes and their shifted variants
    // follow the US QWERTY row (input-event-codes.h).
    Some(match c {
        'a'..='z' => (letter(c), false),
        'A'..='Z' => (letter(c.to_ascii_lowercase()), true),
        '1' => (2, false),
        '2' => (3, false),
        '3' => (4, false),
        '4' => (5, false),
        '5' => (6, false),
        '6' => (7, false),
        '7' => (8, false),
        '8' => (9, false),
        '9' => (10, false),
        '0' => (11, false),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        ' ' => (57, false),
        '\n' => (28, false),
        '\t' => (15, false),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        '\\' => (43, false),
        '|' => (43, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '`' => (41, false),
        '~' => (41, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn region(x: f32, width: f32) -> Region {
        Region {
            x,
            y: 0.0,
            width,
            height: 1080.0,
        }
    }
    fn device(key: usize, regions: Vec<Region>) -> Device {
        Device {
            key,
            resumed: true,
            absolute: true,
            regions,
        }
    }
    #[test]
    fn secondary_output_uses_layout_coordinates() {
        let devices = [device(1, vec![region(0.0, 1920.0), region(1920.0, 2560.0)])];
        assert_eq!(
            route_absolute(&devices, 2000.0, 100.0),
            Ok((1, 2000.0, 100.0))
        );
        assert_eq!(
            route_absolute(&devices, 1919.99999, 100.0),
            Ok((1, 1920.0, 100.0))
        );
        assert_eq!(
            route_absolute(&devices, 4480.0, 100.0),
            Err(RouteRefusal::OutsideRegions)
        );
        assert_eq!(
            route_absolute(&devices, f64::NAN, 0.0),
            Err(RouteRefusal::NonFinite)
        );
        assert_eq!(
            route_absolute(&devices, f64::MAX, 0.0),
            Err(RouteRefusal::NonFinite)
        );
    }
    #[test]
    fn refuses_gaps_missing_regions_and_ineligible_devices() {
        assert_eq!(
            route_absolute(
                &[device(0, vec![region(0.0, 10.0), region(20.0, 10.0)])],
                15.0,
                0.0
            ),
            Err(RouteRefusal::OutsideRegions)
        );
        assert_eq!(
            route_absolute(&[device(0, vec![])], 0.0, 0.0),
            Err(RouteRefusal::NoRegions)
        );
        let mut d = device(0, vec![region(0.0, 10.0)]);
        d.resumed = false;
        assert_eq!(
            route_absolute(&[d], 0.0, 0.0),
            Err(RouteRefusal::OutsideRegions)
        );
        let mut d = device(0, vec![region(0.0, 10.0)]);
        d.absolute = false;
        assert_eq!(
            route_absolute(&[d], 0.0, 0.0),
            Err(RouteRefusal::OutsideRegions)
        );
    }
    #[test]
    fn drag_stays_in_one_region() {
        let d = [device(0, vec![region(0.0, 10.0), region(10.0, 10.0)])];
        assert_eq!(
            plan_drag(&d, (1.0, 2.0), (7.0, 8.0), 3),
            Ok((0, vec![(1.0, 2.0), (3.0, 4.0), (5.0, 6.0), (7.0, 8.0)]))
        );
        assert_eq!(
            plan_drag(&d, (1.0, 2.0), (11.0, 2.0), 2),
            Err(RouteRefusal::CrossRegion)
        );
        let devices = [
            device(0, vec![region(0.0, 10.0)]),
            device(1, vec![region(10.0, 10.0)]),
        ];
        assert_eq!(
            plan_drag(&devices, (1.0, 2.0), (11.0, 2.0), 2),
            Err(RouteRefusal::CrossRegion)
        );
    }
    #[test]
    fn keymaps_are_authoritative_and_admission_is_atomic() {
        let mut map = KeymapChars::new();
        assert!(plan_keycodes("a", Some(&map), Some(42)).is_err());
        assert_eq!(plan_keycodes("a", None, None).unwrap()[0].code, 30);
        map.insert('a', (31, false));
        map.insert('A', (31, true));
        assert_eq!(plan_keycodes("a", Some(&map), None).unwrap()[0].code, 31);
        assert!(plan_keycodes("A", Some(&map), None).is_err());
        assert_eq!(
            plan_keycodes("A", Some(&map), Some(54)).unwrap()[0].shift,
            Some(54)
        );
        assert_eq!(plan_keycodes("!", None, None).unwrap()[0].shift, Some(42));
        let refusal = plan_keycodes("a€b", None, None).unwrap_err();
        assert_eq!(refusal.count, 1);
        assert!(!format!("{refusal:?} {refusal}").contains('€'));
    }
    #[test]
    fn centre_is_in_logical_region_space() {
        assert_eq!(
            coordinate_free_centre(
                &[Region {
                    x: 0.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0
                }],
                None
            ),
            Some((960.0, 540.0))
        );
        assert_eq!(
            coordinate_free_centre(&[region(0.0, 10.0), region(10.0, 10.0)], Some((11.0, 1.0))),
            Some((15.0, 540.0))
        );
    }
}
