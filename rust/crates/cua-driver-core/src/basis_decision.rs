#[derive(Clone, Copy, Default)]
pub struct BasisArguments {
    pub snapshot_id: bool,
    pub x: bool,
    pub y: bool,
    pub element_token: bool,
    pub capture_id: bool,
    pub from_zoom: bool,
    pub desktop: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BasisSelection {
    Legacy,
    Basis,
    Invalid,
}

pub fn select_basis(args: BasisArguments) -> BasisSelection {
    if !args.snapshot_id {
        BasisSelection::Legacy
    } else if !args.x
        || !args.y
        || args.element_token
        || args.capture_id
        || args.from_zoom
        || args.desktop
    {
        BasisSelection::Invalid
    } else {
        BasisSelection::Basis
    }
}

pub fn select_click_basis(tool: &str, args: &serde_json::Value) -> BasisSelection {
    if !matches!(tool, "click" | "double_click" | "right_click") {
        return BasisSelection::Legacy;
    }
    let present = |key| args.get(key).is_some_and(|v| !v.is_null());
    select_basis(BasisArguments {
        snapshot_id: present("snapshot_id"),
        x: args.get("x").is_some_and(serde_json::Value::is_number),
        y: args.get("y").is_some_and(serde_json::Value::is_number),
        element_token: present("element_token"),
        capture_id: present("capture_id"),
        from_zoom: args.get("from_zoom").and_then(serde_json::Value::as_bool) == Some(true),
        desktop: args.get("scope").and_then(serde_json::Value::as_str) == Some("desktop"),
    })
}
