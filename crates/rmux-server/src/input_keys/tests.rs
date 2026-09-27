use super::{
    decode_extended_key, decode_mouse, encode_key, encode_key_with_backspace, encode_mouse_event,
    ExtendedKeyDecode, ExtendedKeyFormat, MouseDecode, MouseForwardEvent,
    MAX_SGR_MOUSE_FRAME_BYTES,
};
use rmux_core::{
    input::mode, key_string_lookup_string, KeyCode, KEYC_CTRL, KEYC_IMPLIED_META, KEYC_KEYPAD,
    KEYC_META, KEYC_SHIFT,
};

const MODIFIED_CURSOR_CASES: [(&str, &[u8]); 8] = [
    ("C-Up", b"\x1b[1;5A"),
    ("C-Down", b"\x1b[1;5B"),
    ("C-Right", b"\x1b[1;5C"),
    ("C-Left", b"\x1b[1;5D"),
    ("S-Up", b"\x1b[1;2A"),
    ("M-Up", b"\x1b[1;3A"),
    ("C-Home", b"\x1b[1;5H"),
    ("C-End", b"\x1b[1;5F"),
];

fn parse_key(name: &str) -> KeyCode {
    key_string_lookup_string(name).expect("key name parses")
}

/// Encodes the named key with the xterm extended-key format.
fn encode_named(pane_mode: u32, name: &str) -> Option<Vec<u8>> {
    encode_key(pane_mode, ExtendedKeyFormat::Xterm, parse_key(name))
}

fn matched(size: usize, key: KeyCode) -> ExtendedKeyDecode {
    ExtendedKeyDecode::Matched { size, key }
}

/// Encodes `name` in extended mode 2, checks the bytes, then decodes them back to the key plus
/// `decoded_modifiers`.
fn assert_extended_round_trip(
    format: ExtendedKeyFormat,
    name: &str,
    expected: &[u8],
    backspace: Option<u8>,
    decoded_modifiers: KeyCode,
) {
    let key = parse_key(name);
    let encoded = encode_key(mode::MODE_KEYS_EXTENDED_2, format, key).expect("encode");
    assert_eq!(encoded, expected);
    assert_eq!(
        decode_extended_key(&encoded, backspace),
        matched(encoded.len(), key | decoded_modifiers)
    );
}

fn forward_event(
    b: u16,
    lb: u16,
    (x, y): (u16, u16),
    (lx, ly): (u16, u16),
    sgr_b: u16,
    sgr_type: char,
) -> MouseForwardEvent {
    MouseForwardEvent {
        b,
        lb,
        x,
        y,
        lx,
        ly,
        sgr_b,
        sgr_type,
        ignore: false,
    }
}

#[test]
fn extended_key_round_trips_xterm_ascii_ctrl_meta() {
    assert_extended_round_trip(
        ExtendedKeyFormat::Xterm,
        "M-C-a",
        b"\x1b[27;7;97~",
        Some(0x7f),
        KEYC_IMPLIED_META,
    );
}

#[test]
fn extended_key_round_trips_csi_u_unicode() {
    assert_extended_round_trip(
        ExtendedKeyFormat::CsiU,
        "C-\u{03c0}",
        "\x1b[960;5u".as_bytes(),
        None,
        0,
    );
}

#[test]
fn extended_key_round_trips_csi_u_shift_enter() {
    assert_extended_round_trip(ExtendedKeyFormat::CsiU, "S-Enter", b"\x1b[13;2u", None, 0);
}

#[test]
fn kitty_keyboard_mode_forces_csi_u_format() {
    let encoded = encode_named(
        mode::MODE_KEYS_EXTENDED_2 | mode::MODE_KEYS_KITTY,
        "S-Enter",
    )
    .expect("encode");
    assert_eq!(encoded, b"\x1b[13;2u");
}

#[test]
fn extended_key_shift_only_printables_strip_shift() {
    assert_eq!(
        decode_extended_key(b"\x1b[65;2u", None),
        matched(7, KeyCode::from(b'A'))
    );
}

