//! Relatório de sessão em HTML autocontido.
//!
//! SPEC-PROBE-014 — "um arquivo `.html` abre no navegador sem rede".
//! SPEC-PROBE-020 — cobre o `run_id` inteiro, com os feeds lado a lado e as
//! timelines alinhadas no mesmo eixo de tempo.

use std::fmt::Write as _;

use chrono::{DateTime, Utc};

use crate::series::{MetricId, TimelineBucket};
use crate::severity::Severity;
use crate::snapshot::{FeedSnapshot, ProbeSnapshot};

/// Quantos eventos do topo entram no relatório.
///
/// SPEC-PROBE-014 — "top-10 eventos".
pub const TOP_EVENTS: usize = 10;

/// Gera o relatório do run inteiro.
///
/// Nenhum recurso externo: CSS embutido, gráficos como `<svg>` inline e
/// nenhuma tag `<script>`, `<img src="http...">` ou `@import`.  É isso que
/// faz o arquivo abrir sem rede (SPEC-PROBE-014).
pub fn render_run_report(snapshot: &ProbeSnapshot) -> String {
    let mut html = String::with_capacity(64 * 1024);

    let _ = write!(
        html,
        "<!doctype html><html lang=\"pt-BR\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>IronPlayer — relatório de sessão {run}</title><style>{css}</style></head><body>",
        run = escape(&snapshot.run_id),
        css = CSS
    );

    let _ = write!(
        html,
        "<header><h1>Relatório de monitoração</h1>\
<p class=\"meta\">run <code>{run}</code> · início {start} · duração {dur} · {n} feed(s)</p></header>",
        run = escape(&snapshot.run_id),
        start = snapshot
            .started_utc
            .map_or("—".to_string(), fmt_utc),
        dur = snapshot.run_clock(),
        n = snapshot.feeds.len(),
    );

    // ── Resumo lado a lado (SPEC-PROBE-020) ─────────────────────────────
    html.push_str(
        "<section><h2>Resumo comparativo</h2><table class=\"cmp\"><thead><tr><th>Métrica</th>",
    );
    for feed in &snapshot.feeds {
        let _ = write!(html, "<th>{}</th>", escape(feed.display_name()));
    }
    html.push_str("</tr></thead><tbody>");

    row(&mut html, "Endereço", snapshot, |f| escape(&f.url));
    row(&mut html, "Encapsulamento", snapshot, |f| {
        f.encapsulation.badge().to_string()
    });
    row(&mut html, "Disponibilidade", snapshot, |f| {
        format!("{:.3} %", f.availability_session * 100.0)
    });
    row(&mut html, "Bitrate atual", snapshot, |f| {
        format!("{:.2} Mbps", f.bitrate_kbps / 1000.0)
    });
    row(&mut html, "Pior severidade", snapshot, |f| {
        f.worst_severity.map_or("—".into(), badge)
    });
    row(&mut html, "Eventos no log", snapshot, |f| {
        f.events.len().to_string()
    });
    row(&mut html, "Indisponibilidades", snapshot, |f| {
        format!("{} ({} s)", f.unavailable.periods, f.unavailable.total_secs)
    });
    row(&mut html, "Descartes locais", snapshot, |f| {
        f.health.local_drops.to_string()
    });
    html.push_str("</tbody></table></section>");

    // ── Timelines alinhadas (SPEC-PROBE-020) ────────────────────────────
    if !snapshot.feeds.is_empty() {
        html.push_str("<section><h2>Linha do tempo de saúde</h2>");
        let span = timeline_span(snapshot);
        for feed in &snapshot.feeds {
            let _ = write!(
                html,
                "<div class=\"tl-row\"><span class=\"tl-name\">{}</span>{}</div>",
                escape(feed.display_name()),
                render_timeline(&feed.timeline, span)
            );
        }
        html.push_str(
            "<p class=\"legend\"><span class=\"sw ok\"></span>sem alarme\
<span class=\"sw info\"></span>info<span class=\"sw warn\"></span>warning\
<span class=\"sw err\"></span>error<span class=\"sw crit\"></span>critical\
<span class=\"sw nd\"></span>sem dado</p></section>",
        );
    }

    // ── Seções individuais ──────────────────────────────────────────────
    for feed in &snapshot.feeds {
        render_feed_section(&mut html, feed);
    }

    html.push_str(
        "<footer><p>Gerado pelo IronPlayer — modo Probe. \
Arquivo autocontido: abre sem acesso à rede.</p></footer></body></html>",
    );
    html
}

