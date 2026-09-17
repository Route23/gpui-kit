//! ハイライトの上の文字を読める色まで持ち上げる（dopamine #276 / ADR-0109）。
//!
//! # なぜ要るか
//!
//! [`super::element::split_runs_by_bg_segments`] は、背景の乗った範囲の文字を
//! **黒か白に塗り替えていた**（`bg.l >= 0.5` で分けるだけ）。読めることは
//! 読めるが、**構文の色が全部消える** —— 検索の一致の上では `fn` も文字列も
//! コメントも同じ色になる。
//!
//! ここは Zed の `minimum_contrast_for_highlights` と同じ考えで、
//! **元の色を保ったまま、届くところまで明暗だけ動かす**。
//!
//! # APCA（Lc）
//!
//! WCAG 2 の比ではなく **APCA の Lc** を使う（Zed と同じ）。Lc は
//! **符号付き**で、|Lc| が大きいほど読みやすい:
//!
//! - 黒地に白 ≈ **+108**
//! - 白地に黒 ≈ **−108**
//! - 同じ色 = **0**
//!
//! 実装は APCA 0.98G-4g の公開式。**丸めの細部までは合わせない** ——
//! ここで要るのは「足りているか / どちらへ動かすか」だけで、
//! 0.1 の差は文字の読みやすさを変えない。

use gpui::Hsla;

/// sRGB の 1 成分（0..=1）を輝度へ。APCA は**ガンマを解かない** ——
/// 単純な 2.4 乗（これが WCAG との大きな違いのひとつ）。
fn channel(c: f32) -> f32 {
    c.clamp(0., 1.).powf(2.4)
}

/// APCA の Y（画面の輝度）。
fn luminance(c: Hsla) -> f32 {
    let rgba = c.to_rgb();
    0.2126729 * channel(rgba.r) + 0.7151522 * channel(rgba.g) + 0.0721750 * channel(rgba.b)
}

/// 黒に近すぎる輝度を持ち上げる（APCA の soft clamp）。
fn soft_clamp(y: f32) -> f32 {
    const THRESHOLD: f32 = 0.022;
    const EXPONENT: f32 = 1.414;
    if y >= THRESHOLD {
        y
    } else {
        y + (THRESHOLD - y).powf(EXPONENT)
    }
}

/// **APCA の Lc**（−108..=108 くらい）。
///
/// 正 = 暗い地に明るい文字、負 = 明るい地に暗い文字。
/// **`|lc|` が 0 に近いほど読めない。**
#[must_use]
pub(super) fn lc(text: Hsla, background: Hsla) -> f32 {
    const SCALE_BOW: f32 = 1.14; // 明るい地
    const SCALE_WOB: f32 = 1.14; // 暗い地
    const OFFSET: f32 = 0.027;
    // **ごく小さい差は 0 にする。** 丸めの誤差で符号が暴れると、
    // `ensure` がどちらへ動かすか決められなくなる。
    const DELTA: f32 = 0.0005;

    let (bg, fg) = (soft_clamp(luminance(background)), soft_clamp(luminance(text)));
    if (bg - fg).abs() < DELTA {
        return 0.;
    }
    let raw = if bg > fg {
        // 明るい地に暗い文字（BoW）。返りは負。
        (bg.powf(0.56) - fg.powf(0.57)) * SCALE_BOW
    } else {
        // 暗い地に明るい文字（WoB）。返りは正。
        (bg.powf(0.65) - fg.powf(0.62)) * SCALE_WOB
    };
    let out = if raw.abs() < OFFSET {
        0.
    } else if raw > 0. {
        raw - OFFSET
    } else {
        raw + OFFSET
    };
    // APCA は 100 倍して出す。地が明るいときは負に揃える。
    if bg > fg { -out * 100. } else { -out * 100. }
}

