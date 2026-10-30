#[derive(Clone, Copy, Default)]
pub(crate) struct BasisArguments {
    pub snapshot_id: bool,
    pub x: bool,
    pub y: bool,
    pub element_token: bool,
    pub capture_id: bool,
    pub from_zoom: bool,
    pub desktop: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BasisSelection {
    Legacy,
    Basis,
    Invalid,
}

pub(crate) fn select_basis(args: BasisArguments) -> BasisSelection {
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
