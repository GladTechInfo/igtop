//! Rendering.
//!
//! Bars are drawn by hand rather than with the stock gauge widget for two
//! reasons: a label, bar and figure share one row, and each cell of the bar
//! gets its own colour from the ramp — a gauge paints one flat colour.

use crate::app::Gpu;
use crate::pmu::engine_label;
use crate::theme::*;
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Paragraph, Row, Sparkline, SparklineBar, Table,
};

/// Columns before the bar: indent, glyph, class, engine name.
const PREFIX_W: usize = 18;
/// Columns after the bar, right-aligned.
const VALUE_W: usize = 17;

pub struct View<'a> {
    pub gpus: &'a [Gpu],
    pub host: String,
    pub show_clients: bool,
    pub show_history: bool,
    pub smooth: bool,
    pub icons: bool,
    pub focus: Option<usize>,
    pub interval_ms: u64,
    pub paused: bool,
}

/// Compact byte formatting: the value column is narrow and `5.94 GiB` reads no
/// better than `5.94G` at a glance.
fn short_bytes(b: u64) -> String {
    const U: [(u64, &str); 4] =
        [(1 << 30, "G"), (1 << 20, "M"), (1 << 10, "K"), (1, "B")];
    for (div, suf) in U {
        if b >= div {
            let v = b as f64 / div as f64;
            return if v >= 100.0 {
                format!("{:.0}{}", v, suf)
            } else {
                format!("{:.2}{}", v, suf)
            };
        }
    }
    "0B".into()
}

/// A bar row: glyph, label, name, bar, right-aligned value.
struct BarRow<'a> {
    icon: &'static str,
    icon_colour: Color,
    class: &'a str,
    name: &'a str,
    pct: f64,
    value: String,
    value_colour: Color,
}

/// The ramp colour of every cell along a bar `width` cells long, worked out
/// once per frame and shared by every bar in it.
fn ramp_cells(width: usize) -> Vec<Color> {
    let span = (width.saturating_sub(1)).max(1) as f64;
    (0..width).map(|i| ramp(i as f64 / span)).collect()
}

/// Draws bar rows straight into the frame buffer, one per line of `area`.
///
/// Bars are written cell by cell rather than as one styled span per cell
/// through `Paragraph`, which built ~3,000 spans a frame on a 250-column pane,
/// each segmented into graphemes and measured for width, sixty times a second
/// while bars moved. Measured with `bench_frames` (12 rows, 248 wide):
/// 776 µs a frame by spans, 117 µs direct. A whole frame went from ~1.5 ms to
/// ~0.86 ms, of which ~0.47 ms is ratatui resetting and diffing the 15,000-cell
/// buffer — fixed, whatever is drawn. `direct_bars_match_spans` checks the
/// cells are identical to the old path's.
fn render_bar_rows(buf: &mut Buffer, area: Rect, rows: &[BarRow]) {
    let bw = bar_width(area);
    let colours = ramp_cells(bw);
    let right = area.right();
    let room = |x: u16| right.saturating_sub(x) as usize;
    for (row, r) in rows.iter().enumerate() {
        let y = area.y + row as u16;
        if y >= area.bottom() {
            break;
        }
        let x = area.x + 2;
        let (x, _) = buf.set_stringn(x, y, r.icon, room(x), Style::default().fg(r.icon_colour));
        let x = x.saturating_add(1);
        let (x, _) = buf.set_stringn(
            x,
            y,
            format!("{:<8}", r.class),
            room(x),
            Style::default().fg(LABEL).add_modifier(Modifier::BOLD),
        );
        let (x, _) = buf.set_stringn(x, y, format!("{:<6}", r.name), room(x), Style::default().fg(DIM));

        // Coloured per cell from the ramp, so magnitude reads as hue as well
        // as length; the tip cell is an eighth-block for sub-cell resolution.
        let filled = (r.pct / 100.0 * bw as f64 * 8.0).round().max(0.0) as usize;
        let full = (filled / 8).min(bw);
        let rem = filled % 8;
        for (i, &colour) in colours.iter().enumerate().take(room(x)) {
            let (sym, fg) = if i < full {
                ("█", colour)
            } else if i == full && rem > 0 {
                (EIGHTHS[rem - 1], colour)
            } else {
                (TRACK_CH, TRACK)
            };
            if let Some(c) = buf.cell_mut((x + i as u16, y)) {
                c.set_symbol(sym).set_fg(fg);
            }
        }
        let x = x.saturating_add(bw as u16);
        buf.set_stringn(
            x,
            y,
            format!("{:>w$}", r.value, w = VALUE_W),
            room(x),
            Style::default().fg(r.value_colour),
        );
    }
}

