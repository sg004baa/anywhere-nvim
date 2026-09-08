//! ウィンドウの生成と表示制御（DESIGN v2 §4.5）。
//!
//! 編集ウィンドウは host 自身のものなので、探索も生存監視も要らない。ここで Win32 を
//! 直に叩くのは **表示位置決め**（対象ウィンドウの矩形とモニタの作業領域）と
//! **前面化** の 2 つだけで、前面化は [`crate::focus`] の作法をそのまま使う。
//!
//! winit の `Window::focus_window` は使わない。あれは `SetForegroundWindow` を素で
//! 呼ぶだけで、実機では呼び出し制限に引っかかって無言で失敗する。前面スレッドへ
//! `AttachThreadInput` してから叩く作法が要る（→ [`crate::focus`]）。

use anyhow::{Context as _, bail};
use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event_loop::ActiveEventLoop;
use winit::platform::windows::WindowAttributesExtWindows as _;
use winit::window::Window;

use crate::focus;

const TITLE: &str = "anvi";

/// セル寸法が分かる前に使う暫定サイズ（論理ピクセル）。
///
/// 本当のセル寸法は `Renderer` が DirectWrite に訊くまで分からないが、ウィンドウは
/// レンダーターゲットより先に無いと作れない。既定フォント（MS Gothic 12pt = 16px、
/// 等幅なので半角の送り幅は 8px）から見積もった値を初期サイズに使い、`Renderer` が
/// できた直後に [`resize_to_grid`] で実測値へ合わせ直す。
const PROVISIONAL_CELL: (f64, f64) = (8.0, 16.0);

/// 編集ウィンドウを作る。**作った時点では見えないし前面にも来ない。**
///
/// ホットキーが押されるまで画面に出てはいけないので `with_visible(false)`、
/// 起動時にユーザーの作業を奪わないので `with_active(false)`。表示中はタスクバーに
/// 出て、`Alt+Tab` の対象になる。
///
/// タイトルバーは出さない（`with_decorations(false)`）。編集中の 1 行に
/// システムの枠を被せる意味が無く、ウィンドウは対象アプリに重ねて出して `ZZ` / `ZQ`
/// で閉じるものなので、ボタン類も要らない。
///
/// **`with_no_redirection_bitmap(true)` は透過の前提条件**（`WS_EX_NOREDIRECTIONBITMAP`）。
/// リダイレクションサーフェスがあると、そこが不透明に塗られて背後が透けない。
/// 実際の合成は [`crate::gui::render`] の DirectComposition が行う。
pub fn create(event_loop: &ActiveEventLoop, grid: (u16, u16)) -> anyhow::Result<Window> {
    let (cols, rows) = grid;
    let attributes = Window::default_attributes()
        .with_title(TITLE)
        .with_visible(false)
        .with_active(false)
        .with_resizable(true)
        .with_decorations(false)
        .with_transparent(true)
        .with_inner_size(LogicalSize::new(
            f64::from(cols) * PROVISIONAL_CELL.0,
            f64::from(rows) * PROVISIONAL_CELL.1,
        ))
        .with_no_redirection_bitmap(true);
    event_loop
        .create_window(attributes)
        .context("failed to create the editor window")
}

/// `focus` と `Renderer` に渡す生の HWND。
///
/// HWND は `!Send` なので、スレッドを跨ぐ経路と同じく `isize` で持ち回す
/// （[`crate::focus::as_hwnd`] が Win32 の型へ戻す）。
pub fn hwnd_of(window: &Window) -> anyhow::Result<isize> {
    let handle = window
        .window_handle()
        .context("the editor window has no raw handle")?;
    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => Ok(win32.hwnd.get()),
        other => bail!("unexpected raw window handle: {other:?}"),
    }
}

/// グリッドがちょうど収まる内側サイズへ合わせる。
///
/// `cell` は物理ピクセルのセル寸法（幅・高さ）、`pad` はグリッドの周囲に空ける
/// 余白（物理ピクセル。上下左右それぞれ）。要求したサイズがそのまま通るとは
/// 限らない（DWM の最小サイズなど）ので、確定は後続の `Resized` イベントで行う。
pub fn resize_to_grid(window: &Window, cell: (f32, f32), grid: (u16, u16), pad: f32) {
    let margin = f64::from(pad) * 2.0;
    let width = to_px(f64::from(grid.0) * f64::from(cell.0) + margin);
    let height = to_px(f64::from(grid.1) * f64::from(cell.1) + margin);
    let size = PhysicalSize::new(width, height);
    if window.request_inner_size(size).is_none() {
        tracing::debug!(
            width,
            height,
            "the resize request is deferred to a Resized event"
        );
    }
}

