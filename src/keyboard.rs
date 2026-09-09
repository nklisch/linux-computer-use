//! Resolve named chords against the running compositor, without a surface or focus.
//!
//! A fresh, short-lived Wayland connection reads the exact keymap for each gesture;
//! no configuration-file reconstruction, US-position table, or stale map cache.
//! Physical events avoid older KWin keysym synthesis replacing held modifiers.
use std::{
    fs::File,
    os::{fd::AsFd, unix::fs::FileExt},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use tokio::io::unix::AsyncFd;
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum,
    protocol::{wl_keyboard, wl_registry, wl_seat},
};
use xkbcommon::xkb;

use crate::keys::{self, Key};

pub(crate) async fn resolve(connection: &zbus::Connection, keys: &[Key]) -> Result<Vec<Key>> {
    if keys.iter().all(|key| matches!(key, Key::Keycode(_))) {
        keys::unique(keys)?;
        return Ok(keys.to_vec());
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        let map = read_keymap().await?;
        // Layout names do not encode variants/options. Only use KWin's current
        // group index; the actual symbol definitions come from wl_keyboard.
        let group = async {
            let reply = connection
                .call_method(
                    Some("org.kde.keyboard"),
                    "/Layouts",
                    Some("org.kde.KeyboardLayouts"),
                    "getLayout",
                    &(),
                )
                .await?;
            reply.body().deserialize::<u32>()
        }
        .await
        .ok();
        resolve_map(&map, group, keys)
    })
    .await
    .context("Keyboard-map lookup exceeded one second")?
    .context("Cannot resolve named chord faithfully; use explicit keycodes or paste_keycodes")
}

#[derive(Default)]
struct MapReader {
    seat_bound: bool,
    keyboard_bound: bool,
    map: Option<Result<String>>,
}
impl Dispatch<wl_registry::WlRegistry, ()> for MapReader {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
            && interface == "wl_seat"
            && !state.seat_bound
        {
            registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(7), qh, ());
            state.seat_bound = true;
        }
    }
}
impl Dispatch<wl_seat::WlSeat, ()> for MapReader {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
            && caps.contains(wl_seat::Capability::Keyboard)
            && !state.keyboard_bound
        {
            seat.get_keyboard(qh, ());
            state.keyboard_bound = true;
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for MapReader {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event {
            state.map = Some((|| {
                ensure!(
                    format == WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                    "Compositor did not supply an XKB keymap"
                );
                read_map_file(File::from(fd), size)
            })());
        }
    }
}
fn read_map_file(file: File, size: u32) -> Result<String> {
    // Wayland may send duplicates of one memfd. Positional reads do not consume
    // the shared file cursor and remain correct on every subsequent connection.
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size as usize)?;
    bytes.resize(size as usize, 0);
    file.read_exact_at(&mut bytes, 0)?;
    Ok(String::from_utf8(bytes)?.trim_end_matches('\0').to_owned())
}