#[test]
fn extended_key_shift_tab_becomes_backtab() {
    assert_eq!(
        decode_extended_key(b"\x1b[9;2u", None),
        matched(6, parse_key("BTab"))
    );
}

#[test]
fn standard_mode_meta_printable_falls_back_to_escape_prefix() {
    assert_eq!(encode_named(0, "M-a").expect("encode"), b"\x1ba");
}

#[test]
fn standard_mode_shift_enter_maps_to_line_feed() {
    assert_eq!(encode_named(0, "S-Enter").expect("encode"), b"\n");
}

#[test]
fn mode1_prefers_vt10x_for_compatible_ctrl_keys() {
    assert_eq!(
        encode_named(mode::MODE_KEYS_EXTENDED, "C-@").expect("encode"),
        [0x00]
    );
}

#[test]
fn standard_mode_control_space_encodes_nul() {
    assert_eq!(encode_named(0, "C-Space").expect("encode"), [0x00]);
    assert_eq!(encode_named(0, "M-C-Space").expect("encode"), [0x1b, 0x00]);
}

#[test]
fn standard_mode_control_question_encodes_delete() {
    assert_eq!(encode_named(0, "C-?").expect("encode"), [0x7f]);
}

#[test]
fn standard_mode_tmux_noop_control_digits_encode_empty() {
    for key in ["C-3", "C-4", "C-5", "C-7", "C-8"] {
        assert_eq!(
            encode_named(0, key).expect("encode"),
            Vec::<u8>::new(),
            "{key} should be a successful no-op"
        );
    }
}

#[test]
fn mouse_decode_supports_standard_and_sgr_sequences() {
    assert_eq!(
        decode_mouse(b"\x1b[M !!", None),
        MouseDecode::Matched {
            size: 6,
            event: forward_event(0, 0, (0, 0), (0, 0), 0, ' '),
        }
    );
    assert_eq!(
        decode_mouse(b"\x1b[<35;12;7M", None),
        MouseDecode::Matched {
            size: 11,
            event: forward_event(35, 0, (11, 6), (0, 0), 35, 'M'),
        }
    );
}

#[test]
fn mouse_decode_discards_putty_release_wheel_sequences() {
    assert_eq!(
        decode_mouse(b"\x1b[<64;10;4m", None),
        MouseDecode::Discard { size: 11 }
    );
}

#[test]
fn mouse_output_encodes_all_three_formats() {
    let event = MouseForwardEvent::button_event(0, 0, 0);
    assert_eq!(
        encode_mouse_event(mode::MODE_MOUSE_STANDARD, &event, 1, 2).expect("legacy"),
        b"\x1b[M \"#"
    );
    assert_eq!(
        encode_mouse_event(
            mode::MODE_MOUSE_STANDARD | mode::MODE_MOUSE_UTF8,
            &event,
            1,
            2
        )
        .expect("utf8"),
        b"\x1b[M \"#"
    );
    assert_eq!(
        encode_mouse_event(
            mode::MODE_MOUSE_STANDARD | mode::MODE_MOUSE_SGR,
            &MouseForwardEvent {
                sgr_b: 0,
                sgr_type: 'M',
                ..event
            },
            1,
            2
        )
        .expect("sgr"),
        b"\x1b[<0;2;3M"
    );
}

#[test]
fn mouse_output_respects_motion_and_release_filters() {
    let drag = forward_event(32, 0, (5, 6), (4, 5), 32, ' ');
    assert!(
        encode_mouse_event(mode::MODE_MOUSE_STANDARD, &drag, 5, 6).is_none(),
        "drag events need motion mode"
    );
    assert!(
        encode_mouse_event(mode::MODE_MOUSE_BUTTON, &drag, 5, 6).is_some(),
        "button mode accepts drag motion"
    );

    let release = forward_event(35, 35, (5, 6), (5, 6), 35, 'm');
    assert!(
        encode_mouse_event(
            mode::MODE_MOUSE_STANDARD | mode::MODE_MOUSE_SGR,
            &release,
            5,
            6
        )
        .is_none(),
        "SGR releases need all-motion mode"
    );
    assert!(
        encode_mouse_event(mode::MODE_MOUSE_ALL | mode::MODE_MOUSE_SGR, &release, 5, 6).is_some(),
        "all mode forwards SGR releases"
    );
}

