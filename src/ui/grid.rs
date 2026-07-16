//! The column-layout engine every aligned table renders through, plus the
//! rendering primitives it is built from ([`Width`], [`Cell`], [`Paint`],
//! [`Table`]).
//!
//! A [`Grid`] owns the layout conventions no call site may re-implement
//! (the drift this module exists to end — each renderer used to hand-roll
//! its own width math):
//!
//! - cells are padded by **visible** width ([`Cell`] carries it), never byte
//!   length, so embedded ANSI escapes don't skew columns;
//! - two blank columns of gutter between adjacent columns;
//! - [`Align::Left`] pads after the text, [`Align::Right`] before it;
//! - a rendered line never carries trailing whitespace.
//!
//! Surfaces build their rows as plain `Vec<Cell>` (no extractor closures —
//! at a handful of columns, data beats indirection) and compose complex
//! tables from several grids plus literal [`Table`] lines (section markers,
//! totals). Columns shared *across* sections (the change-set's size/time
//! columns spanning roots and deps) are expressed with [`Col::min`]: measure
//! the union once, feed both grids the result as a floor.

use super::color_on;

use std::ops::Add;

/// A terminal column width, measured in display cells.
///
/// A newtype over `usize` (not a bare integer) so a width can't be confused
/// with a row index or a package count, and so the pad-on-*visible*-width policy
/// — cells are padded by char count, never byte length, so embedded ANSI
/// escapes don't skew alignment — lives in one place.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Width(usize);

impl Width {
    pub(super) const ZERO: Self = Self(0);

    /// Visible width of a plain (un-colored) cell. Table cells are ASCII
    /// (names, versions, repo labels, sizes), so char count == display columns.
    pub fn of(s: &str) -> Self {
        Self(s.chars().count())
    }

    /// The widest of a set of cell widths — i.e. the column width. [`Self::ZERO`]
    /// when the iterator is empty.
    pub(super) fn widest(widths: impl Iterator<Item = Self>) -> Self {
        widths.max().unwrap_or(Self::ZERO)
    }

    /// `self` blank cells — the filler for a fixed-width column with no content.
    pub(super) fn blanks(self) -> String {
        " ".repeat(self.0)
    }

    /// The padding string needed to widen a cell of visible width `inner` to
    /// `self` (empty when `inner` already meets or exceeds `self`).
    pub(super) fn gap(self, inner: Self) -> String {
        " ".repeat(self.0.saturating_sub(inner.0))
    }
}

impl Add for Width {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

/// A rendered table cell: its (possibly ANSI-colored) display text plus the
/// visible width of the *plain* text.
///
/// The domain type for a single aligned cell — instead of passing a
/// `(plain, rendered)` pair around, the cell carries its own visible width so
/// the grid aligns correctly even when the text holds color escapes.
pub struct Cell {
    text: String,
    width: Width,
}

impl Cell {
    /// A plain (uncolored) cell; its width is its visible char count.
    pub fn plain(s: impl Into<String>) -> Self {
        let text = s.into();
        let width = Width::of(&text);
        Self { text, width }
    }

    /// A cell rendered from `plain`: `f` paints it only when `paint` is colored,
    /// and the cell remembers `plain`'s visible width either way.
    pub(super) fn paint(plain: &str, paint: Paint, f: impl FnOnce(&str) -> String) -> Self {
        Self {
            width: Width::of(plain),
            text: if paint.colored() {
                f(plain)
            } else {
                plain.to_owned()
            },
        }
    }

    /// A pre-rendered composite cell whose visible width the caller already
    /// knows — the version block, which aligns its old/arrow/new parts
    /// internally and hands the grid one fixed-width cell.
    pub(super) const fn sized(text: String, width: Width) -> Self {
        Self { text, width }
    }

