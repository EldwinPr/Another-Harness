//! The web page that shows the database (issue 32): every table with its columns, and every
//! relationship drawn in crow's foot notation.
//!
//! Everything is read from the database file in use (`sqlite_master` and the `pragma_*` functions),
//! so the page cannot drift from the real schema. Only table and column names, their types and the
//! number of rows are read: never the contents of a row. A relationship is a declared foreign key;
//! a link that is only a convention (a text column that names another row) is not shown.
//!
//! Notation (the end of a line at a box says how many rows of that table take part):
//! - `||` exactly one: the parent end when every column of the key is `NOT NULL`;
//! - `|o` zero or one: the parent end when the key may be empty;
//! - crow's foot with `o` zero or many: the child end;
//! - `o|` zero or one: the child end when the key is also unique (a one-to-one link).

use rusqlite::Connection;

use crate::store::StoreError;
use crate::web_pages::escape;

/// How many rows of a table take part at one end of a relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality {
    ExactlyOne,
    ZeroOrOne,
    ZeroOrMany,
}

impl Cardinality {
    /// The notation in words, for the sentence under a drawing.
    pub fn words(self) -> &'static str {
        match self {
            Self::ExactlyOne => "exactly one",
            Self::ZeroOrOne => "zero or one",
            Self::ZeroOrMany => "zero or many",
        }
    }

    /// The same in the characters ERDs are usually written with.
    pub fn symbol(self, parent_side: bool) -> &'static str {
        match (self, parent_side) {
            (Self::ExactlyOne, _) => "||",
            (Self::ZeroOrOne, true) => "|o",
            (Self::ZeroOrOne, false) => "o|",
            (Self::ZeroOrMany, true) => "}o",
            (Self::ZeroOrMany, false) => "o{",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub declared_type: String,
    pub not_null: bool,
    pub primary: bool,
    pub foreign: bool,
    pub unique: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    pub rows: i64,
}

/// One foreign key: `child` points at `parent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub parent: String,
    pub child: String,
    /// The columns in the child, and the columns they point at.
    pub child_columns: Vec<String>,
    pub parent_columns: Vec<String>,
    /// How many parent rows a child row has (`ExactlyOne`, or `ZeroOrOne` when the key may be empty).
    pub parent_end: Cardinality,
    /// How many child rows a parent row has (`ZeroOrMany`, or `ZeroOrOne` for a one-to-one link).
    pub child_end: Cardinality,
    pub on_delete: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Schema {
    pub tables: Vec<Table>,
    pub relations: Vec<Relation>,
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Reads the tables, columns, row counts and foreign keys of `conn`.
pub fn load(conn: &Connection) -> Result<Schema, StoreError> {
    let names: Vec<String> = {
        let mut statement = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut schema = Schema::default();
    for name in &names {
        let table = quote(name);
        // cid, name, type, notnull, dflt_value, pk
        let mut columns: Vec<Column> = {
            let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
            let rows = statement.query_map([], |row| {
                Ok(Column {
                    name: row.get(1)?,
                    declared_type: row.get(2)?,
                    not_null: row.get::<_, i64>(3)? != 0,
                    primary: row.get::<_, i64>(5)? != 0,
                    foreign: false,
                    unique: false,
                })
            })?;
            rows.collect::<Result<_, _>>()?
        };
        let primary: Vec<String> = columns
            .iter()
            .filter(|c| c.primary)
            .map(|c| c.name.clone())
            .collect();
        // The sets of columns each unique index covers.
        let mut unique_sets: Vec<Vec<String>> = Vec::new();
        {
            let mut statement = conn.prepare(&format!("PRAGMA index_list({table})"))?;
            let indexes: Vec<(String, bool)> = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)? != 0))
                })?
                .collect::<Result<_, _>>()?;
            for (index, _) in indexes.into_iter().filter(|(_, unique)| *unique) {
                let mut info = conn.prepare(&format!("PRAGMA index_info({})", quote(&index)))?;
                let set: Vec<String> = info
                    .query_map([], |row| row.get::<_, String>(2))?
                    .collect::<Result<_, _>>()?;
                unique_sets.push(set);
            }
        }
        if !primary.is_empty() {
            unique_sets.push(primary.clone());
        }
        for column in &mut columns {
            column.unique = unique_sets
                .iter()
                .any(|set| set.as_slice() == std::slice::from_ref(&column.name));
        }
        // id, seq, table, from, to, on_update, on_delete, match
        let mut keys: Vec<(i64, i64, String, String, Option<String>, String)> = {
            let mut statement = conn.prepare(&format!("PRAGMA foreign_key_list({table})"))?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(6)?,
                ))
            })?;
            rows.collect::<Result<_, _>>()?
        };
        keys.sort_by_key(|k| (k.0, k.1));
        let mut at = 0;
        while at < keys.len() {
            let id = keys[at].0;
            let group: Vec<_> = keys[at..].iter().take_while(|k| k.0 == id).collect();
            at += group.len();
            let child_columns: Vec<String> = group.iter().map(|k| k.3.clone()).collect();
            let parent_columns: Vec<String> = group.iter().filter_map(|k| k.4.clone()).collect();
            let optional = child_columns.iter().any(|c| {
                columns
                    .iter()
                    .find(|col| &col.name == c)
                    .is_some_and(|col| !col.not_null && !col.primary)
            });
            let one_to_one = unique_sets.iter().any(|set| {
                set.len() == child_columns.len() && set.iter().all(|c| child_columns.contains(c))
            });
            for column in &mut columns {
                if child_columns.contains(&column.name) {
                    column.foreign = true;
                }
            }
            schema.relations.push(Relation {
                parent: group[0].2.clone(),
                child: name.clone(),
                child_columns,
                parent_columns,
                parent_end: if optional {
                    Cardinality::ZeroOrOne
                } else {
                    Cardinality::ExactlyOne
                },
                child_end: if one_to_one {
                    Cardinality::ZeroOrOne
                } else {
                    Cardinality::ZeroOrMany
                },
                on_delete: group[0].5.clone(),
            });
        }
        let rows: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })?;
        schema.tables.push(Table {
            name: name.clone(),
            columns,
            rows,
        });
    }
    schema
        .relations
        .sort_by(|a, b| (&a.parent, &a.child).cmp(&(&b.parent, &b.child)));
    Ok(schema)
}