fn row<F>(html: &mut String, label: &str, snapshot: &ProbeSnapshot, f: F)
where
    F: Fn(&FeedSnapshot) -> String,
{
    let _ = write!(html, "<tr><th scope=\"row\">{label}</th>");
    for feed in &snapshot.feeds {
        let _ = write!(html, "<td>{}</td>", f(feed));
    }
    html.push_str("</tr>");
}

fn render_feed_section(html: &mut String, feed: &FeedSnapshot) {
    let _ = write!(
        html,
        "<section><h2>{name}</h2><p class=\"meta\"><code>{url}</code> · {enc} · \
uptime {up} s · disponibilidade {av:.3} %</p>",
        name = escape(feed.display_name()),
        url = escape(&feed.url),
        enc = feed.encapsulation.badge(),
        up = feed.uptime_secs,
        av = feed.availability_session * 100.0,
    );

    // Gráficos.
    html.push_str("<div class=\"charts\">");
    for metric in MetricId::ALL {
        if let Some(points) = feed.series.get(&metric) {
            if points.values.is_empty() {
                continue;
            }
            let _ = write!(
                html,
                "<figure class=\"chart\"><figcaption>{} <small>{}</small></figcaption>{}</figure>",
                metric.label(),
                metric.unit(),
                sparkline(&points.values, points.min, points.max),
            );
        }
    }
    html.push_str("</div>");

    // Saúde da probe (SPEC-PROBE-013).
    let _ = write!(
        html,
        "<h3>Saúde da probe</h3><ul class=\"health\">\
<li>descartes locais: <b>{drops}</b></li>\
<li>jitter de tick (pico): <b>{jit:.1} ms</b></li>\
<li>eventos descartados: <b>{de}</b></li>\
<li>linhas descartadas pelo writer: <b>{wd}</b></li>\
<li>degradação: <b>{deg}</b></li></ul>",
        drops = feed.health.local_drops,
        jit = feed.health.sched_jitter_peak_ms,
        de = feed.health.dropped_events,
        wd = feed.health.writer_drops,
        deg = feed.health.degradation.label(),
    );

    // Camada IP (spec-14 §5.7 · §8).
    render_ip_section(html, feed);

    // Quantas vezes cada check disparou.
    //
    // spec-14 §8 — os limiares default são "propostas para calibrar na primeira
    // sessão de 12 h, não normativos".  Sem esta tabela, a segunda rodada de
    // ajuste seria feita no escuro.
    render_check_tally(html, feed);

    // Top-10 eventos (SPEC-PROBE-014).
    html.push_str("<h3>Top eventos</h3>");
    let top = top_events(feed);
    if top.is_empty() {
        html.push_str("<p class=\"meta\">Nenhum evento registrado.</p>");
    } else {
        html.push_str(
            "<table class=\"events\"><thead><tr><th>Horário</th><th>Nível</th>\
<th>Check</th><th>Ocorrências</th><th>Medido</th><th>Contexto</th></tr></thead><tbody>",
        );
        for ev in top {
            let _ = write!(
                html,
                "<tr><td>{ts}</td><td>{sev}</td><td><code>{id}</code></td>\
<td class=\"num\">{n}</td><td class=\"num\">{m:.3} {u}</td><td>{ctx}</td></tr>",
                ts = fmt_utc(ev.ts_utc),
                sev = badge(ev.severity),
                id = escape(&ev.check_id),
                n = ev.count,
                m = ev.measured,
                u = escape(&ev.unit),
                ctx = escape(&ev.context),
            );
        }
        html.push_str("</tbody></table>");
    }

    html.push_str("</section>");
}

