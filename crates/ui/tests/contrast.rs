//! Contraste WCAG dos pares texto/fundo usados nas telas (SPEC-05 §8).
//! Lê os tokens do `global Theme` do app.slint, então trocar um hex sem
//! conferir o contraste quebra este teste.

use std::collections::HashMap;

/// Texto normal: AA pede 4,5:1.
const AA: f64 = 4.5;

/// (texto, fundo) que aparecem nas telas. `text-mute` fica de fora: só
/// desabilitado, isento pelo WCAG.
const PAIRS: &[(&str, &str)] = &[
    ("text", "ink"),
    ("text", "surface"),
    ("text", "surface-raised"),
    ("text", "surface-hover"),
    ("text", "danger-dim"),
    ("text-dim", "ink"),
    ("text-dim", "surface"),
    ("text-dim", "surface-raised"),
    ("text-dim", "surface-hover"),
    ("text-placeholder", "surface-raised"),
    ("on-violet", "violet"),
    ("on-violet", "violet-bright"),
    ("on-violet", "violet-deep"),
    ("violet-bright", "surface"),
    ("violet-bright", "surface-raised"),
    ("violet-bright", "surface-hover"),
    ("success", "surface"),
    ("success", "surface-raised"),
    ("warning", "surface"),
    ("warning", "surface-raised"),
    ("warning", "warning-dim"),
    ("danger", "surface"),
    ("danger", "surface-raised"),
    ("danger", "surface-hover"),
    ("danger", "ink"),
    ("text", "violet-dim"),
];

fn theme() -> HashMap<String, [u8; 3]> {
    let src = include_str!("../ui/app.slint");
    let start = src.find("global Theme {").expect("global Theme no app.slint");
    let body = &src[start..start + src[start..].find('}').unwrap()];
    body.lines()
        .filter_map(|l| {
            let l = l.trim().strip_prefix("out property <color> ")?;
            let (name, hex) = l.split_once(": #")?;
            let hex = hex.trim_end_matches(';');
            let c = |i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap();
            Some((name.to_string(), [c(0), c(2), c(4)]))
        })
        .collect()
}

fn luminance([r, g, b]: [u8; 3]) -> f64 {
    let ch = |v: u8| {
        let v = f64::from(v) / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b)
}

fn ratio(a: [u8; 3], b: [u8; 3]) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

#[test]
fn pares_passam_aa() {
    let t = theme();
    let mut fails = Vec::new();
    for (fg, bg) in PAIRS {
        let r = ratio(t[*fg], t[*bg]);
        println!("{fg:>16} sobre {bg:<16} {r:5.2}:1");
        if r < AA {
            fails.push(format!("{fg} sobre {bg}: {r:.2}:1"));
        }
    }
    assert!(fails.is_empty(), "abaixo de AA: {fails:?}");
}

#[test]
fn hex_do_lerrinfc() {
    // P-01: estes vêm do Color.kt do LerriNFC.
    let t = theme();
    assert_eq!(t["ink"], [0x0B, 0x07, 0x10]);
    assert_eq!(t["violet"], [0xB3, 0x6B, 0xFF]);
    assert_eq!(t["violet-bright"], [0xCB, 0x9B, 0xFF]);
    assert_eq!(t["on-violet"], [0x1A, 0x0A, 0x2E]);
}