    /// The cell's visible width — what column widths are measured from.
    pub(super) const fn width(&self) -> Width {
        self.width
    }
}

/// The rendered lines of a table, top to bottom.
///
/// A domain newtype over the raw `Vec<String>` so rendered table output isn't
/// passed around as an anonymous string list (which could be confused with, say,
/// a list of package names), and so "how a table is emitted" lives behind one
/// type. The shell prints each [`Self::lines`] entry; tests assert over them.
pub struct Table(Vec<String>);

impl Default for Table {
    fn default() -> Self {
        Self::new()
    }
}

impl Table {
    /// An empty table to build up with [`Self::push`] / [`Self::append`].
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// Append one rendered line.
    pub fn push(&mut self, line: String) {
        self.0.push(line);
    }

    /// Append another table's lines, consuming it. Lets a renderer assemble a
    /// table from sub-sections (root rows, the deps block, removals) without
    /// dropping to a bare `Vec<String>`.
    pub fn append(&mut self, other: Self) {
        self.0.extend(other.0);
    }

    /// The display lines, top to bottom.
    pub fn lines(&self) -> &[String] {
        &self.0
    }

    /// The same lines bottom-to-top — for a caller whose presentation order
    /// reverses the data order (the shell prints search results worst-first
    /// so the best rows land next to the prompt, while row numbers keep
    /// keying the best-first list).
    #[must_use]
    pub fn reversed(mut self) -> Self {
        self.0.reverse();
        self
    }

    /// Print the table to stderr framed by blank lines — the flag-path
    /// emission convention (`-Qu`, the `-S` plan tables). A no-op when the
    /// table is empty, so callers never emit a stray empty frame. The shell
    /// path doesn't use this — it routes [`Self::lines`] through its
    /// `ShellEnv::print` seam to stdout.
    pub fn eprint_framed(&self) {
        if self.is_empty() {
            return;
        }
        eprintln!();
        for line in &self.0 {
            eprintln!("{line}");
        }
        eprintln!();
    }

    /// Whether the render produced no lines (e.g. an empty change set).
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Whether a rendered row carries ANSI color.
///
/// An explicit per-render argument rather than a re-read of [`color_on`], so
/// every renderer can produce a plain form (for width measurement and tests)
/// and a colored form from the same code path — and so tests pin
/// [`Paint::Plain`] instead of inheriting whatever the ambient terminal
/// supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paint {
    Plain,
    Colored,
}

impl Paint {
    pub(super) const fn colored(self) -> bool {
        matches!(self, Self::Colored)
    }

    /// The ambient paint for the process's terminal ([`color_on`]).
    pub fn detect() -> Self {
        Self::from(color_on())
    }

    /// The visible width of the version separator this paint renders: the
    /// arrow glyph flanked by one space each side. Paint-dependent because
    /// the glyph is — colored draws the one-column `→`, plain falls back to
    /// the two-column ASCII `->` (piped / `NO_COLOR` output stays ASCII). A
    /// method rather than a const so the blank-gap math on arrowless rows is
    /// derived from the same paint as the rendered separator and the two
    /// can't drift apart.
    pub(super) const fn arrow(self) -> Width {
        match self {
            Self::Plain => Width(4),
            Self::Colored => Width(3),
        }
    }
}

impl From<bool> for Paint {
    fn from(colored: bool) -> Self {
        if colored { Self::Colored } else { Self::Plain }
    }
}

/// A grid cell's alignment within its measured column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Align {
    Left,
    Right,
}

/// One column's layout spec: its alignment plus a width floor.
///
/// The floor (default zero) is how columns shared across grid *sections*
/// stay aligned: measure the union of both sections' cells once, hand each
/// section's grid the result via [`Self::min`] — the union max is ≥ each
/// section's own max, so the floor *is* the shared width.
#[derive(Clone, Copy)]
pub struct Col {
    align: Align,
    min: Width,
}

impl Col {
    /// A left-aligned column (names, labels).
    pub const fn left() -> Self {
        Self {
            align: Align::Left,
            min: Width::ZERO,
        }
    }