#[test]
fn legacy_mouse_output_clamps_large_coordinates() {
    let event = MouseForwardEvent::button_event(0, 0, 0);
    let encoded = encode_mouse_event(mode::MODE_MOUSE_STANDARD, &event, 500, 700).expect("legacy");
    assert_eq!(encoded, [0x1b, b'[', b'M', b' ', 0xff, 0xff]);
}

#[test]
fn utf8_mouse_output_rejects_out_of_range_button_values() {
    let event = MouseForwardEvent::button_event(0x900, 0, 0);
    assert!(encode_mouse_event(
        mode::MODE_MOUSE_STANDARD | mode::MODE_MOUSE_UTF8,
        &event,
        0,
        0
    )
    .is_none());
}

#[test]
fn modifier_bits_are_preserved_by_extended_decode() {
    assert_eq!(
        decode_extended_key(b"\x1b[97;8u", None),
        matched(
            7,
            KeyCode::from(b'a') | KEYC_SHIFT | KEYC_CTRL | KEYC_META | KEYC_IMPLIED_META,
        )
    );
}

#[test]
fn extended_key_decode_rejects_invalid_prefix_for_xterm_format() {
    // xterm format requires prefix "27"
    assert_eq!(
        decode_extended_key(b"\x1b[28;2;65~", None),
        ExtendedKeyDecode::Invalid
    );
}

#[test]
fn extended_key_decode_rejects_extra_semicolons() {
    for sequence in [b"\x1b[27;2;65;99~".as_slice(), b"\x1b[65;2;3u".as_slice()] {
        assert_eq!(
            decode_extended_key(sequence, None),
            ExtendedKeyDecode::Invalid
        );
    }
}

#[test]
fn extended_key_decode_partial_returns_for_incomplete_input() {
    for sequence in [
        b"\x1b".as_slice(),
        b"\x1b[".as_slice(),
        b"\x1b[27;2;65".as_slice(),
    ] {
        assert_eq!(
            decode_extended_key(sequence, None),
            ExtendedKeyDecode::Partial
        );
    }
}

#[test]
fn extended_key_decode_rejects_non_esc_start() {
    assert_eq!(decode_extended_key(b"A", None), ExtendedKeyDecode::Invalid);
}

#[test]
fn extended_key_decode_modifiers_zero_means_no_modifiers() {
    // modifiers=0 is unusual but valid - no modifier bits set
    assert_eq!(
        decode_extended_key(b"\x1b[65;0u", None),
        matched(7, KeyCode::from(b'A'))
    );
}

#[test]
fn extended_key_decode_modifiers_one_means_no_modifiers() {
    // modifiers=1 means modifiers-1=0, so no modifier bits
    assert_eq!(
        decode_extended_key(b"\x1b[65;1u", None),
        matched(7, KeyCode::from(b'A'))
    );
}

#[test]
fn extended_key_backspace_option_maps_to_bspace() {
    // When backspace matches the terminal's VERASE, map to KEYC_BSPACE
    let decoded = decode_extended_key(b"\x1b[127;5u", Some(127));
    if let ExtendedKeyDecode::Matched { key, .. } = decoded {
        let base = key & rmux_core::KEYC_MASK_KEY;
        assert_eq!(base, rmux_core::KEYC_BSPACE);
    } else {
        panic!("expected matched");
    }
}

#[test]
fn extended_key_decode_supports_modified_cursor_sequences() {
    for (sequence, name) in [
        (b"\x1b[1;5A".as_slice(), "Up"),
        (b"\x1b[1;5B".as_slice(), "Down"),
        (b"\x1b[1;5C".as_slice(), "Right"),
        (b"\x1b[1;5D".as_slice(), "Left"),
    ] {
        assert_eq!(
            decode_extended_key(sequence, None),
            matched(sequence.len(), parse_key(name) | KEYC_CTRL)
        );
    }
}