/// Bloco da camada IP do feed.
///
/// SPEC-PROBE-IP-047 — o relatório é o artefato que sai da probe para quem
/// opera o sinal; se a aba `Rede` responde a pergunta na tela, o relatório
/// precisa respondê-la em disco.
/// SPEC-PROBE-IP-043 — num feed UDP puro a faixa de RTP/FEC **não** aparece,
/// em vez de aparecer zerada como se tivesse sido verificada.
fn render_ip_section(html: &mut String, feed: &FeedSnapshot) {
    let Some(ip) = feed.ip.as_ref() else {
        return;
    };
    let opt = |v: Option<f64>, unit: &str| {
        v.map_or_else(|| "n/a".to_string(), |x| format!("{x:.1} {unit}"))
    };

    html.push_str("<h3>Rede</h3><ul class=\"health\">");
    let _ = write!(
        html,
        "<li>encapsulamento: <b>{enc}</b></li>\
<li>fontes: <b>{sources}</b></li>\
<li>taxa IP: <b>{mbps:.2} Mbps</b></li>\
<li>TS/datagrama: <b>{tspd}</b></li>\
<li>inter-arrival médio: <b>{avg}</b> · p99 <b>{p99}</b> · esperado <b>{exp}</b></li>\
<li>piso de ruído da probe: <b>{floor}</b></li>",
        enc = ip.encapsulation.badge(),
        sources = if ip.sources.is_empty() {
            "—".to_string()
        } else {
            escape(
                &ip.sources
                    .iter()
                    .map(|a| a.ip().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        },
        mbps = ip.mbps,
        tspd = ip
            .ts_per_datagram
            .map_or_else(|| "n/a".to_string(), |v| format!("{v:.2}")),
        avg = opt(ip.iat.avg_us, "µs"),
        p99 = opt(ip.iat.p99_us, "µs"),
        exp = opt(ip.iat_expected_us, "µs"),
        floor = opt(ip.noise_floor_us, "µs"),
    );

    if let Some(rtp) = ip.rtp {
        let _ = write!(
            html,
            "<li>RTP: recebidos <b>{r}</b> · perdidos <b>{m}</b> · fora de ordem <b>{o}</b> · \
duplicados <b>{d}</b> · too old <b>{t}</b></li>\
<li>razão de perda: <b>{ratio}</b> · jitter RFC 3550: <b>{jit}</b></li>",
            r = rtp.received,
            m = rtp.missing,
            o = rtp.reorder,
            d = rtp.dup,
            t = rtp.too_old,
            ratio = ip
                .loss_ratio
                .map_or_else(|| "n/a".to_string(), |v| format!("{v:.2e}")),
            jit = opt(ip.jitter_us, "µs"),
        );
    }
    if ip.fec.listening {
        let _ = write!(
            html,
            "<li>FEC: <b>{present}</b> · matriz <b>{matrix}</b> · fluxos <b>{streams}</b> · \
overhead <b>{over}</b></li>",
            present = if ip.fec.present {
                "presente"
            } else {
                "ausente"
            },
            matrix = ip.fec.matrix_label(),
            streams = ip.fec.streams,
            over = ip
                .fec
                .overhead_pct
                .map_or_else(|| "n/a".to_string(), |v| format!("{v:.1} %")),
        );
    }
    html.push_str("</ul>");
}

/// Tabela "quantas vezes cada check disparou nesta sessão".
///
/// spec-14 §8
fn render_check_tally(html: &mut String, feed: &FeedSnapshot) {
    let mut tally: std::collections::BTreeMap<(&str, Severity), (u64, u64)> =
        std::collections::BTreeMap::new();
    for ev in &feed.events {
        if ev.phase != crate::event::EventPhase::Open {
            continue;
        }
        let entry = tally
            .entry((ev.check_id.as_str(), ev.severity))
            .or_insert((0, 0));
        entry.0 += 1;
        entry.1 += ev.count;
    }
    if tally.is_empty() {
        return;
    }

    html.push_str(
        "<h3>Checks disparados</h3><table class=\"events\"><thead><tr><th>Check</th>\
<th>Nível</th><th>Eventos</th><th>Ocorrências</th></tr></thead><tbody>",
    );
    let mut rows: Vec<_> = tally.into_iter().collect();
    rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(b.0 .1.cmp(&a.0 .1)));
    for ((check_id, severity), (events, occurrences)) in rows {
        let _ = write!(
            html,
            "<tr><td><code>{id}</code></td><td>{sev}</td><td class=\"num\">{e}</td>\
<td class=\"num\">{o}</td></tr>",
            id = escape(check_id),
            sev = badge(severity),
            e = events,
            o = occurrences,
        );
    }
    html.push_str("</tbody></table>");
}

/// Os eventos mais relevantes: severidade primeiro, depois contagem.
///
/// SPEC-PROBE-014
fn top_events(feed: &FeedSnapshot) -> Vec<&crate::snapshot::EventRow> {
    let mut rows: Vec<&crate::snapshot::EventRow> = feed.events.iter().collect();
    rows.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| b.ts_utc.cmp(&a.ts_utc))
    });
    rows.dedup_by(|a, b| a.event_id == b.event_id);
    rows.truncate(TOP_EVENTS);
    rows
}