    /// A right-aligned column (numbers, sizes, durations).
    pub const fn right() -> Self {
        Self {
            align: Align::Right,
            min: Width::ZERO,
        }
    }

    /// This column with a width floor — for widths shared across sections or
    /// pinned to a widest-possible label.
    #[must_use]
    pub const fn min(self, min: Width) -> Self {
        Self { min, ..self }
    }
}

/// One grid row: its aligned cells plus an optional verbatim tail appended
/// after the last column.
///
/// The tail carries the unaligned appendices (the `built` tag, the
/// `(3d ago)` age, a description), each with its own leading gap, that
/// ride behind the aligned block without perturbing column math.
pub struct GridRow {
    cells: Vec<Cell>,
    tail: String,
}

impl GridRow {
    /// A row from its aligned cells, one per grid column.
    pub const fn new(cells: Vec<Cell>) -> Self {
        Self {
            cells,
            tail: String::new(),
        }
    }

    /// Attach the unaligned tail (must carry its own leading gap).
    #[must_use]
    pub fn tail(mut self, tail: impl Into<String>) -> Self {
        self.tail = tail.into();
        self
    }
}

/// The column-layout engine: measures each column over all rows (the max of
/// the column's floor and its cells' visible widths), then renders the rows
/// as aligned lines under the module-doc conventions.
pub struct Grid {
    cols: Vec<Col>,
    rows: Vec<GridRow>,
    indent: &'static str,
}

impl Grid {
    /// An empty grid over the given column specs.
    pub const fn new(cols: Vec<Col>) -> Self {
        Self {
            cols,
            rows: Vec::new(),
            indent: "",
        }
    }

    /// Prefix every rendered line with `prefix` (the table's left margin —
    /// `"    "` for the flag tables, `"        "` for the dep block).
    #[must_use]
    pub const fn indent(mut self, prefix: &'static str) -> Self {
        self.indent = prefix;
        self
    }

    /// Append one row. Panics when the row's cell count doesn't match the
    /// column specs — a row/spec mismatch is a bug at the call site, not a
    /// renderable state.
    pub fn push(&mut self, row: GridRow) {
        assert_eq!(
            row.cells.len(),
            self.cols.len(),
            "grid row has {} cells for {} columns",
            row.cells.len(),
            self.cols.len(),
        );
        self.rows.push(row);
    }

