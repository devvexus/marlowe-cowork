//! Snapshots of the 2D layout, the one-line forms, the safety of every glyph, and the heuristics.

use super::*;

fn show(src: &str) -> String {
    display(src).rows.join("\n")
}

fn one(src: &str) -> String {
    inline(src).rows[0].clone()
}

#[track_caller]
fn snap(src: &str, want: &str) {
    let got = show(src);
    assert_eq!(got, want.trim_matches('\n'), "\n{src}\n--- got ---\n{got}\n");
}

// ── 2D snapshots ────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_fraction_stacks_with_a_vinculum_that_is_not_box_drawing() {
    snap(
        r"\frac{a+b}{c}",
        "
a + b
⎯⎯⎯⎯⎯
  c",
    );
    assert!(!show(r"\frac{a}{b}").contains('─'), "the vinculum is a reserved border glyph");
}

#[test]
fn the_quadratic_formula() {
    snap(
        r"x = \frac{-b \pm \sqrt{b^2 - 4ac}}{2a}",
        "
          ________
    −b ± √b² − 4ac
x = ⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯
          2a",
    );
}

#[test]
fn a_sum_takes_its_limits_above_and_below() {
    snap(
        r"\sum_{i=1}^{n} i = \frac{n(n+1)}{2}",
        "
 n      n(n + 1)
 ∑  i = ⎯⎯⎯⎯⎯⎯⎯⎯
i=1        2",
    );
}

#[test]
fn an_integral_is_drawn_in_two_halves_with_its_bounds() {
    snap(
        r"\int_0^\infty e^{-x^2} \, dx = \frac{\sqrt{\pi}}{2}",
        "
∞
⌠           √π
⌡ e⁻ˣ² dx = ⎯⎯
0           2",
    );
}

#[test]
fn brackets_scale_to_what_they_enclose() {
    snap(
        r"\operatorname{softmax}\left( \frac{QK^\top}{\sqrt{d_k}} \right) V",
        "
        ⎛QKᵀ⎞
softmax ⎜⎯⎯⎯⎟ V
        ⎝√dₖ⎠",
    );
}

#[test]
fn a_matrix_and_its_determinant() {
    snap(
        r"\det \begin{bmatrix} a & b \\ c & d \end{bmatrix} = ad - bc",
        "
det ⎡a  b⎤ = ad − bc
    ⎣c  d⎦",
    );
    snap(
        r"\begin{pmatrix} 1 & 0 & 0 \\ 0 & 1 & 0 \\ 0 & 0 & 1 \end{pmatrix}",
        "
⎛1  0  0⎞
⎜0  1  0⎟
⎝0  0  1⎠",
    );
}

#[test]
fn cases_take_a_brace_and_left_aligned_columns() {
    snap(
        r"|x| = \begin{cases} x, & x \geq 0 \\ -x, & \text{otherwise} \end{cases}",
        "
|x| = ⎰x,   x ≥ 0
      ⎱−x,  otherwise",
    );
    snap(
        r"\operatorname{sgn}(x) = \begin{cases} -1 & x < 0 \\ 0 & x = 0 \\ 1 & x > 0 \end{cases}",
        "
         ⎧−1  x < 0
sgn(x) = ⎨0   x = 0
         ⎩1   x > 0",
    );
}

#[test]
fn an_alignment_lines_up_on_its_relations() {
    snap(
        r"\begin{aligned} f(x) &= (x+1)^2 \\ &= x^2 + 2x + 1 \end{aligned}",
        "
f(x) = (x + 1)²
     = x² + 2x + 1",
    );
}

#[test]
fn scripts_unicode_cannot_express_drop_to_their_own_row() {
    snap(
        r"\mathcal{L}_{\text{CE}} = -\sum_{c} y_c \log \hat{y}_c",
        "
ℒ   = −∑ y  log ŷ
 CE    c  c      c",
    );
}

#[test]
fn a_tall_radical_draws_its_diagonal() {
    snap(
        r"\sqrt{\frac{1}{n}\sum_{i=1}^n (x_i - \bar{x})^2}",
        "
   ______________
  /1  n
 / ⎯  ∑ (xᵢ − x̄)²
√  n i=1",
    );
}

#[test]
fn a_limit_and_a_scaled_power() {
    snap(
        r"\lim_{n \to \infty} \left(1 + \frac{1}{n}\right)^n = e",
        "
    ⎛    1⎞ⁿ
lim ⎜1 + ⎯⎟  = e
n→∞ ⎝    n⎠",
    );
}