/// 内側サイズの、余白を除いた部分に収まるグリッドの行列数。端数は切り捨て、最低 1x1。
///
/// 0 行 0 列を nvim へ送ると `nvim_ui_try_resize` がエラーを返すので、ここで潰す。
#[must_use]
pub fn grid_for(size: PhysicalSize<u32>, cell: (f32, f32), pad: f32) -> (u16, u16) {
    let margin = (pad * 2.0).ceil().max(0.0) as u32;
    (
        count(size.width.saturating_sub(margin), cell.0),
        count(size.height.saturating_sub(margin), cell.1),
    )
}

/// 画面に出して前面へ持ってくる（DESIGN 6.2 手順 5）。
///
/// `target` は編集対象の入力欄の HWND。編集ウィンドウは**その入力欄と同じモニタで、
/// 対象ウィンドウに重ねて**出す（DESIGN 7.2）。
///
/// 位置決めに失敗しても表示はする。変な位置に出るのは困るが、編集できないよりましで、
/// 原因はログに残る。
pub fn show(window: &Window, hwnd: isize, target: isize) -> anyhow::Result<()> {
    match over_target(window, target) {
        Ok(position) => window.set_outer_position(position),
        Err(err) => tracing::error!(%err, "cannot place the editor window over the target"),
    }
    window.set_visible(true);
    focus::set_foreground(hwnd)
}

/// 画面から消す（DESIGN 6.2 手順 8）。前面は呼び出し側が元のアプリへ戻す。
pub fn hide(window: &Window) {
    window.set_visible(false);
}

/// 対象ウィンドウに重ねる左上座標（物理ピクセル）。
///
/// UIA が返す HWND は子コントロール（メモ帳の編集領域など）なので、矩形もモニタも
/// トップレベルの祖先で見る（[`crate::focus`] と同じ作法）。
fn over_target(window: &Window, target: isize) -> anyhow::Result<PhysicalPosition<i32>> {
    let root = focus::root_of(focus::as_hwnd(target));
    let target_rect = rect_of(root).context("cannot read the target window rect")?;
    let work = work_area(root).context("cannot read the monitor work area")?;
    let size = window.outer_size();
    let (x, y) = place_over(target_rect, work, (size.width, size.height));
    Ok(PhysicalPosition::new(x, y))
}

/// ウィンドウの外枠。仮想スクリーン座標の物理ピクセルで（左, 上, 右, 下）。
fn rect_of(hwnd: HWND) -> anyhow::Result<(i32, i32, i32, i32)> {
    let mut rect = RECT::default();
    // SAFETY: rect は有効なローカル変数。無効な HWND なら Err が返るだけ。
    unsafe { GetWindowRect(hwnd, &raw mut rect) }?;
    Ok(sides(rect))
}

/// 対象ウィンドウが載っているモニタの作業領域（タスクバーを除いた矩形）。
fn work_area(hwnd: HWND) -> anyhow::Result<(i32, i32, i32, i32)> {
    // SAFETY: 問い合わせのみ。どのモニタとも交差しなければ最も近いモニタを返す。
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_invalid() {
        bail!("MonitorFromWindow found no monitor");
    }
    let mut info = MONITORINFO {
        cbSize: u32::try_from(size_of::<MONITORINFO>()).context("MONITORINFO is too large")?,
        ..MONITORINFO::default()
    };
    // SAFETY: monitor は有効なハンドルで、info は cbSize を埋めた有効なローカル変数。
    if !unsafe { GetMonitorInfoW(monitor, &raw mut info) }.as_bool() {
        bail!("GetMonitorInfoW failed");
    }
    Ok(sides(info.rcWork))
}

/// `RECT` をタプルへ。要素の意味は（左, 上, 右, 下）。
fn sides(rect: RECT) -> (i32, i32, i32, i32) {
    (rect.left, rect.top, rect.right, rect.bottom)
}