/// Extensão comum das timelines, para alinhar os feeds no mesmo eixo.
///
/// SPEC-PROBE-020
fn timeline_span(snapshot: &ProbeSnapshot) -> (Option<DateTime<Utc>>, usize) {
    let start = snapshot
        .feeds
        .iter()
        .filter_map(|f| f.timeline.first().map(|b| b.start_utc))
        .min();
    let cells = snapshot
        .feeds
        .iter()
        .map(|f| f.timeline.len())
        .max()
        .unwrap_or(0);
    (start, cells)
}

fn render_timeline(
    cells: &[TimelineBucket],
    (start, width): (Option<DateTime<Utc>>, usize),
) -> String {
    let mut out = String::with_capacity(width * 48 + 64);
    out.push_str("<div class=\"tl\">");

    // Preenche à esquerda quando este feed começou depois do run.
    let lead = match (start, cells.first()) {
        (Some(run_start), Some(first)) if first.start_utc > run_start => {
            let secs = (first.start_utc - run_start).num_seconds().max(0) as usize;
            let bucket = cells
                .get(1)
                .map(|b| (b.start_utc - first.start_utc).num_seconds().max(1) as usize)
                .unwrap_or(300);
            secs / bucket
        }
        _ => 0,
    };
    for _ in 0..lead {
        out.push_str("<i class=\"c nd\"></i>");
    }
    for cell in cells {
        let _ = write!(
            out,
            "<i class=\"c\" style=\"background:#{:06x}\" title=\"{} · {}\"></i>",
            cell.rgb(),
            fmt_utc(cell.start_utc),
            cell.worst.map_or("sem alarme", Severity::label),
        );
    }
    for _ in (lead + cells.len())..width {
        out.push_str("<i class=\"c nd\"></i>");
    }
    out.push_str("</div>");
    out
}

/// Sparkline SVG inline; sem dependência de biblioteca de gráficos.
fn sparkline(values: &[f64], min: f64, max: f64) -> String {
    const W: f64 = 100.0;
    const H: f64 = 30.0;
    if values.is_empty() {
        return String::new();
    }
    let span = (max - min).abs();
    let scale = if span < f64::EPSILON { 1.0 } else { span };
    let step = if values.len() > 1 {
        W / (values.len() - 1) as f64
    } else {
        W
    };

    let mut d = String::with_capacity(values.len() * 14 + 16);
    for (i, v) in values.iter().enumerate() {
        let x = i as f64 * step;
        let y = H - ((v - min) / scale).clamp(0.0, 1.0) * H;
        let _ = write!(d, "{}{x:.2} {y:.2}", if i == 0 { "M " } else { " L " });
    }

    format!(
        "<svg viewBox=\"0 0 {W} {H}\" preserveAspectRatio=\"none\" role=\"img\">\
<path d=\"{d}\" fill=\"none\" stroke=\"#57c08a\" stroke-width=\"0.8\"/></svg>\
<div class=\"range\"><span>{min:.1}</span><span>{max:.1}</span></div>"
    )
}

