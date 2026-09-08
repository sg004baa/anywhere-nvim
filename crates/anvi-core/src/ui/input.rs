//! キー入力を nvim の key-notation（`:h key-notation`）へ変換する。
//!
//! GUI（winit）が受け取るキーイベントと IME の確定文字列を、`nvim_input` にそのまま
//! 渡せる文字列にするのがここの仕事。**OS のキーコードは持ち込まない。** winit の
//! 型に依存すると core が Windows/winit に縛られるため、[`Key`] / [`Mods`] という
//! 最小の中間表現を置き、winit からの変換は GUI 側（`gui/keys.rs`）が持つ。
//!
//! 記法の決めごと（→ v2 計画 §4.2）:
//!
//! - 修飾子の並びは `M-` → `C-` → `S-` で固定する。nvim はどの順でも解釈するが、
//!   出力を一意にしておかないとテストで固定できない。
//! - `S-` を付けるのは [`Key::Named`] のときだけ。[`Key::Char`] には **既にシフトを
//!   適用したあとの文字**（`A`、`!`）が届くので、そこへ更に `S-` を足すと nvim 側で
//!   二重適用になる。
//! - `<` は常に `<lt>`。生の `<` を送ると後続が記法として食われる。

use std::borrow::Cow;

/// 文字に還元できないキー。ここに無いキーは GUI 側で捨てる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedKey {
    Enter,
    Escape,
    Tab,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    Space,
    /// ファンクションキー。`Function(5)` は `<F5>`。
    Function(u8),
}

impl NamedKey {
    /// `<...>` の中身。`Space` だけは「修飾ありのときの綴り」で、修飾なしの空白は
    /// [`encode_key`] が素の `" "` として先に返す。
    fn notation(self) -> Cow<'static, str> {
        match self {
            Self::Enter => Cow::Borrowed("CR"),
            Self::Escape => Cow::Borrowed("Esc"),
            Self::Tab => Cow::Borrowed("Tab"),
            Self::Backspace => Cow::Borrowed("BS"),
            Self::Delete => Cow::Borrowed("Del"),
            Self::Insert => Cow::Borrowed("Insert"),
            Self::Home => Cow::Borrowed("Home"),
            Self::End => Cow::Borrowed("End"),
            Self::PageUp => Cow::Borrowed("PageUp"),
            Self::PageDown => Cow::Borrowed("PageDown"),
            Self::Up => Cow::Borrowed("Up"),
            Self::Down => Cow::Borrowed("Down"),
            Self::Left => Cow::Borrowed("Left"),
            Self::Right => Cow::Borrowed("Right"),
            Self::Space => Cow::Borrowed("Space"),
            Self::Function(n) => Cow::Owned(format!("F{n}")),
        }
    }
}

/// 押されたキー。`Char` にはレイアウトと IME を通したあとの文字が入る。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Named(NamedKey),
}

/// 修飾キー。Windows キー（logo）は握らない。OS 側のホットキーであり、nvim へ
/// 渡す意味がないため。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    /// 修飾がひとつも立っていない。
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.ctrl && !self.alt && !self.shift
    }
}

/// nvim の key-notation へ。送るものが無ければ `None`。
///
/// `None` になるのは [`Key::Char`] が制御文字だったときだけ。制御文字は
/// [`NamedKey`]（`Enter` / `Tab` / `Escape` / `Backspace`）で表現するものであり、
/// 生のまま流すと同じキーが二重に入る。
#[must_use]
pub fn encode_key(key: Key, mods: Mods) -> Option<String> {
    if matches!(key, Key::Char('v' | 'V')) && mods.ctrl && mods.shift && !mods.alt {
        return Some("<C-S-V>".to_owned());
    }

    match key {
        Key::Char(c) if c.is_control() => None,
        // Ctrl / Alt が絡むときだけ `<...>` にする。Shift 単独は文字そのものに
        // 反映済みなので、素の文字として流す（`<S-a>` にはしない）。
        Key::Char(c) if mods.ctrl || mods.alt => Some(wrap(&char_notation(c), mods, false)),
        Key::Char(c) => {
            let mut buf = [0u8; 4];
            Some(encode_text(c.encode_utf8(&mut buf)))
        }
        // 修飾なしの空白は `<Space>` ではなく素の空白。挿入モードで最も多く通る
        // 経路なので、記法を経由させない。
        Key::Named(NamedKey::Space) if mods.is_empty() => Some(" ".to_owned()),
        Key::Named(named) => Some(wrap(&named.notation(), mods, true)),
    }
}

