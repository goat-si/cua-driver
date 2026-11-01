//! Scope-qualified, read-only AX observations. The raw seam never dispatches input.
use super::{Evidence, Hit, Point, Scope, ScopeReader};
use crate::ax::actionability::{NativeReader, Reader};
use crate::ax::bindings::{self, AXUIElementRef};
use crate::ax::snapshot::RetainedElement;
use core_foundation::base::{CFEqual, CFRelease, CFTypeRef};
use cua_driver_core::actionability::{Occluder, Rect};

const MAX_ANCESTORS: usize = 32;
const MAX_CELL_ANCESTORS: usize = 8;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
}

/// Raw reads return owned nodes so that a chain remains retained while read.
pub(super) trait RawScope<E> {
    type Node: Clone;
    fn node(&mut self, element: E) -> Self::Node;
    fn same(&mut self, a: &Self::Node, b: &Self::Node) -> bool;
    fn parent(&mut self, node: &Self::Node) -> Evidence<Self::Node>;
    fn window(&mut self, node: &Self::Node) -> Evidence<Self::Node>;
    fn role(&mut self, node: &Self::Node) -> Evidence<String>;
    fn frame(&mut self, node: &Self::Node) -> Evidence<Rect>;
    fn hidden(&mut self) -> Evidence<bool>;
    fn minimized(&mut self, window: &Self::Node) -> Evidence<bool>;
    fn current_space(&mut self, window: &Self::Node) -> Evidence<bool>;
    fn hit(&mut self, window: &Self::Node, scope: Scope, point: Point) -> Evidence<Self::Node>;
}

pub(crate) struct NativeRaw {
    pid: i32,
}
impl NativeRaw {
    fn ptr(node: &RetainedElement) -> AXUIElementRef {
        node.as_ptr() as AXUIElementRef
    }
    fn copy_attr(node: &RetainedElement, attribute: &str) -> Evidence<RetainedElement> {
        // The copying helper returns None on both missing and failed reads. Neither
        // can prove ancestry or a different hit target.
        match unsafe { bindings::copy_element_attr(Self::ptr(node), attribute) } {
            Some(ptr) => {
                let retained = unsafe { RetainedElement::retain(ptr as usize) };
                unsafe { CFRelease(ptr as CFTypeRef) };
                Evidence::Known(retained)
            }
            None => Evidence::Unknown(format!("{attribute} unavailable")),
        }
    }
}
impl RawScope<AXUIElementRef> for NativeRaw {
    type Node = RetainedElement;
    fn node(&mut self, element: AXUIElementRef) -> Self::Node {
        // Caller holds the incoming retained target and window throughout the probe.
        unsafe { RetainedElement::retain(element as usize) }
    }
    fn same(&mut self, a: &Self::Node, b: &Self::Node) -> bool {
        unsafe { CFEqual(Self::ptr(a) as CFTypeRef, Self::ptr(b) as CFTypeRef) != 0 }
    }
    fn parent(&mut self, node: &Self::Node) -> Evidence<Self::Node> {
        Self::copy_attr(node, "AXParent")
    }
    fn window(&mut self, node: &Self::Node) -> Evidence<Self::Node> {
        Self::copy_attr(node, "AXWindow")
    }
    fn role(&mut self, node: &Self::Node) -> Evidence<String> {
        NativeReader.string(Self::ptr(node), "AXRole")
    }
    fn frame(&mut self, node: &Self::Node) -> Evidence<Rect> {
        NativeReader.frame(Self::ptr(node))
    }
    fn hidden(&mut self) -> Evidence<bool> {
        let app = unsafe { bindings::AXUIElementCreateApplication(self.pid) };
        if app.is_null() {
            return Evidence::Unknown("application unavailable".into());
        }
        let result = NativeReader.boolean(app, "AXHidden");
        unsafe { CFRelease(app as CFTypeRef) };
        result
    }
    fn minimized(&mut self, window: &Self::Node) -> Evidence<bool> {
        NativeReader.boolean(Self::ptr(window), "AXMinimized")
    }
    fn current_space(&mut self, window: &Self::Node) -> Evidence<bool> {
        let id = unsafe { bindings::ax_get_window_id(Self::ptr(window)) };
        let result = id.and_then(|id| {
            let query = crate::input::skylight::SpaceQuery::new()?;
            let spaces = query.window_space_ids(id)?;
            let current = query.current_space_for_window(id)?;
            Some(spaces.contains(&current))
        });
        result.map_or_else(
            || Evidence::Unknown("window Space unavailable".into()),
            Evidence::Known,
        )
    }
    fn hit(&mut self, _: &Self::Node, scope: Scope, point: Point) -> Evidence<Self::Node> {
        // An app-rooted hit cannot see other applications' windows. It is
        // evidence only for background delivery posted directly to this pid
        // and window without moving the desktop pointer; never use it for a
        // headed or foreground policy. The classifier below additionally
        // checks the hit's exact AX window and bounded target ancestry.
        let root = match scope {
            Scope::WindowInternal => unsafe { bindings::AXUIElementCreateApplication(self.pid) },
            Scope::Foreground => unsafe { AXUIElementCreateSystemWide() },
        };
        if root.is_null() {
            return Evidence::Unknown("hit root unavailable".into());
        }
        let mut hit = std::ptr::null_mut();
        let code = unsafe {
            bindings::AXUIElementCopyElementAtPosition(
                root,
                point.x as f32,
                point.y as f32,
                &mut hit,
            )
        };
        unsafe { CFRelease(root as CFTypeRef) };
        if code != bindings::kAXErrorSuccess || hit.is_null() {
            if !hit.is_null() {
                unsafe { CFRelease(hit as CFTypeRef) };
            }
            return Evidence::Unknown(format!("hit test failed: {code}"));
        }
        let retained = unsafe { RetainedElement::retain(hit as usize) };
        unsafe { CFRelease(hit as CFTypeRef) };
        Evidence::Known(retained)
    }
}