#[test]
fn braces_and_binomials() {
    snap(
        r"\underbrace{1 + 1 + \cdots + 1}_{n} = n",
        "
1 + 1 + ⋅⋅⋅ + 1 = n
⎣_____________⎦
       n",
    );
    snap(
        r"\binom{n}{k}",
        "
⎛n⎞
⎝k⎠",
    );
}

// ── one-line forms ──────────────────────────────────────────────────────────────────────────────

#[test]
fn inline_maths_keeps_one_line() {
    for (src, want) in [
        (r"\alpha", "α"),
        (r"x^2", "x²"),
        (r"x_i", "xᵢ"),
        (r"E = mc^2", "E = mc²"),
        (r"x^2+y^2=z^2", "x² + y² = z²"),
        (r"-x", "−x"),
        (r"\sum_{i=1}^{n} i", "∑ᵢ₌₁ⁿ i"),
        (r"\frac{1}{2}", "½"),
        (r"\frac{a+b}{c}", "(a + b)/c"),
        (r"\frac{\partial L}{\partial w}", "∂L/∂w"),
        (r"\frac{a}{b}x", "(a/b)x"),
        (r"\sqrt{2}", "√2"),
        (r"\sqrt{x+1}", "√(x + 1)"),
        (r"\sqrt[3]{x}", "∛x"),
        (r"\hat{y}", "ŷ"),
        (r"\dot{x}", "ẋ"),
        (r"\mathbb{R}^n", "ℝⁿ"),
        (r"f: X \to Y", "f: X → Y"),
        (r"\left( x \right)", "(x)"),
        (r"O(n \log n)", "O(n log n)"),
        (r"\sin(x)", "sin(x)"),
        (r"\sin x", "sin x"),
        (r"p(a \mid s)", "p(a ∣ s)"),
        (r"D(P \| Q)", "D(P ‖ Q)"),
        (r"QK^\top", "QKᵀ"),
        (r"\nabla_\theta J", "∇_θJ"),
        (r"D_{\mathrm{KL}}", "D_(KL)"),
        (r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}", "(a, b; c, d)"),
        (r"\binom{n}{k}", "C(n, k)"),
        (r"1,000", "1,000"),
        (r"a = 1 \qquad b = 2", "a = 1    b = 2"),
        (r"\overline{AB}", "A\u{305}B\u{305}"),
        (r"\boxed{42}", "[42]"),
    ] {
        assert_eq!(one(src), want, "inline {src:?}");
    }
}

#[test]
fn a_formula_with_no_unicode_superscript_still_keeps_its_bound() {
    // The old renderer's founding fear: `\int_0^\infty` collapsing to `∫₀`, a different integral.
    let out = one(r"\int_0^\infty e^{-x}dx");
    assert!(out.contains('∞') && out.contains('₀') && out.contains('∫'), "{out}");
}

// ── never raw ───────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_slip_is_repaired_and_the_rest_is_paraphrased_never_returned_as_source() {
    for (src, fid) in [
        (r"\frac{a}{b", Fidelity::Repaired),
        (r"x^2 + y^2 = z^2}", Fidelity::Repaired),
        (r"\left( \frac{1}{2}", Fidelity::Repaired),
        (r"\begin{aligned} a &= b \\ c &= d", Fidelity::Repaired),
        (r"x = 1$", Fidelity::Repaired),
        (r"\foo{x}{y} + 1", Fidelity::Paraphrased),
        (r"\ce{H2O}", Fidelity::Paraphrased),
    ] {
        for f in [display(src), inline(src)] {
            assert_eq!(f.fidelity, fid, "{src:?}");
            for row in f.rows.iter().chain(&f.linear) {
                assert!(
                    !row.contains(['\\', '$', '{', '}']),
                    "{src:?} put source on screen: {row:?}"
                );
            }
        }
    }
    assert_eq!(one(r"\foo{x}{y} + 1"), "foo(x, y) + 1");
}

#[test]
fn deep_nesting_is_bounded_and_still_not_raw() {
    let deep = format!("{}x{}", "{".repeat(500), "}".repeat(500));
    let f = display(&deep);
    assert!(!f.rows.concat().contains(['{', '}', '\\']), "{:?}", f.rows);
    let fracs = r"\frac{1}{".repeat(60) + "x" + &"}".repeat(60);
    let f = display(&fracs);
    assert!(f.rows.len() <= MAX_ROWS, "{} rows", f.rows.len());
    assert!(!f.rows.concat().contains(['{', '}', '\\']));
}

// ── safety ──────────────────────────────────────────────────────────────────────────────────────

