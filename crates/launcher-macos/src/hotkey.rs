use anyhow::{Context, Result, bail};
use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::{Code, HotKey, Modifiers},
};

/// Retains the Carbon registration for the life of the launcher.
pub struct GlobalHotkey {
    manager: GlobalHotKeyManager,
    hotkey: HotKey,
}

impl GlobalHotkey {
    pub fn new(shortcut: &str) -> Result<Self> {
        let hotkey = parse_shortcut(shortcut)?;
        let manager = GlobalHotKeyManager::new().context("creating global hotkey manager")?;
        manager.register(hotkey).map_err(|err| {
            anyhow::anyhow!("could not register {shortcut:?}; another app may own it: {err}")
        })?;
        Ok(Self { manager, hotkey })
    }

    /// Returns the process-wide event ID assigned to this registration.
    pub fn id(&self) -> u32 {
        self.hotkey.id()
    }

    /// True when `event` is a press of this exact registration, not another hotkey.
    pub fn accepts_event(&self, event: &GlobalHotKeyEvent) -> bool {
        is_activation_event(event, self.id())
    }

    /// Drain pending global-hotkey events without blocking; true only for this hotkey's press.
    pub fn try_toggle_requested(&self) -> bool {
        let mut requested = false;
        loop {
            match GlobalHotKeyEvent::receiver().try_recv() {
                Ok(event)
                    if event.id == self.hotkey.id() && event.state == HotKeyState::Pressed =>
                {
                    requested = true
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        requested
    }
}

impl Drop for GlobalHotkey {
    fn drop(&mut self) {
        let _ = self.manager.unregister(self.hotkey);
    }
}

/// True only for a press of the hotkey identified by `hotkey_id`.
///
/// The global-hotkey event channel is process-wide, so every listener must
/// match on the ID of its own registration; otherwise unrelated hotkeys in
/// the same process would trigger this launcher.
pub fn is_activation_event(event: &GlobalHotKeyEvent, hotkey_id: u32) -> bool {
    event.id == hotkey_id && event.state == HotKeyState::Pressed
}

/// Validate a shortcut string the same way [`GlobalHotkey::new`] would, without
/// touching the OS. Used by configuration validation before registration.
pub fn validate_hotkey(shortcut: &str) -> Result<()> {
    parse_shortcut(shortcut).map(|_| ())
}

fn parse_shortcut(shortcut: &str) -> Result<HotKey> {
    let normalized = shortcut.to_ascii_lowercase().replace([' ', '_'], "-");
    let parts: Vec<_> = normalized
        .split('-')
        .filter(|part| !part.is_empty())
        .collect();
    let (key, mods) = match parts.as_slice() {
        ["alt", "space"] | ["option", "space"] | ["alt-space"] => (Code::Space, Modifiers::ALT),
        ["alt", key] | ["option", key] => (parse_code(key)?, Modifiers::ALT),
        ["ctrl", key] | ["control", key] => (parse_code(key)?, Modifiers::CONTROL),
        ["shift", key] => (parse_code(key)?, Modifiers::SHIFT),
        ["super", key] | ["cmd", key] | ["command", key] => (parse_code(key)?, Modifiers::SUPER),
        _ => bail!(
            "unsupported shortcut {shortcut:?}; use alt-space, alt-<key>, ctrl-<key>, shift-<key>, or cmd-<key>"
        ),
    };
    Ok(HotKey::new(Some(mods), key))
}

fn parse_code(key: &str) -> Result<Code> {
    match key {
        "space" => Ok(Code::Space),
        "a" => Ok(Code::KeyA),
        "b" => Ok(Code::KeyB),
        "c" => Ok(Code::KeyC),
        "d" => Ok(Code::KeyD),
        "e" => Ok(Code::KeyE),
        "f" => Ok(Code::KeyF),
        "g" => Ok(Code::KeyG),
        "h" => Ok(Code::KeyH),
        "i" => Ok(Code::KeyI),
        "j" => Ok(Code::KeyJ),
        "k" => Ok(Code::KeyK),
        "l" => Ok(Code::KeyL),
        "m" => Ok(Code::KeyM),
        "n" => Ok(Code::KeyN),
        "o" => Ok(Code::KeyO),
        "p" => Ok(Code::KeyP),
        "q" => Ok(Code::KeyQ),
        "r" => Ok(Code::KeyR),
        "s" => Ok(Code::KeyS),
        "t" => Ok(Code::KeyT),
        "u" => Ok(Code::KeyU),
        "v" => Ok(Code::KeyV),
        "w" => Ok(Code::KeyW),
        "x" => Ok(Code::KeyX),
        "y" => Ok(Code::KeyY),
        "z" => Ok(Code::KeyZ),
        "enter" | "return" => Ok(Code::Enter),
        "escape" | "esc" => Ok(Code::Escape),
        "tab" => Ok(Code::Tab),
        _ => bail!("unsupported key {key:?}; use a letter, space, enter, escape, or tab"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_default_and_modifiers() {
        assert_eq!(
            parse_shortcut("alt-space").unwrap().id(),
            parse_shortcut("option space").unwrap().id()
        );
        assert!(parse_shortcut("ctrl-k").is_ok());
        assert!(parse_shortcut("nonsense").is_err());
    }

    #[test]
    fn only_matching_press_events_activate() {
        assert!(is_activation_event(
            &GlobalHotKeyEvent {
                id: 42,
                state: HotKeyState::Pressed,
            },
            42
        ));
        assert!(!is_activation_event(
            &GlobalHotKeyEvent {
                id: 42,
                state: HotKeyState::Released,
            },
            42
        ));
        assert!(!is_activation_event(
            &GlobalHotKeyEvent {
                id: 7,
                state: HotKeyState::Pressed,
            },
            42
        ));
        assert!(!is_activation_event(
            &GlobalHotKeyEvent {
                id: 7,
                state: HotKeyState::Released,
            },
            42
        ));
    }

    #[test]
    fn validate_hotkey_accepts_supported_rejects_invalid() {
        assert!(validate_hotkey("alt-space").is_ok());
        assert!(validate_hotkey("cmd-enter").is_ok());
        assert!(validate_hotkey("control-k").is_ok());
        assert!(validate_hotkey("").is_err());
        assert!(validate_hotkey("bogus").is_err());
    }
}
