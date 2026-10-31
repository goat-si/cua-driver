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

#[test]
fn click_argument_wiring_selects_basis_and_refuses_conflicts() {
    use serde_json::json;
    for tool in ["click", "double_click", "right_click"] {
        assert_eq!(
            super::basis_decision::select_click_basis(
                tool,
                &json!({"snapshot_id":"s00000001.f1","x":1,"y":2})
            ),
            super::basis_decision::BasisSelection::Basis
        );
        assert_eq!(
            super::basis_decision::select_click_basis(
                tool,
                &json!({"snapshot_id":"s00000001.f1","x":1})
            ),
            super::basis_decision::BasisSelection::Invalid
        );
        assert_eq!(
            super::basis_decision::select_click_basis(
                tool,
                &json!({"snapshot_id":"s00000001.f1","x":1,"y":2,"scope":"desktop"})
            ),
            super::basis_decision::BasisSelection::Invalid
        );
        for conflict in [
            json!({"element_token":"s00000001:1"}),
            json!({"capture_id":"c"}),
            json!({"from_zoom":true}),
        ] {
            let mut args = json!({"snapshot_id":"s00000001.f1","x":1,"y":2});
            args.as_object_mut()
                .unwrap()
                .extend(conflict.as_object().unwrap().clone());
            assert_eq!(
                super::basis_decision::select_click_basis(tool, &args),
                BasisSelection::Invalid
            );
        }
        assert_eq!(
            super::basis_decision::select_click_basis(tool, &json!({"x":1,"y":2})),
            BasisSelection::Legacy
        );
    }
}