const BOX_W: i32 = 190;
const WIDTH: i32 = 640;
const HEIGHT: i32 = 84;
const MID: i32 = 34;

/// The end of a line at a box edge `x`, at height `y`, drawn away from the box in direction `dir`
/// (+1 right, -1 left).
fn end_marks(x: i32, y: i32, dir: i32, kind: Cardinality) -> String {
    let at = |d: i32| x + dir * d;
    let bar = |d: i32| {
        format!(
            "<line x1=\"{0}\" y1=\"{1}\" x2=\"{0}\" y2=\"{2}\"/>",
            at(d),
            y - 9,
            y + 9
        )
    };
    let circle = |d: i32| {
        format!(
            "<circle cx=\"{}\" cy=\"{y}\" r=\"5\" style=\"fill:var(--card)\"/>",
            at(d)
        )
    };
    match kind {
        Cardinality::ExactlyOne => format!("{}{}", bar(12), bar(19)),
        Cardinality::ZeroOrOne => format!("{}{}", bar(12), circle(24)),
        Cardinality::ZeroOrMany => format!(
            "<line x1=\"{x1}\" y1=\"{y}\" x2=\"{x0}\" y2=\"{top}\"/>\
             <line x1=\"{x1}\" y1=\"{y}\" x2=\"{x0}\" y2=\"{y}\"/>\
             <line x1=\"{x1}\" y1=\"{y}\" x2=\"{x0}\" y2=\"{bottom}\"/>{}",
            circle(25),
            x0 = at(0),
            x1 = at(15),
            top = y - 10,
            bottom = y + 10
        ),
    }
}

/// One relationship as a small SVG: the parent box, a line, the child box, and the notation at
/// each end. The colours come from the page's variables, so it follows light and dark.
pub fn relation_svg(relation: &Relation) -> String {
    let left = 4;
    let right = WIDTH - 4 - BOX_W;
    let (line_from, line_to) = (left + BOX_W, right);
    let label = format!(
        "{} \u{2192} {}{}",
        relation.child_columns.join(", "),
        relation.parent_columns.join(", "),
        if relation.on_delete.is_empty() || relation.on_delete == "NO ACTION" {
            String::new()
        } else {
            format!("   ON DELETE {}", relation.on_delete)
        }
    );
    let boxed = |x: i32, name: &str| {
        format!(
            "<a href=\"#t-{0}\"><rect x=\"{x}\" y=\"{y}\" width=\"{BOX_W}\" height=\"34\" rx=\"4\" \
             style=\"fill:var(--card);stroke:var(--accent)\"/>\
             <text x=\"{cx}\" y=\"{ty}\" text-anchor=\"middle\" font-family=\"ui-monospace,monospace\" \
             font-size=\"14\" style=\"fill:var(--fg);stroke:none\">{1}</text></a>",
            escape(name),
            escape(name),
            y = MID - 17,
            cx = x + BOX_W / 2,
            ty = MID + 5
        )
    };
    format!(
        "<svg viewBox=\"0 0 {WIDTH} {HEIGHT}\" width=\"100%\" role=\"img\" aria-label=\"{aria}\" \
         style=\"max-width:{WIDTH}px;display:block\"><title>{aria}</title>\
         <g stroke=\"currentColor\" stroke-width=\"1.5\" fill=\"none\">\
         <line x1=\"{line_from}\" y1=\"{MID}\" x2=\"{line_to}\" y2=\"{MID}\"/>{parent_end}{child_end}</g>\
         {parent_box}{child_box}\
         <text x=\"{cx}\" y=\"{ly}\" text-anchor=\"middle\" font-size=\"12\" style=\"fill:var(--dim)\">{label}</text></svg>",
        aria = escape(&sentence(relation)),
        parent_end = end_marks(line_from, MID, 1, relation.parent_end),
        child_end = end_marks(line_to, MID, -1, relation.child_end),
        parent_box = boxed(left, &relation.parent),
        child_box = boxed(right, &relation.child),
        cx = WIDTH / 2,
        ly = MID + 32,
        label = escape(&label),
    )
}

/// The relationship in a sentence, for the people who cannot see the drawing and as a check on it.
pub fn sentence(relation: &Relation) -> String {
    let child_has = match relation.child_end {
        Cardinality::ZeroOrOne => "at most one row",
        _ => "zero or many rows",
    };
    let parent_has = match relation.parent_end {
        Cardinality::ExactlyOne => "exactly one row",
        _ => "zero or one row",
    };
    format!(
        "A row of {} has {child_has} of {}; a row of {} has {parent_has} of {}.",
        relation.parent, relation.child, relation.child, relation.parent
    )
}