    /// Lay the rows out into aligned lines.
    pub fn render(self) -> Table {
        let widths: Vec<Width> = self
            .cols
            .iter()
            .enumerate()
            .map(|(i, col)| {
                Width::widest(
                    std::iter::once(col.min).chain(self.rows.iter().map(|r| r.cells[i].width)),
                )
            })
            .collect();

        let mut out = Table::new();
        for row in self.rows {
            let mut line = String::from(self.indent);
            for (i, ((cell, col), width)) in row
                .cells
                .into_iter()
                .zip(&self.cols)
                .zip(&widths)
                .enumerate()
            {
                if i > 0 {
                    line.push_str("  ");
                }
                let pad = width.gap(cell.width);
                match col.align {
                    Align::Left => {
                        line.push_str(&cell.text);
                        line.push_str(&pad);
                    }
                    Align::Right => {
                        line.push_str(&pad);
                        line.push_str(&cell.text);
                    }
                }
            }
            // The tail hugs the aligned block's right edge (its own leading
            // gap is the spacing); the final trim is the no-trailing-
            // whitespace rule for tail-less rows and empty last columns.
            line.push_str(&row.tail);
            line.truncate(line.trim_end_matches(' ').len());
            out.push(line);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid2(a: Col, b: Col, rows: &[(&str, &str)]) -> Vec<String> {
        let mut g = Grid::new(vec![a, b]);
        for (x, y) in rows {
            g.push(GridRow::new(vec![Cell::plain(*x), Cell::plain(*y)]));
        }
        g.render().0
    }

    /// Left columns pad after the text, right columns before it; the gutter
    /// is two blanks; the last column never emits trailing pad.
    #[test]
    fn aligns_left_and_right() {
        let lines = grid2(
            Col::left(),
            Col::right(),
            &[("short", "1"), ("much-longer", "100")],
        );
        assert_eq!(lines, ["short          1", "much-longer  100"]);
    }

    /// A column floor below the measured width is a no-op; above it, it wins.
    #[test]
    fn min_is_a_floor() {
        let lines = grid2(
            Col::left().min(Width::of("wide-floor")),
            Col::left(),
            &[("ab", "x")],
        );
        assert_eq!(lines, ["ab          x"]);
        let lines = grid2(Col::left().min(Width::of("a")), Col::left(), &[("ab", "x")]);
        assert_eq!(lines, ["ab  x"]);
    }

    /// A colored cell pads by its *plain* width, so ANSI escapes never skew
    /// the next column.
    #[test]
    fn colored_cell_does_not_skew_columns() {
        let mut g = Grid::new(vec![Col::left(), Col::left()]);
        g.push(GridRow::new(vec![
            Cell::paint("red", Paint::Colored, |s| {
                console::style(s).red().force_styling(true).to_string()
            }),
            Cell::plain("next"),
        ]));
        g.push(GridRow::new(vec![Cell::plain("wide"), Cell::plain("next")]));
        let lines = g.render().0;
        let stripped = console::strip_ansi_codes(&lines[0]).to_string();
        assert_eq!(stripped, "red   next");
        assert_eq!(lines[1], "wide  next");
    }

    /// The tail rides behind the full aligned block — an empty last cell still
    /// holds its column open so tails line up across rows.
    #[test]
    fn tail_follows_the_aligned_block() {
        let mut g = Grid::new(vec![Col::left(), Col::right()]);
        g.push(GridRow::new(vec![Cell::plain("name"), Cell::plain("9s")]).tail("  built"));
        g.push(GridRow::new(vec![Cell::plain("longer-name"), Cell::plain("")]).tail("  built"));
        let lines = g.render().0;
        assert_eq!(lines[0], "name         9s  built");
        assert_eq!(lines[1], "longer-name      built");
    }

    /// No line ever ends in whitespace — a trailing empty right-aligned
    /// column and a padded left column both trim away.
    #[test]
    fn never_emits_trailing_whitespace() {
        let mut g = Grid::new(vec![Col::left(), Col::right()]);
        g.push(GridRow::new(vec![Cell::plain("a"), Cell::plain("1")]));
        g.push(GridRow::new(vec![Cell::plain("bb"), Cell::plain("")]));
        for line in g.render().0 {
            assert_eq!(line, line.trim_end(), "trailing whitespace in {line:?}");
        }
    }

    /// A column with no content anywhere collapses to width zero — it costs
    /// its gutter only, matching the old fixed format strings.
    #[test]
    fn zero_width_column_collapses() {
        let mut g = Grid::new(vec![Col::left(), Col::left(), Col::left()]);
        g.push(GridRow::new(vec![
            Cell::plain("a"),
            Cell::plain(""),
            Cell::plain("z"),
        ]));
        assert_eq!(g.render().0, ["a    z"]);
    }

    /// The indent prefixes every line; an empty grid renders no lines.
    #[test]
    fn indent_and_empty_grid() {
        let mut g = Grid::new(vec![Col::left()]).indent("    ");
        g.push(GridRow::new(vec![Cell::plain("x")]));
        assert_eq!(g.render().0, ["    x"]);
        assert!(Grid::new(vec![Col::left()]).render().is_empty());
    }

    /// A row whose cell count doesn't match the specs is a call-site bug.
    #[test]
    #[should_panic(expected = "grid row has 1 cells for 2 columns")]
    fn row_column_mismatch_panics() {
        let mut g = Grid::new(vec![Col::left(), Col::left()]);
        g.push(GridRow::new(vec![Cell::plain("only-one")]));
    }
}
