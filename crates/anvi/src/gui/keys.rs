//! winit のキーイベント → nvim の中間表現（DESIGN v2 §4.5）。
//!
//! 変換表を GUI 側に置いているのは、core を winit に縛らないため
//! （[`anvi_core::ui::input`] の doc も参照）。ここは表を引くだけで、
//! 記法の組み立ては core がやる。
//!
//! **表に無いキーは捨てる。** 「とりあえず何か送る」と、押した覚えのない文字が
//! バッファに入る方が害が大きい。捨てた事実はログに残す。
//!
//! IME が食っているキーは Windows が `VK_PROCESSKEY` として送ってくるので、winit は
//! `NamedKey::Process` を配る。これも表に無いので自然に落ちる（変換中のキー抑止は
//! [`crate::gui::ime::ImeState::composing`] と二重に効かせている）。

use anvi_core::ui::input::{Key, Mods, NamedKey};
use winit::event::KeyEvent;
use winit::keyboard::{
    Key as WinitKey, KeyCode, ModifiersState, NamedKey as WinitNamed, PhysicalKey,
};

/// 修飾キーの現在値。Windows キー（`super`）は握らない。
#[must_use]
pub fn mods(state: ModifiersState) -> Mods {
    Mods {
        ctrl: state.control_key(),
        alt: state.alt_key(),
        shift: state.shift_key(),
    }
}

/// winit のキーイベント → nvim へ送れるキー。送れないものは `None`。
///
/// `logical_key` はキーボード配列に依存するため、Windows の Ctrl+Shift+V は
/// `physical_key` も見て特別扱いする。
#[must_use]
pub fn convert(event: &KeyEvent, mods: Mods) -> Option<Key> {
    if event.physical_key == PhysicalKey::Code(KeyCode::KeyV)
        && mods.ctrl
        && mods.shift
        && !mods.alt
    {
        return Some(Key::Char('V'));
    }

    match &event.logical_key {
        WinitKey::Character(text) => {
            let mut chars = text.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                // 合成に失敗した死にキーは「死にキーの文字 + 打鍵の文字」の 2 文字で
                // 届く（winit の `KeyEvent::text` の doc）。打鍵は 1 つなので先頭を採る。
                tracing::debug!(text = text.as_str(), "multi-char key event truncated");
            }
            Some(Key::Char(first))
        }
        WinitKey::Named(named) => named_key(*named).map(Key::Named),
        // 死にキーの途中と、IME が食ったキー・未知の仮想キー。
        WinitKey::Dead(_) | WinitKey::Unidentified(_) => None,
    }
}

/// winit の名前つきキー → nvim の名前つきキー。対応が無ければ `None`。
fn named_key(named: WinitNamed) -> Option<NamedKey> {
    let key = match named {
        WinitNamed::Enter => NamedKey::Enter,
        WinitNamed::Escape => NamedKey::Escape,
        WinitNamed::Tab => NamedKey::Tab,
        WinitNamed::Backspace => NamedKey::Backspace,
        WinitNamed::Delete => NamedKey::Delete,
        WinitNamed::Insert => NamedKey::Insert,
        WinitNamed::Home => NamedKey::Home,
        WinitNamed::End => NamedKey::End,
        WinitNamed::PageUp => NamedKey::PageUp,
        WinitNamed::PageDown => NamedKey::PageDown,
        WinitNamed::ArrowUp => NamedKey::Up,
        WinitNamed::ArrowDown => NamedKey::Down,
        WinitNamed::ArrowLeft => NamedKey::Left,
        WinitNamed::ArrowRight => NamedKey::Right,
        WinitNamed::Space => NamedKey::Space,
        // vim が持つのは K_F1..K_F37。手元にある鍵盤はどれも F24 までなので、
        // winit が知っている F1..F24 をそのまま通す。
        WinitNamed::F1 => NamedKey::Function(1),
        WinitNamed::F2 => NamedKey::Function(2),
        WinitNamed::F3 => NamedKey::Function(3),
        WinitNamed::F4 => NamedKey::Function(4),
        WinitNamed::F5 => NamedKey::Function(5),
        WinitNamed::F6 => NamedKey::Function(6),
        WinitNamed::F7 => NamedKey::Function(7),
        WinitNamed::F8 => NamedKey::Function(8),
        WinitNamed::F9 => NamedKey::Function(9),
        WinitNamed::F10 => NamedKey::Function(10),
        WinitNamed::F11 => NamedKey::Function(11),
        WinitNamed::F12 => NamedKey::Function(12),
        WinitNamed::F13 => NamedKey::Function(13),
        WinitNamed::F14 => NamedKey::Function(14),
        WinitNamed::F15 => NamedKey::Function(15),
        WinitNamed::F16 => NamedKey::Function(16),
        WinitNamed::F17 => NamedKey::Function(17),
        WinitNamed::F18 => NamedKey::Function(18),
        WinitNamed::F19 => NamedKey::Function(19),
        WinitNamed::F20 => NamedKey::Function(20),
        WinitNamed::F21 => NamedKey::Function(21),
        WinitNamed::F22 => NamedKey::Function(22),
        WinitNamed::F23 => NamedKey::Function(23),
        WinitNamed::F24 => NamedKey::Function(24),
        _ => return None,
    };
    Some(key)
}