#[test]
fn extended_key_encode_uses_xterm_modified_cursor_sequences() {
    for (name, expected) in MODIFIED_CURSOR_CASES {
        assert_eq!(
            encode_named(mode::MODE_KEYS_EXTENDED_2, name).as_deref(),
            Some(expected),
            "{name} should use xterm modified cursor encoding"
        );
    }
}

#[test]
fn standard_key_encode_uses_xterm_modified_cursor_sequences() {
    for (name, expected) in MODIFIED_CURSOR_CASES {
        assert_eq!(
            encode_named(0, name).as_deref(),
            Some(expected),
            "{name} should use xterm modified cursor encoding"
        );
    }
}

#[test]
fn meta_backspace_encodes_escape_del() {
    assert_eq!(
        encode_named(0, "M-BSpace").as_deref(),
        Some(b"\x1b\x7f".as_slice())
    );
}

#[test]
fn configured_backspace_byte_controls_plain_and_meta_backspace() {
    let backspace = parse_key("BSpace");
    assert_eq!(
        encode_key_with_backspace(0, ExtendedKeyFormat::Xterm, backspace, 0x08).as_deref(),
        Some(b"\x08".as_slice())
    );
    assert_eq!(
        encode_key_with_backspace(0, ExtendedKeyFormat::Xterm, backspace | KEYC_META, 0x08,)
            .as_deref(),
        Some(b"\x1b\x08".as_slice())
    );
}

#[test]
fn mouse_decode_rejects_non_esc_start() {
    assert_eq!(decode_mouse(b"X", None), MouseDecode::Invalid);
}

#[test]
fn mouse_decode_partial_on_short_legacy() {
    assert_eq!(decode_mouse(b"\x1b[M!!", None), MouseDecode::Partial);
}

#[test]
fn mouse_decode_sgr_partial_on_incomplete() {
    assert_eq!(decode_mouse(b"\x1b[<0;1;", None), MouseDecode::Partial);
}

#[test]
fn mouse_decode_sgr_rejects_invalid_or_missing_decimal_fields() {
    for sequence in [
        b"\x1b[<a".as_slice(),
        b"\x1b[<;1;1M".as_slice(),
        b"\x1b[<0M1;1M".as_slice(),
    ] {
        assert_eq!(
            decode_mouse(sequence, None),
            MouseDecode::Invalid,
            "{sequence:?} must not poison retained input"
        );
    }
}

#[test]
fn mouse_decode_sgr_rejects_zero_coordinates() {
    for sequence in [b"\x1b[<0;0;1M".as_slice(), b"\x1b[<0;1;0M".as_slice()] {
        assert_eq!(
            decode_mouse(sequence, None),
            MouseDecode::Discard { size: 9 }
        );
    }
}

#[test]
fn mouse_decode_legacy_discards_underflow() {
    // b < MOUSE_PARAM_BTN_OFF
    assert_eq!(
        decode_mouse(b"\x1b[M\x10!!", None),
        MouseDecode::Discard { size: 6 }
    );
}

#[test]
fn mouse_decode_preserves_last_event_positions() {
    let last = forward_event(0, 0, (10, 20), (5, 15), 0, ' ');
    let result = decode_mouse(b"\x1b[<0;5;8M", Some(last));
    if let MouseDecode::Matched { event, .. } = result {
        assert_eq!(event.lx, 10, "lx from previous event's x");
        assert_eq!(event.ly, 20, "ly from previous event's y");
        assert_eq!(event.lb, 0, "lb from previous event's b");
    } else {
        panic!("expected matched");
    }
}

#[test]
fn mouse_output_ignores_ignored_events() {
    let event = MouseForwardEvent {
        ignore: true,
        ..forward_event(0, 0, (0, 0), (0, 0), 0, ' ')
    };
    assert!(encode_mouse_event(mode::MODE_MOUSE_STANDARD, &event, 0, 0).is_none());
}

#[test]
fn mouse_output_requires_mouse_mode_enabled() {
    let event = MouseForwardEvent::button_event(0, 0, 0);
    assert!(
        encode_mouse_event(0, &event, 0, 0).is_none(),
        "no mouse mode means no output"
    );
}