/// 確定済みテキスト（IME コミット等）を `nvim_input` に安全に渡せる形へ。
///
/// `<` だけが記法の開始文字なので、それを `<lt>` に逃がす。それ以外は素通し
/// （日本語も絵文字も UTF-8 のまま nvim が受け取る）。
#[must_use]
pub fn encode_text(text: &str) -> String {
    text.replace('<', "<lt>")
}

/// `<...>` の中に置く文字。`<` だけは名前で書かないと閉じ括弧まで壊れる。
fn char_notation(c: char) -> Cow<'static, str> {
    if c == '<' {
        Cow::Borrowed("lt")
    } else {
        Cow::Owned(c.to_string())
    }
}

/// `<` + 修飾子 + 本体 + `>`。`shift_allowed` が false なら `S-` を落とす。
fn wrap(body: &str, mods: Mods, shift_allowed: bool) -> String {
    let mut out = String::with_capacity(body.len() + 8);
    out.push('<');
    if mods.alt {
        out.push_str("M-");
    }
    if mods.ctrl {
        out.push_str("C-");
    }
    if shift_allowed && mods.shift {
        out.push_str("S-");
    }
    out.push_str(body);
    out.push('>');
    out
}

#[cfg(test)]
mod tests {
    use super::{Key, Mods, NamedKey, encode_key, encode_text};