/// Call only while both arguments are retained; all native work here is read-only.
///
/// Window-internal hits use the application AX tree. They do not observe
/// windows of other applications and are only evidence for a background event
/// posted directly to the target pid and window without moving the pointer.
/// Foreground delivery must use a separate system-wide hit test.
pub(crate) struct NativeScope<R = NativeRaw> {
    pub(super) raw: R,
}
impl NativeScope<NativeRaw> {
    pub(crate) fn new(pid: i32) -> Self {
        Self {
            raw: NativeRaw { pid },
        }
    }
}

fn unknown<T>(reason: &str) -> Evidence<T> {
    Evidence::Unknown(reason.to_owned())
}
fn valid(r: Rect) -> bool {
    r.x.is_finite()
        && r.y.is_finite()
        && r.width.is_finite()
        && r.height.is_finite()
        && r.width > 0.0
        && r.height > 0.0
}
fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    if !valid(a) || !valid(b) {
        return None;
    }
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    Some(Rect {
        x,
        y,
        width: (right - x).max(0.0),
        height: (bottom - y).max(0.0),
    })
}
fn contains(r: Rect, p: Point) -> bool {
    valid(r) && p.x >= r.x && p.y >= r.y && p.x < r.x + r.width && p.y < r.y + r.height
}
fn occluded() -> Hit {
    Hit::Occluded(Occluder {
        role: None,
        name: None,
        bounds: None,
    })
}