#[test]
fn mouse_decode_retains_lexically_incomplete_overflow_in_every_decimal_field() {
    for sequence in [
        b"\x1b[<700000".as_slice(),
        b"\x1b[<700000;".as_slice(),
        b"\x1b[<700000;1".as_slice(),
        b"\x1b[<700000;1;".as_slice(),
        b"\x1b[<1;700000".as_slice(),
        b"\x1b[<1;700000;".as_slice(),
        b"\x1b[<1;700000;1".as_slice(),
        b"\x1b[<1;1;700000".as_slice(),
    ] {
        assert_eq!(
            decode_mouse(sequence, None),
            MouseDecode::Overlong,
            "an inevitable overflow stays on the bounded control path until the frame terminates"
        );
    }
}

#[test]
fn mouse_decode_discards_complete_overflow_in_every_decimal_field() {
    for sequence in [
        b"\x1b[<700000;1;1M".as_slice(),
        b"\x1b[<700000;1;1m".as_slice(),
        b"\x1b[<1;700000;1M".as_slice(),
        b"\x1b[<1;700000;1m".as_slice(),
        b"\x1b[<1;1;700000M".as_slice(),
        b"\x1b[<1;1;700000m".as_slice(),
    ] {
        assert_eq!(
            decode_mouse(sequence, None),
            MouseDecode::Discard {
                size: sequence.len()
            },
            "a complete impossible mouse frame is consumed without forwarding"
        );
    }

    let frame_with_tail = b"\x1b[<700000;1;1Mtail";
    assert_eq!(
        decode_mouse(frame_with_tail, None),
        MouseDecode::Discard {
            size: frame_with_tail.len() - b"tail".len()
        },
        "only the complete invalid frame is discarded so ordinary trailing input can resume"
    );
}

#[test]
fn mouse_decode_consumes_a_fixed_prefix_of_attacker_sized_decimal_input() {
    let mut fragmented = b"\x1b[<".to_vec();
    for total_len in fragmented.len() + 1..MAX_SGR_MOUSE_FRAME_BYTES {
        fragmented.push(b'9');
        assert_eq!(fragmented.len(), total_len);
        assert!(
            matches!(
                decode_mouse(&fragmented, None),
                MouseDecode::Partial | MouseDecode::Overlong
            ),
            "the bounded prefix remains retained before the syntax limit"
        );
    }

    fragmented.push(b'9');
    assert_eq!(fragmented.len(), MAX_SGR_MOUSE_FRAME_BYTES);
    assert_eq!(
        decode_mouse(&fragmented, None),
        MouseDecode::Discard {
            size: MAX_SGR_MOUSE_FRAME_BYTES
        },
        "reaching the bound consumes the dangerous control opener"
    );

    fragmented.resize(1024 * 1024, b'9');
    assert_eq!(
        decode_mouse(&fragmented, None),
        MouseDecode::Discard {
            size: fragmented.len()
        },
        "bounded decoding drops the malformed batch so its tail cannot become input"
    );
}

#[test]
fn sgr_mouse_release_falls_through_to_legacy_when_no_sgr_type() {
    // Release (b = 3) from a non-SGR source (sgr_type = ' ').
    let event = forward_event(3, 0, (0, 0), (0, 0), 0, ' ');
    // SGR mode is set but event is from non-SGR terminal
    let result = encode_mouse_event(
        mode::MODE_MOUSE_STANDARD | mode::MODE_MOUSE_SGR,
        &event,
        0,
        0,
    );
    // Should fall through to legacy format since we can't convert
    // a legacy release to SGR format (button identity unknown)
    assert!(result.is_some());
    assert_eq!(result.unwrap(), b"\x1b[M#!!");
}

#[test]
fn non_sgr_source_with_sgr_mode_falls_through_to_legacy() {
    // When the terminal sent a non-SGR event but the application wants SGR,
    // we must fall through to legacy format (tmux behavior: can't convert
    // legacy button encoding to SGR).
    let event = MouseForwardEvent::button_event(0, 0, 0);
    assert_eq!(event.sgr_type, ' ', "source is non-SGR");
    let result = encode_mouse_event(
        mode::MODE_MOUSE_STANDARD | mode::MODE_MOUSE_SGR,
        &event,
        5,
        3,
    );
    assert!(result.is_some());
    assert_eq!(result.unwrap(), b"\x1b[M &$");
}