/// `text` を `background` の上で **Lc が `min_lc` に届くまで**動かす。
///
/// - **色相と彩度は動かさない。** 明度だけ —— 構文の色を残すのが目的
/// - **動かす向きは地の明暗で決める** —— 明るい地なら暗く、暗い地なら明るく
/// - **届かなければ行けるところまで**（黒か白で止まる）。
///   「読めない色のまま」よりはまし
/// - `min_lc <= 0` なら**何もしない**
#[must_use]
pub(super) fn ensure(text: Hsla, background: Hsla, min_lc: f32) -> Hsla {
    if min_lc <= 0. || lc(text, background).abs() >= min_lc {
        return text;
    }
    // 地が明るければ文字を暗く、暗ければ明るく。
    let darken = luminance(background) > luminance(text)
        || luminance(background) > soft_clamp(0.5f32.powf(2.4));
    let mut out = text;
    // **二分ではなく刻みで寄せる。** 32 段で 1/32 ずつ動かせば十分細かく、
    // 同じ入力に同じ答えが返る（二分は打ち切りで揺れる）。
    for i in 1..=32 {
        let t = i as f32 / 32.;
        out.l = if darken {
            text.l * (1. - t)
        } else {
            text.l + (1. - text.l) * t
        };
        if lc(out, background).abs() >= min_lc {
            return out;
        }
    }
    // 行けるところまで。
    out.l = if darken { 0. } else { 1. };
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray(l: f32) -> Hsla {
        Hsla { h: 0., s: 0., l, a: 1. }
    }

    /// 端の値。**符号が向きを表す。**
    #[test]
    fn the_extremes_land_near_a_hundred() {
        let white_on_black = lc(gray(1.), gray(0.));
        let black_on_white = lc(gray(0.), gray(1.));
        assert!(white_on_black > 95., "黒地に白が {white_on_black}");
        assert!(black_on_white < -95., "白地に黒が {black_on_white}");
        assert_eq!(lc(gray(0.5), gray(0.5)), 0., "同じ色は 0");
    }

    /// 近い色ほど読めない。
    #[test]
    fn closer_colours_score_lower() {
        let far = lc(gray(0.95), gray(0.1)).abs();
        let near = lc(gray(0.45), gray(0.4)).abs();
        assert!(far > near, "far={far} near={near}");
        assert!(near < 20., "ほとんど読めないはずが {near}");
    }

    /// **0 なら何もしない**（今までどおりの枝）。
    #[test]
    fn a_threshold_of_zero_changes_nothing() {
        let text = Hsla { h: 0.6, s: 0.8, l: 0.45, a: 1. };
        assert_eq!(ensure(text, gray(0.4), 0.), text);
        assert_eq!(ensure(text, gray(0.4), -5.), text);
    }

    /// **足りていれば触らない。**
    #[test]
    fn enough_contrast_is_left_alone() {
        let text = gray(0.95);
        assert_eq!(ensure(text, gray(0.05), 45.), text);
    }

    /// **色相と彩度は残る。** 動くのは明度だけ。
    #[test]
    fn only_the_lightness_moves() {
        let text = Hsla { h: 0.33, s: 0.7, l: 0.42, a: 1. };
        let bg = gray(0.38);
        let out = ensure(text, bg, 45.);
        assert_eq!((out.h, out.s, out.a), (text.h, text.s, text.a));
        assert!(out.l != text.l, "明度が動いていない");
        assert!(lc(out, bg).abs() >= 45., "届いていない: {}", lc(out, bg));
    }

    /// 明るい地では**暗く**、暗い地では**明るく**。
    #[test]
    fn it_moves_away_from_the_background() {
        let text = gray(0.5);
        assert!(ensure(text, gray(0.55), 45.).l < text.l, "明るい地なら暗くする");
        assert!(ensure(text, gray(0.45), 45.).l > text.l, "暗い地なら明るくする");
    }

    /// 届かない要求でも**落ちないし、行けるところまで行く**。
    #[test]
    fn an_impossible_threshold_goes_as_far_as_it_can() {
        let out = ensure(gray(0.5), gray(0.5), 200.);
        assert!(out.l == 0. || out.l == 1., "端まで行っていない: {}", out.l);
    }
}