/// The legend: the three ends, each with its meaning.
fn legend() -> String {
    let sample = |kind: Cardinality, x: i32, text: &str| {
        let (dir, edge) = if kind == Cardinality::ZeroOrMany {
            (-1, x + 60)
        } else {
            (1, x)
        };
        format!(
            "<g stroke=\"currentColor\" stroke-width=\"1.5\" fill=\"none\">\
             <line x1=\"{x}\" y1=\"{MID}\" x2=\"{}\" y2=\"{MID}\"/>{}</g>\
             <text x=\"{}\" y=\"{}\" text-anchor=\"middle\" font-size=\"12\" style=\"fill:var(--fg)\">{text}</text>",
            x + 60,
            end_marks(edge, MID, dir, kind),
            x + 30,
            MID + 26
        )
    };
    format!(
        "<svg viewBox=\"0 0 640 84\" width=\"100%\" role=\"img\" aria-label=\"The ends of a line: exactly one, zero or one, zero or many\" \
         style=\"max-width:640px;display:block\">{}{}{}</svg>",
        sample(Cardinality::ExactlyOne, 20, "exactly one"),
        sample(Cardinality::ZeroOrOne, 240, "zero or one"),
        sample(Cardinality::ZeroOrMany, 460, "zero or many"),
    )
}

const ROW_H: i32 = 18;
const HEAD_H: i32 = 28;
const MARGIN: i32 = 16;
const GAP_X: i32 = 120;
const GAP_Y: i32 = 26;
/// How far a line from a table to itself (`sessions.parent_id`) goes out to the right of the box.
const SELF_LOOP: i32 = 22;

/// Where one table's box goes in the ERD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub table: usize,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// The column of boxes it is in (`None` for a table that stands alone).
    pub layer: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Layout {
    pub placed: Vec<Placed>,
    pub width: i32,
    pub height: i32,
}

fn box_height(table: &Table) -> i32 {
    HEAD_H + ROW_H * table.columns.len().max(1) as i32 + 6
}

/// How wide every box is: enough for the longest column name, its type and its marks.
fn box_width(schema: &Schema) -> i32 {
    let text = |n: usize, per: f32| (n as f32 * per).ceil() as i32;
    let need = schema
        .tables
        .iter()
        .flat_map(|t| {
            let head = 16 + text(t.name.chars().count(), 8.2) + 14 + text(11, 6.5);
            let rows = t
                .columns
                .iter()
                .map(|c| {
                    14 + text(c.name.chars().count(), 7.4)
                        + 14
                        + text(c.declared_type.chars().count(), 6.6)
                        + 54
                })
                .collect::<Vec<_>>();
            rows.into_iter().chain(std::iter::once(head))
        })
        .max()
        .unwrap_or(200);
    need.clamp(210, 360)
}

