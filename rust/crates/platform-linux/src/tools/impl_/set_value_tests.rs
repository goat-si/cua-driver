use super::*;

#[test]
fn a_file_chooser_keeping_the_basename_counts_as_committed() {
    assert!(path_normalised_to_basename(
        "/home/user/Desktop/tone.wav",
        "tone.wav"
    ));
    assert!(!path_normalised_to_basename(
        "/home/user/Desktop/tone.wav",
        "other.wav"
    ));
    assert!(!path_normalised_to_basename("tone.wav", "tone.wav"));
    let ok = set_value_result(
        3,
        "/home/user/Desktop/tone.wav",
        "ax",
        Some("tone.wav".into()),
        None,
        false,
    );
    let s = ok.structured_content.unwrap();
    assert_eq!(s["verified"], true);
    assert_eq!(s["effect"], "confirmed");
}

#[test]
fn readback_agreement_is_numeric_aware() {
    assert!(values_agree("40", "40.0"));
    assert!(values_agree("40", " 40 "));
    assert!(values_agree("abc", "abc"));
    assert!(!values_agree("40", "41"));
    assert!(!values_agree("40", "forty"));
}

#[test]
fn set_value_result_is_verified_only_on_matching_readback() {
    let ok = set_value_result(3, "40", "click_type_mpx", Some("40.0".into()), None, false);
    let s = ok.structured_content.unwrap();
    assert_eq!(s["verified"], true);
    assert_eq!(s["effect"], "confirmed");
    let noop = set_value_result(3, "40", "ax", Some("10.0".into()), None, false);
    let s = noop.structured_content.unwrap();
    assert_eq!(s["verified"], false);
    assert_eq!(s["effect"], "suspected_noop");
    assert_eq!(s["readback"], "10.0");
    let blind = set_value_result(3, "40", "ax", None, None, false);
    assert_eq!(blind.structured_content.unwrap()["effect"], "unverifiable");
}

#[test]
fn a_password_field_write_does_not_repeat_its_value() {
    let result = set_value_result(1, "set-secret-2", "ax", None, None, true);
    let text = format!("{:?}", result.content);
    assert!(!text.contains("set-secret-2"), "{text}");
    assert!(text.contains("[REDACTED]"), "{text}");
    let plain = set_value_result(1, "draft", "ax", None, None, false);
    assert!(format!("{:?}", plain.content).contains("draft"));
}
