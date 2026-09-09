//! Parse symbolic names without assuming physical positions. The keyboard resolver
//! maps them through the running compositor before any named input is sent.
use anyhow::{Result, bail, ensure};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Key {
    Keysym(i32),
    Keycode(i32),
}

pub(crate) fn chord(keys: &[String]) -> Result<Vec<Key>> {
    ensure!(
        !keys.is_empty() && keys.len() <= 16,
        "A chord needs 1–16 keys"
    );
    let keys = keys
        .iter()
        .map(|name| keysym(name).map(Key::Keysym))
        .collect::<Result<Vec<_>>>()?;
    unique(&keys)?;
    Ok(keys)
}

pub(crate) fn physical(codes: &[u16]) -> Result<Vec<Key>> {
    ensure!(
        !codes.is_empty() && codes.len() <= 16 && codes.iter().all(|c| *c <= 0x2ff),
        "Use 1–16 Linux evdev codes (0–767)"
    );
    let keys = codes
        .iter()
        .map(|c| Key::Keycode(*c as i32))
        .collect::<Vec<_>>();
    unique(&keys)?;
    Ok(keys)
}

pub(crate) fn unique(keys: &[Key]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    ensure!(
        keys.iter().all(|k| seen.insert(*k)),
        "A chord cannot contain the same key twice"
    );
    Ok(())
}

pub(crate) fn choose(named: &[String], codes: &[u16]) -> Result<Vec<Key>> {
    ensure!(
        named.is_empty() != codes.is_empty(),
        "Supply either keys or keycodes, not both"
    );
    if codes.is_empty() {
        chord(named)
    } else {
        physical(codes)
    }
}

pub(crate) fn paste(named: Option<&[String]>, codes: Option<&[u16]>) -> Result<Vec<Key>> {
    ensure!(
        named.is_none() || codes.is_none(),
        "paste_keys and paste_keycodes are mutually exclusive"
    );
    match (named, codes) {
        (Some(keys), _) => chord(keys),
        (_, Some(codes)) => physical(codes),
        _ => chord(&["CTRL".into(), "V".into()]),
    }
}

pub fn keysym(name: &str) -> Result<i32> {
    // A single character is a literal keysym. Unicode text belongs in clipboard paste;
    // most keyboard layouts cannot synthesize arbitrary Unicode keysyms.
    if name.len() == 1 && name.as_bytes()[0].is_ascii_graphic() {
        return Ok(name.as_bytes()[0].to_ascii_lowercase() as i32);
    }
    let upper = name.to_ascii_uppercase();
    let code = match upper.as_str() {
        "CTRL" | "CONTROL" => 0xffe3,
        "SHIFT" => 0xffe1,
        "ALT" => 0xffe9,
        "SUPER" | "META" | "WIN" => 0xffeb,
        "ENTER" | "RETURN" => 0xff0d,
        "TAB" => 0xff09,
        "ESC" | "ESCAPE" => 0xff1b,
        "SPACE" | " " => 0x20,
        "BACKSPACE" => 0xff08,
        "DELETE" | "DEL" => 0xffff,
        "INSERT" => 0xff63,
        "HOME" => 0xff50,
        "END" => 0xff57,
        "LEFT" => 0xff51,
        "UP" => 0xff52,
        "RIGHT" => 0xff53,
        "DOWN" => 0xff54,
        "PAGEUP" | "PAGE_UP" => 0xff55,
        "PAGEDOWN" | "PAGE_DOWN" => 0xff56,
        "CAPSLOCK" => 0xffe5,
        "PRINT" | "PRINTSCREEN" => 0xff61,
        _ => {
            if let Some(n) = upper.strip_prefix('F').and_then(|s| s.parse::<u32>().ok())
                && (1..=24).contains(&n)
            {
                return Ok((0xffbe + n - 1) as i32);
            }
            bail!(
                "Unknown key {name:?}; use a named key (CTRL, ENTER, arrows, F1–F24) or an ASCII character"
            )
        }
    };
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn common_chords() {
        assert_eq!(
            chord(&["CTRL".into(), "L".into()]).unwrap(),
            vec![Key::Keysym(0xffe3), Key::Keysym(108)]
        );
    }
    #[test]
    fn validates_entire_chord_before_input() {
        assert!(chord(&["CTRL".into(), "bogus".into()]).is_err());
        assert!(chord(&["CTRL".into(), "CONTROL".into()]).is_err());
        assert!(chord(&[]).is_err());
    }
    #[test]
    fn physical_options_validate_before_side_effects() {
        assert!(choose(&[], &[]).is_err());
        assert!(choose(&["CTRL".into()], &[29]).is_err());
        assert!(physical(&[29, 29]).is_err());
        assert!(physical(&[768]).is_err());
        assert!(paste(Some(&[]), None).is_err());
        assert!(paste(None, Some(&[])).is_err());
        assert!(paste(Some(&["CTRL".into()]), Some(&[29])).is_err());
        assert_eq!(
            paste(None, Some(&[29, 47])).unwrap(),
            vec![Key::Keycode(29), Key::Keycode(47)]
        );
        let mut mixed = chord(&["CTRL".into()]).unwrap();
        mixed.extend(physical(&[29]).unwrap());
        // Aliasing against physical codes is checked using the actual map, not US assumptions.
        assert!(unique(&mixed).is_ok());
        for name in ["CONTROL", "ctrl"] {
            assert_eq!(chord(&[name.into()]).unwrap(), vec![Key::Keysym(0xffe3)]);
        }
        for name in ["SUPER", "META", "WIN"] {
            assert_eq!(chord(&[name.into()]).unwrap(), vec![Key::Keysym(0xffeb)]);
        }
        assert_eq!(
            chord(&["!".into(), "LEFT".into()]).unwrap(),
            vec![Key::Keysym(33), Key::Keysym(0xff51)]
        );
    }
    #[test]
    fn functions() {
        assert_eq!(keysym("F24").unwrap(), 0xffd5);
        assert!(keysym("F25").is_err());
    }
}