/// Lays the tables out in columns: a table sits one column to the right of the tables it points at,
/// so every line runs left to right; inside a column the order follows where its parents are, so
/// the lines cross as little as a single pass allows. Tables with no relationship go in rows below.
pub fn layout(schema: &Schema) -> Layout {
    let n = schema.tables.len();
    let index = |name: &str| schema.tables.iter().position(|t| t.name == name);
    let edges: Vec<(usize, usize)> = schema
        .relations
        .iter()
        .filter_map(|r| Some((index(&r.parent)?, index(&r.child)?)))
        .filter(|(p, c)| p != c)
        .collect();
    let mut layer = vec![0usize; n];
    for _ in 0..n {
        let mut changed = false;
        for &(p, c) in &edges {
            if layer[c] < layer[p] + 1 && layer[p] + 1 < n {
                layer[c] = layer[p] + 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let linked: Vec<bool> = (0..n)
        .map(|t| edges.iter().any(|&(p, c)| p == t || c == t))
        .collect();
    let w = box_width(schema);
    let mut placed: Vec<Placed> = Vec::with_capacity(n);
    let layers = (0..n)
        .filter(|&t| linked[t])
        .map(|t| layer[t])
        .max()
        .map_or(0, |m| m + 1);
    for l in 0..layers {
        let mut members: Vec<(f32, usize)> = (0..n)
            .filter(|&t| linked[t] && layer[t] == l)
            .map(|t| {
                let parents: Vec<f32> = edges
                    .iter()
                    .filter(|&&(_, c)| c == t)
                    .filter_map(|&(p, _)| placed.iter().find(|b| b.table == p))
                    .map(|b| (b.y + b.h / 2) as f32)
                    .collect();
                let desired = if parents.is_empty() {
                    f32::MAX
                } else {
                    parents.iter().sum::<f32>() / parents.len() as f32
                };
                (desired, t)
            })
            .collect();
        members.sort_by(|a, b| {
            a.0.total_cmp(&b.0)
                .then_with(|| schema.tables[a.1].name.cmp(&schema.tables[b.1].name))
        });
        let mut cursor = MARGIN;
        for (desired, t) in members {
            let h = box_height(&schema.tables[t]);
            let y = if desired == f32::MAX {
                cursor
            } else {
                cursor.max(desired as i32 - h / 2)
            };
            placed.push(Placed {
                table: t,
                x: MARGIN + l as i32 * (w + GAP_X),
                y,
                w,
                h,
                layer: Some(l),
            });
            cursor = y + h + GAP_Y;
        }
    }
    let layered_height = placed.iter().map(|b| b.y + b.h).max().unwrap_or(0);
    let layered_width = if layers == 0 {
        0
    } else {
        MARGIN * 2 + layers as i32 * w + (layers as i32 - 1) * GAP_X
    };
    let loops = schema.relations.iter().any(|r| r.parent == r.child);
    let width =
        layered_width.max(MARGIN * 2 + w * 2 + GAP_X) + if loops { SELF_LOOP + 8 } else { 0 };
    let mut x = MARGIN;
    let mut y = if layered_height == 0 {
        MARGIN + 20
    } else {
        layered_height + 2 * GAP_Y
    };
    let mut row_height = 0;
    for t in (0..n).filter(|&t| !linked[t]) {
        let h = box_height(&schema.tables[t]);
        if x + w + MARGIN > width && x > MARGIN {
            x = MARGIN;
            y += row_height + GAP_Y;
            row_height = 0;
        }
        placed.push(Placed {
            table: t,
            x,
            y,
            w,
            h,
            layer: None,
        });
        x += w + GAP_X / 2;
        row_height = row_height.max(h);
    }
    let height = placed.iter().map(|b| b.y + b.h).max().unwrap_or(0) + MARGIN;
    Layout {
        placed,
        width,
        height,
    }
}

/// The marks of a column, as shown at the right of its row: `PK`, `FK`, `UK`.
fn marks(column: &Column) -> String {
    let mut out = Vec::new();
    if column.primary {
        out.push("PK");
    }
    if column.foreign {
        out.push("FK");
    }
    if column.unique && !column.primary {
        out.push("UK");
    }
    out.join(" ")
}

/// The heights at which a line may cross column `layer` without touching a box: the middle of each
/// gap between two boxes, above the first and below the last.
fn lanes(layout: &Layout, layer: usize) -> Vec<i32> {
    let mut boxes: Vec<&Placed> = layout
        .placed
        .iter()
        .filter(|b| b.layer == Some(layer))
        .collect();
    boxes.sort_by_key(|b| b.y);
    let mut out = Vec::new();
    if let Some(first) = boxes.first() {
        out.push(first.y / 2);
    }
    for pair in boxes.windows(2) {
        out.push((pair[0].y + pair[0].h + pair[1].y) / 2);
    }
    if let Some(last) = boxes.last() {
        out.push(last.y + last.h + GAP_Y / 2);
    }
    out
}

/// The path of one line, and the `(column, height)` of each column it crosses on the way. A line
/// to the next column is one curve; a line that skips columns runs straight through the free gap
/// of each column it passes, so it never goes behind a box. `used` counts the lines already
/// sent through each lane, so lines sharing a lane are spread a few pixels apart.
fn edge_path(
    layout: &Layout,
    (x1, y1, from_layer): (i32, i32, usize),
    (x2, y2, to_layer): (i32, i32, usize),
    used: &mut std::collections::HashMap<(usize, i32), usize>,
) -> (String, Vec<(usize, i32)>) {
    let curve = |ax: i32, ay: i32, bx: i32, by: i32| {
        let mid = (ax + bx) / 2;
        format!("C{mid} {ay} {mid} {by} {bx} {by}")
    };
    let width = layout.placed.first().map_or(0, |b| b.w);
    let mut d = format!("M{x1} {y1}");
    let (mut cx, mut cy) = (x1, y1);
    let mut crossed = Vec::new();
    for layer in from_layer + 1..to_layer {
        let t = (layer - from_layer) as f32 / (to_layer - from_layer) as f32;
        let target = y1 + ((y2 - y1) as f32 * t) as i32;
        let free = lanes(layout, layer);
        let Some(&lane) = free.iter().min_by_key(|y| (**y - target).abs()) else {
            continue;
        };
        let n = used.entry((layer, lane)).or_insert(0);
        let spread = [0, 6, -6, 12, -12][*n % 5];
        *n += 1;
        let y = lane + spread;
        let left = MARGIN + layer as i32 * (width + GAP_X);
        d.push_str(&curve(cx, cy, left, y));
        d.push_str(&format!(" L{} {y}", left + width));
        (cx, cy) = (left + width, y);
        crossed.push((layer, y));
    }
    d.push_str(&curve(cx, cy, x2, y2));
    (d, crossed)
}

/// The whole schema as one entity-relationship diagram: a box per table (name, row count, every
/// column with its type and `PK`/`FK`/`UK`) and a line per foreign key, from the column that points
/// to the column it points at, with crow's foot notation at both ends.
pub fn erd_svg(schema: &Schema) -> String {
    let layout = layout(schema);
    let at = |name: &str| {
        let t = schema.tables.iter().position(|t| t.name == name)?;
        layout.placed.iter().find(|b| b.table == t)
    };
    let row_y = |b: &Placed, column: &str| {
        let i = schema.tables[b.table]
            .columns
            .iter()
            .position(|c| c.name == column)
            .unwrap_or(0);
        b.y + HEAD_H + ROW_H * i as i32 + ROW_H / 2
    };
    let mut lines = String::new();
    let mut ends = String::new();
    let mut used = std::collections::HashMap::new();
    for r in &schema.relations {
        let (Some(p), Some(c)) = (at(&r.parent), at(&r.child)) else {
            continue;
        };
        let parent_column = r.parent_columns.first().map_or_else(
            || {
                schema.tables[p.table]
                    .columns
                    .iter()
                    .find(|c| c.primary)
                    .map_or("", |c| c.name.as_str())
            },
            String::as_str,
        );
        let child_column = r.child_columns.first().map_or("", String::as_str);
        if p.table == c.table {
            // A table that points at itself: a loop on the right of its box, from the column
            // pointed at to the column that points.
            let (x, y1, y2) = (p.x + p.w, row_y(p, parent_column), row_y(c, child_column));
            let d = format!("M{x} {y1} H{} V{y2} H{x}", x + SELF_LOOP);
            lines.push_str(&format!(
                "<g class=\"edge\"><path d=\"{d}\" stroke=\"transparent\" stroke-width=\"10\"/>\
                 <path class=\"line\" d=\"{d}\" opacity=\".75\"><title>{}</title></path></g>",
                escape(&sentence(r))
            ));
            ends.push_str(&end_marks(x, y1, 1, r.parent_end));
            ends.push_str(&end_marks(x, y2, 1, r.child_end));
            continue;
        }
        let (Some(from_layer), Some(to_layer)) = (p.layer, c.layer) else {
            continue;
        };
        if from_layer >= to_layer {
            continue;
        }
        let (x1, y1) = (p.x + p.w, row_y(p, parent_column));
        let (x2, y2) = (c.x, row_y(c, child_column));
        let (d, _) = edge_path(&layout, (x1, y1, from_layer), (x2, y2, to_layer), &mut used);
        lines.push_str(&format!(
            "<g class=\"edge\"><path d=\"{d}\" stroke=\"transparent\" stroke-width=\"10\"/>\
             <path class=\"line\" d=\"{d}\" opacity=\".75\"><title>{}</title></path></g>",
            escape(&sentence(r))
        ));
        ends.push_str(&end_marks(x1, y1, 1, r.parent_end));
        ends.push_str(&end_marks(x2, y2, -1, r.child_end));
    }
    let mut boxes = String::new();
    for b in &layout.placed {
        let table = &schema.tables[b.table];
        let mut rows = String::new();
        for (i, column) in table.columns.iter().enumerate() {
            let y = b.y + HEAD_H + ROW_H * i as i32 + ROW_H / 2 + 4;
            rows.push_str(&format!(
                "<text x=\"{}\" y=\"{y}\" style=\"fill:var(--fg);font-weight:{}\">{}</text>\
                 <text x=\"{}\" y=\"{y}\" text-anchor=\"end\" style=\"fill:var(--dim)\">{}</text>\
                 <text x=\"{}\" y=\"{y}\" text-anchor=\"end\" style=\"fill:var(--accent);font-weight:600\">{}</text>",
                b.x + 10,
                if column.primary { 700 } else { 400 },
                escape(&column.name),
                b.x + b.w - 52,
                escape(&column.declared_type.to_lowercase()),
                b.x + b.w - 8,
                marks(column)
            ));
        }
        boxes.push_str(&format!(
            "<g id=\"t-{name}\"><rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" rx=\"5\" style=\"fill:var(--card);stroke:var(--accent)\"/>\
             <rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{HEAD_H}\" rx=\"5\" style=\"fill:var(--line);stroke:var(--accent)\"/>\
             <text x=\"{tx}\" y=\"{ty}\" font-size=\"14\" font-weight=\"700\" style=\"fill:var(--fg)\">{name}</text>\
             <text x=\"{rx}\" y=\"{ty}\" text-anchor=\"end\" font-size=\"11\" style=\"fill:var(--dim)\">{rows_n} row{plural}</text>{rows}</g>",
            name = escape(&table.name),
            x = b.x,
            y = b.y,
            w = b.w,
            h = b.h,
            tx = b.x + 10,
            rx = b.x + b.w - 8,
            ty = b.y + 19,
            rows_n = table.rows,
            plural = if table.rows == 1 { "" } else { "s" },
        ));
    }
    let standalone = layout.placed.iter().any(|b| b.layer.is_none());
    let caption = if standalone {
        let top = layout
            .placed
            .iter()
            .filter(|b| b.layer.is_none())
            .map(|b| b.y)
            .min()
            .unwrap_or(0);
        format!(
            "<text x=\"{MARGIN}\" y=\"{}\" font-size=\"12\" style=\"fill:var(--dim)\">Tables with no declared relationship</text>",
            top - 8
        )
    } else {
        String::new()
    };
    format!(
        "<div class=\"erd\"><svg viewBox=\"0 0 {w} {h}\" width=\"{w}\" height=\"{h}\" role=\"img\" \
         aria-label=\"Entity relationship diagram: {} tables, {} relationships\" \
         style=\"display:block;width:100%;height:auto;min-width:{min}px;max-width:{w}px\" font-family=\"ui-monospace,monospace\" font-size=\"12\">\
         <g stroke=\"currentColor\" stroke-width=\"1.4\" fill=\"none\">{lines}</g>\
         <g stroke=\"currentColor\" stroke-width=\"1.5\" fill=\"none\">{ends}</g>{caption}{boxes}</svg></div>",
        schema.tables.len(),
        schema.relations.len(),
        w = layout.width,
        h = layout.height,
        min = layout.width * 7 / 10,
    )
}

/// The body of the Database page.
pub fn page_body(schema: &Schema) -> String {
    let mut out = String::new();
    out.push_str(
        "<p class=\"dim\">Drawn from the database file in use: names, types and row counts only, never the rows. \
         A line is a declared foreign key, from the column that points to the column it points at; the end of a line at a box \
         says how many rows of that table take part. The diagram scrolls sideways when it is wider than the screen.</p>",
    );
    out.push_str(&legend());
    out.push_str(&erd_svg(schema));
    out.push_str(&format!(
        "<h2>Relationships in words <span class=\"dim\">({})</span></h2>",
        schema.relations.len()
    ));
    out.push_str("<ul>");
    for relation in &schema.relations {
        out.push_str(&format!(
            "<li><code>{}.{}</code> &rarr; <code>{}.{}</code>{}<br><span class=\"dim\">{}</span></li>",
            escape(&relation.child),
            escape(&relation.child_columns.join(", ")),
            escape(&relation.parent),
            escape(&relation.parent_columns.join(", ")),
            if relation.on_delete.is_empty() || relation.on_delete == "NO ACTION" {
                String::new()
            } else {
                format!(" <span class=\"dim\">ON DELETE {}</span>", escape(&relation.on_delete))
            },
            escape(&sentence(relation))
        ));
    }
    out.push_str("</ul>");
    out.push_str("<details><summary>Each relationship drawn on its own</summary>");
    for relation in &schema.relations {
        out.push_str(&format!(
            "<div class=\"relation\">{}</div>",
            relation_svg(relation)
        ));
    }
    out.push_str("</details>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(sql: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(sql).unwrap();
        conn
    }

    fn relation<'a>(schema: &'a Schema, parent: &str, child: &str) -> &'a Relation {
        schema
            .relations
            .iter()
            .find(|r| r.parent == parent && r.child == child)
            .unwrap_or_else(|| panic!("no relation {parent} -> {child}: {:?}", schema.relations))
    }

    #[test]
    fn a_required_key_is_exactly_one_and_the_child_end_is_many() {
        let schema = load(&conn(
            "CREATE TABLE a (id INTEGER PRIMARY KEY);
             CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER NOT NULL REFERENCES a (id) ON DELETE CASCADE);",
        ))
        .unwrap();
        let r = relation(&schema, "a", "b");
        assert_eq!(r.parent_end, Cardinality::ExactlyOne);
        assert_eq!(r.child_end, Cardinality::ZeroOrMany);
        assert_eq!(r.on_delete, "CASCADE");
        assert_eq!(
            (r.child_columns.clone(), r.parent_columns.clone()),
            (vec!["a_id".to_owned()], vec!["id".to_owned()])
        );
        let b = schema.tables.iter().find(|t| t.name == "b").unwrap();
        assert!(b.columns.iter().find(|c| c.name == "a_id").unwrap().foreign);
        assert!(b.columns[0].primary);
    }

    #[test]
    fn a_nullable_key_is_zero_or_one_at_the_parent_end() {
        let schema = load(&conn(
            "CREATE TABLE a (id INTEGER PRIMARY KEY);
             CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER REFERENCES a (id) ON DELETE SET NULL);",
        ))
        .unwrap();
        let r = relation(&schema, "a", "b");
        assert_eq!(r.parent_end, Cardinality::ZeroOrOne);
        assert_eq!(r.child_end, Cardinality::ZeroOrMany);
    }

    #[test]
    fn a_unique_key_is_a_one_to_one_link() {
        let schema = load(&conn(
            "CREATE TABLE a (id INTEGER PRIMARY KEY);
             CREATE TABLE b (a_id INTEGER NOT NULL UNIQUE REFERENCES a (id));
             CREATE TABLE c (a_id INTEGER PRIMARY KEY REFERENCES a (id));",
        ))
        .unwrap();
        assert_eq!(
            relation(&schema, "a", "b").child_end,
            Cardinality::ZeroOrOne
        );
        let c = relation(&schema, "a", "c");
        assert_eq!(
            c.child_end,
            Cardinality::ZeroOrOne,
            "a primary key that is the key"
        );
        assert_eq!(c.parent_end, Cardinality::ExactlyOne);
    }

    #[test]
    fn a_composite_key_is_one_relationship_and_a_link_table_has_two() {
        let schema = load(&conn(
            "CREATE TABLE a (x INTEGER, y INTEGER, PRIMARY KEY (x, y));
             CREATE TABLE b (id INTEGER PRIMARY KEY);
             CREATE TABLE link (ax INTEGER NOT NULL, ay INTEGER NOT NULL, b_id INTEGER NOT NULL REFERENCES b (id),
                                FOREIGN KEY (ax, ay) REFERENCES a (x, y), PRIMARY KEY (ax, ay, b_id));",
        ))
        .unwrap();
        assert_eq!(schema.relations.len(), 2);
        let r = relation(&schema, "a", "link");
        assert_eq!(r.child_columns, ["ax", "ay"]);
        assert_eq!(r.parent_columns, ["x", "y"]);
        assert_eq!(
            r.child_end,
            Cardinality::ZeroOrMany,
            "the key is only part of link's own key"
        );
        assert_eq!(
            relation(&schema, "b", "link").child_end,
            Cardinality::ZeroOrMany
        );
    }

    #[test]
    fn row_counts_and_standing_alone_are_shown_and_only_names_are_read() {
        let conn = conn(
            "CREATE TABLE secrets (id INTEGER PRIMARY KEY, api_key TEXT);
             INSERT INTO secrets (api_key) VALUES ('sk-DISTINCT-KEY-12345'), ('sk-other');",
        );
        let schema = load(&conn).unwrap();
        assert_eq!(schema.tables[0].rows, 2);
        let html = page_body(&schema);
        assert!(
            html.contains("2 rows") && html.contains("Tables with no declared relationship"),
            "{html}"
        );
        assert!(html.contains("api_key"), "the column name is shown");
        assert!(!html.contains("sk-DISTINCT"), "{html}");
    }

    #[test]
    fn the_drawing_has_the_marks_of_each_end_and_escapes_names() {
        let required = Relation {
            parent: "a".into(),
            child: "b".into(),
            child_columns: vec!["a_id".into()],
            parent_columns: vec!["id".into()],
            parent_end: Cardinality::ExactlyOne,
            child_end: Cardinality::ZeroOrMany,
            on_delete: "CASCADE".into(),
        };
        let svg = relation_svg(&required);
        // Two bars at the parent end; three lines of the foot and a circle at the child end.
        assert_eq!(svg.matches("<circle").count(), 1, "{svg}");
        assert_eq!(svg.matches("<line").count(), 1 + 2 + 3, "{svg}");
        assert!(svg.contains("ON DELETE CASCADE"));
        let optional = Relation {
            parent_end: Cardinality::ZeroOrOne,
            child_end: Cardinality::ZeroOrOne,
            ..required.clone()
        };
        let svg = relation_svg(&optional);
        assert_eq!(svg.matches("<circle").count(), 2, "{svg}");
        assert_eq!(svg.matches("<line").count(), 1 + 1 + 1, "{svg}");
        let nasty = Relation {
            parent: "<script>".into(),
            child: "\"x\"".into(),
            ..required
        };
        let svg = relation_svg(&nasty);
        assert!(
            !svg.contains("<script>") && !svg.contains("\"x\"\""),
            "{svg}"
        );
        assert!(svg.contains("&lt;script&gt;"));
    }

    #[test]
    fn the_sentence_and_symbols_match_the_notation() {
        let r = Relation {
            parent: "profiles".into(),
            child: "profile_prompts".into(),
            child_columns: vec!["profile_id".into()],
            parent_columns: vec!["id".into()],
            parent_end: Cardinality::ExactlyOne,
            child_end: Cardinality::ZeroOrMany,
            on_delete: "CASCADE".into(),
        };
        assert_eq!(
            sentence(&r),
            "A row of profiles has zero or many rows of profile_prompts; a row of profile_prompts has exactly one row of profiles."
        );
        assert_eq!(r.parent_end.symbol(true), "||");
        assert_eq!(r.child_end.symbol(false), "o{");
        assert_eq!(Cardinality::ZeroOrOne.symbol(true), "|o");
    }

    fn overlap(a: &Placed, b: &Placed) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    #[test]
    fn a_table_sits_right_of_its_parents_and_no_boxes_overlap() {
        let schema = load(&conn(
            "CREATE TABLE a (id INTEGER PRIMARY KEY);
             CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER NOT NULL REFERENCES a (id));
             CREATE TABLE c (id INTEGER PRIMARY KEY, b_id INTEGER NOT NULL REFERENCES b (id), a_id INTEGER REFERENCES a (id));
             CREATE TABLE d (id INTEGER PRIMARY KEY, a_id INTEGER NOT NULL REFERENCES a (id));
             CREATE TABLE lonely (id INTEGER PRIMARY KEY, note TEXT);",
        ))
        .unwrap();
        let layout = layout(&schema);
        let place = |name: &str| {
            let t = schema.tables.iter().position(|t| t.name == name).unwrap();
            layout.placed.iter().find(|b| b.table == t).unwrap().clone()
        };
        let (a, b, c, d, lonely) = (
            place("a"),
            place("b"),
            place("c"),
            place("d"),
            place("lonely"),
        );
        assert_eq!(
            (a.layer, b.layer, c.layer, d.layer, lonely.layer),
            (Some(0), Some(1), Some(2), Some(1), None)
        );
        assert!(a.x < b.x && b.x < c.x && a.x < d.x);
        assert!(
            lonely.y > c.y + c.h,
            "stand-alone tables go below the diagram"
        );
        for (i, one) in layout.placed.iter().enumerate() {
            assert!(
                one.x >= 0
                    && one.y >= 0
                    && one.x + one.w <= layout.width
                    && one.y + one.h <= layout.height
            );
            for other in &layout.placed[i + 1..] {
                assert!(!overlap(one, other), "{one:?} overlaps {other:?}");
            }
        }
        assert_eq!(layout.placed.len(), 5);
    }

    #[test]
    fn the_erd_has_a_box_per_table_a_line_per_key_and_the_marks_of_each_column() {
        let schema = load(&conn(
            "CREATE TABLE a (id INTEGER PRIMARY KEY);
             CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER REFERENCES a (id), code TEXT UNIQUE);",
        ))
        .unwrap();
        let svg = erd_svg(&schema);
        assert_eq!(svg.matches("class=\"line\"").count(), 1, "{svg}");
        assert_eq!(svg.matches("<g id=\"t-").count(), 2);
        // A nullable key: |o at the parent end (bar and circle), a crow's foot with a circle at the child.
        assert_eq!(svg.matches("<circle").count(), 2, "{svg}");
        assert!(
            svg.contains(">PK<") && svg.contains(">FK<") && svg.contains(">UK<"),
            "{svg}"
        );
        assert!(
            svg.contains("aria-label=\"Entity relationship diagram: 2 tables, 1 relationships\"")
        );
        // The line leaves the box at the row of the column it points from, to the row it points at.
        let layout = layout(&schema);
        let a = layout
            .placed
            .iter()
            .find(|b| schema.tables[b.table].name == "a")
            .unwrap();
        let b = layout
            .placed
            .iter()
            .find(|b| schema.tables[b.table].name == "b")
            .unwrap();
        let from = format!("M{} {}", a.x + a.w, a.y + HEAD_H + ROW_H / 2);
        let to = format!("{} {}\"", b.x, b.y + HEAD_H + ROW_H + ROW_H / 2);
        assert!(
            svg.contains(&from) && svg.contains(&to),
            "{from} .. {to}: {svg}"
        );
    }

    #[test]
    fn the_erd_escapes_names_and_a_schema_with_no_tables_is_empty_but_valid() {
        let schema = load(&conn(
            r#"CREATE TABLE "<b>x</b>" (id INTEGER PRIMARY KEY, "<i>" TEXT);"#,
        ))
        .unwrap();
        let svg = erd_svg(&schema);
        assert!(!svg.contains("<b>x") && !svg.contains("<i>"), "{svg}");
        assert!(svg.contains("&lt;b&gt;x&lt;/b&gt;"));
        let empty = erd_svg(&Schema::default());
        assert!(empty.contains("0 tables, 0 relationships"), "{empty}");
        assert_eq!(layout(&Schema::default()).placed.len(), 0);
    }

    #[test]
    fn a_table_that_points_at_itself_gets_a_loop_inside_the_picture() {
        let schema = load(&conn(
            "CREATE TABLE node (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES node (id));
             CREATE TABLE leaf (id INTEGER PRIMARY KEY, node_id INTEGER NOT NULL REFERENCES node (id));",
        ))
        .unwrap();
        assert_eq!(schema.relations.len(), 2);
        let svg = erd_svg(&schema);
        assert_eq!(svg.matches("class=\"line\"").count(), 2, "{svg}");
        let l = layout(&schema);
        let node = l
            .placed
            .iter()
            .find(|b| schema.tables[b.table].name == "node")
            .unwrap();
        // The loop leaves the right edge of the box and stays inside the drawing.
        assert!(
            svg.contains(&format!("M{} ", node.x + node.w)),
            "the loop starts at the box: {svg}"
        );
        assert!(node.x + node.w + SELF_LOOP < l.width);
        // A table whose only relationship is with itself is drawn too.
        let alone = load(&conn(
            "CREATE TABLE tree (id INTEGER PRIMARY KEY, up INTEGER REFERENCES tree (id));",
        ))
        .unwrap();
        assert_eq!(erd_svg(&alone).matches("class=\"line\"").count(), 1);
    }

    #[test]
    fn a_line_that_skips_columns_runs_through_a_free_gap_never_behind_a_box() {
        let dir = crate::tools::testutil::Scratch::new("web-schema-lanes");
        let store = crate::store::Store::open(&dir.path().join("harness.db")).unwrap();
        let schema = load(&store.conn).unwrap();
        let layout = layout(&schema);
        let mut used = std::collections::HashMap::new();
        let mut skipping = 0;
        for r in &schema.relations {
            let find = |name: &str| {
                let t = schema.tables.iter().position(|t| t.name == name).unwrap();
                layout.placed.iter().find(|b| b.table == t).unwrap()
            };
            if r.parent == r.child {
                continue;
            }
            let (p, c) = (find(&r.parent), find(&r.child));
            let (Some(lp), Some(lc)) = (p.layer, c.layer) else {
                continue;
            };
            let (_, crossed) = edge_path(
                &layout,
                (p.x + p.w, p.y + 30, lp),
                (c.x, c.y + 30, lc),
                &mut used,
            );
            assert_eq!(crossed.len(), lc - lp - 1, "{r:?}");
            skipping += crossed.len();
            for (layer, y) in crossed {
                for b in layout.placed.iter().filter(|b| b.layer == Some(layer)) {
                    assert!(
                        y < b.y - 2 || y > b.y + b.h + 2,
                        "a line at {y} crosses {b:?}"
                    );
                }
            }
        }
        assert!(skipping > 0, "the real schema has lines that skip a column");
    }

    #[test]
    fn lanes_are_the_gaps_between_boxes_and_the_space_above_and_below() {
        let schema = load(&conn(
            "CREATE TABLE a (id INTEGER PRIMARY KEY);
             CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER REFERENCES a (id));
             CREATE TABLE c (id INTEGER PRIMARY KEY, a_id INTEGER REFERENCES a (id));",
        ))
        .unwrap();
        let layout = layout(&schema);
        let found = lanes(&layout, 1);
        assert_eq!(found.len(), 3, "above, between, below: {found:?}");
        assert!(found.windows(2).all(|w| w[0] < w[1]));
        assert!(lanes(&layout, 7).is_empty());
    }

    #[test]
    fn the_real_schema_is_drawn_with_every_foreign_key() {
        let dir = crate::tools::testutil::Scratch::new("web-schema-real");
        let store = crate::store::Store::open(&dir.path().join("harness.db")).unwrap();
        let schema = load(&store.conn).unwrap();
        let declared: i64 = schema
            .tables
            .iter()
            .map(|t| {
                store
                    .conn
                    .query_row(
                        &format!(
                            "SELECT COUNT(*) FROM pragma_foreign_key_list({})",
                            quote(&t.name)
                        ),
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap()
            })
            .sum();
        assert!(declared > 0);
        // A composite key would be one relationship made of several rows; this schema has none.
        assert_eq!(schema.relations.len() as i64, declared);
        for table in [
            "profiles",
            "profile_prompts",
            "profile_skills",
            "sessions",
            "calls",
        ] {
            assert!(schema.tables.iter().any(|t| t.name == table), "{table}");
        }
        let link = relation(&schema, "profiles", "profile_prompts");
        assert_eq!(link.on_delete, "CASCADE");
        let personality = relation(&schema, "prompts", "profiles");
        assert_eq!(personality.parent_end, Cardinality::ZeroOrOne);
        let html = page_body(&schema);
        assert!(html.contains("id=\"t-profiles\""));
        let svg = erd_svg(&schema);
        assert_eq!(
            svg.matches("class=\"line\"").count(),
            schema.relations.len(),
            "one line per foreign key"
        );
        let layout = layout(&schema);
        assert_eq!(layout.placed.len(), schema.tables.len());
        for (i, one) in layout.placed.iter().enumerate() {
            for other in &layout.placed[i + 1..] {
                assert!(!overlap(one, other), "{one:?} overlaps {other:?}");
            }
        }
    }
}