/// What the animated part of a frame would show, reduced to the units the
/// screen can display: each bar's length in eighth-cells, and each engine
/// figure to the 0.1 it is printed at. Two frames with the same signature (and
/// nothing else changed) are identical on screen, so the second need not be
/// drawn. Between samples only the damped bars move, and as a bar eases in,
/// most frames shift it by less than an eighth of a cell.
///
/// Measured during a transcode: drawing is nearly all of igtop's cost (4.8% of a core at 60 fps, 0.8% at 5 fps, sampling included),
/// so a frame not drawn is the saving.
pub fn motion_signature(v: &View, width: u16) -> Vec<i64> {
    // Same geometry as `draw_gpu`: the bars sit inside a bordered block.
    let bw = bar_width(Rect::new(0, 0, width.saturating_sub(2), 1)) as f64;
    let eighths = |pct: f64| (pct / 100.0 * bw * 8.0).round() as i64;
    let mut sig = Vec::new();
    for (i, g) in v.gpus.iter().enumerate() {
        if v.focus.is_some_and(|f| f != i && f < v.gpus.len()) {
            continue;
        }
        let media = g.media_busy(v.smooth);
        for &pct in std::iter::once(&media).chain(g.engine_values(v.smooth)) {
            sig.push(eighths(pct));
            sig.push((pct * 10.0).round() as i64);
        }
        for pct in g.capacity_values(v.smooth).into_iter().flatten() {
            sig.push(eighths(pct));
        }
    }
    sig
}

pub fn draw(f: &mut Frame, v: &View) {
    let area = f.area();
    let shown: Vec<usize> = match v.focus {
        Some(i) if i < v.gpus.len() => vec![i],
        _ => (0..v.gpus.len()).collect(),
    };
    let n = shown.len().max(1);

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    draw_header(f, outer[0], v);

    let slots = Layout::default()
        .direction(Direction::Vertical)
        .constraints((0..n).map(|_| Constraint::Ratio(1, n as u32)).collect::<Vec<_>>())
        .split(outer[1]);
    for (slot, &gi) in shown.iter().enumerate() {
        draw_gpu(f, slots[slot], &v.gpus[gi], v);
    }
}