    const NONE: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: false,
    };
    const CTRL: Mods = Mods {
        ctrl: true,
        alt: false,
        shift: false,
    };
    const ALT: Mods = Mods {
        ctrl: false,
        alt: true,
        shift: false,
    };
    const SHIFT: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: true,
    };
    const ALT_CTRL: Mods = Mods {
        ctrl: true,
        alt: true,
        shift: false,
    };
    const CTRL_SHIFT: Mods = Mods {
        ctrl: true,
        alt: false,
        shift: true,
    };
    const ALL: Mods = Mods {
        ctrl: true,
        alt: true,
        shift: true,
    };

    fn ch(c: char, mods: Mods) -> Option<String> {
        encode_key(Key::Char(c), mods)
    }

    fn named(key: NamedKey, mods: Mods) -> Option<String> {
        encode_key(Key::Named(key), mods)
    }

    fn some(text: &str) -> Option<String> {
        Some(text.to_owned())
    }

    #[test]
    fn plain_characters_pass_through() {
        assert_eq!(ch('a', NONE), some("a"));
        assert_eq!(ch('A', SHIFT), some("A"));
        assert_eq!(ch('!', SHIFT), some("!"));
        assert_eq!(ch('あ', NONE), some("あ"));
    }

    #[test]
    fn ctrl_shift_v_uses_a_distinct_canonical_notation() {
        assert_eq!(ch('v', CTRL_SHIFT), some("<C-S-V>"));
        assert_eq!(ch('V', CTRL_SHIFT), some("<C-S-V>"));
        assert_eq!(ch('v', CTRL), some("<C-v>"));
        assert_eq!(ch('v', ALT_CTRL), some("<M-C-v>"));
        assert_eq!(ch('v', SHIFT), some("v"));
    }

    #[test]
    fn ctrl_and_alt_use_the_bracket_notation() {
        assert_eq!(ch('a', CTRL), some("<C-a>"));
        assert_eq!(ch('x', ALT), some("<M-x>"));
    }

    #[test]
    fn the_modifier_order_is_alt_ctrl_shift() {
        assert_eq!(ch('x', ALT_CTRL), some("<M-C-x>"));
        assert_eq!(named(NamedKey::Tab, ALL), some("<M-C-S-Tab>"));
    }

    #[test]
    fn shift_is_only_spelled_out_for_named_keys() {
        assert_eq!(named(NamedKey::Tab, SHIFT), some("<S-Tab>"));
        // Char には既にシフト適用後の文字が来る。`<C-S-a>` ではなく `<C-A>`。
        assert_eq!(ch('A', CTRL_SHIFT), some("<C-A>"));
    }

    #[test]
    fn the_less_than_sign_is_always_escaped() {
        assert_eq!(ch('<', NONE), some("<lt>"));
        assert_eq!(ch('<', SHIFT), some("<lt>"));
        assert_eq!(ch('<', CTRL), some("<C-lt>"));
        assert_eq!(ch('<', ALT), some("<M-lt>"));
    }

    #[test]
    fn space_is_bare_unless_modified() {
        assert_eq!(named(NamedKey::Space, NONE), some(" "));
        assert_eq!(named(NamedKey::Space, CTRL), some("<C-Space>"));
        assert_eq!(named(NamedKey::Space, SHIFT), some("<S-Space>"));
    }

    #[test]
    fn function_keys_are_numbered() {
        assert_eq!(named(NamedKey::Function(5), NONE), some("<F5>"));
        assert_eq!(named(NamedKey::Function(5), CTRL), some("<C-F5>"));
        assert_eq!(named(NamedKey::Function(12), NONE), some("<F12>"));
    }

    #[test]
    fn named_keys_use_nvims_spelling() {
        assert_eq!(named(NamedKey::Enter, NONE), some("<CR>"));
        assert_eq!(named(NamedKey::Escape, NONE), some("<Esc>"));
        assert_eq!(named(NamedKey::Backspace, NONE), some("<BS>"));
        assert_eq!(named(NamedKey::Delete, NONE), some("<Del>"));
        assert_eq!(named(NamedKey::Insert, NONE), some("<Insert>"));
        assert_eq!(named(NamedKey::Home, NONE), some("<Home>"));
        assert_eq!(named(NamedKey::End, NONE), some("<End>"));
        assert_eq!(named(NamedKey::PageUp, NONE), some("<PageUp>"));
        assert_eq!(named(NamedKey::PageDown, NONE), some("<PageDown>"));
        assert_eq!(named(NamedKey::Up, NONE), some("<Up>"));
        assert_eq!(named(NamedKey::Down, NONE), some("<Down>"));
        assert_eq!(named(NamedKey::Left, NONE), some("<Left>"));
        assert_eq!(named(NamedKey::Right, NONE), some("<Right>"));
    }

    #[test]
    fn control_characters_are_dropped() {
        // 制御文字は NamedKey で表す。生で流すと同じキーが二重に入る。
        assert_eq!(ch('\r', NONE), None);
        assert_eq!(ch('\t', NONE), None);
        assert_eq!(ch('\u{1b}', NONE), None);
        assert_eq!(ch('\u{1}', CTRL), None);
    }

    #[test]
    fn encode_text_only_escapes_the_less_than_sign() {
        assert_eq!(encode_text("a<b"), "a<lt>b");
        assert_eq!(encode_text("<<"), "<lt><lt>");
        assert_eq!(encode_text("plain"), "plain");
        assert_eq!(encode_text(""), "");
    }

    #[test]
    fn encode_text_passes_japanese_through() {
        assert_eq!(encode_text("こんにちは"), "こんにちは");
        assert_eq!(encode_text("全角＜と半角<"), "全角＜と半角<lt>");
        assert_eq!(encode_text("絵文字🍣も"), "絵文字🍣も");
    }
}