async fn read_keymap() -> Result<String> {
    let connection =
        Connection::connect_to_env().context("Read-only Wayland connection unavailable")?;
    let mut queue = connection.new_event_queue();
    connection.display().get_registry(&queue.handle(), ());
    let io = AsyncFd::new(connection.as_fd())?;
    let mut state = MapReader::default();
    loop {
        queue.dispatch_pending(&mut state)?;
        if let Some(map) = state.map.take() {
            return map;
        }
        queue.flush()?;
        if let Some(guard) = queue.prepare_read() {
            let mut ready = io.readable().await?;
            let result = guard.read();
            ready.clear_ready();
            match result {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
}

fn resolve_map(text: &str, group: Option<u32>, keys: &[Key]) -> Result<Vec<Key>> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let map = xkb::Keymap::new_from_string(
        &context,
        text.to_owned(),
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::COMPILE_NO_FLAGS,
    )
    .context("Compositor keymap could not be parsed")?;
    let group = match group {
        Some(group) => group,
        None if map.num_layouts() == 1 => 0,
        None => anyhow::bail!("Active keyboard group unavailable for a multi-layout keymap"),
    };
    ensure!(
        group < map.num_layouts(),
        "Active keyboard group changed; observe and retry preparation"
    );
    resolve_in_map(&map, group, keys)
}

fn key_group(map: &xkb::Keymap, code: xkb::Keycode, group: u32) -> u32 {
    group % map.num_layouts_for_key(code).max(1)
}

fn resolve_in_map(map: &xkb::Keymap, group: u32, keys: &[Key]) -> Result<Vec<Key>> {
    // Only momentary level modifiers may be inferred. Never toggle Caps Lock or
    // guess global latched/locked state (surface-less Wayland does not expose it).
    let mut levels = Vec::<(u32, u32)>::new();
    for raw in map.min_keycode().raw().max(8)..=map.max_keycode().raw().min(775) {
        let code = xkb::Keycode::new(raw);
        let syms = map.key_get_syms_by_level(code, key_group(map, code, group), 0);
        if syms
            .iter()
            .any(|s| matches!(s.raw(), 0xffe1 | 0xffe2 | 0xfe03 | 0xfe11 | 0xff7e))
        {
            let mut state = xkb::State::new(map);
            state.update_mask(0, 0, 0, 0, 0, group);
            state.update_key(code, xkb::KeyDirection::Down);
            let mask = state.serialize_mods(xkb::STATE_MODS_DEPRESSED);
            if mask != 0 {
                // Prefer a mapped physical modifier such as RALT over XKB's
                // synthetic LVL3/MDSW key when both produce the same mask.
                let synthetic = |raw| {
                    map.key_get_name(xkb::Keycode::new(raw))
                        .is_some_and(|name| matches!(name, "LVL3" | "LVL5" | "MDSW"))
                };
                if let Some((old, _)) = levels.iter_mut().find(|(_, m)| *m == mask) {
                    if synthetic(*old + 8) && !synthetic(raw) {
                        *old = raw - 8;
                    }
                } else {
                    levels.push((raw - 8, mask));
                }
            }
        }
    }
    let mut explicit = Vec::new();
    let mut inferred = Vec::new();
    for key in keys {
        let (code, extra) = match key {
            Key::Keycode(code) => (*code as u32, vec![]),
            Key::Keysym(sym) => {
                let mut best: Option<(u32, Vec<u32>)> = None;
                for raw in map.min_keycode().raw().max(8)..=map.max_keycode().raw().min(775) {
                    let code = xkb::Keycode::new(raw);
                    let layout = key_group(map, code, group);
                    for level in 0..map.num_levels_for_key(code, layout) {
                        if !map
                            .key_get_syms_by_level(code, layout, level)
                            .iter()
                            .any(|s| s.raw() == *sym as u32)
                        {
                            continue;
                        }
                        let mut masks = [0; 64];
                        let count = map.key_get_mods_for_level(code, layout, level, &mut masks);
                        for mask in &masks[..count] {
                            let mut remaining = *mask;
                            let mut modifiers = Vec::new();
                            for (modifier, bits) in &levels {
                                if remaining & bits == *bits {
                                    modifiers.push(*modifier);
                                    remaining &= !bits;
                                }
                            }
                            if remaining == 0
                                && best.as_ref().is_none_or(|(old_code, old_mods)| {
                                    (modifiers.len(), raw - 8) < (old_mods.len(), *old_code)
                                })
                            {
                                best = Some((raw - 8, modifiers));
                            }
                        }
                    }
                }
                best.with_context(|| {
                    format!("Symbol {sym:#x} has no supported physical chord in the active layout")
                })?
            }
        };
        ensure!(
            !explicit.contains(&code),
            "Named and physical inputs resolve to the same key"
        );
        explicit.push(code);
        for modifier in extra {
            if !inferred.contains(&modifier) {
                inferred.push(modifier);
            }
        }
    }
    inferred.retain(|code| !explicit.contains(code));
    inferred.extend(explicit);
    Ok(inferred
        .into_iter()
        .map(|code| Key::Keycode(code as i32))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn map(layout: &str, variant: &str, options: Option<String>) -> xkb::Keymap {
        xkb::Keymap::new_from_names(
            &xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            "evdev",
            "pc105",
            layout,
            variant,
            options,
            xkb::COMPILE_NO_FLAGS,
        )
        .unwrap()
    }
    fn named(map: &xkb::Keymap, group: u32, names: &[&str]) -> Vec<Key> {
        resolve_in_map(
            map,
            group,
            &keys::chord(&names.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap(),
        )
        .unwrap()
    }
    #[test]
    fn keymap_descriptor_reads_do_not_consume_shared_cursor() {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"map\0").unwrap();
        file.seek(SeekFrom::Start(2)).unwrap();
        for _ in 0..2 {
            assert_eq!(read_map_file(file.try_clone().unwrap(), 4).unwrap(), "map");
        }
        assert_eq!(file.stream_position().unwrap(), 2);
    }

    #[test]
    fn actual_layout_changes_physical_positions_without_symbol_injection() {
        assert_eq!(
            named(&map("us", "", None), 0, &["CTRL", "L"]),
            vec![Key::Keycode(29), Key::Keycode(38)]
        );
        assert_eq!(
            named(&map("de", "", None), 0, &["CTRL", "Y"]),
            vec![Key::Keycode(29), Key::Keycode(44)]
        );
        assert_eq!(
            named(&map("fr", "", None), 0, &["CTRL", "A"]),
            vec![Key::Keycode(29), Key::Keycode(16)]
        );
        assert_eq!(
            named(&map("us", "dvorak", None), 0, &["CTRL", "L"]),
            vec![Key::Keycode(29), Key::Keycode(25)]
        );
    }
    #[test]
    fn shifted_tab_preserves_the_native_backtab_symbol() {
        let map = map("us", "", None);
        assert_eq!(
            named(&map, 0, &["SHIFT", "TAB"]),
            vec![Key::Keycode(42), Key::Keycode(15)]
        );
        let mut state = xkb::State::new(&map);
        state.update_key(xkb::Keycode::new(42 + 8), xkb::KeyDirection::Down);
        assert_eq!(
            xkb::keysym_get_name(state.key_get_one_sym(xkb::Keycode::new(15 + 8))),
            "ISO_Left_Tab"
        );
    }
    #[test]
    fn level_modifiers_and_remapped_control_are_resolved() {
        let us = map("us", "", None);
        assert_eq!(
            named(&us, 0, &["!"]),
            vec![Key::Keycode(42), Key::Keycode(2)]
        );
        assert_eq!(
            named(&us, 0, &["CTRL", "SHIFT", "V"]),
            vec![Key::Keycode(29), Key::Keycode(42), Key::Keycode(47)]
        );
        assert_eq!(
            named(
                &map("us", "", Some("ctrl:swapcaps".into())),
                0,
                &["CTRL", "L"]
            ),
            vec![Key::Keycode(58), Key::Keycode(38)]
        );
        assert_eq!(
            named(&map("de", "", None), 0, &["@"]),
            vec![Key::Keycode(100), Key::Keycode(16)]
        );
    }
    #[test]
    fn group_changes_and_duplicate_resolution_are_checked() {
        let layouts = map("us,de", "", None);
        assert_eq!(named(&layouts, 0, &["Y"]), vec![Key::Keycode(21)]);
        assert_eq!(named(&layouts, 1, &["Y"]), vec![Key::Keycode(44)]);
        assert_eq!(
            named(&layouts, 1, &["@"]),
            vec![Key::Keycode(100), Key::Keycode(16)]
        );
        let text = layouts.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        assert!(resolve_map(&text, None, &[Key::Keysym(121)]).is_err());
        assert!(resolve_map(&text, Some(9), &[Key::Keysym(121)]).is_err());
        assert!(resolve_map("not a keymap", Some(0), &[Key::Keysym(121)]).is_err());
        assert!(resolve_in_map(&layouts, 0, &[Key::Keysym(0xffe3), Key::Keycode(29)]).is_err());
    }
}