#[test]
fn every_glyph_the_engine_can_emit_is_renderable_single_width_and_not_chrome() {
    let mut checked = 0;
    for c in symbols::all_glyphs().into_iter().chain(layout::layout_glyphs()) {
        assert!(marlowe_contract::text::is_renderable(c), "U+{:04X} is refused by the predicate", c as u32);
        assert!(
            !crate::chrome::is_reserved(c),
            "U+{:04X} is harness chrome; a formula must not be able to draw it",
            c as u32
        );
        assert!(
            unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) <= 1,
            "U+{:04X} is double width and would break the grid",
            c as u32
        );
        checked += 1;
    }
    assert!(checked > 400, "only {checked} glyphs walked");
    println!("math: {checked} emittable glyphs, all renderable, single width, none chrome");
}

#[test]
fn a_reserved_glyph_in_a_formula_is_replaced_on_the_way_out() {
    let f = display(r"\text{a─b}");
    assert!(!f.rows.concat().contains('─'), "{:?}", f.rows);
}

// ── the dollar-sign heuristics ──────────────────────────────────────────────────────────────────

#[test]
fn a_dollar_sign_in_prose_is_not_maths() {
    for content in ["5 and ", "5 to ", "100–", "3.99 or ", "x and y", "5/", "5-"] {
        assert!(!looks_like_inline_maths(content), "{content:?} would be read as maths");
    }
    for content in ["x^2", r"\alpha", "a + b", "n", "E = mc^2", r"\sum_{i}", "5", "2x", "f(x)"] {
        assert!(looks_like_inline_maths(content), "{content:?} is maths");
    }
    // A word in a script is a label (ui-preview5: the first was shown as source).
    for content in ["G = -L'(f(X_{batch}))", "X_{train}", "W^{out}h", "L_{total} = L_{ce} + L_{reg}", "x_batch", "p_{model}(x)"] {
        assert!(looks_like_inline_maths(content), "{content:?} is maths");
    }
    // A word outside a script is still prose, even next to one.
    for content in ["5 and x_{i}", "x_{i} and y"] {
        assert!(!looks_like_inline_maths(content), "{content:?} would be read as maths");
    }
    // Minus, brackets, commas, primes and factorials are maths with no other signal.
    for content in ["n-1", "t-1", "x-y", "-1", "[0,1]", "x,y", "f'", "n!"] {
        assert!(looks_like_inline_maths(content), "{content:?} is maths");
    }
    // ...and the prices they could be mistaken for are not.
    for content in ["5, ", "5,", "5,000-", "1,000 and ", "5! Then ", "5-10 vs "] {
        assert!(!looks_like_inline_maths(content), "{content:?} would be read as maths");
    }
    // A command settles a padded span; padding without one is still a price.
    for content in [r" \alpha ", r" \frac{a}{b} ", r"x = \beta "] {
        assert!(looks_like_inline_maths(content), "{content:?} is maths");
    }
    for content in [" 5 ", " x "] {
        assert!(!looks_like_inline_maths(content), "{content:?} would be read as maths");
    }
}

#[test]
fn an_unclosed_dollar_is_held_only_while_it_could_still_be_maths() {
    for partial in ["", "5", "x", r"\fra", "x + ", "5 ar", "G = -L'(f(X_{bat", "X_{batch", " ", r" \", r" \fr", r" \frac{1}{2} "] {
        assert!(could_become_inline_maths(partial), "{partial:?}");
    }
    for partial in [" 5", "5 and", "10 with tax", "x\ny"] {
        assert!(!could_become_inline_maths(partial), "{partial:?}");
    }
}

#[test]
fn the_reserved_height_of_a_partial_display_matches_the_finished_one() {
    let full = r"\begin{aligned} a &= \frac{1}{2} \\ b &= c \end{aligned}";
    let partial = &full[..full.len() - r"\end{aligned}".len()];
    assert_eq!(reserve_rows(partial, 80), display(full).rows.len());
}

#[test]
#[ignore]
fn eyeball() {
    for s in [
        r"\nabla_\theta J(\theta) = \mathbb{E}_{\pi_\theta}\left[ \nabla_\theta \log \pi_\theta(a \mid s) \, Q(s, a) \right]",
        r"\boxed{E = mc^2}",
        r"\left\{ x \in \mathbb{R} \;\middle|\; x^2 < 2 \right\}",
        r"a \xrightarrow{f} b",
        r"\overline{AB} = \sqrt[n]{\frac{a}{b}}",
    ] {
        let f = display(s);
        println!("{s}\n  [{:?} / inline: {}]\n{}\n", f.fidelity, one(s), show(s));
    }
}