impl<R> NativeScope<R> {
    /// Walk from the exact retained target to the exact window and intersect
    /// their frames. A missing parent or geometry cannot establish visibility.
    fn clipping<E: Copy>(&mut self, target: E, window: E) -> Evidence<Rect>
    where
        R: RawScope<E>,
    {
        let mut node = self.raw.node(target);
        let window = self.raw.node(window);
        let mut clip = match self.raw.frame(&node) {
            Evidence::Known(r) if valid(r) => r,
            _ => return unknown("target frame unavailable"),
        };
        for _ in 0..MAX_ANCESTORS {
            if self.raw.same(&node, &window) {
                return Evidence::Known(clip);
            }
            node = match self.raw.parent(&node) {
                Evidence::Known(parent) => parent,
                _ => return unknown("target window ancestry unavailable"),
            };
            let frame = match self.raw.frame(&node) {
                Evidence::Known(frame) => frame,
                _ => return unknown("ancestor frame unavailable"),
            };
            if !valid(frame) {
                return unknown("invalid ancestor frame");
            }
            if clip.width > 0.0 && clip.height > 0.0 {
                clip = match intersect(clip, frame) {
                    Some(clip) => clip,
                    None => return unknown("invalid ancestor frame"),
                };
            }
        }
        unknown("target window ancestry exceeded limit")
    }
    /// App-rooted hits are scoped to direct delivery to the retained window;
    /// no result from this classifier proves foreground desktop visibility.
    fn matches_window_hit<E: Copy>(
        &mut self,
        target: E,
        window: E,
        hit: <R as RawScope<E>>::Node,
    ) -> Hit
    where
        R: RawScope<E>,
    {
        let target = self.raw.node(target);
        let window = self.raw.node(window);
        if !self.raw.same(&hit, &window) {
            let hit_window = match self.raw.window(&hit) {
                Evidence::Known(value) => value,
                _ => return Hit::Unknown,
            };
            if !self.raw.same(&hit_window, &window) {
                return occluded();
            }
        }
        let mut current = hit;
        let mut target_seen = false;
        for depth in 0..=MAX_ANCESTORS {
            target_seen |= self.raw.same(&current, &target);
            if self.raw.same(&current, &window) {
                return if target_seen {
                    Hit::ReachesTarget
                } else {
                    occluded()
                };
            }
            if depth == MAX_ANCESTORS {
                return Hit::Unknown;
            }
            current = match self.raw.parent(&current) {
                Evidence::Known(parent) => parent,
                _ => return Hit::Unknown,
            };
        }
        Hit::Unknown
    }
    fn matches_hit<E: Copy>(&mut self, target: E, window: E, hit: <R as RawScope<E>>::Node) -> Hit
    where
        R: RawScope<E>,
    {
        let target = self.raw.node(target);
        let window = self.raw.node(window);
        if self.raw.same(&target, &hit) {
            return Hit::ReachesTarget;
        }
        let hit_window = match self.raw.window(&hit) {
            Evidence::Known(w) => w,
            _ => return Hit::Unknown,
        };
        if !self.raw.same(&hit_window, &window) {
            return occluded();
        }
        // Target's nearest cell is an accepted container, but no more distant
        // cell, and an unknown role cannot be skipped.
        let mut ancestor = target.clone();
        let mut cell_unknown = false;
        for _ in 0..MAX_CELL_ANCESTORS {
            ancestor = match self.raw.parent(&ancestor) {
                Evidence::Known(a) => a,
                _ => {
                    cell_unknown = true;
                    break;
                }
            };
            match self.raw.role(&ancestor) {
                Evidence::Known(role) if role == "AXCell" => {
                    if self.raw.same(&ancestor, &hit) {
                        return Hit::ReachesTarget;
                    }
                    break;
                }
                Evidence::Known(role) if role == "AXWindow" || role == "AXApplication" => break,
                Evidence::Known(_) => {}
                _ => {
                    cell_unknown = true;
                    break;
                }
            }
        }
        let mut current = hit;
        for _ in 0..MAX_ANCESTORS {
            if self.raw.same(&current, &target) {
                return Hit::ReachesTarget;
            }
            if self.raw.same(&current, &window) {
                return if cell_unknown {
                    Hit::Unknown
                } else {
                    occluded()
                };
            }
            current = match self.raw.parent(&current) {
                Evidence::Known(parent) => parent,
                _ => return Hit::Unknown,
            };
        }
        Hit::Unknown
    }
}
impl<E: Copy, R: RawScope<E>> ScopeReader<E> for NativeScope<R> {
    fn visible(&mut self, target: E, window: E, _: Scope) -> Evidence<bool> {
        let w = self.raw.node(window);
        match (self.raw.hidden(), self.raw.minimized(&w)) {
            (Evidence::Known(true), _) | (_, Evidence::Known(true)) => {
                return Evidence::Known(false)
            }
            (Evidence::Known(false), Evidence::Known(false)) => {}
            _ => return unknown("window visibility unavailable"),
        }
        match self.clipping(target, window) {
            Evidence::Known(r) => Evidence::Known(r.width > 0.0 && r.height > 0.0),
            _ => unknown("clipping or ancestry unavailable"),
        }
    }
    fn offscreen(&mut self, target: E, window: E, scope: Scope) -> Evidence<bool> {
        if scope == Scope::WindowInternal {
            return match self.clipping(target, window) {
                Evidence::Known(r) => Evidence::Known(r.width == 0.0 || r.height == 0.0),
                _ => unknown("clipping or ancestry unavailable"),
            };
        }
        let w = self.raw.node(window);
        match self.raw.current_space(&w) {
            Evidence::Known(current) => Evidence::Known(!current),
            _ => unknown("current Space unavailable"),
        }
    }
    fn hit(&mut self, target: E, window: E, scope: Scope, point: Point) -> Hit {
        match self.clipping(target, window) {
            Evidence::Known(r) if contains(r, point) => {}
            Evidence::Known(_) => return Hit::Unknown,
            _ => return Hit::Unknown,
        }
        let w = self.raw.node(window);
        match self.raw.hit(&w, scope, point) {
            Evidence::Known(hit) => match scope {
                Scope::WindowInternal => self.matches_window_hit(target, window, hit),
                Scope::Foreground => self.matches_hit(target, window, hit),
            },
            _ => Hit::Unknown,
        }
    }
}