/// 対象矩形 `target` の中心へ大きさ `size` のウィンドウを合わせた左上座標。
///
/// 作業領域 `work` からはみ出す分は中へ押し戻し、`work` より大きい辺は作業領域の
/// 左（上）端へ寄せる。矩形は（左, 上, 右, 下）、`size` は（幅, 高さ）で、すべて
/// 仮想スクリーン座標の物理ピクセル。セカンダリモニタが左や上にあると負になる。
fn place_over(
    target: (i32, i32, i32, i32),
    work: (i32, i32, i32, i32),
    size: (u32, u32),
) -> (i32, i32) {
    (
        centered(target.0, target.2, work.0, work.2, size.0),
        centered(target.1, target.3, work.1, work.3, size.1),
    )
}

/// 1 軸ぶんの中心合わせとクランプ。
///
/// 座標は仮想スクリーンの端で i32 を溢れうるので、加減算は全て `saturating_*`。
fn centered(target_min: i32, target_max: i32, work_min: i32, work_max: i32, span: u32) -> i32 {
    // 画面より大きいウィンドウは存在しないが、飽和しておけば下のクランプが効く。
    let span = i32::try_from(span).unwrap_or(i32::MAX);
    // 中心は (min + max) / 2 だが、その和は溢れうるので幅の半分を足して求める。
    let center = target_min.saturating_add(target_max.saturating_sub(target_min) / 2);
    let start = center.saturating_sub(span / 2);
    // 収まる最大の開始位置。ウィンドウが作業領域より大きいと `work_min` を下回るので、
    // 続く `max` が左（上）端へ寄せる。
    let last = work_max.saturating_sub(span);
    start.min(last).max(work_min)
}

/// 画素数へ丸める。負にも巨大にもならないよう挟む。
fn to_px(value: f64) -> u32 {
    value.ceil().clamp(1.0, f64::from(u32::MAX)) as u32
}

/// 辺 `span`（画素）に幅 `unit` のセルが何個入るか。
fn count(span: u32, unit: f32) -> u16 {
    if unit <= 0.0 {
        // セル寸法が 0 以下になるのは `Renderer` の実装バグでしかない。
        tracing::error!(unit, "non-positive cell metric; clamping the grid to 1");
        return 1;
    }
    let cells = (f64::from(span) / f64::from(unit)).floor();
    cells.clamp(1.0, f64::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use super::place_over;

    /// 収まりきる限り、対象ウィンドウの中心に重なる。
    #[test]
    fn centers_on_the_target() {
        let work = (0, 0, 1920, 1080);
        assert_eq!(
            place_over((100, 100, 500, 400), work, (200, 100)),
            (200, 200)
        );
    }

    /// 対象が右下隅にあっても作業領域の外へは出ない（タスクバーの下へも潜らない）。
    #[test]
    fn clamps_to_the_bottom_right_of_the_work_area() {
        let work = (0, 0, 1920, 1032);
        assert_eq!(
            place_over((1800, 960, 1900, 1020), work, (400, 300)),
            (1520, 732)
        );
    }

    /// 作業領域より大きいウィンドウは左上に寄せる（はみ出しは右下へ）。
    #[test]
    fn pins_an_oversized_window_to_the_top_left() {
        assert_eq!(
            place_over((10, 20, 700, 500), (0, 0, 800, 600), (1000, 700)),
            (0, 0)
        );
    }

    /// 左・上にあるセカンダリモニタ（座標が負）でも同じモニタ内に収まる。
    #[test]
    fn stays_on_a_monitor_with_negative_coordinates() {
        let work = (-1920, -100, 0, 980);
        assert_eq!(
            place_over((-1500, 200, -1100, 400), work, (600, 200)),
            (-1600, 200)
        );
        // 対象がそのモニタの左端に張り付いていても、左へはみ出さない。
        assert_eq!(
            place_over((-1900, 0, -1850, 50), work, (600, 200)),
            (-1920, -75)
        );
    }

    /// 仮想スクリーンの端でも桁溢れせず、クランプだけが効く。
    #[test]
    fn saturates_instead_of_overflowing() {
        // 中心の (左 + 右) が i32 を溢れる組み合わせ。
        let huge = (i32::MIN, i32::MIN, i32::MAX, i32::MAX);
        assert_eq!(
            place_over(
                (i32::MAX - 10, i32::MAX - 10, i32::MAX, i32::MAX),
                huge,
                (100, 100)
            ),
            (i32::MAX - 100, i32::MAX - 100)
        );
        // 幅が i32 に収まらないウィンドウでも、作業領域の左上に寄るだけ。
        assert_eq!(
            place_over((0, 0, 10, 10), (0, 0, 100, 100), (u32::MAX, u32::MAX)),
            (0, 0)
        );
    }
}