fn badge(sev: Severity) -> String {
    let class = match sev {
        Severity::Info => "info",
        Severity::Warning => "warn",
        Severity::Error => "err",
        Severity::Critical => "crit",
    };
    format!("<span class=\"b {class}\">{}</span>", sev.label())
}

fn fmt_utc(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

/// Escapa texto que vai para o HTML.
///
/// Nomes de feed e contexto vêm do TOML e do stream — dados externos.
/// RNF-PRB-003: nada de dado externo pode virar markup.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const CSS: &str = "\
:root{color-scheme:dark light}\
body{margin:0;padding:24px 28px;background:#0a0c0e;color:#dde4ec;\
font:14px/1.5 'Segoe UI',system-ui,sans-serif}\
h1{font-size:20px;margin:0 0 4px}h2{font-size:15px;letter-spacing:1px;text-transform:uppercase;\
color:#8a95a3;margin:32px 0 10px;border-bottom:1px solid #20262e;padding-bottom:6px}\
h3{font-size:13px;letter-spacing:1px;text-transform:uppercase;color:#8a95a3;margin:20px 0 8px}\
.meta{color:#8a95a3;font-size:12px;margin:0 0 8px}\
code{font-family:Consolas,ui-monospace,monospace;color:#e8943a}\
table{border-collapse:collapse;width:100%;font-size:13px}\
th,td{text-align:left;padding:6px 10px;border-bottom:1px solid #1a1f25;vertical-align:top}\
thead th{color:#8a95a3;font-size:11px;letter-spacing:1px;text-transform:uppercase}\
tbody th{color:#8a95a3;font-weight:500;width:180px}\
td.num{text-align:right;font-family:Consolas,ui-monospace,monospace}\
.b{display:inline-block;padding:1px 7px;border-radius:3px;font-size:11px;font-weight:700;color:#0a0c0e}\
.b.info{background:#5aa0d0}.b.warn{background:#e8943a}.b.err{background:#d6605f}.b.crit{background:#8e2c2b;color:#fff}\
.tl-row{display:flex;align-items:center;gap:10px;margin:5px 0}\
.tl-name{width:190px;flex:none;font-size:12px;color:#8a95a3;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}\
.tl{display:flex;gap:1px;flex:1;height:22px}\
.tl .c{flex:1;min-width:2px;border-radius:1px;background:#3a434d}\
.legend{font-size:11px;color:#8a95a3;margin-top:12px}\
.sw{display:inline-block;width:10px;height:10px;border-radius:2px;margin:0 5px 0 14px;vertical-align:-1px}\
.sw.ok{background:#57c08a}.sw.info{background:#5aa0d0}.sw.warn{background:#e8943a}\
.sw.err{background:#d6605f}.sw.crit{background:#8e2c2b}.sw.nd{background:#3a434d}\
.charts{display:grid;grid-template-columns:repeat(auto-fit,minmax(220px,1fr));gap:12px}\
.chart{margin:0;background:#15191f;border:1px solid #20262e;border-radius:6px;padding:10px}\
.chart figcaption{font-size:11px;letter-spacing:1px;color:#8a95a3;margin-bottom:6px}\
.chart svg{width:100%;height:52px;display:block}\
.range{display:flex;justify-content:space-between;font-size:10px;color:#5f6b78;margin-top:2px}\
.health{margin:0;padding-left:18px;font-size:13px;color:#8a95a3}\
.health b{color:#dde4ec;font-family:Consolas,ui-monospace,monospace}\
footer{margin-top:40px;padding-top:12px;border-top:1px solid #20262e;color:#5f6b78;font-size:11px}\
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventPhase;
    use crate::series::SeriesPoints;
    use crate::session::Encapsulation;
    use crate::snapshot::EventRow;
    use std::collections::BTreeMap;

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("timestamp")
    }

    fn feed(slot: usize, name: &str) -> FeedSnapshot {
        let mut series = BTreeMap::new();
        series.insert(
            MetricId::BitrateKbps,
            SeriesPoints {
                values: vec![14_900.0, 15_100.0, 15_000.0],
                min: 14_900.0,
                max: 15_100.0,
                last: 15_000.0,
                bucket_secs: 60,
            },
        );
        FeedSnapshot {
            slot,
            name: name.into(),
            url: format!("rtp://@239.15.0.{}:50000", 180 + slot),
            encapsulation: crate::session::Encapsulation::RtpFec,
            connected: true,
            uptime_secs: 3_600,
            availability_session: 0.994,
            bitrate_kbps: 15_000.0,
            series,
            timeline: vec![
                TimelineBucket {
                    start_utc: ts(0),
                    worst: None,
                    samples: 300,
                    connected_samples: 300,
                },
                TimelineBucket {
                    start_utc: ts(300),
                    worst: Some(Severity::Error),
                    samples: 300,
                    connected_samples: 280,
                },
            ],
            events: vec![
                EventRow {
                    event_id: "e1".into(),
                    ts_utc: ts(305),
                    severity: Severity::Error,
                    check_id: "cc_error".into(),
                    phase: EventPhase::Open,
                    count: 1_000,
                    measured: 10.0,
                    unit: "errors".into(),
                    context: "pid 6100".into(),
                    pid: Some(6100),
                    service_id: None,
                    local: false,
                    caused_by: None,
                },
                EventRow {
                    event_id: "e2".into(),
                    ts_utc: ts(310),
                    severity: Severity::Warning,
                    check_id: "rtp_out_of_order".into(),
                    phase: EventPhase::Open,
                    count: 4,
                    measured: 4.0,
                    unit: "pkts".into(),
                    context: String::new(),
                    pid: None,
                    service_id: None,
                    local: false,
                    caused_by: None,
                },
            ],
            ..Default::default()
        }
    }

    fn run() -> ProbeSnapshot {
        ProbeSnapshot {
            run_id: "2026-08-07T09-15-32".into(),
            started_utc: Some(ts(0)),
            run_secs: 12_861,
            recording: true,
            feeds: vec![feed(0, "0084_CANAL_A"), feed(1, "0116_CANAL_B")],
            ..Default::default()
        }
    }

    /// SPEC-PROBE-014 — o HTML é autocontido: nenhum recurso externo.
    #[test]
    fn spec_probe_014_report_is_self_contained() {
        let html = render_run_report(&run());
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.ends_with("</html>"));
        for forbidden in ["<script", "http://", "https://", "@import", "src=\"//"] {
            assert!(
                !html.contains(forbidden),
                "relatório não pode conter {forbidden}"
            );
        }
    }

    /// SPEC-PROBE-014 — o relatório traz resumo, timeline e top eventos.
    #[test]
    fn spec_probe_014_report_has_summary_timeline_and_top_events() {
        let html = render_run_report(&run());
        assert!(html.contains("Resumo comparativo"));
        assert!(html.contains("Linha do tempo de saúde"));
        assert!(html.contains("Top eventos"));
        assert!(html.contains("cc_error"));
        assert!(html.contains("1000"), "contagem agregada aparece no top");
        assert!(html.contains("Saúde da probe"));
    }

    /// SPEC-PROBE-020 — os dois feeds aparecem lado a lado e cada um tem sua
    /// faixa de timeline.
    #[test]
    fn spec_probe_020_report_covers_both_feeds_side_by_side() {
        let html = render_run_report(&run());
        assert!(html.contains("0084_CANAL_A"));
        assert!(html.contains("0116_CANAL_B"));
        assert_eq!(
            html.matches("class=\"tl\"").count(),
            2,
            "uma faixa de timeline por feed"
        );
        // Cabeçalho da tabela comparativa tem uma coluna por feed.
        assert!(html.contains("<th>0084_CANAL_A</th><th>0116_CANAL_B</th>"));
    }

    /// RNF-PRB-003 — nome vindo do TOML não injeta markup.
    #[test]
    fn rnf_prb_003_feed_name_is_escaped() {
        let mut snapshot = run();
        snapshot.feeds[0].name = "<script>alert(1)</script>".into();
        let html = render_run_report(&snapshot);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    /// SPEC-PROBE-014 — run sem feeds gera um HTML válido em vez de estourar.
    #[test]
    fn spec_probe_014_empty_run_still_renders() {
        let html = render_run_report(&ProbeSnapshot {
            run_id: "vazio".into(),
            ..Default::default()
        });
        assert!(html.contains("Resumo comparativo"));
        assert!(html.ends_with("</html>"));
    }

    /// SPEC-PROBE-IP-047 · §8 — o relatório carrega a camada de rede e a
    /// contagem por check, que é o insumo do ajuste de limiares na segunda
    /// rodada de 12 h.
    #[test]
    fn spec_probe_ip_047_report_carries_the_network_layer_and_check_tally() {
        let mut f = feed(0, "0084_CANAL_A");
        f.ip = Some(crate::ip::IpTick {
            encapsulation: Encapsulation::RtpFec,
            datagrams: 1_400,
            mbps: 15.0,
            ts_per_datagram: Some(7.0),
            sources: vec!["10.0.0.9:50000".parse().expect("addr")],
            rtp: Some(crate::ip::RtpDelta {
                received: 1_400,
                missing: 3,
                ..Default::default()
            }),
            loss_ratio: Some(2.1e-3),
            iat: crate::IatSummary {
                avg_us: Some(701.9),
                p99_us: Some(1_200.0),
                ..Default::default()
            },
            iat_expected_us: Some(701.9),
            noise_floor_us: Some(120.0),
            fec: crate::ip::FecStatus {
                present: true,
                listening: true,
                l: Some(8),
                d: Some(5),
                streams: 2,
                overhead_pct: Some(12.5),
                ..Default::default()
            },
            ..Default::default()
        });

        let snapshot = ProbeSnapshot {
            feeds: vec![f],
            ..run()
        };
        let html = render_run_report(&snapshot);

        assert!(
            html.contains("<h3>Rede</h3>"),
            "o relatório precisa da camada IP"
        );
        assert!(html.contains("RTP+FEC"));
        assert!(html.contains("10.0.0.9"));
        assert!(html.contains("701.9 µs"));
        assert!(html.contains("8×5"), "a matriz FEC observada");
        assert!(html.contains("piso de ruído"));

        assert!(html.contains("<h3>Checks disparados</h3>"));
        assert!(html.contains("<code>cc_error</code>"));
    }

    /// SPEC-PROBE-IP-043 — num feed UDP puro o relatório **não** mostra faixa
    /// de RTP nem de FEC: uma linha zerada seria uma afirmação que ninguém
    /// verificou.
    #[test]
    fn spec_probe_ip_043_report_omits_rtp_block_for_a_udp_feed() {
        let mut f = feed(0, "PLAYOUT_LOCAL");
        f.encapsulation = Encapsulation::Udp;
        f.ip = Some(crate::ip::IpTick {
            encapsulation: Encapsulation::Udp,
            datagrams: 1_400,
            mbps: 15.0,
            rtp: None,
            ..Default::default()
        });
        let snapshot = ProbeSnapshot {
            feeds: vec![f],
            ..run()
        };
        let html = render_run_report(&snapshot);

        assert!(html.contains("<h3>Rede</h3>"));
        assert!(
            !html.contains("razão de perda"),
            "sem RTP, sem razão de perda"
        );
        assert!(!html.contains("FEC:"), "sem RTP, sem bloco de FEC");
    }

    /// SPEC-PROBE-014 — o top é limitado a 10 e ordenado por severidade.
    #[test]
    fn spec_probe_014_top_events_are_capped_and_ranked() {
        let mut f = feed(0, "x");
        f.events.clear();
        for i in 0..30 {
            f.events.push(EventRow {
                event_id: format!("e{i}"),
                ts_utc: ts(i as i64),
                severity: if i == 29 {
                    Severity::Critical
                } else {
                    Severity::Info
                },
                check_id: "cc_error".into(),
                phase: EventPhase::Open,
                count: i as u64,
                measured: 1.0,
                unit: "errors".into(),
                context: String::new(),
                pid: None,
                service_id: None,
                local: false,
                caused_by: None,
            });
        }
        let top = top_events(&f);
        assert_eq!(top.len(), TOP_EVENTS);
        assert_eq!(top[0].severity, Severity::Critical);
    }
}
