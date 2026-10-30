use crate::basis_decision::{select_basis, BasisArguments, BasisSelection};

#[test]
fn basis_requires_unambiguous_coordinates() {
    let valid = BasisArguments {
        snapshot_id: true,
        x: true,
        y: true,
        ..Default::default()
    };
    assert_eq!(select_basis(valid), BasisSelection::Basis);
    assert_eq!(
        select_basis(BasisArguments { y: false, ..valid }),
        BasisSelection::Invalid
    );
    assert_eq!(
        select_basis(BasisArguments {
            element_token: true,
            ..valid
        }),
        BasisSelection::Invalid
    );
    assert_eq!(
        select_basis(BasisArguments {
            capture_id: true,
            ..valid
        }),
        BasisSelection::Invalid
    );
    assert_eq!(
        select_basis(BasisArguments {
            from_zoom: true,
            ..valid
        }),
        BasisSelection::Invalid
    );
    assert_eq!(
        select_basis(BasisArguments {
            desktop: true,
            ..valid
        }),
        BasisSelection::Invalid
    );
    assert_eq!(
        select_basis(BasisArguments::default()),
        BasisSelection::Legacy
    );
}