fn draw_header(f: &mut Frame, area: Rect, v: &View) {
    let ic = icons(v.icons);
    let mut spans = vec![
        Span::styled(" igtop", Style::default().fg(TITLE).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {}", v.host), Style::default().fg(TEXT)),
        Span::styled(format!("  {} ms", v.interval_ms), Style::default().fg(DIM)),
        Span::styled(
            match v.focus {
                Some(i) => format!(
                    "  {} {}",
                    ic.gpu,
                    v.gpus.get(i).map(|g| g.name()).unwrap_or_else(|| "?".into())
                ),
                None if v.gpus.len() > 1 => format!("  {} all {}", ic.gpu, v.gpus.len()),
                None => String::new(),
            },
            Style::default().fg(TITLE),
        ),
    ];
    if !v.smooth {
        spans.push(Span::styled("  raw", Style::default().fg(AMBER)));
    }
    if v.paused {
        spans.push(Span::styled(
            "  paused",
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled(
        "   q quit  c clients  h history  s smooth  g gpu  i icons  space pause  +/- rate",
        Style::default().fg(CHROME),
    ));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_gpu(f: &mut Frame, area: Rect, g: &Gpu, v: &View) {
    let ic = icons(v.icons);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(CHROME))
        .title(Line::from(vec![
            Span::styled(format!(" {} ", ic.gpu), Style::default().fg(TITLE)),
            Span::styled(g.name(), Style::default().fg(TITLE).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" · {} ", g.pci_addr.clone().unwrap_or_else(|| "-".into())),
                Style::default().fg(DIM),
            ),
            // Without this a failed read leaves the last good figures on screen,
            // looking live.
            Span::styled(
                if g.pmu_failed { " PMU read failed — figures frozen " } else { "" },
                Style::default().fg(WARN).add_modifier(Modifier::BOLD),
            ),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 4 || inner.width < 34 {
        return;
    }

    let engines = g.reader.engines.len();
    let caps = capacity_rows(g, v.smooth).len();
    // The client table needs a fixed, known height: title, header, one row per
    // client. Everything left over belongs to the history graph, which is the
    // only element that genuinely improves with more room.
    let clients_h = if v.show_clients {
        2 + g.clients.len().clamp(1, 8) as u16
    } else {
        0
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),                     // summary + spacer
            Constraint::Length(engines as u16 + 1),    // MEDIA + engine bars
            Constraint::Length(1),                     // divider
            Constraint::Length(caps as u16),           // power / clock / vram
            // Bounded at 8 rows — two thirds of what it used to take — so the
            // client list gets the space instead. A sparkline gains little past
            // this height; a process list gains a row per process.
            Constraint::Max(if v.show_history { 8 } else { 0 }),
            Constraint::Min(clients_h),
        ])
        .split(inner);

    draw_summary(f, rows[0], g, v);
    draw_engines(f, rows[1], g, v);
    draw_divider(f, rows[2]);
    draw_capacity(f, rows[3], g, v);
    if v.show_history && rows[4].height > 1 {
        draw_history(f, rows[4], g, v);
    }
    if v.show_clients && rows[5].height >= 3 {
        draw_clients(f, rows[5], g, v);
    }
}

fn draw_summary(f: &mut Frame, area: Rect, g: &Gpu, v: &View) {
    let ic = icons(v.icons);
    let d = &g.dynamic;
    let s = &g.sample;
    let mut spans = vec![
        Span::raw("  "),
        Span::styled("rc6 ", Style::default().fg(DIM)),
        Span::styled(format!("{:.0}%", g.rc6_pct()), Style::default().fg(TEXT)),
        Span::styled("   awake ", Style::default().fg(DIM)),
        Span::styled(format!("{:.0}%", s.awake_pct), Style::default().fg(TEXT)),
        Span::styled("   irq ", Style::default().fg(DIM)),
        Span::styled(format!("{:.0}/s", s.interrupts_per_s), Style::default().fg(TEXT)),
    ];
    if let Some(cur) = d.link_cur {
        spans.push(Span::styled(format!("   {} ", ic.link), Style::default().fg(DIM)));
        // Judged against what this slot can actually train to, not the card's
        // own capability: a Gen4 card in a Gen3 slot at Gen3 is at full speed
        // and says nothing more. Only a genuine shortfall is called out.
        let ceiling = g.sysfs.as_ref().and_then(|s| s.stat.link_max);
        match ceiling.filter(|m| cur.below(m)) {
            Some(max) => {
                spans.push(Span::styled(cur.to_string(), Style::default().fg(WARN)));
                spans.push(Span::styled(format!(" of {}", max), Style::default().fg(DIM)));
            }
            None => spans.push(Span::styled(cur.to_string(), Style::default().fg(TEXT))),
        }
    }
    // Some cards report no tacho at all — fan1_input is always 0 — so showing
    // "0 rpm" would read as a stopped fan rather than an absent sensor.
    if let Some(r) = d.fan_rpm.filter(|r| *r > 0) {
        spans.push(Span::styled(format!("   {} ", ic.fan), Style::default().fg(DIM)));
        spans.push(Span::styled(format!("{} rpm", r), Style::default().fg(TEXT)));
    }
    if !d.throttles.is_empty() {
        spans.push(Span::styled(format!("    {} ", ic.warn), Style::default().fg(WARN)));
        spans.push(Span::styled(
            d.throttles.join(" "),
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn engine_icon(class: &str, ic: &Icons) -> (&'static str, Color) {
    match class {
        "vcs" => (ic.video, TITLE),
        "vecs" => (ic.enhance, BLUE),
        "rcs" => (ic.render, VIOLET),
        "bcs" => (ic.copy, DIM),
        "ccs" => (ic.compute, MINT),
        _ => (ic.render, DIM),
    }
}

fn bar_width(area: Rect) -> usize {
    // Fills whatever width the pane has, so the bars line up with the right
    // edge exactly as the client table does. The margin keeps the value column
    // off the border.
    (area.width as usize)
        .saturating_sub(PREFIX_W + VALUE_W + 2)
        .max(8)
}

fn draw_engines(f: &mut Frame, area: Rect, g: &Gpu, v: &View) {
    let ic = icons(v.icons);
    let vals = g.engine_values(v.smooth);
    let mut rows = Vec::with_capacity(g.reader.engines.len() + 1);

    // The headline sits directly above the engines it summarises: the busiest
    // of the video and enhance engines, which on a transcoding box is the one
    // number worth reading first.
    let media = g.media_busy(v.smooth);
    rows.push(BarRow {
        icon: ic.media,
        icon_colour: TITLE,
        class: "MEDIA",
        name: "",
        pct: media,
        value: format!("{:.1} %", media),
        value_colour: value_colour(media),
    });

    let mut prev = "";
    for (i, e) in g.reader.engines.iter().enumerate() {
        let pct = vals.get(i).copied().unwrap_or(0.0);
        let class = if e.class == prev { "" } else { engine_label(&e.class) };
        prev = &e.class;
        let (icon, icon_colour) = engine_icon(&e.class, &ic);
        rows.push(BarRow {
            icon,
            icon_colour,
            class,
            name: &e.name,
            pct,
            value: format!("{:.1} %", pct),
            value_colour: value_colour(pct),
        });
    }
    render_bar_rows(f.buffer_mut(), area, &rows);
}

fn draw_divider(f: &mut Frame, area: Rect) {
    let w = (area.width as usize).saturating_sub(4);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled("╌".repeat(w), Style::default().fg(TRACK)),
        ])),
        area,
    );
}

/// The bounded quantities. Each has a real ceiling, so each gets a bar and both
/// figures — a lone "25.4 W" says nothing without the 43 W it is measured against.
/// The bar is damped like the engine bars (raw with `s`); the figures are raw.
fn capacity_rows(g: &Gpu, smooth: bool) -> Vec<(&'static str, f64, String)> {
    let st = match g.sysfs.as_ref() {
        Some(s) => &s.stat,
        None => return Vec::new(),
    };
    let d = &g.dynamic;
    let [temp, power, clock, memory] = g.capacity_values(smooth);
    let mut out = Vec::new();

    if let (Some(pct), Some(t)) = (temp, d.temp_c) {
        out.push(("TEMP", pct, format!("{:.0} / {:.0} °C", t, st.temp_max_c)));
    }
    if let Some(pct) = power {
        let w = d.power_w.unwrap_or(0.0);
        out.push(("POWER", pct, format!("{:.1} / {:.0} W", w, st.power_max_w)));
    }
    if let Some(pct) = clock {
        let act = g.sample.actual_freq_mhz;
        out.push(("CLOCK", pct, format!("{:.0} / {} MHz", act, st.freq_max_mhz)));
    }
    if let Some(pct) = memory {
        let used = d.vram_used_bytes.unwrap_or(0);
        out.push(("MEMORY", pct, format!("{} / {}", short_bytes(used), short_bytes(st.vram_total_bytes))));
    }
    out
}

fn draw_capacity(f: &mut Frame, area: Rect, g: &Gpu, v: &View) {
    let ic = icons(v.icons);
    let rows: Vec<BarRow> = capacity_rows(g, v.smooth)
        .into_iter()
        .map(|(label, pct, value)| {
            let (icon, icon_colour) = match label {
                "POWER" => (ic.power, AMBER),
                "CLOCK" => (ic.clock, TITLE),
                "TEMP" => (ic.temp, MINT),
                _ => (ic.memory, BLUE),
            };
            BarRow { icon, icon_colour, class: label, name: "", pct, value, value_colour: TEXT }
        })
        .collect();
    render_bar_rows(f.buffer_mut(), area, &rows);
}

fn draw_history(f: &mut Frame, area: Rect, g: &Gpu, v: &View) {
    let ic = icons(v.icons);
    // Sparkline draws the FIRST `width` samples, not the last. The buffer holds
    // more than the pane can show, so handing it the whole thing would render
    // the OLDEST window and leave the graph minutes behind the live figures.
    // Only the most recent `width` samples are drawn, so the right edge is now.
    let w = area.width as usize;
    fn visible(h: &std::collections::VecDeque<u64>, w: usize) -> impl Iterator<Item = u64> + '_ {
        h.iter().skip(h.len().saturating_sub(w)).copied()
    }

    // Tracked engine is chosen by its peak over the *visible* window, not its
    // current value — choosing on the current value makes the selection flip
    // arbitrarily the moment everything falls to zero. And not over the whole
    // ten-minute buffer either: a peak that has scrolled off the left edge
    // would hold the graph on a flat line while another engine is busy. Ties
    // go to the first engine in display order, so an idle card shows vcs0.
    let rings = if v.smooth { &g.history_smooth } else { &g.history };
    let mut idx = 0;
    let mut best = 0;
    for (i, h) in rings.iter().enumerate() {
        let peak = visible(h, w).max().unwrap_or(0);
        if peak > best {
            best = peak;
            idx = i;
        }
    }
    let empty = std::collections::VecDeque::new();
    let data: Vec<u64> = visible(g.history_of(idx, v.smooth).unwrap_or(&empty), w).collect();
    let name = g.reader.engines.get(idx).map(|e| e.name.clone()).unwrap_or_default();
    // Peak over what is actually on screen, so the label and the graph agree.
    let peak = data.iter().copied().max().unwrap_or(0);
    // One column per second (`app::HISTORY`), so the span shown is the count.
    let secs = data.len();
    let span = if secs >= 60 { format!("{}m{:02}s", secs / 60, secs % 60) } else { format!("{}s", secs) };

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(TRACK))
        .title(Line::from(vec![
            Span::styled(format!(" {} ", ic.history), Style::default().fg(DIM)),
            Span::styled(name, Style::default().fg(LABEL)),
            Span::styled(format!("  peak {}%  · last {}  ", peak, span), Style::default().fg(DIM)),
        ]));
    // Each column is coloured by its own height from the same ramp the bars
    // use, so a spike in the graph is the colour the bar was at the time.
    let bars: Vec<SparklineBar> = data
        .iter()
        .map(|&v| SparklineBar::from(v).style(Style::default().fg(ramp(v as f64 / 100.0))))
        .collect();

    // Right-aligned: the newest column is always at the right edge, and a
    // graph that has not yet filled the pane grows leftwards from there, as
    // a scrolling chart does. Sparkline itself draws from its left edge, so it
    // is given a strip exactly as wide as the data, pinned to the right.
    let inner = block.inner(area);
    f.render_widget(block, area);
    let n = (bars.len() as u16).min(inner.width);
    let strip = Rect { x: inner.right() - n, width: n, ..inner };
    f.render_widget(Sparkline::default().data(bars).max(100), strip);
}

fn draw_clients(f: &mut Frame, area: Rect, g: &Gpu, v: &View) {
    let ic = icons(v.icons);
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(TRACK))
        .title(Line::from(Span::styled(
            // fdinfo reports each engine *class*, summed across its engines
            // and divided here by the class size, so a client saturating one
            // of the two video engines reads 50% against vcs0's 100% above.
            format!(
                " {} clients · engine-class averages over {} s  ",
                ic.container,
                crate::clients::WINDOW_S
            ),
            Style::default().fg(DIM),
        )));

    if g.clients.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "   idle — no process holds this GPU",
                Style::default().fg(DIM),
            )))
            .block(block),
            area,
        );
        return;
    }

    let header =
        Row::new(vec!["PID", "WHERE", "PROCESS", "VIDEO", "ENHANCE", "RENDER", "COMPUTE", "MEM"])
        .style(Style::default().fg(DIM));
    let cap = (area.height as usize).saturating_sub(2);
    let rows: Vec<Row> = g
        .clients
        .iter()
        .take(cap)
        .map(|c| {
            let get = |k: &str| c.engines.get(k).copied().unwrap_or(0.0);
            let cells = [get("video"), get("video-enhance"), get("render"), get("compute")];
            let mut r = vec![
                Cell::from(c.pid.to_string()).style(Style::default().fg(DIM)),
                Cell::from(c.container.clone().unwrap_or_else(|| "host".into()))
                    .style(Style::default().fg(if c.container.is_some() { BLUE } else { DIM })),
                Cell::from(c.comm.clone()).style(Style::default().fg(TEXT)),
            ];
            for p in cells {
                r.push(Cell::from(format!("{:.1}%", p)).style(Style::default().fg(value_colour(p))));
            }
            r.push(Cell::from(short_bytes(c.mem_resident)).style(Style::default().fg(DIM)));
            Row::new(r)
        })
        .collect();

    f.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(8),
                Constraint::Length(9),
                Constraint::Min(12),
                Constraint::Length(8),
                Constraint::Length(9),
                Constraint::Length(8),
                Constraint::Length(9),
                Constraint::Length(8),
            ],
        )
        .header(header)
        .block(block),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pmu::EngineDef;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::Widget;
    use std::time::Instant;

    /// The original span-per-cell bar, kept as the reference the direct path
    /// must reproduce cell for cell.
    fn old_line(r: &BarRow, bw: usize) -> Line<'static> {
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(r.icon, Style::default().fg(r.icon_colour)),
            Span::raw(" "),
            Span::styled(format!("{:<8}", r.class), Style::default().fg(LABEL).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:<6}", r.name), Style::default().fg(DIM)),
        ];
        let filled = (r.pct / 100.0 * bw as f64 * 8.0).round().max(0.0) as usize;
        let full = (filled / 8).min(bw);
        let rem = filled % 8;
        let span = (bw.saturating_sub(1)).max(1) as f64;
        for i in 0..full {
            spans.push(Span::styled("█", Style::default().fg(ramp(i as f64 / span))));
        }
        let mut used = full;
        if full < bw && rem > 0 {
            spans.push(Span::styled(EIGHTHS[rem - 1], Style::default().fg(ramp(full as f64 / span))));
            used += 1;
        }
        if used < bw {
            spans.push(Span::styled(TRACK_CH.repeat(bw - used), Style::default().fg(TRACK)));
        }
        spans.push(Span::styled(format!("{:>w$}", r.value, w = VALUE_W), Style::default().fg(r.value_colour)));
        Line::from(spans)
    }

    fn rows(pcts: &[f64]) -> Vec<BarRow<'static>> {
        pcts.iter()
            .enumerate()
            .map(|(i, &pct)| BarRow {
                icon: if i % 2 == 0 { NERD.video } else { PLAIN.video },
                icon_colour: TITLE,
                class: if i % 3 == 0 { "VIDEO" } else { "" },
                name: "vcs0",
                pct,
                value: format!("{:.1} %", pct),
                value_colour: value_colour(pct),
            })
            .collect()
    }

    const PCTS: [f64; 9] = [0.0, 0.3, 0.7, 12.5, 33.3, 50.0, 99.9, 100.0, 140.0];

    #[test]
    fn direct_bars_match_spans() {
        for width in [20u16, 34, 41, 60, 121, 250] {
            let rs = rows(&PCTS);
            let area = Rect::new(3, 2, width, rs.len() as u16);
            let whole = Rect::new(0, 0, width + 6, rs.len() as u16 + 4);
            let mut new = Buffer::empty(whole);
            render_bar_rows(&mut new, area, &rs);
            let mut old = Buffer::empty(whole);
            let bw = bar_width(area);
            Paragraph::new(rs.iter().map(|r| old_line(r, bw)).collect::<Vec<_>>()).render(area, &mut old);
            assert_eq!(new, old, "width {}", width);
        }
    }

    #[test]
    fn history_grows_from_the_right() {
        let mut g = Gpu::fake(engines());
        for _ in 0..10 {
            g.sample.engine_busy = vec![100.0; 7];
            g.sample.dt_s = 1.0;
            g.tick_fake();
        }
        let view = View {
            gpus: std::slice::from_ref(&g),
            host: String::new(),
            show_clients: false,
            show_history: true,
            smooth: false,
            icons: false,
            focus: None,
            interval_ms: 1000,
            paused: false,
        };
        let mut term = ratatui::Terminal::new(TestBackend::new(40, 6)).unwrap();
        term.draw(|f| draw_history(f, f.area(), &g, &view)).unwrap();
        let buf = term.backend().buffer();
        let bottom = 5;
        for x in 0..40u16 {
            let filled = buf[(x, bottom)].symbol() != " ";
            assert_eq!(filled, x >= 30, "column {}", x);
        }
    }

    #[test]
    fn signature_ignores_invisible_motion() {
        let mut g = Gpu::fake(engines());
        let sig = |g: &Gpu| {
            let view = View {
                gpus: std::slice::from_ref(g),
                host: String::new(),
                show_clients: false,
                show_history: false,
                smooth: true,
                icons: false,
                focus: None,
                interval_ms: 1000,
                paused: false,
            };
            motion_signature(&view, 120)
        };
        g.smooth[0] = 40.0;
        let a = sig(&g);
        // 120 columns leave an 83-cell bar: one eighth is ~0.15 points, and
        // the figure prints to 0.1, so 0.02 changes nothing on screen.
        g.smooth[0] = 40.02;
        assert_eq!(a, sig(&g));
        g.smooth[0] = 40.3;
        assert_ne!(a, sig(&g));
    }

    fn engines() -> Vec<EngineDef> {
        [("vcs", 0), ("vcs", 1), ("vecs", 0), ("vecs", 1), ("rcs", 0), ("bcs", 0), ("ccs", 0)]
            .iter()
            .map(|(c, i)| EngineDef { name: format!("{}{}", c, i), class: c.to_string(), instance: *i })
            .collect()
    }

    /// `cargo test --release -- --ignored --nocapture bench`: per-frame cost
    /// of the bar rows by each path, and of a whole frame, on a 250×60 pane.
    #[test]
    #[ignore]
    fn bench_frames() {
        const N: usize = 3000;
        let area = Rect::new(1, 1, 248, 12);
        let mut buf = Buffer::empty(Rect::new(0, 0, 250, 60));
        let sets: Vec<Vec<BarRow>> =
            (0..64).map(|k| rows(&(0..12).map(|i| ((k * 7 + i * 13) % 100) as f64 + 0.37).collect::<Vec<_>>())).collect();

        let t = Instant::now();
        for n in 0..N {
            buf.reset();
            let rs = &sets[n % sets.len()];
            let bw = bar_width(area);
            Paragraph::new(rs.iter().map(|r| old_line(r, bw)).collect::<Vec<_>>()).render(area, &mut buf);
        }
        let old = t.elapsed().as_secs_f64() / N as f64 * 1e6;

        let t = Instant::now();
        for n in 0..N {
            buf.reset();
            render_bar_rows(&mut buf, area, &sets[n % sets.len()]);
        }
        let new = t.elapsed().as_secs_f64() / N as f64 * 1e6;
        println!("12 bar rows, 248 wide: spans {:.1} µs/frame, direct {:.1} µs/frame ({:.1}x)", old, new, old / new);

        let mut g = Gpu::fake(engines());
        for n in 0..crate::app::HISTORY {
            let v: Vec<f64> = (0..7).map(|i| ((n * 5 + i * 11) % 100) as f64).collect();
            g.sample.engine_busy = v;
            g.sample.dt_s = 1.0;
            g.tick_fake();
        }
        {
            let mut term = ratatui::Terminal::new(TestBackend::new(250, 60)).unwrap();
            let t = Instant::now();
            for _ in 0..N {
                term.draw(|_| {}).unwrap();
            }
            println!("empty frame (harness + diff only): {:.1} µs/frame", t.elapsed().as_secs_f64() / N as f64 * 1e6);
        }
        for (hist, clients) in [(true, true), (false, true), (true, false), (false, false)] {
            let mut term = ratatui::Terminal::new(TestBackend::new(250, 60)).unwrap();
            let t = Instant::now();
            for n in 0..N {
                for (i, v) in g.smooth.iter_mut().enumerate() {
                    *v = ((n * 3 + i * 17) % 100) as f64 + 0.5;
                }
                let view = View {
                    gpus: std::slice::from_ref(&g),
                    host: "host".into(),
                    show_clients: clients,
                    show_history: hist,
                    smooth: true,
                    icons: true,
                    focus: None,
                    interval_ms: 1000,
                    paused: false,
                };
                term.draw(|f| draw(f, &view)).unwrap();
            }
            println!(
                "whole frame incl. diff, history {} clients {}: {:.1} µs/frame",
                hist, clients,
                t.elapsed().as_secs_f64() / N as f64 * 1e6
            );
        }
    }
}