#[test]
fn encode_key_backtab_without_extended_mode_produces_escape_sequence() {
    assert_eq!(encode_named(0, "BTab").expect("backtab"), b"\x1b[Z");
}

#[test]
fn encode_key_backtab_in_extended_mode_produces_shift_tab() {
    let encoded = encode_named(mode::MODE_KEYS_EXTENDED_2, "BTab").expect("encode");
    // Should encode as Shift-Tab: \x1b[27;2;9~
    assert_eq!(encoded, b"\x1b[27;2;9~");
}

#[test]
fn trivial_keys_bypass_extended_encoding() {
    // Plain 'a' should just produce 'a' even in extended mode
    let key = KeyCode::from(b'a');
    let encoded =
        encode_key(mode::MODE_KEYS_EXTENDED_2, ExtendedKeyFormat::Xterm, key).expect("trivial");
    assert_eq!(encoded, b"a");
}

#[test]
fn vt10x_arrow_keys_match_tmux_standard_and_cursor_modes() {
    assert_eq!(encode_named(0, "Up").expect("standard up"), b"\x1b[A");
    assert_eq!(
        encode_named(mode::MODE_KCURSOR, "Up").expect("cursor mode up"),
        b"\x1bOA"
    );
}

#[test]
fn extended_modes_do_not_drop_plain_navigation_keys() {
    for mode in [mode::MODE_KEYS_EXTENDED, mode::MODE_KEYS_EXTENDED_2] {
        for format in [ExtendedKeyFormat::Xterm, ExtendedKeyFormat::CsiU] {
            for (name, expected) in [
                ("Up", b"\x1b[A".as_slice()),
                ("Down", b"\x1b[B".as_slice()),
                ("Left", b"\x1b[D".as_slice()),
                ("Right", b"\x1b[C".as_slice()),
                ("Home", b"\x1b[1~".as_slice()),
                ("End", b"\x1b[4~".as_slice()),
                ("DC", b"\x1b[3~".as_slice()),
                ("PageUp", b"\x1b[5~".as_slice()),
                ("PageDown", b"\x1b[6~".as_slice()),
                ("F1", b"\x1bOP".as_slice()),
            ] {
                assert_eq!(
                    encode_key(mode, format, parse_key(name)).as_deref(),
                    Some(expected),
                    "{name} should fall back to its VT sequence in extended mode"
                );
            }
        }
    }
}

#[test]
fn vt10x_navigation_keys_match_tmux_standard_sequences() {
    for (name, expected, label) in [
        ("Home", b"\x1b[1~", "home"),
        ("DC", b"\x1b[3~", "delete"),
        ("PageUp", b"\x1b[5~", "page up"),
    ] {
        assert_eq!(encode_named(0, name).expect(label), expected);
    }
}

#[test]
fn golden_standard_key_trace_for_navigation_and_modifiers() {
    let keys = [
        "Up", "Down", "Left", "Right", "Home", "End", "DC", "PageUp", "PageDown", "BTab", "F1",
        "C-a", "M-a",
    ];
    let encoded = keys
        .into_iter()
        .flat_map(|key| encode_named(0, key).unwrap_or_else(|| panic!("{key} must encode")))
        .collect::<Vec<_>>();

    assert_eq!(
        encoded,
        b"\x1b[A\x1b[B\x1b[D\x1b[C\x1b[1~\x1b[4~\x1b[3~\x1b[5~\x1b[6~\x1b[Z\x1bOP\x01\x1ba"
    );
}

#[test]
fn vt10x_keypad_keys_follow_application_mode() {
    let kp1 = parse_key("KP1");
    assert_eq!(kp1 & KEYC_KEYPAD, KEYC_KEYPAD);
    assert_eq!(
        encode_key(0, ExtendedKeyFormat::Xterm, kp1).expect("numeric keypad"),
        b"1"
    );
    assert_eq!(
        encode_key(mode::MODE_KKEYPAD, ExtendedKeyFormat::Xterm, kp1).expect("application keypad"),
        b"\x1bOq"
    );
}
